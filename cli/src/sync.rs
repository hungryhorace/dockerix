//! Work-area sync over the exec channel.
//!
//! There is no live filesystem share by default (RISC iX is NFSv2-only; the
//! opt-in `DOCKERIX_NFS=1` mount is the exception — see docs/how-it-works.md,
//! "The live NFS mount").
//! Instead the container pushes its bind-mounted `/workarea` into the guest
//! before each command: tar, uuencode (7-bit safe for the tty), and let the
//! guest's `uudecode`/`tar` unpack it.
//!
//! The push is a **stateful one-way delta** (docs/how-it-works.md, "Syncing
//! the work area"). A manifest in `/state/sync/manifest` records what was
//! last pushed (kind,
//! size and mtime per path, as read through the bind mount — so the host's
//! own clock, never the guest's), and each push `stat`s the tree, ships only
//! the paths that are new or changed, and deletes on the guest only paths the
//! manifest proves we pushed earlier. Anything else on the guest (its build
//! products above all) is off-limits. A sync epoch, stored in the manifest and
//! mirrored as a marker file in the guest tree, tells a trustworthy manifest
//! from one that no longer describes the guest tree (disc reset or swapped,
//! manifest lost): on mismatch the whole tree is pushed again and nothing is
//! deleted — today's cost, never silent corruption.
//!
//! Nothing comes back automatically: the guest's products stay on the guest
//! (where the next incremental build wants them) until `dockerix get` fetches
//! one explicitly — see transfer.rs, which also uses the helpers here. A path
//! `get` lands in the work area is recorded as *mirrored*, not owned: the
//! push skips it while it is unchanged, ships it once the host edits it (it is
//! then owned), and never deletes the guest's copy because the host's went.
//!
//! What the sweep leaves out: `.git`, editor litter and the sync's own files
//! (built in), plus the patterns in a `.dockerixignore` at the work-area
//! root (`Ignore`). A path that becomes ignored is forgotten, not deleted on
//! the guest. Sockets, fifos and devices, which a tar cannot carry, are left
//! out with a warning naming them, as are paths too long for the guest's tar
//! (over 254 bytes, roughly; see transport). Change detection is size+mtime; `DOCKERIX_SYNC_HASH=1` adds a
//! content hash for the rare tool that rewrites a file to the same size and
//! mtime, at the cost of reading every file on every sweep. The manifest is
//! guarded by a file lock so the agent's push and a `get`/`put` running in
//! their own process never interleave their read-modify-write of it.
//!
//! This file has the sweep and the push. The rest: `rules` (the ignore
//! rules and the content hash), `manifest` (the manifest, its epoch and its
//! lock), `transport` (landing an archive in the guest, and the packing,
//! shared with `get`/`put`), `fetch` (bringing one up from the guest for
//! `get`), and `cpio` (the guest's `cpio` archive rewritten as a tar).

use std::collections::BTreeSet;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Instant;

use crate::config::Config;
use crate::exec::shell_quote;
use crate::progress::{self, Progress};
use crate::session::Session;

mod cpio;
mod fetch;
mod manifest;
mod rules;
mod transport;

#[cfg(test)]
pub use cpio::{Member as CpioMember, write as write_cpio};

pub use fetch::{Estimate, fetch_from_guest};
pub use manifest::{manifest_files, record};
pub use rules::hash_enabled;
pub use transport::{archive_bytes, encoded_size, extract_on_guest, one_file, pack, place, stage_dir};

use manifest::{Entry, Kind, Manifest, Tree, lock_manifest, trust};
use rules::{Rules, builtin_ignored};
use transport::{archivable, tar_paths};

/// The epoch marker in the guest work-area root. Ignored by the sweep.
const EPOCH_MARKER: &str = ".dockerix-epoch";

/// The push size ceiling from the environment (`DOCKERIX_SYNC_MAX`).
pub fn max_bytes_from_env() -> usize {
    crate::config::sync_max()
}

/// What a sweep left out that `rules` did not, each for a warning.
#[derive(Default)]
struct LeftOut {
    /// Sockets, fifos and devices, which a tar cannot carry.
    special: BTreeSet<String>,
    /// Paths too long for the guest's tar to unpack whole. A directory
    /// is named alone, without what it holds.
    too_long: BTreeSet<String>,
}

