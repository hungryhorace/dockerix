# Architecture

How the `dockerix` binary builds and starts the container, and what
happens between `dockerix exec make` on the host and `make` running in RISC
iX. [how-it-works.md](how-it-works.md) explains each mechanism; this page
is the map.

## One binary, three roles

There is one Rust crate (`cli/`) and one binary, `dockerix`. It is built
twice: for the host as the CLI you install, and for Linux
inside the image, where it is both the container's entrypoint and, under
the name `rix`, the in-container command runner.

| Where | Invoked as | Role |
|---|---|---|
| Host | `dockerix start`, `exec`, `login`, `get`, `put`, … | The CLI: builds the image, runs the container, maps your directory and command onto a `docker exec` |
| Container, PID 1 | `dockerix agent entrypoint` (under `tini`) | The agent: seeds `/state`, starts the display, the NFS and time servers and Arculator, holds the guest sessions, halts cleanly on `docker stop` |
| Container, per command | `rix` (a symlink to `dockerix`; the `exec` subcommand) | Runs one command in the guest: over the agent's socket for a plain command, by its own telnet login for a tty |

`dockerix` on the host never talks to the guest itself. Every guest
operation is a `docker exec dockerix-vm rix …` with the host's settings
passed in the environment, so plain Docker users get the same `rix`:

```sh
docker exec dockerix-vm rix uname -a      # a command
docker exec -it dockerix-vm rix           # a root shell
```

## Building the image

The host binary carries the image's sources. At build time `cli/build.rs`
packs the files the Dockerfile copies (the Dockerfile itself, `patches/`,
`configs/`, `cmos/`, `hostfs/`, `container/` and the crate) into a
deterministic tar, and the binary embeds it with its sha256. So an
installed `dockerix` needs no checkout:

```
dockerix start
  │  embedded context.tar (sha256 → tag)
  │
  ├─ docker image inspect dockerix:<hash12>      exists? run it
  │
  └─ docker build -t dockerix:<hash12> -         (several minutes, once)
       ├─ build stage:       Arculator at a pinned commit + patches/arculator/
       ├─ podule-roms stage: podule ROMs from Arculator's release tarball
       ├─ nfs-build stage:   nfs-user-server + patches/nfs-user-server/
       ├─ cli-build stage:   this crate, for Linux (the agent and rix)
       └─ runtime stage:     ubuntu + Xvfb, x11vnc, noVNC, rpcbind, tini
                              /opt/arculator (binary, podules, hostfs/!Boot,
                              configs/riscix.cfg, cmos/), /usr/local/bin/dockerix
```

The tag is the content hash, so an updated binary whose sources differ
builds a new image and `dockerix status` warns while an older one is still
running. `dockerix build` does the build ahead of time; `--image TAG`
(`DOCKERIX_IMAGE`) runs a named image with no building.

## Starting the container

```
Host                                       Container  dockerix-vm
────                                       ──────────────────────
dockerix start --workarea DIR [--nfs …]
  │ check ~/.dockerix/assets (ROMs, hdimage.hdf)
  │ ensure the image (above)
  │
  ├─ docker run -d --name dockerix-vm --stop-timeout 60
  │     -v ~/.dockerix/state:/state            guest disc + CMOS (persists)
  │     -v ~/.dockerix/assets:/assets:ro       ROMs + pristine disc
  │     -v DIR:/workarea                       the work area (optional)
  │     -p 127.0.0.1:6080:6080                 noVNC console (unless --no-vnc)
  │     -p 127.0.0.1:42323:42323               telnet into the guest
  │     -e DOCKERIX_NFS=… DOCKERIX_X11=… …     the start options
  │     dockerix:<hash12>
  │                                            tini → dockerix agent entrypoint
  │                                              lock /state (one container per disc)
  │                                              seed /state/hdimage.hdf from /assets,
  │                                                patch its network config (first time)
  │                                              reset /state/cmos.bin from the committed CMOS
  │                                              copy /assets/roms → roms/
  │                                              Xvfb :99 ─ x11vnc ─ websockify :6080
  │                                              rpc.nfsd + rpc.mountd (exports /xfer; /workarea with --nfs)
  │                                              RFC 868 time server :37
  │                                              X11 relay :6000 → host.docker.internal:6000 (--x11)
  │                                              ./arculator riscix
  │                                                └─ RISC OS 3.11 ← hostfs/!Boot → RISCiXFS → RISC iX
  │                                                     SLiRP: guest 10.0.2.15, gateway 10.0.2.2
  │                                                     :42323 → guest telnetd
  │                                              exec socket /tmp/dockerix-exec.sock
  │                                              session pool: login, hold N sessions
  │                                              clock keeper: rdate at boot, every 300 s
  │
  └─ wait for HEALTHCHECK healthy              dockerix agent wait: the login banner on :42323
     (about a minute)
```

