# Patches applied at image build time

One directory per upstream project, each with its own README:

- `arculator/` — the emulator (SLiRP thread safety and IP reassembly,
  podule sound, prefetch-abort pipeline, NE2000 ring wrap).
- `nfs-user-server/` — the NFSv2 server that serves the work area.

Both upstreams are GPLv2, so the patches are modifications under GPLv2; the
rest of this repository is GPL-2.0-or-later (see `LICENSE`).