impl LeftOut {
    /// The warnings to show for what was left out, if anything was.
    fn warnings(&self) -> Vec<String> {
        let mut out = Vec::new();
        let (special, long) = (&self.special, &self.too_long);
        match special.len() {
            0 => {}
            1 => out.push(format!(
                "dockerix: not pushing {}: a socket, fifo or device (.dockerixignore it to silence this)",
                some_of(special)
            )),
            n => out.push(format!(
                "dockerix: not pushing {n} sockets, fifos or devices (.dockerixignore them to silence this): {}",
                some_of(special)
            )),
        }
        match long.len() {
            0 => {}
            1 => out.push(format!(
                "dockerix: not pushing {}: too long a path for the guest's tar (.dockerixignore it to silence this)",
                some_of(long)
            )),
            n => out.push(format!(
                "dockerix: not pushing {n} paths too long for the guest's tar (.dockerixignore them to silence this): {}",
                some_of(long)
            )),
        }
        out
    }
}

/// The first few of `paths`, for a message, and how many more there are.
fn some_of<'a>(paths: impl IntoIterator<Item = &'a String, IntoIter: ExactSizeIterator>) -> String {
    const SHOWN: usize = 3;
    let paths = paths.into_iter();
    let more = paths.len().saturating_sub(SHOWN);
    let mut list = paths.take(SHOWN).cloned().collect::<Vec<_>>().join(", ");
    if more > 0 {
        list.push_str(&format!(" and {more} more"));
    }
    list
}

/// `stat` sweep of `root`: every path under it minus what `rules` leave out.
/// What a tar cannot carry (socket, fifo, device) or the guest's tar cannot
/// unpack whole (too long a path) is left out too, and added to `left_out`
/// for a warning.
fn sweep(root: &Path, rules: &Rules, left_out: &mut LeftOut) -> io::Result<Tree> {
    sweep_below(root, "", rules, left_out)
}

/// The sweep of the directory `start` (relative to `root`, `""` for the
/// root itself); keys stay relative to `root`. Errors name the path.
fn sweep_below(root: &Path, start: &str, rules: &Rules, left_out: &mut LeftOut) -> io::Result<Tree> {
    let at = |rel: &Path| {
        let rel = rel.to_string_lossy().into_owned();
        move |e: io::Error| {
            io::Error::new(e.kind(), format!("work area {}: {e}", if rel.is_empty() { "." } else { &rel }))
        }
    };
    let mut tree = Tree::new();
    let mut stack: Vec<PathBuf> = vec![PathBuf::from(start)];
    while let Some(rel) = stack.pop() {
        let dir = root.join(&rel);
        for ent in std::fs::read_dir(&dir).map_err(at(&rel))? {
            let ent = ent.map_err(at(&rel))?;
            let name = ent.file_name();
            let Some(name) = name.to_str() else { continue }; // not UTF-8: skip
            if builtin_ignored(name) || name.contains('\n') {
                continue;
            }
            let rel_path = if rel.as_os_str().is_empty() { PathBuf::from(name) } else { rel.join(name) };
            let key = rel_path.to_string_lossy().into_owned();
            let md = match std::fs::symlink_metadata(ent.path()) {
                Ok(md) => md,
                Err(e) => {
                    let plain = ent.file_type().is_ok_and(|t| t.is_file() || t.is_dir() || t.is_symlink());
                    if plain && e.kind() == io::ErrorKind::NotFound {
                        continue; // deleted since the listing
                    }
                    if plain {
                        return Err(at(&rel_path)(e));
                    }
                    // A Mac socket, which Docker Desktop's file share lists
                    // but can fail to stat (ENOENT or EOPNOTSUPP, varying).
                    if !rules.ignore.matches(&key, name, false) {
                        left_out.special.insert(key);
                    }
                    continue;
                }
            };
            if rules.ignore.matches(&key, name, md.is_dir()) {
                continue;
            }
            if !archivable(&key, md.is_dir()) {
                left_out.too_long.insert(key);
                continue;
            }
            let Some(entry) = Entry::read(&ent.path(), &md, rules.hash).map_err(at(&rel_path))? else {
                left_out.special.insert(key);
                continue;
            };
            if entry.kind == Kind::Dir {
                stack.push(rel_path);
            }
            tree.insert(key, entry);
        }
    }
    Ok(tree)
}

