//! The guest's `cpio -o` archive, read and rewritten as a tar for the rest
//! of `get` (`place`, the wrapper's stream) to unpack.
//!
//! Why cpio: the guest's 1990 tar writes names of at most 99 bytes and
//! leaves a longer path out, with a message and a 0 exit. Its cpio has no
//! such limit (2-byte name lengths), and the guest's `find` lists paths to
//! about 780 bytes. Its binary format, not `-c`: under `-c` it gives a
//! symlink a binary header all the same, and the ASCII archive is unreadable
//! from there on (measured).
//!
//! The binary format: a 26-byte header of 13 16-bit words, little-endian on
//! the ARM guest (magic 070707, dev, ino, mode, uid, gid, nlink, rdev, mtime
//! as two words high first, the name's length with its NUL, the size as
//! two words), the name, the data; name and data each padded to an even
//! length. `TRAILER!!!` ends it. The tar is ustar, with a pax record for a
//! name or link target too long for its fields; GNU tar and bsdtar read
//! both.

use std::collections::HashMap;
use std::io;

/// One member of an archive.
#[derive(Debug, Clone, PartialEq)]
pub struct Member {
    pub name: Vec<u8>,
    /// Type and permission bits, as `st_mode`.
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
    pub mtime: u32,
    /// Its device and link count, for hard links, and the archive's inode
    /// number, which is no use for them (`to_tar`).
    pub dev: u16,
    pub ino: u16,
    pub nlink: u16,
    /// A file's contents, or a symlink's target.
    pub data: Vec<u8>,
}

/// What a tar header says of a member besides its name, type and size.
#[derive(Clone, Copy)]
struct Stat {
    /// Permission bits only.
    mode: u32,
    uid: u32,
    gid: u32,
    mtime: u32,
}

const MAGIC: u16 = 0o070707;
const TRAILER: &[u8] = b"TRAILER!!!";
const S_IFMT: u32 = 0o170000;
const S_IFDIR: u32 = 0o040000;
const S_IFREG: u32 = 0o100000;
const S_IFLNK: u32 = 0o120000;

/// The members of a binary cpio archive, up to its trailer, as the
/// little-endian guest writes it. An archive cut short is an error.
pub fn read(archive: &[u8]) -> io::Result<Vec<Member>> {
    let bad =
        |at: usize, why: &str| io::Error::other(format!("the guest's cpio archive at byte {at}: {why}"));
    let mut members = Vec::new();
    let mut at = 0;
    loop {
        let Some(h) = archive.get(at..at + 26) else {
            return Err(bad(at, "cut short, with no trailer"));
        };
        let w = |i: usize| u16::from_le_bytes([h[2 * i], h[2 * i + 1]]);
        let long = |i: usize| (u32::from(w(i)) << 16) | u32::from(w(i + 1));
        if w(0) != MAGIC {
            return Err(bad(at, "no binary cpio header where one should be"));
        }
        let (name_len, size) = (usize::from(w(10)), long(11) as usize);
        let name_at = at + 26;
        let data_at = name_at + name_len + name_len % 2;
        let (Some(name), Some(data)) =
            (archive.get(name_at..name_at + name_len), archive.get(data_at..data_at + size))
        else {
            return Err(bad(at, "cut short, with no trailer"));
        };
        let name = name.strip_suffix(b"\0").unwrap_or(name).to_vec();
        if name == TRAILER {
            return Ok(members);
        }
        members.push(Member {
            name,
            mode: u32::from(w(3)),
            uid: u32::from(w(4)),
            gid: u32::from(w(5)),
            mtime: long(8),
            dev: w(1),
            ino: w(2),
            nlink: w(6),
            data: data.to_vec(),
        });
        at = data_at + size + size % 2;
    }
}

/// The guest's `ls -i` lines (`  28682 bin/ex`) as `to_tar`'s `inodes`.
pub fn parse_inodes(listing: &[u8]) -> HashMap<Vec<u8>, u32> {
    listing
        .split(|&b| b == b'\n')
        .filter_map(|line| {
            let line = line.trim_ascii();
            let gap = line.iter().position(|&b| b == b' ')?;
            let ino = std::str::from_utf8(&line[..gap]).ok()?.parse().ok()?;
            Some((line[gap..].trim_ascii_start().to_vec(), ino))
        })
        .collect()
}

/// Whether any member is a file with other links, which `to_tar` needs
/// `inodes` for.
pub fn has_links(members: &[Member]) -> bool {
    members.iter().any(|m| m.mode & S_IFMT == S_IFREG && m.nlink > 1)
}

