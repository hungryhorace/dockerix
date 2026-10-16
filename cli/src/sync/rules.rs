//! What the sweep leaves out, and how it compares files: the built-in
//! ignore list, the work area's `.dockerixignore`, and the optional content
//! hash (`DOCKERIX_SYNC_HASH=1`).

use std::fs::File;
use std::io::{self, Read};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use super::EPOCH_MARKER;
use super::manifest::Kind;

/// Project-specific excludes, in the work-area root. See `Ignore`.
pub(super) const IGNORE_FILE: &str = ".dockerixignore";

/// Built-in ignore list: `.git` (large, and git stays a host-side operation),
/// editor litter, the epoch marker and the ignore file itself, and what
/// RISC iX's NFS client leaves renamed in a tree it once had mounted
/// (`nfs_server::silly_renamed`). Applies to any component of a path, so a
/// `.git` anywhere prunes its subtree.
pub(super) fn builtin_ignored(name: &str) -> bool {
    matches!(name, ".git" | ".DS_Store" | EPOCH_MARKER | IGNORE_FILE)
        || name.ends_with(".swp")
        || name.ends_with('~')
        || crate::nfs_server::silly_renamed(name)
}

/// `.dockerixignore`: one pattern per line; blank lines and `#` comments are
/// skipped. A pattern without a `/` matches a name anywhere in the tree
/// (`*.o`, `build`); one with a `/` is a path from the work-area root
/// (`doc/*.ps`, `/core`). A trailing `/` restricts it to directories. `*`
/// and `?` do not cross a `/`; `**` does. An ignored directory is skipped
/// whole. There is no negation.
#[derive(Default)]
pub(super) struct Ignore {
    patterns: Vec<Pattern>,
}

struct Pattern {
    glob: Vec<u8>,
    anchored: bool,
    dir_only: bool,
}

impl Ignore {
    fn parse(text: &str) -> Ignore {
        let mut patterns = Vec::new();
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let dir_only = line.ends_with('/');
            let body = line.trim_end_matches('/');
            let anchored = body.contains('/');
            let body = body.trim_start_matches('/');
            if !body.is_empty() {
                patterns.push(Pattern { glob: body.as_bytes().to_vec(), anchored, dir_only });
            }
        }
        Ignore { patterns }
    }

    /// The work area's ignore file, or no patterns if there is none.
    fn load(root: &Path) -> Ignore {
        std::fs::read_to_string(root.join(IGNORE_FILE)).map(|t| Ignore::parse(&t)).unwrap_or_default()
    }

    /// Does any pattern match the path `rel` (whose last component is
    /// `name`)? Ancestors are the sweep's business: it prunes them.
    pub(super) fn matches(&self, rel: &str, name: &str, is_dir: bool) -> bool {
        self.patterns
            .iter()
            .any(|p| (is_dir || !p.dir_only) && glob(&p.glob, if p.anchored { rel } else { name }.as_bytes()))
    }
}

/// Glob match of the whole of `t`: `*` and `?` stop at a `/`, `**` does
/// not (and `**/` also matches no directory at all).
fn glob(p: &[u8], t: &[u8]) -> bool {
    match p {
        [] => t.is_empty(),
        [b'*', b'*', rest @ ..] => (0..=t.len()).any(|i| {
            glob(rest, &t[i..])
                || (rest.first() == Some(&b'/') && (i == 0 || t[i - 1] == b'/') && glob(&rest[1..], &t[i..]))
        }),
        [b'*', rest @ ..] => {
            (0..=t.len()).take_while(|&i| i == 0 || t[i - 1] != b'/').any(|i| glob(rest, &t[i..]))
        }
        [b'?', rest @ ..] => matches!(t, [c, ..] if *c != b'/') && glob(rest, &t[1..]),
        [c, rest @ ..] => t.first() == Some(c) && glob(rest, &t[1..]),
    }
}

/// What a sweep applies: the ignore rules, and whether to hash contents.
pub(super) struct Rules {
    pub(super) ignore: Ignore,
    pub(super) hash: bool,
}

impl Rules {
    pub(super) fn load(root: &Path, hash: bool) -> Rules {
        Rules { ignore: Ignore::load(root), hash }
    }

    /// Is `rel` (a directory if `is_dir`) left out of the sweep — because
    /// it, or any directory above it, is ignored? The sweep decides this
    /// incrementally as it descends; this is the same answer for a path
    /// already in the manifest.
    pub(super) fn excluded(&self, rel: &str, is_dir: bool) -> bool {
        let mut end = 0;
        for comp in rel.split('/') {
            end += comp.len();
            let prefix = &rel[..end];
            let dir = end < rel.len() || is_dir;
            if builtin_ignored(comp) || self.ignore.matches(prefix, comp, dir) {
                return true;
            }
            end += 1;
        }
        false
    }
}

/// `DOCKERIX_SYNC_HASH=1`: compare file contents, not just size and mtime.
/// Read where the environment is the user's (`rix`, `get`/`put`); the
/// serve-mode push gets it in the request.
pub fn hash_enabled() -> bool {
    crate::config::sync_hash()
}