/// How a push behaves.
pub struct PushOpts {
    /// Skip the push if its archive would be bigger (`DOCKERIX_SYNC_MAX`).
    pub max_bytes: usize,
    /// Compare contents rather than size and mtime (see `Rules`).
    pub hash: bool,
    /// First delete the guest tree outright, guest products and all, so the
    /// push is a full one under a new manifest (`dockerix sync --clear`).
    pub clear: bool,
}

/// Push the container work area (`cfg.container_workarea`) into the guest at
/// `cfg.guest_workarea` as a delta against the manifest (see the module doc).
/// Returns warnings to show the user: the push was skipped or degraded, or
/// left out paths a tar cannot carry. The work-area paths, manifest path and
/// staging settings all come from `cfg`, resolved once at process entry.
///
/// A pass that moved anything is followed by another sweep, and another
/// pass if the tree changed meanwhile: a push takes a second or two, during
/// which the user may have saved again — and the bind mount's attribute
/// cache can lag a host edit by up to ~1 s, so a sweep right after a save can
/// miss it; by the time the push is done the cache has caught up. Bounded
/// so a file being written continuously cannot hold the command forever.
pub fn push(
    session: &mut Session,
    cfg: &Config,
    opts: &PushOpts,
    deadline: Instant,
    progress: &mut dyn FnMut(&Progress),
) -> io::Result<Vec<String>> {
    const MAX_PASSES: usize = 3;
    let container_dir = cfg.container_workarea.as_str();
    let guest_dir = cfg.guest_workarea.as_str();
    let root = Path::new(container_dir);
    if !root.exists() {
        return Ok(Vec::new());
    }
    let manifest_path = &cfg.sync_manifest;
    // Held from before the clear, so no other push can see the old tree.
    let _lock = lock_manifest(manifest_path)?;
    if opts.clear {
        clear_guest(session, guest_dir, deadline)?;
    }

    // Make sure the guest root exists (no `mkdir -p`: RISC iX's mkdir has no
    // such flag and would create a directory called `-p`), and read the
    // epoch marker so we know whether the manifest still describes the tree.
    let marker = format!("{guest_dir}/{EPOCH_MARKER}");
    let (out, _) = session.run(
        &format!(
            "test -d {guest_dir} || mkdir {guest_dir}; \
             if test -f {marker}; then cat {marker}; else echo -; fi"
        ),
        deadline,
    )?;
    let guest_epoch = String::from_utf8_lossy(&out).trim().to_string();

    // After a clear the marker is gone, so the manifest is untrusted: a new
    // one, and the whole tree pushed.
    let (mut manifest, mut trusted) = trust(Manifest::load(manifest_path), &guest_epoch);
    let rules = Rules::load(root, opts.hash);
    // A path that has become ignored is forgotten, not deleted on the guest.
    let before = manifest.entries.len();
    manifest.entries.retain(|p, e| !rules.excluded(p, e.kind == Kind::Dir));
    let mut dirty = manifest.entries.len() != before;

    let mut warning = None;
    let mut left_out = LeftOut::default();
    for _ in 0..MAX_PASSES {
        let tree = sweep(root, &rules, &mut left_out)?;
        let delta = Delta::between(&manifest.entries, &tree);
        if delta.is_empty() && trusted {
            // Nothing for the guest, but the sweep may still have taught the
            // manifest something (hashes newly on, a `touch`ed mtime).
            if manifest.adopt(tree) || dirty {
                manifest.save(manifest_path)?;
            }
            break;
        }
        let tar =
            if delta.changed.is_empty() { Vec::new() } else { tar_paths(container_dir, &delta.changed)? };
        if tar.len() > opts.max_bytes {
            return Ok(vec![format!(
                "dockerix: skipping work-area push ({} bytes exceeds DOCKERIX_SYNC_MAX)",
                tar.len()
            )]);
        }
        if !trusted {
            let bytes: u64 = tree.values().filter(|e| e.kind != Kind::Dir).map(|e| e.size).sum();
            let why = if opts.clear {
                "the guest work area was cleared"
            } else {
                "no sync manifest for this guest tree yet"
            };
            warning =
                Some(format!("dockerix: full work-area push ({} paths, {bytes} bytes): {why}", tree.len()));
        }
        let label = format!("pushing {} paths, {}", delta.changed.len(), progress::bytes(tar.len() as u64));
        delta.apply(session, cfg, &tar, deadline, &label, progress)?;
        if !trusted {
            // Stamp the guest tree with the new epoch before recording it: if
            // the stamp fails the manifest stays unwritten and the next push
            // starts over, rather than trusting a tree the guest cannot vouch
            // for.
            let (out, code) = session.run(&format!("echo {} > {marker}", manifest.epoch), deadline)?;
            if code != 0 {
                return Err(io::Error::other(format!(
                    "could not write the sync epoch marker {marker}: {}",
                    String::from_utf8_lossy(&out).trim()
                )));
            }
            trusted = true;
        }
        // Everything on the host is now in sync.
        manifest.adopt(tree);
        manifest.save(manifest_path)?;
        dirty = false;
    }
    Ok(left_out.warnings().into_iter().chain(warning).collect())
}