/// `members` as a tar, and the names of those left out: sockets, fifos and
/// devices, which `get` does not bring back. A file with the same inode as
/// one earlier, by `inodes` (name to inode number, from the guest's `ls
/// -i`), is a hard link to it. Not by the archive's own inode numbers: the
/// guest's cpio numbers files itself, in the order it meets them, and gives
/// most links numbers of their own (in `/usr/bin`, `ex` and `vi` came out
/// as 279 and 291). A file `inodes` does not name is a file of its own.
pub fn to_tar(members: &[Member], inodes: &HashMap<Vec<u8>, u32>) -> (Vec<u8>, Vec<String>) {
    let mut tar = Vec::new();
    let mut left_out = Vec::new();
    let mut seen: HashMap<(u16, u32), &Member> = HashMap::new();
    for m in members {
        let mut name = m.name.clone();
        let inode = inodes.get(&m.name).filter(|_| m.nlink > 1).map(|&ino| (m.dev, ino));
        let (kind, link, size) = match m.mode & S_IFMT {
            S_IFDIR => {
                name.push(b'/');
                (b'5', &[][..], 0)
            }
            S_IFLNK => (b'2', &m.data[..], 0),
            S_IFREG => match inode.and_then(|i| seen.get(&i)) {
                // It came with its contents again: the same, or not a link.
                Some(first) if first.data == m.data => (b'1', &first.name[..], 0),
                _ => {
                    if let Some(i) = inode {
                        seen.insert(i, m);
                    }
                    (b'0', &[][..], m.data.len())
                }
            },
            _ => {
                left_out.push(String::from_utf8_lossy(&m.name).into_owned());
                continue;
            }
        };
        let stat = Stat { mode: m.mode & 0o7777, uid: m.uid, gid: m.gid, mtime: m.mtime };
        let mut pax = Vec::new();
        if name.len() > 100 {
            pax.extend(pax_record("path", &name));
        }
        if link.len() > 100 {
            pax.extend(pax_record("linkpath", link));
        }
        if !pax.is_empty() {
            let owner = Stat { mode: 0o644, uid: 0, gid: 0, ..stat };
            tar.extend(header(b"././@PaxHeader", b'x', b"", pax.len(), owner));
            push_padded(&mut tar, &pax);
        }
        tar.extend(header(&name, kind, link, size, stat));
        if kind == b'0' {
            push_padded(&mut tar, &m.data);
        }
    }
    tar.resize(tar.len() + 1024, 0);
    (tar, left_out)
}

/// A pax extended header record, `LEN key=value\n`, its length counting
/// its own digits.
fn pax_record(key: &str, value: &[u8]) -> Vec<u8> {
    let rest = key.len() + value.len() + 3; // the space, the `=`, the newline
    let mut len = rest + 1;
    while len != rest + len.to_string().len() {
        len = rest + len.to_string().len();
    }
    let mut record = format!("{len} {key}=").into_bytes();
    record.extend_from_slice(value);
    record.push(b'\n');
    record
}

/// A ustar header block of type `kind`. A field too short for `name` or
/// `link` holds the start of it, the pax record before it the whole.
fn header(name: &[u8], kind: u8, link: &[u8], size: usize, stat: Stat) -> [u8; 512] {
    let mut h = [0u8; 512];
    let mut put = |at: usize, len: usize, bytes: &[u8]| {
        let n = bytes.len().min(len);
        h[at..at + n].copy_from_slice(&bytes[..n]);
    };
    let octal = |width: usize, v: u64| format!("{v:0w$o}\0", w = width - 1).into_bytes();
    put(0, 100, name);
    put(100, 8, &octal(8, stat.mode.into()));
    put(108, 8, &octal(8, stat.uid.into()));
    put(116, 8, &octal(8, stat.gid.into()));
    put(124, 12, &octal(12, size as u64));
    put(136, 12, &octal(12, stat.mtime.into()));
    put(148, 8, b"        ");
    put(156, 1, &[kind]);
    put(157, 100, link);
    put(257, 8, b"ustar\x0000");
    let sum: u32 = h.iter().map(|&b| u32::from(b)).sum();
    h[148..156].copy_from_slice(format!("{sum:06o}\0 ").as_bytes());
    h
}

/// `data`, padded with NULs to a whole number of 512-byte blocks.
fn push_padded(tar: &mut Vec<u8>, data: &[u8]) {
    tar.extend_from_slice(data);
    tar.resize(tar.len().next_multiple_of(512), 0);
}