/// FNV-1a over a file's content (a link's target). Not cryptographic: it
/// tells a rewrite from a no-op, nothing more.
pub(super) fn content_hash(path: &Path, kind: Kind) -> io::Result<u64> {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    let mut mix = |bytes: &[u8]| {
        for &b in bytes {
            h ^= b as u64;
            h = h.wrapping_mul(0x0100_0000_01b3);
        }
    };
    match kind {
        Kind::Link => mix(std::fs::read_link(path)?.as_os_str().as_bytes()),
        Kind::File => {
            let mut f = File::open(path)?;
            let mut buf = [0u8; 65536];
            loop {
                let n = f.read(&mut buf)?;
                if n == 0 {
                    break;
                }
                mix(&buf[..n]);
            }
        }
        Kind::Dir => {}
    }
    Ok(h)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sync::manifest::Manifest;
    use crate::sync::{LeftOut, sweep};
    use crate::testsupport::tmp_dir;
    use std::time::SystemTime;

    #[test]
    fn ignore_file_patterns() {
        let ig = Ignore::parse("# products\n*.o\nbuild/\n\ndoc/*.ps\n/core\n**/gen/*.h\nx?\n");
        let m = |rel: &str, dir: bool| ig.matches(rel, rel.rsplit('/').next().unwrap(), dir);
        assert!(m("a.o", false) && m("deep/er/b.o", false));
        assert!(!m("a.oo", false) && !m("a.c", false));
        assert!(m("build", true) && m("src/build", true) && !m("build", false));
        assert!(m("doc/x.ps", false) && !m("doc/sub/x.ps", false) && !m("other/x.ps", false));
        assert!(m("core", false) && !m("src/core", false));
        assert!(m("gen/a.h", false) && m("a/b/gen/a.h", false) && !m("gen/sub/a.h", false));
        assert!(m("xy", false) && !m("xyz", false) && !m("x/", false));

        // A sweep applies them, prunes ignored directories, and agrees with
        // `excluded` on every path it does and does not visit.
        let tmp = tmp_dir("ignore");
        let dir = tmp.path();
        std::fs::write(dir.join(IGNORE_FILE), "*.o\nbuild/\n/core\n").unwrap();
        std::fs::create_dir_all(dir.join("build/deep")).unwrap();
        std::fs::create_dir_all(dir.join("src")).unwrap();
        for f in ["build/deep/x.c", "build/y", "src/a.c", "src/a.o", "core", "src/core", "src/build"] {
            std::fs::write(dir.join(f), "z").unwrap();
        }
        let rules = Rules::load(dir, false);
        let t = sweep(dir, &rules, &mut LeftOut::default()).unwrap();
        let keys: Vec<&str> = t.keys().map(String::as_str).collect();
        assert_eq!(keys, ["src", "src/a.c", "src/build", "src/core"]);
        for (p, d) in [
            ("build", true),
            ("build/deep/x.c", false),
            ("src/a.o", false),
            ("core", false),
            (".git/HEAD", false),
            ("src/.git", true),
        ] {
            assert!(rules.excluded(p, d), "{p}");
        }
        for (p, e) in &t {
            assert!(!rules.excluded(p, e.kind == Kind::Dir), "{p}");
        }
    }

    #[test]
    fn content_hash_catches_same_size_rewrite() {
        let tmp = tmp_dir("hash");
        let dir = tmp.path();
        let f = dir.join("f");
        std::fs::write(&f, "aaa").unwrap();
        let when = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
        File::options().write(true).open(&f).unwrap().set_modified(when).unwrap();
        let plain = Rules { ignore: Ignore::default(), hash: false };
        let hashed = Rules { ignore: Ignore::default(), hash: true };
        let before = sweep(dir, &plain, &mut LeftOut::default()).unwrap();
        let before_h = sweep(dir, &hashed, &mut LeftOut::default()).unwrap();
        std::fs::write(&f, "bbb").unwrap();
        File::options().write(true).open(&f).unwrap().set_modified(when).unwrap();
        assert!(!before["f"].differs(&sweep(dir, &plain, &mut LeftOut::default()).unwrap()["f"]));
        assert!(before_h["f"].differs(&sweep(dir, &hashed, &mut LeftOut::default()).unwrap()["f"]));
        // Links hash their target.
        std::os::unix::fs::symlink("f", dir.join("l")).unwrap();
        let h1 = content_hash(&dir.join("l"), Kind::Link).unwrap();
        std::fs::remove_file(dir.join("l")).unwrap();
        std::os::unix::fs::symlink("g", dir.join("l")).unwrap();
        assert_ne!(h1, content_hash(&dir.join("l"), Kind::Link).unwrap());
        // Adopting a hashed sweep into a hashless manifest is a change worth
        // saving; adopting the same sweep again is not.
        let mut m = Manifest { epoch: "e".into(), entries: before };
        assert!(m.adopt(before_h.clone()));
        assert!(!m.adopt(before_h));
    }
}
