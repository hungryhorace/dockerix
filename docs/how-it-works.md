# How it works

What stood between "Arculator runs RISC iX" and "`dockerix exec make`", and
how each piece is solved. The README says how to use it;
[architecture.md](architecture.md) shows how the pieces are wired. Each
section here is self-contained, so read the one you need.

- [A fresh disc image: the network edits](#a-fresh-disc-image-the-network-edits)
- [CMOS and the boot script](#cmos-and-the-boot-script)
- [The exec channel: telnet into a 1990 Unix](#the-exec-channel-telnet-into-a-1990-unix)
- [Held sessions](#held-sessions)
- [Syncing the work area](#syncing-the-work-area)
- [The live NFS mount](#the-live-nfs-mount)
- [The guest clock](#the-guest-clock)
- [X11 forwarding](#x11-forwarding)
- [The console](#the-console)
- [Arculator bugs](#arculator-bugs)
- [Configuration reference](#configuration-reference)

## A fresh disc image: the network edits

Stock RISC iX has a fixed LAN address and no default route, nameserver or
root-capable network ttys, so the telnet exec channel would not work. When
the pristine `hdimage.hdf` is copied into `/state` at the first start, the
agent applies these edits to the copy (the asset itself is never written):

- `/etc/hosts`: riscix is 10.0.2.15 (SLiRP's guest address); `gateway` is
  10.0.2.2. `rc.net` gives `en0` the hostname's address, so this one line
  moves the guest from its stock 192.168.128.200 onto SLiRP's network, the
  only address SLiRP forwards inbound connections to.
- `/etc/rc.net`: broadcast 10.0.2.255; `route add default 10.0.2.2 1`,
  without which inbound forwarded connections (off-subnet source) can never
  be answered; and a line that creates `/etc/resolv.conf` (`nameserver
  10.0.2.3`) on the first boot, because telnetd's reverse lookup of the peer
  hangs without a nameserver.
- `/etc/ttys`: ttyp0–ttypf marked `secure`, or `login` refuses root on them.

The disc is a 4.3BSD `ffs` image the host cannot otherwise write, so every
edit is a same-length overwrite at a fixed offset: comment lines become the
new lines, values are space-padded, and in `ttys` the type `network` becomes
`net` to make room (it is only ever seen when a telnet client sends no
terminal type; it is not a termcap entry either way). Each edit is checked
against the bytes it expects first, so an image that is not the known
pristine one is left alone with a message, and the equivalent console
recipe is:

```sh
sed 's/192.168.128.200/10.0.2.15/' /etc/hosts > /tmp/h && cp /tmp/h /etc/hosts
echo '10.0.2.2  gateway' >> /etc/hosts
sed 's/192.168.128.0/10.0.2.255/' /etc/rc.net > /tmp/n && cp /tmp/n /etc/rc.net
echo 'route add default 10.0.2.2 1' >> /etc/rc.net
echo 'nameserver 10.0.2.3' > /etc/resolv.conf
sed '/^ttyp/s/network/network secure/' /etc/ttys > /tmp/t && cp /tmp/t /etc/ttys
```

The patch table, with the exact bytes each edit expects to find, is
`cli/src/discpatch.rs`. The edits persist in `/state` with the rest of the
disc; `dockerix reset` discards the copy and the next start seeds and
patches it again.

## CMOS and the boot script

RISC OS boots from Arculator's HostFS, whose root is `/opt/arculator/hostfs`
in the image (the repository's `hostfs/` directory), and runs `hostfs/!Boot`
at every start. That script configures the machine with `*Configure` (SCSI
disc, RISCiXFS root device, the `NoRISCOS` autoboot flag), loads RISCiXFS
from the SCSI disc and boots RISC iX. The boot script is therefore under
host control and a fresh `/state` needs nothing typed at the RISC OS
console.

Booting from HostFS is what keeps the console out of it: the boot script is
a file in the image rather than CMOS bytes or a disc edit, so it is
versioned with the tree, applies to any state directory, and is idempotent.
On a clean CMOS the `RMLoad` only loads RISCiXFS (its Device/Unit/Partition
options exist only once it is loaded), the `Configure` lines set the SCSI
root and `NoRISCOS`, and `RMReInit RISCiXFS` makes it read them and boot the
kernel. On every later start `NoRISCOS` is already set, so loading RISCiXFS
boots RISC iX at once and nothing after that line runs.

The committed `cmos/riscix.riscos3_old.cmos.bin` is Arculator's default A540
CMOS plus what that script configured. Only two settings have to be in the
CMOS before RISC OS will run the script at all, and they are the whole
recipe for rebuilding it from a clean CMOS: at the noVNC console press F12
and type `*Configure FileSystem HostFS` and `*Configure Boot`. The next
start runs the script, which fills in the rest.

The CMOS is reset from the committed file on every boot, not persisted.
RISC iX writes the hardware clock whenever its time is set, and the
emulated PCF8583 holds the year in two bits that round-trip with a +2
error, so a persisted CMOS booted two years further ahead each time the
clock was corrected (see [The guest clock](#the-guest-clock)). Since the
committed CMOS is already fully configured, a clean copy each boot costs
nothing; only the disc image in `/state` persists.

To get a RISC OS desktop instead of a RISC iX boot, start the container
with an empty directory mounted over `/opt/arculator/hostfs`: with no
`!Boot`, RISCiXFS is never loaded and RISC OS stays at the desktop.

## The exec channel: telnet into a 1990 Unix

Commands reach the guest through its own telnetd: Arculator's SLiRP
network forwards container port 42323 to the guest's port 23, and the agent
logs in as root and types. Getting from "telnet connects" to a working root
shell took these fixes, each a surprise:

1. **The guest ships configured for the wrong network.** SLiRP hard-codes
   the guest at 10.0.2.15 and forwards inbound only to that address; the
   stock image has `en0` at 192.168.128.200. Fixed by the `/etc/hosts` edit
   above.
2. **telnetd hangs on reverse DNS of the peer.** It does a `gethostbyaddr`
   before sending anything; with no `/etc/resolv.conf` the resolver stalls
   and the connection looks dead. Fixed by the `resolv.conf` line (SLiRP
   answers DNS at 10.0.2.3).
3. **Root may not log in over the network.** 4.3BSD `login` refuses root on
   any tty not marked `secure`, and the pty lines are not. The correct
   password still gives "Login incorrect". Fixed by the `/etc/ttys` edit.
   The published port is bound to loopback, which is why this is
   acceptable.
4. **No default route.** SLiRP presents a forwarded connection with an
   off-subnet source address (the Docker host's), so with only the
   interface route the guest cannot answer the SYN and the handshake sits
   in `SYN_RCVD`. Fixed by `route add default 10.0.2.2 1`.
5. **Command lines over ~250 bytes deadlock the session.** The 4.3BSD tty
   has a 255-byte canonical input limit, and the pty driver blocks the
   writer, telnetd, rather than discard. So the agent keeps typed lines
   under 200 bytes and stages anything longer into a `/tmp/.rix<mark>`
   script, then runs `sh <script>`.

The guest `/bin/sh` is the pre-POSIX Bourne shell: no `printf(1)`, no
`$(...)`, no `$((...))`, no functions. The agent's framing captures `$?`
first and emits the exit-code sentinel with `echo`, since `echo` clobbers
it; a `cd D && cmd` is guarded with `if test -d D` because the interactive
shell abandons the whole line, sentinel included, on a failed `cd`. A
command that wants POSIX features runs `sh -c` on a shell that has them.

Terminal details the agent takes care of at each login:

- The guest's 1990 termcap knows `xterm` and `vt100` but not modern names
  like `xterm-256color`, so the agent advertises `xterm` for any xterm
  variant and `vt100` otherwise. Override with `DOCKERIX_TERM` (e.g.
  `xterms`).
- RISC iX's telnetd ignores window-size negotiation, so the agent sets the
  tty size with `stty rows/columns` at login. Resizing the terminal
  mid-session is not propagated; run `stty rows N columns M` yourself after
  a resize.
- Backspace: the guest's erase character is already DEL, but its tty has
  the crt echo modes off, so an erased character stayed on screen and
  Backspace looked broken. The agent turns `crtbs crterase crtkill` on.
- Debug the wire with `DOCKERIX_TELNET_TRACE=1` (raw telnet bytes on
  stderr).

The guest's telnet takes root in cleartext and the console has no
password, so both published ports are bound to `127.0.0.1`.

## Held sessions

A telnet login (banner, password, prompt, tty setup) costs 2–3 s on the
emulated machine. Rather than pay it per command, the container's agent
keeps a small pool of logged-in non-tty sessions and listens on a Unix
socket inside the container (`/tmp/dockerix-exec.sock`); `rix` sends the
command there and the output streams back, so a command costs the guest's
own execution time plus a few milliseconds.

- **Affinity.** Commands from the same host terminal reuse the same session
  (`DOCKERIX_SESSION`, the terminal name by default), so shell variables
  carry over: `dockerix exec FOO=1` then `dockerix exec eval 'echo $FOO'`.
  The current directory is set per command from where you run `dockerix`.
  A new client gets a fresh login while the pool has room, and only at the
  limit borrows another client's idle session, state and all.
- **The pool** holds two sessions (`start --sessions N` for more), so a
  long build does not block a second terminal; `dockerix ps` shows a
  command waiting for one. The work-area push stays single-writer across
  sessions; commands themselves run in parallel.
- **Idle reaping.** A session idle for ten minutes
  (`DOCKERIX_SESSION_IDLE`) is logged out to free its guest pty and
  re-established on the next command. A session found dead at checkout
  (guest rebooted, `exit` typed into it) is discarded silently; one left
  in an unknown state by an error or timeout is logged out and dropped.
- **Interactive commands** (`exec -t`) and `dockerix login` always get a
  pty of their own, by a fresh telnet login. `DOCKERIX_DIRECT=1` makes
  every command do that, bypassing the pool.
- **Ctrl-C.** `docker exec` forwards no signals, so Ctrl-C on a plain
  `docker exec … rix` kills the Docker client and the guest command runs
  on. `dockerix` tags each run with an id, catches SIGINT and sends the
  agent an abort for that id; the agent sends the guest tty its interrupt
  character, as the console would, and returns 130. An interrupt during
  the push takes effect once the push has finished (cutting a push short
  would leave the guest shell reading the rest of the archive as
  commands), so a large work area can make Ctrl-C look slow.
- A command is abandoned after an hour; `dockerix exec --timeout SECS` (or
  `run --timeout`) sets another limit, and `--timeout 0` none.

## Syncing the work area

There is no live filesystem share by default (the NFS mount below is the
opt-in exception). Instead the directory given to `dockerix start
--workarea` is bind-mounted into the container at `/workarea`, and before
each command the agent pushes it into the guest's `/workarea` on its own
disc. The work area flows **one way**: the guest's products (`.o` files,
binaries, anything a build writes) stay on the guest disc, where the next
incremental build wants them, and survive across commands and reboots.
Nothing comes back by itself: `dockerix get` copies a file or directory to
the host, `dockerix put` the reverse.

**The push is a delta.** A manifest under `/state/sync/manifest` records
what was last sent (kind, size and mtime per path), so an unchanged tree
pushes nothing (~0.25 s) and a one-line edit pushes one file (under a
second); the first command after a fresh `/state` or `dockerix reset`
pushes everything. Changes are detected by size and mtime; `--hash` (on
`exec`, `run` or `sync`) compares contents instead, for the rare tool that
rewrites a file to the same size and mtime (it reads every file on every
push, and the first push with it on only records the hashes). Deleting or
renaming a source on the host does propagate, but only for paths the push
itself put there, never a guest product. A file `get` lands inside the work
area is remembered as the guest's: the next push leaves it alone until you
edit it, and deleting your copy never deletes the guest's.

**The transport is NFS, with the tty as fallback.** The agent tars the
changed files and stages the archive in an export the guest mounts at
`/tmp/.xfer`; the guest's `tar` unpacks it from there, at a few hundred
KB/s (an 8.5 MB tree in 30 s). `put` goes the same way, and `get` the other
way: the guest's `cpio` writes the archive into the export for the agent to
read (6.5 MB in 13 s). If the guest cannot reach the export, they fall back
to the telnet session, the archive typed down it `uuencode`d at about 10
KB/s (that 6.5 MB took three and a half minutes), and say so
(`DOCKERIX_PUSH_TTY=1` forces that path). The agent keeps the NFS daemons
running: it restarts one that dies, after a wait that grows while it keeps
dying, and logs it (`docker logs dockerix-vm`). A push over
`DOCKERIX_SYNC_MAX` (16 MB) is skipped with a message.

**What is left out.** The push ignores `.git`, `.DS_Store`, `*.swp`, `*~`
and the guest NFS client's leftovers (`.nfs0F43`; see below), and whatever
a `.dockerixignore` in the work-area root lists: one pattern per line, `#`
comments; a pattern without a `/` matches a name anywhere (`*.o`, `build`),
one with a `/` is a path from the root (`doc/*.ps`, `/core`), a trailing `/`
means directories only, and `**` crosses directories where `*` does not. A
path that becomes ignored is forgotten by the push, not deleted on the
guest.

**The guest's 1990 `tar`** sets limits on path length. It unpacks a path of
up to 254 bytes, with no name in it over 100 and a split at a `/` within
the first 153. The push leaves out a longer path, and a socket, fifo or
device, with a warning naming it (`.dockerixignore` it to silence that),
and `put` refuses one. `get` archives with the guest's `cpio` instead,
since its tar writes only paths under 100 bytes; that brings back paths as
long as the guest's `find` lists, about 780 bytes, and a `get` of a tree
holding a longer one fails rather than come back without it.

**`dockerix sync`** is the push on demand, for when you edit on the host and
build in a login shell (a login shell never pushes). It is `dockerix exec
true` in all but name. `sync --clear` starts over: it deletes everything in
the guest's `/workarea`, build products included, and pushes the whole host
work area under a new manifest, for when the guest tree has drifted or you
want a clean build. `exec --no-sync` skips the push for one command
(`DOCKERIX_SYNC=0` for every command); the next push without it catches up.

**Paths.** With a work area, the current directory must be inside it, and
the command runs in the guest's copy of that directory; `dockerix` refuses
with both paths if you are elsewhere. `-w DIR` runs in a guest directory
instead, relative to `/workarea` unless absolute. Guest paths given to
`get` and `put` are relative to the guest's copy of the current directory
unless absolute; the host path defaults to the current directory and works
like `cp`, and `-` is stdin or stdout (`get <path> -` writes the file to
stdout, the archive itself with `-r`; `echo hi | put - <guest-path>` takes
a file's contents from stdin). A file actually named `-` is `./-`.

## The live NFS mount

`dockerix start --workarea DIR --nfs` shares the work area as a live NFS
mount instead of the push: the container runs a small NFSv2 server and the
guest mounts `10.0.2.2:/workarea`, so edits are visible both ways with no
push and no `get`, and a warm command costs about 0.1 s. It is off by
default because building on the guest disc is faster (a one-file rebuild of
a mid-sized C tree: 20 s pushed, 35 s over the mount, where every link
reads its objects over the network), and the delta push costs well under a
second per command. The mount is better when you want guest products to
appear on the host by themselves. In NFS mode nothing is pushed, `get`/`put`
are plain copies, and `sync --clear` refuses, since the guest's `/workarea`
is the host's own directory.

**Why it was hard.** RISC iX is an NFSv2-only client, over UDP, and nothing
modern serves v2: the Docker Desktop kernel's nfsd advertises v3 and up,
and unfs3 is v3-only by name and design. The only userspace NFSv2 server is
jgoerzen's modernised fork of the 1998 `nfs-user-server`, which the image
builds with `-std=gnu89` against today's libtirpc. The same daemons also
carry the push's staging export, so they run in every container; `--nfs`
adds the work-area export.

The patches that made it work are in `patches/nfs-user-server/` and
`patches/arculator/`, each with its README; in short:

- nfs-user-server: build against libtirpc (its `svc_getcaller()` is a
  `sockaddr_in6` buffer now); `listen()` on the TCP rendezvous socket, or
  `rpc.nfsd` spins at 100% CPU the whole time it runs; stable readdir
  cookies and file ids, so a file saved on the host by write-new-and-rename
  (editors do this) reads as rewritten in the guest, not as a stale handle;
  sub-second file times, so a file rewritten at the same size within a
  second is not served from the guest's cache; and `STATFS` in 1 KB blocks
  capped at 2 GB, so `df` on a modern disc does not show negative.
- Arculator: SLiRP's `ip_reass()` segfaulted the emulator on any
  guest-originated datagram spanning more than one mbuf, which an 8 KB NFS
  write is (the QEMU/libslirp fix); and the NE2000 emulation's remote-DMA
  ring wrap corrupted about one frame in 37, so one 8 KB read in six was
  retransmitted after a 2.2 s timeout and reads crawled at 23 KB/s.

The export is `all_squash,anonuid=0,anongid=0,insecure`: the guest mounts
as root, its writes keep the bind mount's ownership (Docker Desktop maps
them to the host user), and `insecure` because SLiRP's NAT rewrites the
guest's reserved source port to a high one. The daemons run with `-r`
(re-export) because `/workarea` is a bind mount, which the server would
otherwise take for an NFS mount and refuse to serve.

Behaviour worth knowing:

- Timestamps are correct on both sides: NFS writes are stamped by the
  server, and the guest clock is set from the container at boot anyway.
- If the guest loses the mount (an `umount` in a login shell, say), the
  next `dockerix exec` mounts it again before running, rather than run in
  the guest disc's own `/workarea` underneath. The guest leaves a file
  `.dockerix-unmounted` there, which shows only while nothing is mounted
  over it; the check costs each command one `test -f`. While the NFS
  server is down the mount retries, and the command waits for it. A login
  shell is left alone, so a `mount` of your own through `dockerix exec`
  stacks a second mount on the first one this makes: remount by hand in
  `dockerix login`.
- Files named like `.nfs0F43` (`.nfs` and four hex digits) can appear in
  the work area. RISC iX's NFS client renames a file instead of removing it
  while it still holds the file (any file read recently: `cat f; rm f`
  leaves one), and removes it once it lets go, which can take hours. Each
  `--nfs` start removes those left from the last one, before the guest
  boots, and the push never sends one. For git:
  `.nfs[0-9A-F][0-9A-F][0-9A-F][0-9A-F]` in `.gitignore`.
- Edits saved on the host reach `make` in the guest within a second. RISC
  iX keeps a file's attributes for up to a minute, the longer the file has
  gone unchanged, and `make` (which looks at them without opening the
  file) missed an edit for 56 s on its default mount. So the mount sets
  `actimeo=1`. That costs a little: a no-op `make` of a mid-sized C tree
  takes 2.1 s, against 1.7 s.
- The switch is read when the container starts; flip it by recreating the
  container (`dockerix restart --nfs`, or `--no-nfs`; `dockerix status`
  says which is in use). `DOCKERIX_NFS_OPTS` overrides the mount options
  (default `intr,actimeo=1,rsize=8192,wsize=8192`; without the SLiRP patch
  writes would have to stay at `wsize=1024`) and `DOCKERIX_NFS_SERVER` the
  server address (default `10.0.2.2`, the SLiRP gateway).
- A guest's mounts carry on across a server restart, after a pause of a
  few seconds.

## The guest clock

RISC iX boots believing it is 1992: the emulated RTC's century is wrong,
and its `date` command takes a two-digit year (`1900 + yy`, underflowing to
1970 for anything below 70), so it cannot be set past 1999 by hand. The
agent fixes this after boot without touching the guest's software: a tiny
RFC 868 time server in the agent listens on TCP 37, and once the guest is
up the agent runs its stock `/usr/sbin/rdate 10.0.2.2` against it, which
calls `settimeofday`. The port is reached only from the guest over SLiRP.

The guest clock runs on the emulator's timer ticks, which stop while the
emulator is frozen (a host sleep, `docker pause`) and do not catch up on
resume: a 90 s freeze left the guest exactly 90 s behind. So the agent
re-syncs every `DOCKERIX_CLOCK_INTERVAL` seconds (`start --clock-interval`,
default 300); after a sleep the clock is behind until the next sync. The
periodic login also gates on guest availability, so it re-corrects the
clock a few minutes after a guest reboot. `start --no-clock-sync` turns the
whole thing off, and `DOCKERIX_TIME_SERVER` points `rdate` elsewhere.

Setting the clock has a side effect: on 4.3BSD `settimeofday` also writes
the hardware clock, so every sync writes the emulated PCF8583, which
Arculator persists into the CMOS file. The chip holds the year in two bits,
and the write→persist→reload→read round-trip lands two years ahead, so a
synced guest once booted as 2028 and stamped its boot-touched directories
with that date. The fix is in [CMOS and the boot
script](#cmos-and-the-boot-script): the CMOS is reset from the committed
file on every boot, so the corrupted RTC never survives a restart. The
boot-touched directories (`/etc`, `/dev`, `/sbin`, `/net`) therefore read
1992 as they always did; everything created after the sync is dated now.

RISC iX's own date handling is fine. Tested by setting the clock across
the boundaries: the year 2000 and its leap day are correct, and the
ceiling is the signed 32-bit `time_t`, 2038-01-19 03:14:07, past which the
clock saturates rather than wrapping to 1901. The "1992" was cosmetic.

## X11 forwarding

RISC iX ships a full X11 environment: the `xdt` desktop (Motif), `twm`,
and ~120 clients in `/usr/bin/X11`. With an X server on the host (XQuartz
on a Mac), `dockerix start --x11` plus `dockerix x11` on the host display
them on your desktop, the whole `xdt` desktop included.

The path is two hops, not one. The guest can only see the container (SLiRP
maps everything to the `10.0.2.2` gateway), and XQuartz listens on a unix
socket the container cannot reach, so the same built-in relay (`dockerix
relay`, `cli/src/relay.rs`, a byte-transparent TCP copier) bridges each
hop: the container forwards `10.0.2.2:6000` → `host.docker.internal:6000`,
and `dockerix x11` on the host forwards that → `/tmp/.X11-unix/X0`. The
guest login exports `DISPLAY=10.0.2.2:0` when X11 is on. X's own protocol
and any authentication ride through untouched.

- `xhost +local:` in an XQuartz terminal is needed because guest clients
  arrive without an X cookie; it allows local connections only (the relay
  reaches XQuartz over its unix socket, so it counts as local) and does not
  open your X server to the network. It applies to the running X server
  only, so re-run it after each XQuartz restart (or add it to XQuartz's
  startup); revert with `xhost -local:`.
- `dockerix x11` binds a local TCP port (6000 by default, `--port`) so the
  Docker VM can reach it, and forwards to `/tmp/.X11-unix/X0` (`--socket`).
  It runs in the foreground; Ctrl-C stops it.
- Latency is fine for real use: drawing is outbound from the guest, the
  fast direction, and the events coming back are tiny.
- The switch is read at container start; enabling it later needs
  `dockerix restart --x11`.
- `xterm` bus-errors (a latent bug in the 1990 build; `xclock`, `xeyes`,
  `xlogo`, Motif apps are fine). For a guest shell use `dockerix exec -t`
  or `dockerix login`, not `xterm`.

## The console

Arculator needs an X display, so the container runs one: Xvfb on `:99`
(`DOCKERIX_SCREEN`, default `1024x768x24`), with nothing listening on TCP.
Unless the console is left out, x11vnc serves that display over VNC on the
container's port 5900 and websockify serves noVNC's web client on port
6080, which `dockerix start` publishes on `127.0.0.1:6080`; `dockerix
console` opens it in the browser. That is the RISC OS and RISC iX screen
and keyboard, for the two-command CMOS recipe above, for watching a boot,
or for a RISC OS desktop. noVNC has no password, which is why the port is
loopback-only.

`dockerix start --no-vnc` leaves x11vnc and noVNC out (`dockerix run`
always does, being for CI); Xvfb still runs, since the emulator draws
regardless. Keyboard injection into the console from the host (`xdotool`)
does not reach RISC OS, so the console steps that remain are typed by hand.

## Arculator bugs

Five bugs in upstream Arculator stood in the way, and the image applies a
patch for each at build time. `patches/arculator/README.md` describes them
properly, with the diagnosis; the short version:

- **SLiRP thread safety.** The network podule drives SLiRP from two threads
  with no locking, and inbound telnet data was intermittently lost
  mid-connection. A mutex around everything but the `select()` wait.
- **Podule sound.** The container has no sound hardware, and the only SDL
  audio driver that does not spin at 100% CPU without one is `dummy`, under
  which only the first device open succeeds. The shared podule sound output
  then returns NULL and the AKA31 podule dereferenced it every 100 ms. Make
  the output functions tolerate NULL.
- **SLiRP IP reassembly.** `ip_reass()` computed a pointer from the wrong
  fragment and segfaulted the emulator on any multi-mbuf datagram, which an
  8 KB NFS write is. The QEMU/libslirp fix.
- **Prefetch-abort pipeline.** The CPU core took a prefetch abort one
  pipeline stage early, so when execution ran off the end of a page into
  one not yet paged in, the last instruction of the resident page was
  skipped. RISC OS never notices (its code is not demand paged); RISC iX
  pages text in all the time, and this showed up as corrupted registers in
  a cross-build project's tests under `-O`.
  [register-corruption.md](register-corruption.md) is the full account.
- **NE2000 remote-DMA ring wrap.** The receive-ring wrap compared a 16-bit
  address with an end of 0x10000, which it can never equal, so a frame
  crossing the end of the ring was read back with its tail from the wrong
  buffer and about one 8 KB NFS read in six was retransmitted after a
  timeout.

These are intended to go upstream as issues or pull requests; until then
the Dockerfile fetches Arculator at a pinned commit and applies them in
order.

## Configuration reference

Every choice a user makes has a command-line option, and the option wins
when both are given; the variables remain for scripts and CI. The rest are
tuning, debugging aids, or how the image is put together.

They fall into four groups by who reads them. Booleans are on with `1`
(or off with `0` where the default is on).

**Read by the `dockerix` CLI on the host.** These never reach the container.
The options are global: they go before or after the command name.

| Variable | Default | Option | What it does |
|---|---|---|---|
| `DOCKERIX_WORKAREA` | none | `start`/`run --workarea DIR` | Directory to share as the work area |
| `DOCKERIX_STATE` | `~/.dockerix/state` | `--state DIR` | Guest disc and CMOS, mounted at `/state`; a second one keeps a scratch guest apart |
| `DOCKERIX_ASSETS` | `~/.dockerix/assets` | `--assets DIR` | ROMs and pristine disc image, mounted read-only at `/assets` |
| `DOCKERIX_IMAGE` | `dockerix:<hash>`, built from the binary's sources | `--image TAG` | Image to run as is, with no building |
| `DOCKERIX_PLAIN` | off | `--plain` | One-line progress notices instead of bars |

**Read by the container's agent when it starts.** Fixed for the
container's life. The "From the host" column says how `dockerix start`,
`run` and `restart` set each one: the options there, or the variable passed
from the shell; "`docker run -e` only" means they never pass it, so it
takes plain `docker run -e` (a host shell setting is ignored). `restart`
keeps what the container was started with unless an option says otherwise.

| Variable | Default | From the host | What it does |
|---|---|---|---|
| `DOCKERIX_NFS` | `0` | `--nfs` / `--no-nfs`, or from the shell | Live NFS mount of the work area instead of the push |
| `DOCKERIX_X11` | `0` | `--x11` / `--no-x11`, or from the shell | X11 forwarding: container relay and guest `DISPLAY` |
| `DOCKERIX_SET_CLOCK` | `1` | `--clock-sync` / `--no-clock-sync`, or from the shell | Set the guest clock after boot |
| `DOCKERIX_CLOCK_INTERVAL` | `300` | `--clock-interval SECS`, or from the shell | Seconds between guest clock re-syncs |
| `DOCKERIX_VNC` | on | `start --no-vnc` sets `0` (`run` always does) | noVNC console on port 6080 |
| `DOCKERIX_PROFILE` | `fast` | `start`/`run --profile fast\|r140` | Emulated machine: CPU, MEMC and RAM |
| `DOCKERIX_WORKDIR` | none | `start -w DIR` | Default guest directory for logins and commands (see also below) |
| `DOCKERIX_NFS_OPTS` | `intr,actimeo=1,rsize=8192,wsize=8192` | `docker run -e` only | Guest NFS mount options |
| `DOCKERIX_NFS_SERVER` | `10.0.2.2` | `docker run -e` only | NFS server address the guest mounts from |
| `DOCKERIX_TIME_SERVER` | `10.0.2.2` | `docker run -e` only | Address the guest's `rdate` asks for the time |
| `DOCKERIX_X11_DISPLAY` | `10.0.2.2:0` | `docker run -e` only | The guest's `DISPLAY` when X11 is on |
| `DOCKERIX_SESSIONS` | `2` | `--sessions N`, or from the shell | Guest sessions the agent holds logged in: how many commands can run at once |
| `DOCKERIX_SESSION_IDLE` | `600` | `docker run -e` only | Seconds before an idle held session is logged out |
| `DOCKERIX_PUSH_TTY` | `0` | `docker run -e` only | Push, `put` and `get` through the telnet session instead of staging over NFS (a debugging aid) |
| `DOCKERIX_SCREEN` | `1024x768x24` | `docker run -e` only | Xvfb screen the emulator draws on |
| `DOCKERIX_CONFIG` | `riscix` | `docker run -e` only | Arculator machine config name |
| `DOCKERIX_TELNET` | `127.0.0.1:42323` | `docker run -e` only | Guest telnet address, as seen from the container |
| `DOCKERIX_USER` / `DOCKERIX_PASS` | `root` / `Tal540bo` | `docker run -e` only | Guest login |
| `DOCKERIX_BOOT_TIMEOUT` | `1800` | `docker run -e` only | One-shot `docker run <image> <cmd>`: seconds to wait for boot |
| `DOCKERIX_EXEC_TIMEOUT` | `3600` | `exec`/`run --timeout SECS`, or from the shell (per command, below) | Seconds before a command is abandoned, `0` for no limit |

**Read by `rix` in the container, per command.** `dockerix exec`, `login`
and `sync` pass each one from the host shell when it is set, and an option
given in its place; with plain `docker exec` they are `-e` options.
`DOCKERIX_EXEC_TIMEOUT` (above) is read here too. The ones with no option
are tuning and debugging aids.

| Variable | Default | Option | What it does |
|---|---|---|---|
| `DOCKERIX_SYNC` | on | `exec --no-sync` sets `0` | Push the work area before the command |
| `DOCKERIX_SYNC_MAX` | 16 MB | — | Push size limit in bytes; a bigger push is skipped |
| `DOCKERIX_SYNC_HASH` | `0` | `exec`/`run`/`sync --hash` | Compare file contents, not just size and mtime |
| `DOCKERIX_SESSION` | the host terminal's name | — | Session-affinity key for held sessions |
| `DOCKERIX_DIRECT` | `0` | — | Log in per command instead of using a held session |
| `DOCKERIX_PROGRESS` | `bar` on a terminal, else `plain` | `--plain` makes it `plain`, `--quiet` `0` | Transfer progress: `bar`, `plain`, or `0` for none (`get`/`put` too) |
| `DOCKERIX_WORKDIR` | the `start -w` directory | `exec`/`login -w DIR` | Guest directory for this command |
| `DOCKERIX_TERM` | `xterm` or `vt100`, from `TERM` | — | Terminal type advertised to the guest |
| `DOCKERIX_TELNET_TRACE` | `0` | — | Dump the raw telnet bytes on stderr |

`DOCKERIX_TERM` and `DOCKERIX_TELNET_TRACE` reach only the logins `rix` makes
itself: `-t` commands, `dockerix login`, and `DOCKERIX_DIRECT=1`. A
non-tty command runs on a session the agent logged in with the container's
own environment.

**Internal.** Set by the CLI, or path overrides kept for testing (nothing sets
them now); not for setting by hand.

| Variable | Set by | What it does |
|---|---|---|
| `DOCKERIX_SUBDIR` | `exec`, `get`, `put` | The host cwd relative to the work area, for the guest cwd |
| `DOCKERIX_RUN_ID` | `exec` | Run id so Ctrl-C can interrupt the guest command (`rix --abort`) |
| `DOCKERIX_SYNC_CLEAR` | `sync --clear` | Empty the guest work area and start a new manifest |
| `DOCKERIX_SOCK` | — | Agent's exec socket (`/tmp/dockerix-exec.sock`) |
| `DOCKERIX_CONTAINER_WORKAREA` | — | Work-area mount point in the container (`/workarea`) |
| `DOCKERIX_GUEST_WORKAREA` | — | Work area in the guest (`/workarea`) |
| `DOCKERIX_SYNC_MANIFEST` | — | Push manifest (`/state/sync/manifest`) |
| `DOCKERIX_STAGE` | — | Where transfers stage their archives, exported to the guest (`/xfer`) |
