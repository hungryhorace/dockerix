//! The sync manifest: what the push last sent to the guest, path by path,
//! with the epoch that ties it to one guest tree. Stored under `/state`
//! beside the guest disc, written atomically, and guarded by a file lock.

use std::collections::BTreeMap;
use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use super::rules::{Rules, content_hash};
use super::{LeftOut, sweep_below};
use crate::config::Config;
use crate::session::token;

/// Manifest header: `dockerix-manifest <format> <epoch>`. Any other format
/// is untrusted (a full push follows).
const MANIFEST_MAGIC: &str = "dockerix-manifest";
const MANIFEST_FORMAT: &str = "2";

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Kind {
    File,
    Dir,
    Link,
}

impl Kind {
    /// Manifest tag: lower case for a path the push owns, upper case for one
    /// merely mirrored from the guest (`get`).
    fn tag(self, owned: bool) -> char {
        let c = match self {
            Kind::File => 'f',
            Kind::Dir => 'd',
            Kind::Link => 'l',
        };
        if owned { c } else { c.to_ascii_uppercase() }
    }
    fn from_tag(c: &str) -> Option<(Kind, bool)> {
        let kind = match c {
            "f" | "F" => Kind::File,
            "d" | "D" => Kind::Dir,
            "l" | "L" => Kind::Link,
            _ => return None,
        };
        Some((kind, c.chars().all(|c| c.is_ascii_lowercase())))
    }
}

/// One tracked path: what it is, and (for files and links) the size and
/// mtime it had when last pushed. Directories are tracked so that a new
/// empty directory reaches the guest and a removed one is `rmdir`ed; their
/// mtime is recorded but not compared (it changes whenever a child does).
/// `owned` is false for a path `get` copied from the guest: in sync, but the
/// guest's, so its disappearance from the host deletes nothing. `hash` is
/// the content hash when hashing is on (`DOCKERIX_SYNC_HASH=1`).
#[derive(Clone, PartialEq, Eq, Debug)]
pub(super) struct Entry {
    pub(super) kind: Kind,
    pub(super) size: u64,
    pub(super) mtime: i64,
    pub(super) owned: bool,
    pub(super) hash: Option<u64>,
}

impl Entry {
    /// Does `new` need pushing, given `self` was last pushed? By content
    /// when both sides have a hash (so a `touch` ships nothing), else by
    /// size and mtime.
    pub(super) fn differs(&self, new: &Entry) -> bool {
        if self.kind != new.kind {
            return true;
        }
        if new.kind == Kind::Dir {
            return false;
        }
        match (self.hash, new.hash) {
            (Some(a), Some(b)) => a != b,
            _ => self.size != new.size || self.mtime != new.mtime,
        }
    }

    /// What `path` (whose `symlink_metadata` is `md`) is now; `None` for
    /// something a tar cannot carry (socket, fifo, device).
    pub(super) fn read(path: &Path, md: &std::fs::Metadata, hash: bool) -> io::Result<Option<Entry>> {
        let ft = md.file_type();
        let kind = if ft.is_symlink() {
            Kind::Link
        } else if ft.is_dir() {
            Kind::Dir
        } else if ft.is_file() {
            Kind::File
        } else {
            return Ok(None);
        };
        let hash = if hash && kind != Kind::Dir { Some(content_hash(path, kind)?) } else { None };
        Ok(Some(Entry { kind, size: md.len(), mtime: mtime_secs(md), owned: true, hash }))
    }
}

/// Paths are relative to the work-area root, `/`-separated, no leading `./`.
pub(super) type Tree = BTreeMap<String, Entry>;

pub(super) struct Manifest {
    pub(super) epoch: String,
    pub(super) entries: Tree,
}

impl Manifest {
    fn empty() -> Manifest {
        Manifest { epoch: token(), entries: Tree::new() }
    }

