# DockerIX

[![CI](https://github.com/hungryhorace/dockerix/actions/workflows/ci.yml/badge.svg)](https://github.com/hungryhorace/dockerix/actions/workflows/ci.yml)

Simple tool to use Acorn RISC iX within a Docker container. Build and test RISC iX software using modern development workflows. Integrate RISC iX as a container into your Docker environments.

```
$ cd workarea/hello
$ dockerix exec cc -o hello hello.c
$ dockerix exec ./hello
Hello, world from RISC iX
```

```
$ dockerix login
# uname -a
RISC iX riscix 1.2 1c arm
# date
Thu Oct 15 23:32:07 GMT0BST 2026
```


RISC iX 1.21c, Acorn's 1990 BSD Unix for the Archimedes, running headless in
a Docker container under [Arculator](https://b-em.bbcmicro.com/arculator) and
driven from the host. `dockerix start` builds the image and boots RISC iX to
its login prompt with nothing typed into the emulator; `dockerix exec` runs a
command in the guest with a directory of yours shared as its work area,
pushed as a delta before each command or, with `--nfs`, mounted live;
`dockerix login` gives a shell, `get`/`put` copy files either way, and the
guest's X11 clients can display on the host. It runs on Apple Silicon Macs
and amd64 Linux, and needs Docker and either a release binary or a Rust
toolchain.


## Installing

The `dockerix` binary is the whole product: it carries the container
image's sources and builds the image itself, so it needs no checkout.
Either:

- Download the tarball for your platform from the
  [Releases](https://github.com/hungryhorace/dockerix/releases) page
  (`aarch64-apple-darwin` for an Apple Silicon Mac, `x86_64-unknown-linux-gnu`
  for Linux), check it against the `.sha256` beside it, and put `dockerix`
  on your `PATH`:

  ```sh
  curl -LO https://github.com/hungryhorace/dockerix/releases/download/vX.Y.Z/dockerix-vX.Y.Z-aarch64-apple-darwin.tar.gz
  tar xzf dockerix-vX.Y.Z-aarch64-apple-darwin.tar.gz
  install dockerix-vX.Y.Z-aarch64-apple-darwin/dockerix ~/.local/bin/
  ```

  On a Mac, download with `curl` as above rather than a browser: Gatekeeper
  does not quarantine a command-line binary fetched that way. For one a
  browser saved, `xattr -d com.apple.quarantine dockerix` clears the flag.
- Or, with a Rust toolchain, from a checkout: `just install` (`cargo
  install --locked --path cli`), into `~/.cargo/bin`.

## Quick start

With Docker running and `dockerix` installed:

```sh
# Your own RISC OS 3.11 ROM and RISC iX 1.21c disc image (see "What you need"):
mkdir -p ~/.dockerix/assets/roms/riscos311 ~/.dockerix/assets/images
cp /path/to/riscos311-rom-files ~/.dockerix/assets/roms/riscos311/
cp /path/to/riscix121c.hdf ~/.dockerix/assets/images/hdimage.hdf
# Build the image (several minutes, the first time only) and boot RISC iX
# (about a minute), with workarea/ shared into the guest:
dockerix start --workarea workarea
cd workarea/hello
dockerix exec cc -o hello hello.c   # compiled by RISC iX's own cc
dockerix exec ./hello               # Hello, world from RISC iX
dockerix stop                       # halts RISC iX cleanly
```

Nothing has to be typed into the emulator: at the first start RISC OS
configures itself from a boot script served by the container, and the
RISC iX network settings are patched into the disc copy as it is seeded.
Both are redone by `dockerix reset`.

## What you need

Your own RISC OS 3.11 ROM and RISC iX disc image. They are not in the
image: `dockerix start` mounts them read-only from `~/.dockerix/assets`
(`DOCKERIX_ASSETS` or `--assets DIR` for another directory) and refuses to
start if they are missing.

- `roms/riscos311/`: the RISC OS 3.11 ROM image(s). Any names, in any
  number of files; Arculator loads them in name order.
- `images/hdimage.hdf`: the RISC iX 1.21c disc image from
  https://www.4corn.co.uk/articles/riscix121c/ , unzipped and renamed.

The disc image is read only to seed an empty `~/.dockerix/state`, so after
the first boot it is needed again only after `dockerix reset`.

This repository contains no Acorn copyright material and never will: no
ROM, no disc image, nothing from either operating system. RISC OS 3.11 ROM
images are distributed for emulator use by
[RISC OS Open](https://www.riscosopen.org/) under its licence; RISC iX is
not available from anyone holding its rights, and this project makes no
claim about the status of the 4corn image it links to. Please do not open
issues or pull requests that attach either.

## Using it

### The work area

The directory given to `dockerix start --workarea` (or `DOCKERIX_WORKAREA`)
is shared into the guest as `/workarea`. Run `dockerix` from inside it and
the command runs in the guest's copy of the current directory, so after
`dockerix start --workarea workarea`, `cd workarea/game && dockerix exec cc
-o game game.c` compiles in `/workarea/game` on the guest. Without a work
area, commands and logins run in the guest's `/`.

By default the work area flows **one way**: before each command the host
tree is pushed into the guest as a delta (only what changed since the last
push, well under a second), and what the guest builds stays on its disc,
where the next incremental build wants it. Nothing comes back by itself:
`dockerix get <path>` copies a file (or a directory with `-r`) to the host,
`dockerix put` the reverse. A `.dockerixignore` in the work-area root keeps
things out of the push.

With `dockerix start --workarea DIR --nfs` the work area is a **live NFS
mount** instead: edits are visible both ways at once, and guest products
appear on the host by themselves. It is off by default because building on
the guest's own disc is faster for anything but a small tree. Either way,
`dockerix status` says what is shared and how.

### Commands

```
dockerix start [--workarea DIR] [-w GUESTDIR] [--no-vnc] [--profile fast|r140]
               [--nfs] [--x11] [--no-clock-sync] [--clock-interval SECS] [--sessions N]
dockerix stop           clean halt, container removed
dockerix restart        stop, then start again with the same options; options
                        given now win (restart --nfs, --no-x11, …)
dockerix status [-v]    how it was started and whether it is running; exit 0
                        booted, 1 booting, 2 not running
dockerix exec <cmd>     run in the guest, in the guest's copy of the current directory
dockerix exec -t <cmd>  the same with a terminal (vi, curses programs)
dockerix exec -w DIR <cmd>   run in guest directory DIR instead
dockerix exec --no-sync <cmd>   without the work-area push
dockerix exec --hash <cmd>      push by comparing contents, not sizes and times
dockerix exec --timeout SECS <cmd>   give up after SECS (default 3600; 0: never)
dockerix login [-w DIR] interactive root shell (in /workarea, or / without a work area)
dockerix get [-r] <guest-path> [host-path]   copy a guest file (or dir) to the host
dockerix put [-r] <host-path> [guest-path]   the reverse ('-' for stdin/stdout)
dockerix sync           push work-area changes now, running nothing
dockerix sync --clear   empty the guest work area first, then push it all afresh
dockerix run [start options] <cmd>   one-shot for CI: start, run, stop
dockerix ps             what dockerix is running in the guest, and for how long
dockerix console        the noVNC console in the browser (none after --no-vnc)
dockerix x11            host-side relay for guest X clients (needs start --x11)
dockerix reset          discard the guest disc and CMOS (the next start re-seeds them)
dockerix build          build the image now, if it is not already there
dockerix --plain …      one-line progress notices instead of bars; --quiet: none
dockerix --state DIR …  another guest disc and CMOS (default ~/.dockerix/state)
dockerix --assets DIR … another ROMs and disc image directory
dockerix --image TAG …  run that image as is, with no building
```

`dockerix --help` and `dockerix <command> --help` have the detail, and
[how-it-works.md](docs/how-it-works.md) ends with every `DOCKERIX_*`
variable.

Things worth knowing on day one:

- `exec`, `login`, `sync`, `get` and `put` work in a running emulator and
  never start one; `run` refuses if one is already running, since that is
  someone's live session.
- Commands run on guest sessions the container keeps logged in, so a
  command costs its own execution time plus milliseconds, and shell
  variables carry over between commands from the same terminal. Ctrl-C
  interrupts the command in the guest.
- `--profile` picks the emulated machine, an A540 either way: `fast` (the
  default) is an ARM3 at 40 MHz with 16 MB, the fastest Arculator offers;
  `r140` is an ARM2 at 8 MHz with 4 MB, the CPU and memory of an R140,
  which boots in two minutes rather than one and compiles about five times
  slower.
- The guest clock is set from the host after boot and kept in step; RISC
  iX would otherwise believe it is 1992.
- The `start` options are fixed for the container's life; `dockerix
  restart --x11` and the like change them.

### X11

RISC iX ships a full X11 environment, the `xdt` desktop (Motif) included.
With an X server on the host (XQuartz on a Mac):

```sh
dockerix start --x11
dockerix x11 &                     # host-side relay to XQuartz (foreground; Ctrl-C stops)
xhost +local:                      # in an XQuartz terminal, once per XQuartz session
dockerix exec xclock               # a clock appears on your desktop
dockerix exec xdt                  # …or the whole RISC iX X desktop
```

### Plain Docker

The `dockerix` CLI wraps `docker`, and the image works without it:

```sh
image=$(dockerix build)
docker run -d --name dockerix-vm --stop-timeout 60 \
  -p 127.0.0.1:6080:6080 -p 127.0.0.1:42323:42323 \
  -v ~/.dockerix/state:/state -v ~/.dockerix/assets:/assets:ro \
  -v "$PWD/workarea:/workarea" "$image"     # boots RISC iX headless (~1 min)
docker exec dockerix-vm rix uname -a         # run a command in the guest
docker exec -it dockerix-vm rix              # root shell
docker stop dockerix-vm && docker rm dockerix-vm   # halts RISC iX cleanly
```

The ports are bound to loopback because the guest's telnet takes root in
cleartext and noVNC has no password.

## Logging in

RISC iX login: `root` / `Tal540bo` (case sensitive). RISC OS SCSI disc
password, when RISC OS prompts: `Tal540bo`.

Shut RISC iX down with `halt`, never by stopping the emulator, or the
filesystem is left dirty. (`dockerix stop` and `docker stop` do this for
you.)

## Further reading

- [docs/how-it-works.md](docs/how-it-works.md): what stood in the way and
  how each piece is solved — the disc edits, the CMOS and boot script, the
  telnet exec channel, held sessions, the work-area push, the live NFS
  mount and its patches, the guest clock, X11, the console, the Arculator
  bugs — and the configuration reference.
- [docs/architecture.md](docs/architecture.md): how the CLI builds and
  starts the container, and what `dockerix` and `rix` are to each other.
- [patches/README.md](patches/README.md): the Arculator and nfs-user-server
  patches the image applies.
## References

- https://stardot.org.uk/forums/viewtopic.php?t=28042
- https://www.4corn.co.uk/articles/riscix121c/
- https://github.com/evansm7/rixrun

## Licence

GPL-2.0-or-later: see `LICENSE`. The patches under `patches/` are
modifications to Arculator and nfs-user-server, which are GPLv2, and carry
that licence regardless.
