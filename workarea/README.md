# Work area

An example work area. It is not mounted by default: start with
`dockerix start --workarea workarea` (or set `DOCKERIX_WORKAREA`) to share it,
or point either at another directory. `hello/` is the README's quick start.

Sharing is one way. Before each `dockerix exec` the host tree is pushed into
the RISC iX guest at `/workarea` as a delta (only what changed since the last
push), and what the guest builds stays on the guest's disc, for the next
incremental build. Nothing comes back by itself: `dockerix get <path>` copies
a file (or, with `-r`, a directory) to the host, and `dockerix put` the other
way.

The push, `put` and `get` go over NFS at a few hundred KB/s (through the
telnet session, far slower, if the guest cannot reach it). A push over
`DOCKERIX_SYNC_MAX` (default 16 MB) is skipped, and `.dockerixignore` in
this directory leaves things out. `dockerix exec --no-sync` runs without
pushing. For a live mount instead of the push, start with `--nfs`. The
main README ("Using it") and `docs/how-it-works.md` ("Syncing the work
area", "The live NFS mount") have the details.