/// Delete the guest work area `guest_dir` and everything in it. Refused for
/// `/`, and for a mount point: an NFS-mounted work area is the host's own
/// directory.
fn clear_guest(session: &mut Session, guest_dir: &str, deadline: Instant) -> io::Result<()> {
    if !guest_dir.starts_with('/') || guest_dir.trim_end_matches('/').is_empty() {
        return Err(io::Error::other(format!("refusing to clear guest directory {guest_dir:?}")));
    }
    let (out, code) = session.run(
        &format!(
            "cd /; if mount | grep ' on {guest_dir} ' >/dev/null; then \
             echo '{guest_dir} is a mount point'; false; else rm -rf {guest_dir}; fi"
        ),
        deadline,
    )?;
    if code != 0 {
        return Err(io::Error::other(format!(
            "could not clear the guest work area {guest_dir}: {}",
            String::from_utf8_lossy(&out).trim()
        )));
    }
    Ok(())
}

/// What one pass has to do to the guest: paths to ship, and paths to remove
/// because the manifest shows we pushed them (owned) and they are gone from
/// the host (or changed kind). A mirrored path that went is simply forgotten.
struct Delta<'a> {
    changed: Vec<&'a str>,
    rm_files: Vec<&'a str>,
    rm_dirs: Vec<&'a str>,
}

impl<'a> Delta<'a> {
    fn between(old: &'a Tree, new: &'a Tree) -> Delta<'a> {
        let changed = new
            .iter()
            .filter(|(p, e)| old.get(*p).is_none_or(|o| o.differs(e)))
            .map(|(p, _)| p.as_str())
            .collect();
        let mut rm_files = Vec::new();
        let mut rm_dirs = Vec::new();
        for (p, o) in old {
            if o.owned && new.get(p).is_none_or(|e| e.kind != o.kind) {
                if o.kind == Kind::Dir {
                    rm_dirs.push(p.as_str());
                } else {
                    rm_files.push(p.as_str());
                }
            }
        }
        // Deepest directories first, so nested empties go before their parents.
        rm_dirs.sort_by(|a, b| b.len().cmp(&a.len()).then(b.cmp(a)));
        Delta { changed, rm_files, rm_dirs }
    }

    fn is_empty(&self) -> bool {
        self.changed.is_empty() && self.rm_files.is_empty() && self.rm_dirs.is_empty()
    }

    /// Apply to the guest: deletions first, so a path that changed kind
    /// (file → dir or back) is out of the way before the archive lands.
    /// Files go with `rm -f`; directories with `rmdir`, only if empty — a
    /// guest product inside keeps the directory, which is the point.
    fn apply(
        &self,
        session: &mut Session,
        cfg: &Config,
        tar: &[u8],
        deadline: Instant,
        label: &str,
        progress: &mut dyn FnMut(&Progress),
    ) -> io::Result<()> {
        let guest_dir = &cfg.guest_workarea;
        if !self.rm_files.is_empty() || !self.rm_dirs.is_empty() {
            let mut cmd = format!("cd {guest_dir}");
            for chunk in self.rm_files.chunks(50) {
                cmd.push_str(" && rm -f");
                for p in chunk {
                    cmd.push(' ');
                    cmd.push_str(&shell_quote(&format!("./{p}")));
                }
            }
            for p in &self.rm_dirs {
                cmd.push_str(&format!("; rmdir {} 2>/dev/null", shell_quote(&format!("./{p}"))));
            }
            cmd.push_str("; true");
            session.run(&cmd, deadline)?;
        }
        if tar.is_empty() {
            return Ok(());
        }
        extract_on_guest(session, cfg, guest_dir, tar, deadline, label, progress)
    }
}

