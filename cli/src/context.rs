//! The image's build context: the files the Dockerfile copies, packed as a
//! deterministic ustar archive. `build.rs` writes it into OUT_DIR and the
//! binary embeds it, so `dockerix start` can build the image without the
//! checkout (docs/architecture.md, "Building the image"). This file is
//! compiled into `build.rs`; the crate compiles it only for its tests.

use std::io;
use std::path::Path;

/// What the Dockerfile copies, relative to the repository root. The archive
/// keeps these paths, so the Dockerfile's `COPY` lines work on it unchanged.
pub const ROOTS: [&str; 10] = [
    "Dockerfile",
    "cli/Cargo.toml",
    "cli/Cargo.lock",
    "cli/build.rs",
    "cli/src",
    "cmos",
    "configs",
    "container",
    "hostfs",
    "patches",
];

/// One file in the archive.
pub struct Member {
    pub path: String,
    pub executable: bool,
    pub data: Vec<u8>,
}

/// The files under `ROOTS` in the checkout at `root`, sorted by path.
/// Dotfiles (.DS_Store, editor state) are left out.
pub fn collect(root: &Path) -> io::Result<Vec<Member>> {
    let mut out = Vec::new();
    for rel in ROOTS {
        walk(root, rel, &mut out)?;
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(out)
}

fn walk(root: &Path, rel: &str, out: &mut Vec<Member>) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let path = root.join(rel);
    let with_path = |e: io::Error| io::Error::new(e.kind(), format!("{}: {e}", path.display()));
    let md = std::fs::metadata(&path).map_err(with_path)?;
    if md.is_dir() {
        for entry in std::fs::read_dir(&path).map_err(with_path)? {
            let name = entry?.file_name();
            let Some(name) = name.to_str() else {
                return Err(io::Error::other(format!("{}: non-UTF-8 name in {rel}", path.display())));
            };
            if !name.starts_with('.') {
                walk(root, &format!("{rel}/{name}"), out)?;
            }
        }
    } else {
        out.push(Member {
            path: rel.to_string(),
            executable: md.permissions().mode() & 0o111 != 0,
            data: std::fs::read(&path).map_err(with_path)?,
        });
    }
    Ok(())
}

/// The members as a ustar archive: mtime 0, uid/gid 0, mode 0644 (0755 if
/// executable), contents byte for byte, so the same files always give the
/// same bytes.
pub fn archive(members: &[Member]) -> io::Result<Vec<u8>> {
    let mut tar = Vec::new();
    for m in members {
        tar.extend_from_slice(&header(m)?);
        tar.extend_from_slice(&m.data);
        tar.resize(tar.len().next_multiple_of(512), 0);
    }
    // End of archive: two zero blocks.
    tar.resize(tar.len() + 1024, 0);
    Ok(tar)
}

fn header(m: &Member) -> io::Result<[u8; 512]> {
    let name = m.path.as_bytes();
    if name.len() > 100 {
        return Err(io::Error::other(format!("{}: path too long for a ustar name", m.path)));
    }
    let mut h = [0u8; 512];
    h[..name.len()].copy_from_slice(name);
    octal(&mut h[100..108], if m.executable { 0o755 } else { 0o644 });
    octal(&mut h[108..116], 0); // uid
    octal(&mut h[116..124], 0); // gid
    octal(&mut h[124..136], m.data.len() as u64);
    octal(&mut h[136..148], 0); // mtime
    h[156] = b'0'; // regular file
    h[257..263].copy_from_slice(b"ustar\0");
    h[263..265].copy_from_slice(b"00");
    // The checksum is summed with its own field read as spaces.
    h[148..156].fill(b' ');
    let sum: u32 = h.iter().map(|&b| u32::from(b)).sum();
    h[148..156].copy_from_slice(format!("{sum:06o}\0 ").as_bytes());
    Ok(h)
}