    /// Read the manifest; `None` if it is missing or unreadable (either way
    /// it cannot be trusted and a full push follows).
    pub(super) fn load(path: &Path) -> Option<Manifest> {
        let text = std::fs::read_to_string(path).ok()?;
        let mut lines = text.lines();
        let header = lines.next()?;
        let (format, epoch) = header.strip_prefix(MANIFEST_MAGIC)?.trim().split_once(' ')?;
        if format != MANIFEST_FORMAT {
            return None;
        }
        let epoch = epoch.trim();
        if epoch.is_empty() {
            return None;
        }
        let mut entries = Tree::new();
        for line in lines {
            if line.is_empty() {
                continue;
            }
            let mut f = line.splitn(5, ' ');
            let (kind, owned) = Kind::from_tag(f.next()?)?;
            let size = f.next()?.parse().ok()?;
            let mtime = f.next()?.parse().ok()?;
            let hash = match f.next()? {
                "-" => None,
                h => Some(u64::from_str_radix(h, 16).ok()?),
            };
            let path = f.next()?;
            entries.insert(path.to_string(), Entry { kind, size, mtime, owned, hash });
        }
        Some(Manifest { epoch: epoch.to_string(), entries })
    }

    /// Write atomically (temp file + rename) so a crash mid-write leaves the
    /// previous manifest, not a truncated one.
    pub(super) fn save(&self, path: &Path) -> io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)
                .map_err(|e| io::Error::new(e.kind(), format!("{}: {e}", dir.display())))?;
        }
        let mut text = format!("{MANIFEST_MAGIC} {MANIFEST_FORMAT} {}\n", self.epoch);
        for (p, e) in &self.entries {
            let hash = e.hash.map_or("-".to_string(), |h| format!("{h:016x}"));
            text.push_str(&format!("{} {} {} {hash} {p}\n", e.kind.tag(e.owned), e.size, e.mtime));
        }
        let tmp = tmp_path(path);
        let with_path = |e: io::Error| io::Error::new(e.kind(), format!("{}: {e}", path.display()));
        std::fs::write(&tmp, text).map_err(with_path)?;
        std::fs::rename(&tmp, path).map_err(with_path)
    }

    /// Make `tree` (a sweep of the host, all owned) the record of the guest:
    /// a path that was mirrored and did not move stays mirrored, the rest
    /// is ours. Returns whether anything changed.
    pub(super) fn adopt(&mut self, mut tree: Tree) -> bool {
        for (p, e) in tree.iter_mut() {
            if let Some(old) = self.entries.get(p)
                && !old.owned
                && !old.differs(e)
            {
                e.owned = false;
            }
        }
        if self.entries == tree {
            return false;
        }
        self.entries = tree;
        true
    }
}

/// The manifest to diff against: the saved one if its epoch is the guest
/// tree's, else a fresh one — untrusted, so the whole tree is pushed and
/// nothing is deleted. Covers a manifest that is missing or unreadable, a
/// guest tree with no marker (fresh disc, `dockerix reset`), and the two
/// describing different trees (disc swapped, manifest from elsewhere).
pub(super) fn trust(saved: Option<Manifest>, guest_epoch: &str) -> (Manifest, bool) {
    match saved {
        Some(m) if m.epoch == guest_epoch => (m, true),
        _ => (Manifest::empty(), false),
    }
}

/// The manifest's lock file (see `lock_manifest`).
fn lock_path(manifest: &Path) -> PathBuf {
    manifest.with_extension("lock")
}

/// Where `Manifest::save` writes before renaming into place; left behind by
/// a crash mid-save.
fn tmp_path(manifest: &Path) -> PathBuf {
    manifest.with_extension("tmp")
}

/// Every file the manifest at `manifest` may leave on disc, for `dockerix
/// reset`.
pub fn manifest_files(manifest: &Path) -> [PathBuf; 3] {
    [manifest.to_path_buf(), lock_path(manifest), tmp_path(manifest)]
}

/// Serialise manifest read-modify-writes across processes (the agent's
/// pushes; `get`/`put` recording in their own `docker exec`): an exclusive
/// `flock` on a file beside the manifest, held while the guard lives.
pub(super) fn lock_manifest(manifest: &Path) -> io::Result<File> {
    let path = lock_path(manifest);
    let with_path = |e: io::Error| io::Error::new(e.kind(), format!("{}: {e}", path.display()));
    if let Some(dir) = manifest.parent() {
        std::fs::create_dir_all(dir).map_err(with_path)?;
    }
    let f = File::options().create(true).write(true).truncate(false).open(&path).map_err(with_path)?;
    f.lock().map_err(with_path)?;
    Ok(f)
}

fn mtime_secs(md: &std::fs::Metadata) -> i64 {
    md.modified()
        .ok()
        .map(|t| match t.duration_since(UNIX_EPOCH) {
            Ok(d) => d.as_secs() as i64,
            Err(e) => -(e.duration().as_secs() as i64),
        })
        .unwrap_or(0)
}