`docker stop` (what `dockerix stop` runs) sends SIGTERM: the agent drains
the session pool, logs in and runs `halt` so the disc is left clean, then
stops Arculator. `dockerix restart` reads the running container's mounts
and environment back and recreates it with the same options, plus any
given now.

`docker run dockerix:<hash> <cmd>`, with a trailing command, is the
one-shot path: the same entrypoint boots, runs the command once, halts and
exits with its status. `dockerix run` is the CLI's one-shot form and does
not use it; it is `start`, `exec`, `stop` in sequence, so it shares every
code path with the persistent one.

## Running a command

```
Host                                  Container                      Guest (RISC iX)
────                                  ─────────                      ───────────────
$ cd DIR/game
$ dockerix exec cc -o game game.c
  │ cwd inside the work area?
  │ container running and healthy?
  │ tag the run with DOCKERIX_RUN_ID
  │
  └─ docker exec -e DOCKERIX_SUBDIR=game -e … dockerix-vm rix cc -o game game.c
                                      rix: connect /tmp/dockerix-exec.sock
                                        frames: command, sync, key, dir, go
                                      agent: pick this terminal's session
                                        (or a free one, or log a new one in)
                                        push the work area delta ──────▶ /tmp/.xfer  (NFS)
                                          tar changed files → /xfer       tar xf … in /workarea
                                        type on the session's tty ──────▶ cd /workarea/game &&
                                                                          cc -o game game.c;
                                                                          echo SENTINEL$?
                                        stream output back  ◀──────────── (telnet :42323)
                                      rix: write output, exit with the status
  │ Ctrl-C? → rix --abort <id>  ────▶ agent sends the tty ^C
```

The three paths through `rix`:

- **Plain command** (`dockerix exec cmd`): the agent's socket and a held
  session, as drawn. Output streams; the exit status is the guest's.
- **Terminal** (`dockerix exec -t cmd`, `dockerix login`): `rix` makes its
  own telnet login, puts the host terminal in raw mode and copies bytes
  both ways, so `vi` and curses programs work. It still pushes first
  (`login` does not) and tells the agent, so `dockerix ps` lists it.
- **Transfers** (`get`, `put`): `docker exec dockerix-vm dockerix agent
  get|put`, on a telnet session of their own rather than the pool (a
  deliberate transfer is rare enough that the login does not matter).
  `get` has the guest's `cpio` write an archive into the export for the
  agent to read; `put` is the push's transport for one path. `dockerix
  sync` is a plain command (`true`) with the push forced on.

With `--nfs` the push step disappears: `/workarea` in the guest is a mount
of the container's `/workarea`, which is the host's directory, and the
command simply runs.

## What lives where

| Path | Host | Container | Guest |
|---|---|---|---|
| The work area | `DIR` (`--workarea`) | `/workarea` (bind mount) | `/workarea` on the guest disc, pushed; or the NFS mount of the container's |
| Guest disc and CMOS | `~/.dockerix/state` | `/state` | `sd0`, the SCSI disc |
| ROMs and pristine disc | `~/.dockerix/assets` | `/assets` (read-only) | — |
| Transfer staging | — | `/xfer` (NFS export) | `/tmp/.xfer` |
| Push manifest | `~/.dockerix/state/sync/manifest` | `/state/sync/manifest` | — |
| Exec socket | — | `/tmp/dockerix-exec.sock` | — |

| Port | Container | Published on the host |
|---|---|---|
| Guest telnetd (via SLiRP) | 42323 | `127.0.0.1:42323` |
| noVNC console | 6080 (VNC itself on 5900) | `127.0.0.1:6080` |
| NFS, time server, X11 relay | 2049/111, 37, 6000 | not published; reached from the guest at 10.0.2.2 |

## The crate

`cli/src`, by role:

- Host: `host.rs` and `host/` (`image.rs` builds, `spec.rs` composes
  `docker run` and reads a container back, `rix.rs` maps a command onto
  `docker exec`, `transfer.rs` the host side of `get`/`put`, `interrupt.rs`
  Ctrl-C), `docker.rs` (the `docker` CLI), `relay.rs` and `main.rs`.
- Agent: `agent.rs` (entrypoint, seed, halt), `discpatch.rs` (the disc
  edits), `nfs_server.rs`, `timeserver.rs`, `serve.rs` and `serve/` (the
  socket, the pool, `dockerix ps`).
- Guest side of a command: `exec.rs` (run a command on a session, with the
  push and the NFS mount check), `session.rs` and `telnet.rs` (log in,
  type, read), `sync.rs` and `sync/` (the delta, manifest, ignore rules,
  transport), `transfer.rs` (`get`/`put` in the container), `uu.rs` and
  `tty.rs`.
- `context.rs` is the build-context packer, shared with `build.rs`;
  `config.rs` reads every `DOCKERIX_*` variable in one place.
