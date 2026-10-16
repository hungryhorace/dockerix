# syntax=docker/dockerfile:1

# ---- build stage: Arculator at a pinned upstream commit ----------------------
FROM ubuntu:26.04 AS build

ENV DEBIAN_FRONTEND=noninteractive
RUN apt-get update && apt-get install -y --no-install-recommends \
        build-essential autoconf automake libtool pkg-config patch \
        git ca-certificates \
        libsdl2-dev libwxgtk3.2-dev zlib1g-dev libasound2-dev libx11-dev \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /src/arculator
# A shallow fetch of the pinned commit (`clone --depth 1` would get the branch
# tip instead). The full SHA pins the source as a checksum would.
ARG ARCULATOR_REPO=https://github.com/sarah-walker-pcem/arculator.git
ARG ARCULATOR_COMMIT=7d3d2679589bba759197e3a9288a12749c797381
RUN git init -q . && git fetch -q --depth 1 "$ARCULATOR_REPO" "$ARCULATOR_COMMIT" \
    && git checkout -q FETCH_HEAD && rm -rf .git
# Local fixes on top of upstream (see patches/arculator/README.md).
COPY patches/arculator/ /src/patches/
RUN for p in /src/patches/*.patch; do patch -p1 < "$p" || exit 1; done
# GCC 15 defaults to C23, where `f()` means `f(void)`; upstream still uses
# K&R-style empty prototypes, so build as gnu17.
RUN autoreconf -fi \
    && ./configure --enable-release-build --enable-podules CFLAGS="-std=gnu17" \
    && make -j"$(nproc)"

# ---- podule ROMs from the upstream binary release ------------------------------
# The podule ID/firmware ROMs (aka31 "scsirom", aeh50 "EthernetII_ID_ROM.ROM",
# etc.) are not in the git tree; they ship in the Arculator 2.2 release tarball.
FROM ubuntu:26.04 AS podule-roms
# A pipe fails if any part of it does, not only the last.
SHELL ["/bin/bash", "-o", "pipefail", "-c"]
ARG ARC_RELEASE_URL=https://b-em.bbcmicro.com/arculator/Arculator_V2.2_Linux.tar.gz
ARG ARC_RELEASE_SHA256=14ac9aeb12675eda69e596acc944e564867d04043c25a5074e8d64d1d9c40e7a
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates curl \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /rel
RUN curl -fsSL -o arc.tgz "$ARC_RELEASE_URL" \
    && echo "$ARC_RELEASE_SHA256  arc.tgz" | sha256sum -c - \
    && tar xzf arc.tgz --wildcards 'podules/*' 'roms/podules/*' \
    && find podules -type f \( -name '*.so' -o -name '*.c' -o -name '*.h' -o -name 'Makefile*' \) -delete \
    && rm -rf podules/*/src podules/common

# ---- CLI/agent build stage ----------------------------------------------------
# The `dockerix` binary is the container entrypoint, healthcheck, graceful-halt
# hook and exec channel (and, symlinked as `rix`, the in-guest exec shortcut).
# Built natively for the image's architecture (linux/arm64 on Apple Silicon).
# Rust pinned to the minor CI tests on, at or above `rust-version` in
# cli/Cargo.toml; move them together.
# The same minor as .github/workflows/ci.yml's toolchain; move them together.
FROM rust:1.96-slim AS cli-build
RUN apt-get update && apt-get install -y --no-install-recommends gcc libc6-dev \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /src/cli
# Compile the dependencies into a cached layer first: the manifests, build.rs
# and the one source it packs (src/context.rs — compiled by build.rs with a
# `#[path]` mod, and empty here, since the agent never builds an image), plus
# a stub main, then `cargo build --release`. A later source-only change leaves
# this layer — and so clap, rustix and the rest — untouched; only the crate
# itself recompiles below. Keep `target/` between the two builds so the real
# build reuses the cached dependencies and the build script's OUT_DIR output.
COPY cli/Cargo.toml cli/Cargo.lock cli/build.rs ./
COPY cli/src/context.rs ./src/context.rs
RUN echo 'fn main() {}' > src/main.rs && cargo build --release
# The real sources, built against the cached dependencies. `COPY` gives them
# mtimes that can look older than the stub build, so cargo would think the
# crate is already built and ship the stub; `touch` them so it rebuilds the
# crate (the dependencies, untouched, stay cached).
COPY cli/src ./src
RUN find src -name '*.rs' -exec touch {} + && cargo build --release