#[cfg(test)]
mod tests {
    use super::rules::IGNORE_FILE;
    use super::*;
    use crate::testsupport::tmp_dir;

    pub(super) fn entry(kind: Kind, size: u64, mtime: i64, owned: bool) -> Entry {
        Entry { kind, size, mtime, owned, hash: None }
    }

    #[test]
    fn delta_never_touches_what_the_push_does_not_own() {
        let f = entry(Kind::File, 1, 1, true);
        let old: Tree = [
            ("mine", f.clone()),
            ("theirs", entry(Kind::File, 1, 1, false)),
            ("dir", entry(Kind::Dir, 0, 1, false)),
            ("dir/x", entry(Kind::File, 1, 1, false)),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect();
        let none = Tree::new();
        let delta = Delta::between(&old, &none);
        assert_eq!(delta.rm_files, ["mine"]);
        assert!(delta.rm_dirs.is_empty());
        // Nothing the manifest never knew (a guest product) can appear.
        assert!(Delta::between(&none, &none).is_empty());
        for e in old.values() {
            if !e.owned {
                assert!(!delta.rm_files.iter().chain(&delta.rm_dirs).any(|p| old[*p] == *e));
            }
        }
    }

    #[test]
    fn delta_between_trees() {
        let f = |s: u64| entry(Kind::File, s, 1, true);
        let d = entry(Kind::Dir, 0, 1, true);
        let mirrored = entry(Kind::File, 5, 1, false);
        let old: Tree = [
            ("a", f(1)),
            ("b", f(1)),
            ("d", d.clone()),
            ("d/x", d.clone()),
            ("t", f(0)),
            ("m", mirrored.clone()),
            ("m2", mirrored.clone()),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect();
        let new: Tree = [("a", f(1)), ("b", f(2)), ("c", f(1)), ("t", d.clone()), ("m2", f(6))]
            .into_iter()
            .map(|(k, v)| (k.to_string(), v))
            .collect();
        let delta = Delta::between(&old, &new);
        // m2: mirrored, edited on the host → shipped; m: mirrored, gone → not deleted.
        assert_eq!(delta.changed, ["b", "c", "m2", "t"]);
        assert_eq!(delta.rm_files, ["t"]);
        assert_eq!(delta.rm_dirs, ["d/x", "d"]);
        assert!(!delta.is_empty());
        assert!(Delta::between(&old, &old).is_empty());
    }

    #[test]
    fn sweep_skips_ignored() {
        let tmp = tmp_dir("sweep");
        let dir = tmp.path();
        std::fs::create_dir_all(dir.join("src/.git")).unwrap();
        std::fs::create_dir_all(dir.join("empty")).unwrap();
        std::fs::write(dir.join("src/a.c"), "int x;").unwrap();
        std::fs::write(dir.join("src/a.c~"), "").unwrap();
        std::fs::write(dir.join(".DS_Store"), "").unwrap();
        std::fs::write(dir.join(EPOCH_MARKER), "e").unwrap();
        std::fs::write(dir.join("src/.git/HEAD"), "ref").unwrap();
        // Left by RISC iX's NFS client in a tree it had mounted; a file of
        // the user's own with a name like it is pushed.
        std::fs::write(dir.join("src/.nfs0F43"), "").unwrap();
        std::fs::write(dir.join("src/.nfsrc"), "").unwrap();
        let t = sweep(dir, &Rules::load(dir, false), &mut LeftOut::default()).unwrap();
        let keys: Vec<&str> = t.keys().map(String::as_str).collect();
        assert_eq!(keys, ["empty", "src", "src/.nfsrc", "src/a.c"]);
        assert_eq!(t["src/a.c"].kind, Kind::File);
        assert_eq!(t["src/a.c"].size, 6);
        assert_eq!(t["src/a.c"].hash, None);
        assert_eq!(t["empty"].kind, Kind::Dir);
    }

    #[test]
    fn sweep_skips_special_files() {
        let tmp = tmp_dir("special");
        let dir = tmp.path();
        std::fs::create_dir_all(dir.join(".risced")).unwrap();
        std::fs::write(dir.join("a.c"), "int x;").unwrap();
        let _sock = std::os::unix::net::UnixListener::bind(dir.join(".risced/daemon.sock")).unwrap();
        let mut left_out = LeftOut::default();
        let t = sweep(dir, &Rules::load(dir, false), &mut left_out).unwrap();
        let keys: Vec<&str> = t.keys().map(String::as_str).collect();
        assert_eq!(keys, [".risced", "a.c"]);
        assert_eq!(left_out.special.iter().collect::<Vec<_>>(), [".risced/daemon.sock"]);
        let warnings = left_out.warnings();
        assert!(warnings.len() == 1 && warnings[0].contains(".risced/daemon.sock"), "{warnings:?}");
        // Ignoring it silences the warning.
        std::fs::write(dir.join(IGNORE_FILE), "*.sock\n").unwrap();
        let mut left_out = LeftOut::default();
        sweep(dir, &Rules::load(dir, false), &mut left_out).unwrap();
        assert!(left_out.special.is_empty() && left_out.warnings().is_empty());
    }

    /// A path too long for the guest's tar is left out with a warning
    /// naming it; a directory too long is named alone and not swept.
    #[test]
    fn sweep_leaves_out_paths_too_long_for_the_guest() {
        let tmp = tmp_dir("toolong");
        let dir = tmp.path();
        // A name of 100 bytes goes in the name field, split from its `./`;
        // one of 101 cannot.
        let (name, long_name) = ("f".repeat(100), "g".repeat(101));
        // 254 bytes, split at the `/` after the first 153 (155 with the
        // `./`); a byte more on that side is a byte too many for either.
        let dirs = ["d234567890"; 14].join("/");
        let (deep, long_deep) =
            (format!("{dirs}/{}", "a".repeat(100)), format!("{dirs}1/{}", "a".repeat(100)));
        // A directory whose name, with its trailing `/`, is too long; the
        // file in it would fit, split at its own `/`, but is not swept.
        let long_dir = format!("e/{}", "h".repeat(100));
        for f in [&name, &long_name, &deep, &long_deep, &format!("{long_dir}/x")] {
            std::fs::create_dir_all(dir.join(Path::new(f).parent().unwrap())).unwrap();
            std::fs::write(dir.join(f), "x").unwrap();
        }
        let mut left_out = LeftOut::default();
        let t = sweep(dir, &Rules::load(dir, false), &mut left_out).unwrap();
        assert!(t.contains_key(&name) && t.contains_key(&deep) && t.contains_key("e"), "{:?}", t.keys());
        assert!(!t.keys().any(|k| k.starts_with(&long_dir)), "{:?}", t.keys());
        assert_eq!(left_out.too_long, [&long_deep, &long_dir, &long_name].into_iter().cloned().collect());
        let warnings = left_out.warnings();
        assert!(
            warnings.len() == 1
                && warnings[0].contains("3 paths too long")
                && warnings[0].contains(&long_dir),
            "{warnings:?}"
        );
    }

    #[test]
    fn sweep_errors_name_the_path() {
        let tmp = tmp_dir("sweeperr");
        let dir = tmp.path();
        let e = sweep(&dir.join("gone"), &Rules::load(dir, false), &mut LeftOut::default()).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::NotFound);
        assert!(e.to_string().starts_with("work area .: "), "{e}");
    }
}