/// Record `rel` (a path relative to the work area, with everything under it)
/// in the manifest as in sync with the guest — `owned` false for a mirror
/// the guest is authoritative for. Anything previously recorded at or under
/// `rel` is replaced. A no-op without a manifest (the next push starts from
/// scratch anyway).
pub fn record(cfg: &Config, rel: &str, owned: bool) -> io::Result<()> {
    let path = &cfg.sync_manifest;
    let _lock = lock_manifest(path)?;
    let Some(mut manifest) = Manifest::load(path) else { return Ok(()) };
    let root = Path::new(&cfg.container_workarea);
    let rules = Rules::load(root, cfg.sync_hash);
    let prefix = format!("{rel}/");
    manifest.entries.retain(|p, _| p != rel && !p.starts_with(&prefix));
    let full = root.join(rel);
    let md = std::fs::symlink_metadata(&full)?;
    let Some(mut top) = Entry::read(&full, &md, rules.hash)? else { return Ok(()) };
    top.owned = owned;
    if top.kind == Kind::Dir {
        for (p, mut e) in sweep_below(root, rel, &rules, &mut LeftOut::default())? {
            e.owned = owned;
            manifest.entries.insert(p, e);
        }
    }
    manifest.entries.insert(rel.to_string(), top);
    manifest.save(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sync::tests::entry;
    use crate::testsupport::tmp_dir;

    #[test]
    fn manifest_round_trip() {
        let tmp = tmp_dir("manifest");
        let dir = tmp.path();
        let path = dir.join("sync").join("manifest");
        let mut m = Manifest::empty();
        m.entries.insert("a b/c.c".into(), entry(Kind::File, 12, 1788981279, true));
        m.entries.insert("a b".into(), entry(Kind::Dir, 96, 5, false));
        m.entries.insert("-p".into(), entry(Kind::Link, 3, -7, true));
        m.entries.insert("h".into(), Entry { hash: Some(0xdead_beef), ..entry(Kind::File, 1, 1, true) });
        m.save(&path).unwrap();
        let back = Manifest::load(&path).unwrap();
        assert_eq!(back.epoch, m.epoch);
        assert_eq!(back.entries, m.entries);
    }

    #[test]
    fn missing_or_bad_manifest_is_untrusted() {
        assert!(Manifest::load(Path::new("/nonexistent/manifest")).is_none());
        let tmp = tmp_dir("bad");
        let p = tmp.path().join("manifest");
        for bad in [
            "not a manifest\n",
            "dockerix-manifest 2 \n",
            "dockerix-manifest 1 e\nf 1 2 p\n",
            "dockerix-manifest 2 e\nf 1 x p\n",
        ] {
            std::fs::write(&p, bad).unwrap();
            assert!(Manifest::load(&p).is_none(), "{bad:?}");
        }
    }

    #[test]
    fn trust_needs_matching_epochs() {
        let saved = || {
            Some(Manifest {
                epoch: "e1".into(),
                entries: [("a".to_string(), entry(Kind::File, 1, 1, true))].into(),
            })
        };
        let (m, ok) = trust(saved(), "e1");
        assert!(ok && m.entries.len() == 1);
        // Guest marker missing (fresh disc, reset), different (swapped), or
        // manifest missing: an empty, untrusted manifest with a new epoch.
        for (saved, guest) in [(saved(), "-"), (saved(), "e2"), (None, "e1"), (None, "-")] {
            let (m, ok) = trust(saved, guest);
            assert!(!ok && m.entries.is_empty() && m.epoch != "e1" && m.epoch != "-");
        }
    }

    #[test]
    fn diff_rules() {
        let old = entry(Kind::File, 1, 1, true);
        assert!(!old.differs(&old));
        assert!(old.differs(&entry(Kind::File, 2, 1, true)));
        assert!(old.differs(&entry(Kind::File, 1, 2, true)));
        assert!(old.differs(&entry(Kind::Dir, 1, 1, true)));
        let d = entry(Kind::Dir, 96, 1, true);
        assert!(!d.differs(&entry(Kind::Dir, 128, 9, true)));
        // With hashes on both sides the content decides: a touch is not a
        // change, a same-size same-mtime rewrite is. One side hashless:
        // back to size+mtime.
        let h = |h: u64, mtime: i64| Entry { hash: Some(h), ..entry(Kind::File, 1, mtime, true) };
        assert!(!h(7, 1).differs(&h(7, 2)));
        assert!(h(7, 1).differs(&h(8, 1)));
        assert!(!h(7, 1).differs(&entry(Kind::File, 1, 1, true)));
        assert!(h(7, 1).differs(&entry(Kind::File, 1, 2, true)));
    }

    /// A work area (`lib/new.c`, `lib/sub/deep.c`, `libz.c`, `other.c`) with
    /// a manifest beside it, which `saved` (if given) is saved as. Returns the
    /// temp dir, the manifest path, and a `Config` whose `record` targets the
    /// work area (hashing off). The work-area path is `cfg.container_workarea`.
    fn record_dirs(tag: &str, saved: Option<&Manifest>) -> (tempfile::TempDir, PathBuf, Config) {
        let tmp = tmp_dir(tag);
        let root = tmp.path().join("workarea");
        std::fs::create_dir_all(root.join("lib/sub")).unwrap();
        for (p, text) in
            [("lib/new.c", "new\n"), ("lib/sub/deep.c", "deep\n"), ("libz.c", "z\n"), ("other.c", "o\n")]
        {
            std::fs::write(root.join(p), text).unwrap();
        }
        let path = tmp.path().join("state/manifest");
        if let Some(m) = saved {
            m.save(&path).unwrap();
        }
        let cfg = Config {
            container_workarea: root.to_string_lossy().into_owned(),
            sync_manifest: path.clone(),
            sync_hash: false,
            ..Config::default()
        };
        (tmp, path, cfg)
    }

    fn manifest_of(entries: &[(&str, Entry)]) -> Manifest {
        Manifest {
            epoch: "e1".into(),
            entries: entries.iter().map(|(p, e)| (p.to_string(), e.clone())).collect(),
        }
    }

    #[test]
    fn record_replaces_a_file_and_nothing_else() {
        let stale = entry(Kind::File, 9, 9, false);
        let saved = manifest_of(&[
            ("other.c", stale.clone()),
            ("other.c.orig", stale.clone()),
            ("libz.c", stale.clone()),
        ]);
        let (_tmp, path, cfg) = record_dirs("record-file", Some(&saved));
        record(&cfg, "other.c", true).unwrap();
        let m = Manifest::load(&path).unwrap();
        assert_eq!(m.epoch, "e1");
        let e = &m.entries["other.c"];
        assert_eq!((e.kind, e.size, e.owned), (Kind::File, 2, true));
        assert_eq!((&m.entries["other.c.orig"], &m.entries["libz.c"]), (&stale, &stale));
        assert_eq!(m.entries.len(), 3);
    }

    /// A directory is recorded with everything now in it, all with the
    /// same ownership; what was recorded under it before and is gone from
    /// the host is dropped, and a sibling sharing its prefix is untouched.
    #[test]
    fn record_sweeps_a_directory_with_its_children() {
        let owned = |kind| entry(kind, 1, 1, true);
        let saved = manifest_of(&[
            ("lib", owned(Kind::Dir)),
            ("lib/old.c", owned(Kind::File)),
            ("lib/sub", owned(Kind::Dir)),
            ("lib/sub/x.c", owned(Kind::File)),
            ("libz.c", owned(Kind::File)),
        ]);
        let (_tmp, path, cfg) = record_dirs("record-dir", Some(&saved));
        record(&cfg, "lib", false).unwrap();
        let m = Manifest::load(&path).unwrap();
        let recorded: Vec<(&str, Kind, bool)> =
            m.entries.iter().map(|(p, e)| (p.as_str(), e.kind, e.owned)).collect();
        assert_eq!(
            recorded,
            [
                ("lib", Kind::Dir, false),
                ("lib/new.c", Kind::File, false),
                ("lib/sub", Kind::Dir, false),
                ("lib/sub/deep.c", Kind::File, false),
                ("libz.c", Kind::File, true),
            ]
        );
        assert_eq!(m.entries["lib/sub/deep.c"].size, 5);
    }

    /// With no manifest the next push is a full one anyway: nothing to
    /// record into, and none is made.
    #[test]
    fn record_without_a_manifest_does_nothing() {
        let (_tmp, path, cfg) = record_dirs("record-none", None);
        record(&cfg, "lib", false).unwrap();
        assert!(!path.exists());
    }
}