# ---- NFSv2 server: jgoerzen's 1998 nfs-user-server (push staging, live mount) -
# RISC iX is an NFSv2/UDP client and nothing modern serves v2 (the linuxkit
# kernel offers v3/v4 only, unfs3 is v3-only); see docs/how-it-works.md,
# "The live NFS mount". This fork is
# the only userspace NFSv2 server that still builds — against glibc/tirpc with
# the patches in patches/nfs-user-server/. The agent always runs it, to carry
# the work-area push, put and get (a staging export), and with --nfs also
# exports the work area itself as a live mount.
FROM ubuntu:26.04 AS nfs-build
SHELL ["/bin/bash", "-o", "pipefail", "-c"]
ENV DEBIAN_FRONTEND=noninteractive
RUN apt-get update && apt-get install -y --no-install-recommends \
        build-essential autoconf ca-certificates curl libtirpc-dev patch \
    && rm -rf /var/lib/apt/lists/*
# Pinned to a commit of https://github.com/jgoerzen/nfs-user-server (the
# modernised 2.2beta47 fork). The tarball is the GitHub archive of that commit.
ARG NFS_URL=https://github.com/jgoerzen/nfs-user-server/archive/ba6ee936acc8e295dcaf0ade01c0f77718f7ef7e.tar.gz
ARG NFS_SHA256=d9d7ffa25e629325033a20e563cce8df4d83bc27edf3e42126a537f9081b89b4
WORKDIR /src/nfs
RUN curl -fsSL -o nfs.tgz "$NFS_URL" \
    && echo "$NFS_SHA256  nfs.tgz" | sha256sum -c - \
    && tar xzf nfs.tgz --strip-components=1 && rm nfs.tgz
COPY patches/nfs-user-server/ /src/patches/
RUN for p in /src/patches/*.patch; do patch -p1 < "$p" || exit 1; done
# BUILD writes site.h/config.h.in non-interactively (--batch) and runs the
# autoconf configure itself; features we do not use are off. CFLAGS/LIBS are
# exported so that configure inherits them: gnu89 because the K&R source (and
# autoconf 2.13's own "am I cross-compiling?" probe, `main(){return(0);}`) is
# rejected by gcc's C23 default — without it the probe fails to compile, every
# sizeof test then "can not run test program while cross compiling", and the
# build aborts; tirpc for the RPC headers/lib; HAVE_XDRPROC_T so system.h does
# not redeclare the typedef libtirpc already provides. Then the three binaries.
ENV CFLAGS="-std=gnu89 -O2 -I/usr/include/tirpc -DHAVE_XDRPROC_T"
ENV LIBS="-ltirpc"
RUN ./BUILD --batch --multi=no --devtab=no --ugidd=no --nis=no \
        --hosts-access=no --log-mounts=no \
    && make rpc.nfsd rpc.mountd showmount

# ---- runtime stage ------------------------------------------------------------
FROM ubuntu:26.04

ENV DEBIAN_FRONTEND=noninteractive
RUN apt-get update && apt-get install -y --no-install-recommends \
        libsdl2-2.0-0 libwxgtk3.2-1t64 zlib1g libasound2t64 \
        xvfb x11vnc novnc websockify xdotool x11-apps \
        netcat-openbsd inetutils-telnet \
        rpcbind libtirpc3t64 \
        tini \
    && rm -rf /var/lib/apt/lists/*

# The NFSv2 daemons for the optional live mount (DOCKERIX_NFS=1).
COPY --from=nfs-build /src/nfs/rpc.nfsd /src/nfs/rpc.mountd /usr/local/sbin/
COPY --from=nfs-build /src/nfs/showmount /usr/local/bin/showmount

# Arculator resolves roms/, podules/, configs/, cmos/ relative to its binary.
WORKDIR /opt/arculator
COPY --from=build /src/arculator/arculator ./arculator
COPY --from=build /src/arculator/podules ./podules
RUN rm -rf podules/*/src podules/common
COPY --from=podule-roms /rel/podules ./podules
COPY --from=podule-roms /rel/roms/podules ./roms/podules
COPY --from=build /src/arculator/cmos ./cmos
COPY --from=build /src/arculator/ddnoise ./ddnoise
COPY --from=build /src/arculator/roms ./roms
COPY --from=build /src/arculator/hostfs ./hostfs
# RISC OS boot script (HostFS::$.!Boot): configures CMOS and boots RISC iX.
COPY hostfs/ ./hostfs/
COPY --from=build /src/arculator/configs ./configs

# Baked machine config and, once captured, its RISC OS CMOS.
COPY configs/ ./configs/
COPY cmos/ ./cmos/

COPY container/asound.conf /etc/asound.conf
# Host directory shared into the guest: pushed over the exec channel as a
# delta before each command, or a live NFSv2 mount with DOCKERIX_NFS=1
# (docs/how-it-works.md, "Syncing the work area" and "The live NFS mount").
RUN mkdir -p /workarea

# The agent binary is the entrypoint and, as `rix`, the in-guest exec shortcut.
COPY --from=cli-build /src/cli/target/release/dockerix /usr/local/bin/dockerix
RUN ln -sf dockerix /usr/local/bin/rix

# SDL_AUDIODRIVER=dummy: there is no sound hardware, and with ALSA's null
# plugin SDL's audio threads spin at 100% CPU each (null writes return at once,
# nothing paces them). The dummy driver sleeps per buffer instead. The AKA31
# podule's second device open fails under it, which patches/arculator/0002
# makes harmless.
ENV DISPLAY=:99 \
    DOCKERIX_CONFIG=riscix \
    DOCKERIX_SCREEN=1024x768x24 \
    DOCKERIX_NFS=0 \
    DOCKERIX_SET_CLOCK=1 \
    DOCKERIX_X11=0 \
    SDL_MOUSE_RELATIVE_MODE_WARP=1 \
    SDL_AUDIODRIVER=dummy

VOLUME /state
# The user-supplied ROMs (roms/<set>/) and pristine disc image
# (images/hdimage.hdf) are not in the image: mount them read-only at /assets.
# The entrypoint copies the ROMs into roms/ and seeds /state from the disc.
# 6080 noVNC console; 42323 SLiRP-forwarded telnet straight into the guest.
EXPOSE 6080 42323

# Healthy once the guest login banner answers. A hands-off cold boot takes about
# a minute; allow much longer for the fsck after an unclean stop.
HEALTHCHECK --interval=30s --timeout=10s --start-period=10m --retries=3 \
    CMD ["/usr/local/bin/dockerix", "agent", "wait", "--timeout", "5"]

# No trailing command -> persistent guest. `docker run -v DIR:/assets:ro
# dockerix <cmd>` appends the command here, triggering one-shot mode (boot,
# run, halt).
ENTRYPOINT ["tini", "--", "/usr/local/bin/dockerix", "agent", "entrypoint"]
