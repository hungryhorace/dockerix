# Patches to build nfs-user-server in the image

Applied with `patch -p1` from the source root of the pinned
[jgoerzen/nfs-user-server](https://github.com/jgoerzen/nfs-user-server)
tarball (the modernised 2.2beta47 fork), in the `nfs-build` stage of the
`Dockerfile`. This is the only userspace NFS**v2** server that still builds,
and RISC iX is an NFSv2-only client; see `docs/how-it-works.md`, "The live
NFS mount", for why nothing else in the container can serve the live
`/workarea` mount.
nfs-user-server is GPL, so these patches are modifications to it under the
GPL (the rest of this repository is GPL-2.0-or-later; see `LICENSE`).

The commit and tarball SHA are pinned in the `Dockerfile` (`NFS_URL`,
`NFS_SHA256`). The agent always runs the daemons: they carry the work-area
push, `put` and `get` (a staging export the guest unpacks from and archives
into), and with `--nfs` the live work-area mount too.

- `0001-build-with-libtirpc.patch` — two changes so the 1998 K&R source links
  against modern glibc + libtirpc:
  - `svc_getcaller()` in libtirpc's `svc_soc.h` returns the transport's
    `xp_raddr`, now a `sockaddr_in6`-sized buffer, so `->sin_addr`/`->sin_port`
    no longer compile. Redefine it to cast that buffer to `sockaddr_in` (the
    IPv4 sockets the server uses), matching the old ABI the code expects.
  - Add `<time.h>` to `system.h` so `struct tm` / `localtime()` in `logging.c`
    are declared (C89 got them implicitly from other headers; C23 does not).

  Built as `-std=gnu89` with `-DHAVE_XDRPROC_T` (libtirpc already provides
  `xdrproc_t`, so `system.h` must not redeclare it).

- `0002-listen-on-tcp-rendezvous-socket.patch` — makes `makesock()` in
  `rpcmisc.c` call `listen()` on the TCP socket it binds. `rpc.nfsd` binds its
  own socket to a fixed port and hands it to `svctcp_create()`; the old Sun RPC
  `svctcp_create()` would `listen()` on such a socket, but libtirpc's only does
  so for a socket it creates itself (`RPC_ANYSOCK`). Left un-listened, the TCP
  rendezvous fd reports `POLLHUP`, `svc_run()`'s `accept()` returns `EINVAL`,
  the poll event never clears, and the daemon **spins at 100% CPU** the whole
  time `DOCKERIX_NFS=1` (even idle). `rpc.mountd` is unaffected because it
  passes port 0, so its socket is the `RPC_ANYSOCK` one libtirpc listens itself.
  Confirmed by strace (`ppoll` → `accept … EINVAL` loop) and fixed to a
  blocking `ppoll` at 0% CPU.

- `0003-stable-readdir-cookies-and-file-ids.patch` — the server assumed a
  1998 Linux disc in two ways that a Docker Desktop bind mount (FUSE,
  virtiofs) breaks, and the first real build over the mount wedged:
  - **READDIR cookies.** It sent `telldir()` positions as cookies and
    `seekdir()`ed a fresh stream to them on the next call. The bind mount
    ignores that seek, so a client asking for the rest of a directory (any
    of more than ~75 entries) got the start of it again for ever: `ls` died
    "out of memory", `make clean`'s glob hung. (ext4 and overlayfs give
    64-bit hash positions a 32-bit cookie truncates, which fails as badly.)
    New `efsdir.c` reads a directory into memory and counts positions; a
    listing (a READDIR at cookie 0) keeps its snapshot for the calls that
    continue it, so `rm -r` empties a large directory while deleting from it.
  - **File identity.** File handles and fileids came from `lstat()`'s inode
    number, but on the bind mount that is a number the FUSE server deals
    out in lookup order and deals out afresh once the kernel forgets the
    file; only `readdir()`'s `d_ino` is the Mac's real, lasting inode. So
    handles went stale at random, READDIR's fileids never matched GETATTR's
    (`pwd` failed "read error in .."), and `fh_buildpath()`, which rebuilds
    a handle's path by matching `d_ino`s (and backtracked with `seekdir()`,
    looping for ever here), could never find one again. `path_psi()` now
    takes the inode number from the parent's listing (read once per
    request: the bind mount never updates the container's view of a
    directory's times for changes made on the Mac, so no listing can be
    trusted longer), READDIR gives "." and ".." the numbers GETATTR does,
    and a handle whose path now holds another file follows its own file if
    it moved, else names the file that replaced it — so a file saved on
    the Mac by writing a new one and renaming it over the old (editors,
    Claude Code) reads as rewritten in the guest, not as a stale handle or
    the old contents.
- `0004-sub-second-file-times.patch` — GETATTR sent times in whole seconds
  (`useconds` 0). A client keeps its cached data while the size and mtime it
  saw stand, so a file rewritten at the same size within the second went on
  being read as it was. Send the microseconds.
- `0005-statfs-sizes-a-1990-client-can-hold.patch` — STATFS reported the
  export's disc in 512-byte blocks. RISC iX's `df` multiplies blocks by the
  block size in 32 bits, so a Mac's disc through the bind mount showed as
  negative and with 0 KB free. Answer in 1 KB blocks, at most 2 GB of them.