/// `members` as the guest's `cpio -o` writes them (little-endian, as the
/// ARM guest): for the fake guest and the tests.
#[cfg(test)]
pub fn write(members: &[Member]) -> Vec<u8> {
    let trailer = Member {
        name: TRAILER.to_vec(),
        mode: 0,
        uid: 0,
        gid: 0,
        mtime: 0,
        dev: 0,
        ino: 0,
        nlink: 1,
        data: Vec::new(),
    };
    let mut out = Vec::new();
    for m in members.iter().chain([&trailer]) {
        let name_len = m.name.len() + 1;
        let size = m.data.len() as u32;
        let (uid, gid) = (m.uid as u16, m.gid as u16);
        let (mtime, mode) = ([(m.mtime >> 16) as u16, m.mtime as u16], m.mode as u16);
        let words = [MAGIC, m.dev, m.ino, mode, uid, gid, m.nlink, 0, mtime[0], mtime[1]];
        for w in words.into_iter().chain([name_len as u16, (size >> 16) as u16, size as u16]) {
            out.extend_from_slice(&w.to_le_bytes());
        }
        out.extend_from_slice(&m.name);
        out.push(0);
        if name_len % 2 == 1 {
            out.push(0);
        }
        out.extend_from_slice(&m.data);
        if m.data.len() % 2 == 1 {
            out.push(0);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testsupport::tmp_dir;
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    use std::process::{Command, Stdio};

    fn member(name: &str, mode: u32, ino: u16, nlink: u16, data: &[u8]) -> Member {
        let (name, data) = (name.into(), data.into());
        Member { name, mode, uid: 0, gid: 0, mtime: 1_700_000_000, dev: 0xc00, ino, nlink, data }
    }

    /// Unpack `tar` with the tar here (bsdtar on a Mac, GNU tar on Linux)
    /// into a new directory.
    fn untar(tar: &[u8]) -> tempfile::TempDir {
        use std::io::Write;
        let dir = tmp_dir("cpio-untar");
        let mut child = Command::new("tar")
            .args(["xf", "-", "-C"])
            .arg(dir.path())
            .stdin(Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(tar).unwrap();
        assert!(child.wait().unwrap().success(), "the tar here unpacks it");
        dir
    }

    /// The start of an archive the guest wrote (`find t -print | cpio -o`):
    /// a directory, a file and its hard link, with `write`'s
    /// symlink and trailer after them.
    #[test]
    fn reads_what_the_guest_writes() {
        #[rustfmt::skip]
        let mut guest = vec![
            0xc7, 0x71, 0x00, 0x0c, 0x01, 0x00, 0xed, 0x41, 0x00, 0x00, 0x00, 0x00, 0x04, 0x00, 0x09, 0xec,
            0xcf, 0x6a, 0x16, 0xe8, 0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0x74, 0x00, 0xc7, 0x71, 0x00, 0x0c,
            0x02, 0x00, 0xa4, 0x81, 0x00, 0x00, 0x00, 0x00, 0x02, 0x00, 0x0a, 0xec, 0xcf, 0x6a, 0x15, 0xe8,
            0x04, 0x00, 0x00, 0x00, 0x03, 0x00, 0x74, 0x2f, 0x66, 0x00, 0x68, 0x69, 0x0a, 0x00, 0xc7, 0x71,
            0x00, 0x0c, 0x02, 0x00, 0xa4, 0x81, 0x00, 0x00, 0x00, 0x00, 0x02, 0x00, 0x0a, 0xec, 0xcf, 0x6a,
            0x15, 0xe8, 0x07, 0x00, 0x00, 0x00, 0x03, 0x00, 0x74, 0x2f, 0x68, 0x61, 0x72, 0x64, 0x00, 0x00,
            0x68, 0x69, 0x0a, 0x00,
        ];
        guest.extend_from_slice(&write(&[member("t/soft", 0o120777, 3, 1, b"f")]));
        let members = read(&guest).unwrap();
        let names: Vec<_> = members.iter().map(|m| String::from_utf8_lossy(&m.name).into_owned()).collect();
        assert_eq!(names, ["t", "t/f", "t/hard", "t/soft"]);
        assert_eq!((members[0].mode, members[0].nlink, members[0].mtime), (0o040755, 4, 0x6acf_e816));
        assert_eq!((members[1].mode, members[1].ino, members[1].nlink), (0o100644, 2, 2));
        assert_eq!((&members[2].data[..], members[2].ino), (&b"hi\n"[..], 2));
        assert_eq!(members[3].data, b"f");
    }

    #[test]
    fn an_archive_cut_short_is_an_error() {
        let whole = write(&[member("t/f", 0o100644, 2, 1, b"hello\n")]);
        assert!(read(&whole).is_ok());
        for len in [0, 1, 20, 40, whole.len() - 1] {
            assert!(read(&whole[..len]).is_err(), "{len} bytes");
        }
        assert!(read(b"0707070006000000").is_err(), "an ASCII (-c) archive");
    }

    /// The tar unpacks, with the tar here, into what the guest had: long
    /// names whole, the symlinks, the hard link, modes and times.
    #[test]
    fn the_tar_unpacks_to_what_the_guest_had() {
        let dirs = |n: usize| format!("t/{}", ["d234567890"; 70][..n].join("/"));
        let deep = format!("{}/file", dirs(70));
        let target = format!("../{}", "x".repeat(150));
        let mut members = vec![
            member("t", 0o040755, 1, 3, b""),
            member("t/f", 0o100640, 2, 2, b"hi\n"),
            member("t/hard", 0o100640, 2, 2, b"hi\n"),
            member("t/soft", 0o120777, 3, 1, b"f"),
            member("t/far", 0o120777, 4, 1, target.as_bytes()),
            member("t/e", 0o040700, 5, 2, b""),
            member("t/fifo", 0o010644, 6, 1, b""),
        ];
        members.extend((1..=70).map(|n| member(&dirs(n), 0o040755, 6 + n as u16, 3, b"")));
        members.push(member(&deep, 0o100755, 100, 1, b"deep\n"));
        let members = read(&write(&members)).unwrap();
        assert!(has_links(&members));
        let (tar, left_out) = to_tar(&members, &parse_inodes(b" 28682 t/f\r\n 28682 t/hard\r\n"));
        assert_eq!(left_out, ["t/fifo"]);
        let dir = untar(&tar);
        let t = dir.path();
        assert_eq!(std::fs::read(t.join(&deep)).unwrap(), b"deep\n");
        assert_eq!(std::fs::metadata(t.join(&deep)).unwrap().permissions().mode() & 0o7777, 0o755);
        assert_eq!(std::fs::read_link(t.join("t/soft")).unwrap().to_str(), Some("f"));
        assert_eq!(std::fs::read_link(t.join("t/far")).unwrap().to_str(), Some(target.as_str()));
        let stat = |p: &str| std::fs::metadata(t.join(p)).unwrap();
        let (f, hard) = (stat("t/f"), stat("t/hard"));
        assert_eq!((f.ino(), f.mode() & 0o7777, f.mtime()), (hard.ino(), 0o640, 1_700_000_000));
        assert!(stat("t/e").is_dir());
        assert!(!t.join("t/fifo").exists());
    }

    /// A link is told by the guest's inode numbers, not the archive's,
    /// which the guest's cpio makes up; and never between different
    /// contents.
    #[test]
    fn hard_links_by_the_guests_inodes() {
        let linked = |members: &[Member], inodes: &[u8]| {
            let dir = untar(&to_tar(members, &parse_inodes(inodes)).0);
            let ino = |p: &str| std::fs::metadata(dir.path().join(p)).unwrap().ino();
            let b = std::fs::read(dir.path().join("b")).unwrap();
            (ino("a") == ino("b"), b)
        };
        // As the guest wrote `ex` and `vi`: one file, two archive numbers.
        let same = [member("a", 0o100644, 279, 2, b"one\n"), member("b", 0o100644, 291, 2, b"one\n")];
        assert_eq!(linked(&same, b"28682 a\n28682 b\n"), (true, b"one\n".to_vec()));
        assert_eq!(linked(&same, b""), (false, b"one\n".to_vec()), "no inodes, no link");
        let differ = [member("a", 0o100644, 7, 2, b"one\n"), member("b", 0o100644, 7, 2, b"two\n")];
        assert_eq!(linked(&differ, b"5 a\n5 b\n"), (false, b"two\n".to_vec()));
        assert!(!has_links(&[member("a", 0o100644, 7, 1, b"")]));
    }

    #[test]
    fn parses_the_guests_ls_i() {
        let got = parse_inodes(b"  28682 bin/ex\r\n28682 bin/a b\n\nnot a line\n");
        assert_eq!(got.len(), 2);
        assert_eq!((got[&b"bin/ex"[..]], got[&b"bin/a b"[..]]), (28682, 28682));
    }

    #[test]
    fn pax_records_count_their_own_length() {
        assert_eq!(pax_record("path", b"ab"), b"11 path=ab\n");
        // 98 bytes and two digits make 100, which takes three.
        let r = pax_record("path", &[b'x'; 91]);
        assert_eq!(r.len(), 101);
        assert!(r.starts_with(b"101 path="));
    }
}
