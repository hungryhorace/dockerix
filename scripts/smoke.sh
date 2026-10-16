#!/usr/bin/env bash
# End-to-end smoke test against the real guest (the Jenkins `Smoke` stage).
# Two phases on one checkout, since one boot cannot test both ways of
# sharing the work area:
#
# 1. Push mode: boot a VM on workarea/hello, build and run hello.c in RISC
#    iX, check that an exit status comes back and that `get` fetches the
#    binary.
# 2. The live NFS mount: `restart --nfs`, which must sweep orphaned
#    `.nfsXXXX` files as it starts; an edit on the host reaching `make` in
#    the guest within a second or two (the attribute cache, `actimeo=1`);
#    `put` and `cat` back; `status -v` reporting the server up; `rm` in the
#    guest removing the host's file. Then halt.
#
# Run from the repository root, with this checkout's `dockerix` (cargo run).
#
# Needs the ROMs and disc image in ~/.dockerix/assets (or $DOCKERIX_ASSETS);
# without them it says so and passes, so a checkout without assets is not
# red. The guest's state goes in a scratch directory of its own, seeded
# fresh from the disc image: a disc left dirty by a failed run cannot
# affect the next, and ~/.dockerix/state is never touched.
set -euo pipefail

root=$(pwd)
assets=${DOCKERIX_ASSETS:-$HOME/.dockerix/assets}
if [ ! -d "$assets/roms" ] || [ ! -d "$assets/images" ]; then
    echo "smoke: skipped: no ROMs and disc image in $assets"
    exit 0
fi

dockerix() { cargo run --locked -q --manifest-path "$root/cli/Cargo.toml" -- "$@"; }
fail() {
    echo "smoke: FAILED: $*" >&2
    exit 1
}

# One VM per Docker host (`dockerix-vm`). In CI one left behind by an
# earlier build is stopped (builds on the agent never overlap); anywhere
# else it may be someone's, so refuse.
if docker inspect dockerix-vm >/dev/null 2>&1; then
    if [ -n "${JENKINS_URL:-}" ]; then
        echo "smoke: stopping a dockerix-vm left by an earlier build"
        dockerix stop
    else
        fail "dockerix-vm is running: stop it first (this test needs its own)"
    fi
fi

image=$(dockerix build)
state=$(mktemp -d "${TMPDIR:-/tmp}/dockerix-smoke.XXXXXX")
scratch=$(mktemp -d "${TMPDIR:-/tmp}/dockerix-smoke-put.XXXXXX")
wa=$root/workarea/hello
export DOCKERIX_STATE=$state DOCKERIX_PLAIN=1
cleanup() {
    # By name: after `restart`, the new container.
    dockerix stop >/dev/null 2>&1 || true
    # The container writes /state as root, so empty it from a container.
    docker run --rm -v "$state:/state" --entrypoint sh "$image" -c 'rm -rf /state/* /state/.[!.]*' || true
    rm -rf "$state" "$scratch" "$wa/hello" "$wa/smoke-src" "$wa/smoke-out" "$wa/smoke-rm.c" "$wa/.nfsrc"
    # What the guest's NFS client left renamed (it can hold one for hours);
    # the next `--nfs` start would remove them too.
    find "$wa" -maxdepth 1 -type f -name '.nfs[0-9A-F][0-9A-F][0-9A-F][0-9A-F]' -exec rm -f {} + || true
}
trap cleanup EXIT

dockerix start --no-vnc --workarea "$wa"
cd "$wa"

dockerix exec cc -o hello hello.c
out=$(dockerix exec ./hello)
[ "$out" = "Hello, world from RISC iX" ] || fail "./hello printed '$out'"

rc=0
dockerix exec sh -c 'exit 3' || rc=$?
[ "$rc" -eq 3 ] || fail "'exit 3' in the guest came back as $rc"

dockerix get hello
[ -s hello ] || fail "get hello: nothing arrived"

# Phase 2: the live NFS mount, on the same work area and state.

# An orphan of the guest's NFS client, and a name that only looks like one:
# the `--nfs` start removes the first and must keep the second.
touch .nfs0F43 .nfsrc
dockerix restart --nfs
[ ! -e .nfs0F43 ] || fail "restart --nfs left the orphaned .nfs0F43"
[ -e .nfsrc ] || fail "restart --nfs removed .nfsrc, which is not the client's"

# An edit on the host must reach `make` in the guest. `make` only stats the
# source, and RISC iX caches attributes for up to 60 s, the longer the older
# the file or directory. A lookup fetches them afresh, and the guest looks a
# name up again once its directory has changed. So the source and its
# directory start out old, the edit rewrites the file in place, and the
# binary is built elsewhere: only `actimeo=1` then lets the second `make`
# see the edit a few seconds later (without it, it says up to date).
smoke_c() { printf '#include <stdio.h>\n\nint main()\n{\n    printf("%s\\n");\n    return 0;\n}\n' "$1" >smoke-src/smoke.c; }
mkdir smoke-src smoke-out
printf 'smoke-out/smoke: smoke-src/smoke.c\n\tcc -o smoke-out/smoke smoke-src/smoke.c\n' >smoke-src/smoke.mk
smoke_c "before the edit"
touch -t 202001010000 smoke-src/smoke.c smoke-src/smoke.mk smoke-src
dockerix exec make -f smoke-src/smoke.mk
out=$(dockerix exec smoke-out/smoke)
[ "$out" = "before the edit" ] || fail "smoke-out/smoke printed '$out' before the edit"
sleep 2 # so the edit is newer than the binary, to the second
smoke_c "after the edit"
sleep 2 # past actimeo=1
dockerix exec make -f smoke-src/smoke.mk
out=$(dockerix exec smoke-out/smoke)
[ "$out" = "after the edit" ] || fail "the edit did not reach make in the guest: smoke-out/smoke printed '$out'"

# `put` from outside the work area, onto the guest disc.
echo "put over a live NFS mount" >"$scratch/smoke-put.txt"
dockerix put "$scratch/smoke-put.txt" /tmp/smoke-put.txt
out=$(dockerix exec cat /tmp/smoke-put.txt)
[ "$out" = "put over a live NFS mount" ] || fail "cat of the put file printed '$out'"

status=$(dockerix status -v)
echo "$status"
grep -q '^  sharing *live NFS mount (--nfs)$' <<<"$status" || fail "status -v: not sharing by NFS"
grep -q '^  nfs *server up' <<<"$status" || fail "status -v: the NFS server is not up"
grep -q '^  in guest *NFS share mounted' <<<"$status" || fail "status -v: the guest has not mounted the share"

# A file the guest has just read and then removes: gone from the host
# (the client may keep it a while as a `.nfsXXXX`, which cleanup removes).
cp hello.c smoke-rm.c
dockerix exec sh -c 'cat smoke-rm.c >/dev/null; rm smoke-rm.c'
[ ! -e smoke-rm.c ] || fail "rm in the guest left smoke-rm.c on the host"

dockerix stop
echo "smoke: ok"