/// Zero-padded octal, NUL-terminated, filling the field.
fn octal(field: &mut [u8], value: u64) {
    let digits = format!("{value:0width$o}\0", width = field.len() - 1);
    field.copy_from_slice(digits.as_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::process::{Command, Stdio};

    /// The archive build.rs made and the binary embeds.
    const EMBEDDED: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/context.tar"));

    fn repo_root() -> &'static Path {
        Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/.."))
    }

    fn field(h: &[u8], range: std::ops::Range<usize>) -> u64 {
        let s = std::str::from_utf8(&h[range]).unwrap();
        u64::from_str_radix(s.trim_end_matches(['\0', ' ']), 8).unwrap()
    }

    /// (name, mode) of each member, read back from the headers.
    fn list(tar: &[u8]) -> Vec<(String, u64)> {
        let mut out = Vec::new();
        let mut at = 0;
        while tar[at..at + 512].iter().any(|&b| b != 0) {
            let h = &tar[at..at + 512];
            let name = h[..100].split(|&b| b == 0).next().unwrap();
            assert_eq!(field(h, 108..116), 0, "uid");
            assert_eq!(field(h, 116..124), 0, "gid");
            assert_eq!(field(h, 136..148), 0, "mtime");
            out.push((String::from_utf8(name.to_vec()).unwrap(), field(h, 100..108)));
            at += 512 + (field(h, 124..136) as usize).next_multiple_of(512);
        }
        out
    }

    /// Run `tar <args>` on the embedded archive, returning stdout.
    fn tar(args: &[&str]) -> Vec<u8> {
        let mut child =
            Command::new("tar").args(args).stdin(Stdio::piped()).stdout(Stdio::piped()).spawn().unwrap();
        child.stdin.take().unwrap().write_all(EMBEDDED).unwrap();
        let out = child.wait_with_output().unwrap();
        assert!(out.status.success(), "tar {args:?} failed");
        out.stdout
    }

    #[test]
    fn archive_is_deterministic_and_embedded() {
        let a = archive(&collect(repo_root()).unwrap()).unwrap();
        let b = archive(&collect(repo_root()).unwrap()).unwrap();
        assert!(a == b, "two archives of the same checkout differ");
        assert!(a == EMBEDDED, "the embedded archive is not this checkout's");
    }

    #[test]
    fn archive_lists_the_context() {
        let members = list(EMBEDDED);
        let names: Vec<&str> = members.iter().map(|(n, _)| n.as_str()).collect();
        // Sorted, so the Dockerfile at the root comes first.
        assert_eq!(names[0], "Dockerfile");
        assert!(names.is_sorted());
        for want in [
            "cli/Cargo.lock",
            "cli/build.rs",
            "cli/src/context.rs",
            "hostfs/!Boot,feb",
            "patches/arculator/0001-slirp-thread-safety.patch",
        ] {
            assert!(names.contains(&want), "{want} missing");
        }
        for (name, mode) in &members {
            assert!(
                ROOTS.iter().any(|r| name == r || name.starts_with(&format!("{r}/"))),
                "{name} is outside ROOTS"
            );
            assert!(!name.split('/').any(|c| c.starts_with('.')), "{name} is a dotfile");
            assert!(*mode == 0o644 || *mode == 0o755, "{name} has mode {mode:o}");
        }
    }

    #[test]
    fn tar_reads_the_archive() {
        let listed = String::from_utf8(tar(&["-tf", "-"])).unwrap();
        let ours: Vec<String> = list(EMBEDDED).into_iter().map(|(n, _)| n).collect();
        assert_eq!(listed.lines().collect::<Vec<_>>(), ours);
        // Contents come back byte for byte (the Arculator patches carry CRs).
        let patch = "patches/arculator/0001-slirp-thread-safety.patch";
        assert_eq!(tar(&["-xOf", "-", patch]), std::fs::read(repo_root().join(patch)).unwrap());
    }
}
