# Patches applied to Arculator at image build time

Applied in order with `patch -p1` from the Arculator source root (see the
build stage in the `Dockerfile`, which fetches upstream at the commit named by
`ARCULATOR_COMMIT`). These are the local differences from that commit.
Arculator is GPLv2, so these patches are modifications to it under GPLv2
(the rest of this repository is GPL-2.0-or-later; see `LICENSE`).

To make or update one, start from a clone at that commit:

```
git clone https://github.com/sarah-walker-pcem/arculator.git
cd arculator && git checkout <ARCULATOR_COMMIT>
```

To update an existing patch, apply it first (`patch -p1 < .../NNNN-*.patch`).
Edit the file, `git diff` it from the clone's root into a new numbered file
here (or over the one being updated), then `git checkout .` to get back to the
pinned commit. Preserve the file's existing line endings — several
upstream files are CRLF — or the diff rewrites every line. `.gitattributes`
marks `patches/**/*.patch` as `-text` so git commits those CRs byte for byte
(with `core.autocrlf=input` it would otherwise strip them, and the patch
would fail on a fresh clone). Each patch should
be a self-contained upstream-quality change: the intent is to submit them as
pull requests to Arculator (https://github.com/sarah-walker-pcem/arculator)
later and drop them here once merged.

- `0001-slirp-thread-safety.patch` — the aeh50 network podule drives SLiRP
  from two threads (its socket poll thread, and the emulator thread injecting
  guest packets) with no locking; SLiRP is not thread-safe and the packet
  queue between them isn't either. Inbound telnet data was intermittently
  lost mid-connection (seen as telnet logins stalling). Adds a mutex around
  everything but the `select()` wait.
- `0003-slirp-ip-reass-ext.patch` — SLiRP's `ip_reass()` (the bundled
  `podules/common/net/slirp/`) computes the post-realloc header pointer from
  the wrong fragment: it uses `ip` (the fragment that just arrived, in another
  mbuf) instead of `q` (the first fragment, which owns the reassembly buffer).
  When the reassembled datagram outgrows its mbuf and moves to an `m_ext`
  buffer, the recomputed pointer is garbage and the emulator segfaults. This
  fires on any guest-originated datagram that spans more than one mbuf, e.g. an
  8 KB NFS write over the optional live mount. Same fix as QEMU/libslirp.
  Needed for `DOCKERIX_NFS=1` with the default `wsize=8192`.
- `0002-podule-sound-null-device.patch` — the shared podule sound output
  (`podules/common/sound/sound_out_sdl2.c`) returns NULL from `sound_out_init`
  when `SDL_OpenAudioDevice` fails, and the AKA31 podule then dereferences it
  every 100 ms in `sound_out_buffer` (segfault at startup). Make the output
  functions tolerate NULL. Needed to run with `SDL_AUDIODRIVER=dummy`, where
  only the first device open succeeds; that driver is what stops Arculator's
  audio threads spinning at 100% CPU in a container with no sound hardware.
- `0004-prefetch-abort-pipeline.patch` — the CPU core (`src/arm.c`,
  `execarm()`) takes a prefetch abort one pipeline stage early. A fetch that
  fails sets `prefabort` for the next loop iteration, where it both stops the
  instruction in the execute stage running and raises the exception with
  `r14` = that instruction + 8, which is the right `r14` for the instruction
  *behind* it (the one that could not be fetched). So when execution runs off
  the end of a page into one that is not resident, the last instruction of
  the resident page is skipped: the abort handler returns to the start of the
  new page as it should, but the `mov ip, sp` or `bl` before the boundary
  never ran. RISC OS never notices (its code is not demand paged); RISC iX
  pages text in all the time, and this is the "register corruption" of
  docs/register-corruption.md. Execute the instruction regardless and take
  the exceptions in the ARM's priority order (data abort, FIQ, IRQ, then
  prefetch abort), so an abort or interrupt of the executing instruction
  wins and the refill it causes drops the pending prefetch abort, to recur
  when that instruction is fetched again.
- `0005-ne2000-remote-dma-ring-wrap.patch` — the NE2000 core the aeh50
  podule uses (`podules/common/net/ne2000.c`) wraps the remote DMA address
  from the end of the receive ring to its start by comparing the 16-bit
  address with `PSTOP << 8`. RISC iX's driver sets `PSTOP` to 0, a ring
  that runs to the top of the 64 KB (`page_stop` 0x100), so the end is
  0x10000: the address rolls over to 0 instead, never equals it, and a frame
  that crosses the end of the ring is read back with its tail taken from the
  transmit buffer at the bottom of memory. The guest drops it for a bad
  checksum (`netstat -s`, udp "bad checksums") and the sender has to time
  out and resend. A full frame takes 6 of the ring's 224 pages, so about one
  in 37 crosses the end, and one 8 KB NFS reply (six frames) in six or so:
  NFS reads crawled at ~23 KB/s, a 2.2s retransmit per corrupted reply. Do the
  arithmetic in 32 bits. Found by recording what the card received and what
  the guest read back out of the ring; a 512 KB cold NFS read went from
  ~15–25s to 1.2s.
