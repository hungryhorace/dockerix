//! First-boot network configuration for the guest disc, applied when the
//! pristine RISC iX image is seeded into the user's state directory.
//!
//! The image is a 4.3BSD fast filesystem, and we have no ffs writer, so every
//! change is an in-place, same-length overwrite of bytes at a fixed offset:
//! inode sizes and block maps are untouched and fsck stays happy. Values are
//! padded with spaces (harmless in sh and in these tables). Each edit is
//! verified against the bytes it expects to find before anything is written,
//! so an image that is not the known pristine one is left alone and the
//! console recipe in docs/how-it-works.md applies instead.
//!
//! What the edits do (why each is needed is in docs/how-it-works.md, "A
//! fresh disc image" and "The exec channel"):
//!
//! - /etc/hosts: riscix's address becomes SLiRP's guest IP 10.0.2.15, and a
//!   comment line becomes the `gateway` entry for 10.0.2.2.
//! - /etc/rc.net: the broadcast address becomes 10.0.2.255; a comment line
//!   becomes `route add default 10.0.2.2 1`, without which inbound forwarded
//!   connections (off-subnet source) can never be answered; and another
//!   comment line writes /etc/resolv.conf if it is missing. A new file cannot
//!   be made by same-length edits, and telnetd's reverse lookup of the peer
//!   hangs without a nameserver, so rc.net (run by /etc/rc after the root
//!   filesystem is mounted read-write) creates it on the first boot.
//! - /etc/ttys: the network ptys ttyp0–ttypf become `secure`, or login refuses
//!   root on them. `network secure` does not fit in the line, so the terminal
//!   type is shortened to `net`; it only ever reaches TERM when a telnet
//!   client sends no terminal type, and `rix` always does.

use std::fs::OpenOptions;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::Path;

/// One same-length overwrite: at `offset`, `from` is expected and `to` written.
struct Edit {
    what: &'static str,
    offset: u64,
    from: &'static [u8],
    to: &'static [u8],
}

const EDITS: &[Edit] = &[
    Edit {
        what: "/etc/rc.net resolv.conf hook",
        offset: 67080793,
        from: b"# You will also need a /etc/hosts file that corresponds to your local network ",
        to: b"[ -f /etc/resolv.conf ] || echo nameserver 10.0.2.3 >/etc/resolv.conf         ",
    },
    Edit {
        what: "/etc/rc.net broadcast address",
        offset: 67081073,
        from: b"BROADCAST='broadcast 192.168.128.0'",
        to: b"BROADCAST='broadcast 10.0.2.255'   ",
    },
    Edit {
        what: "/etc/rc.net default route",
        offset: 67082203,
        from: b"# $Header: /smsa/bsd:system.admin/rc.net:bsd.riscix1_2  1.10  $",
        to: b"route add default 10.0.2.2 1 # dockerix (SLiRP gateway)        ",
    },
    Edit {
        what: "/etc/hosts gateway entry",
        offset: 67082967,
        from: b"# before the nameserver is started.",
        to: b"10.0.2.2\t\tgateway                  ",
    },
    Edit {
        what: "/etc/hosts riscix address",
        offset: 67083064,
        from: b"192.168.128.200",
        to: b"10.0.2.15      ",
    },
];

/// /etc/ttys: `ttypN<TAB>none<TAB><TAB><TAB><TAB>network` lines, one per
/// pty, 22 bytes each including the newline, starting with ttyp0.
const TTYS_FIRST: u64 = 67221226;
const TTYS_STRIDE: u64 = 22;
const TTYS_PTYS: &[u8] = b"0123456789abcdef";

fn ttys_edit(i: usize) -> (u64, Vec<u8>, Vec<u8>) {
    let c = TTYS_PTYS[i];
    let mut from = b"ttyp".to_vec();
    from.push(c);
    from.extend_from_slice(b"\tnone\t\t\t\tnetwork");
    let mut to = b"ttyp".to_vec();
    to.push(c);
    to.extend_from_slice(b"\tnone\tnet\tsecure");
    (TTYS_FIRST + i as u64 * TTYS_STRIDE, from, to)
}

fn all_edits() -> Vec<(String, u64, Vec<u8>, Vec<u8>)> {
    let mut v: Vec<_> =
        EDITS.iter().map(|e| (e.what.to_string(), e.offset, e.from.to_vec(), e.to.to_vec())).collect();
    for (i, &ch) in TTYS_PTYS.iter().enumerate() {
        let (off, from, to) = ttys_edit(i);
        v.push((format!("/etc/ttys ttyp{} secure", ch as char), off, from, to));
    }
    for (what, _, from, to) in &v {
        assert_eq!(from.len(), to.len(), "edit '{what}' changes the length");
    }
    v
}

/// Outcome of `apply`, for the seeding log line.
pub enum Outcome {
    /// All edits applied.
    Patched,
    /// Every edit's target already held the new bytes.
    AlreadyPatched,
    /// The named edit found neither the old nor the new bytes; nothing written.
    NotPristine(String),
}

/// Apply the network configuration to the disc image at `path`.
pub fn apply(path: &Path) -> io::Result<Outcome> {
    let edits = all_edits();
    let mut f = OpenOptions::new().read(true).write(true).open(path)?;
    // Verify everything before writing anything.
    let mut already = 0;
    for (what, off, from, to) in &edits {
        let mut buf = vec![0u8; from.len()];
        f.seek(SeekFrom::Start(*off))?;
        f.read_exact(&mut buf)?;
        if &buf == to {
            already += 1;
        } else if &buf != from {
            return Ok(Outcome::NotPristine(what.clone()));
        }
    }
    if already == edits.len() {
        return Ok(Outcome::AlreadyPatched);
    }
    for (_, off, _, to) in &edits {
        f.seek(SeekFrom::Start(*off))?;
        f.write_all(to)?;
    }
    f.sync_all()?;
    Ok(Outcome::Patched)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edits_are_same_length() {
        all_edits();
    }

    /// The offsets and expected bytes match the pristine image, when the
    /// (undistributed) asset is where `dockerix start` would look for it:
    /// `DOCKERIX_ASSETS`, else `~/.dockerix/assets`.
    #[test]
    fn edits_match_pristine_image() {
        let path = crate::config::assets()
            .unwrap_or_else(|| crate::host::dockerix_home("assets"))
            .join("images/hdimage.hdf");
        if !path.exists() {
            eprintln!("skipped: {} not present", path.display());
            return;
        }
        let mut f = std::fs::File::open(path).unwrap();
        for (what, off, from, _) in all_edits() {
            let mut buf = vec![0u8; from.len()];
            f.seek(SeekFrom::Start(off)).unwrap();
            f.read_exact(&mut buf).unwrap();
            assert_eq!(buf, from, "{what}: unexpected bytes at {off}");
        }
    }
}
