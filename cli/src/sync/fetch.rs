//! Fetching an archive up from the guest, for `get`: the guest's `find |
//! cpio -o` archives the path into the NFS staging export (or up the tty
//! when staging is unavailable) and `cpio` rewrites it here as a tar.
//! `transport` has the other direction (`tar x` on the guest), the packing
//! and unpacking, and the staging helpers this shares.
//!
//! Up the tty costs more than down: the guest's `uuencode` and its tty
//! output make a `get` well over an NFS read, so staging is preferred (see
//! `transport` for the measured rates). The guest's `find` goes as deep as
//! about 780 bytes; a path past that it leaves out, and the fetch fails
//! rather than come back without it.

use std::io;
use std::path::Path;
use std::time::Instant;

use crate::config::Config;
use crate::exec::shell_quote;
use crate::progress::{self, Progress, Stage};
use crate::session::{Session, token};
use crate::uu;

use super::transport::{GUEST_STAGE, Unstage, mount_stage, staging};

/// What the guest's `find` says (with no newline) of a path too long for
/// it, which it leaves out of the listing.
const FIND_TOO_LONG: &str = "Pathname too long";

/// A fetch's progress estimate: the expected encoded size (`encoded_size` of
/// the archive, since there is no local tar to measure yet) and the bar's
/// label. Bundled so the fetch functions stay within the argument limit.
#[derive(Clone, Copy)]
pub struct Estimate<'a> {
    pub encoded: u64,
    pub label: &'a str,
}

/// Fetch one guest path (`guest_dir/name`, a file or a whole directory) as
/// a tar: the guest's `find | cpio -o` archives it, into the staging export
/// or up the tty, and `cpio` rewrites that as a tar. The caller has checked
/// the path exists: a guest pipeline reports the status of its last
/// command, not `find`'s. Anything the guest says while archiving, past
/// cpio's count of blocks, fails the fetch: a path left out, a file it could
/// not read. `est` is the caller's size estimate and label for the progress
/// report (there is no local tar to measure yet).
pub fn fetch_from_guest(
    session: &mut Session,
    cfg: &Config,
    guest_dir: &str,
    name: &str,
    deadline: Instant,
    est: Estimate<'_>,
    progress: &mut dyn FnMut(&Progress),
) -> io::Result<Vec<u8>> {
    if staging(cfg, &cfg.stage) {
        match fetch_staged(session, cfg, guest_dir, name, deadline, est, progress)? {
            Ok(tar) => return Ok(tar),
            Err(why) => eprintln!("dockerix: NFS staging unavailable ({why}); fetching up the tty"),
        }
    }
    fetch_via_tty(session, guest_dir, name, deadline, est, progress)
}

/// The guest command that archives `name` in `dir` into the staged `file`.
pub(super) fn stage_archive_command(dir: &str, name: &str, file: &str) -> String {
    format!("cd {dir} && find {name} -print | cpio -o > {GUEST_STAGE}/{file}")
}

/// `fetch_from_guest` by NFS: have the guest mount the staging export if it
/// has not, `cpio -o` into it, and read the archive here. Ok(Err(why)) when
/// that did not work, for the tty to try: a guest `cpio` that fails is
/// tried again there, which reports a real failure (the caller has checked
/// that the path exists) as a tty fetch always has.
fn fetch_staged(
    session: &mut Session,
    cfg: &Config,
    guest_dir: &str,
    name: &str,
    deadline: Instant,
    est: Estimate<'_>,
    progress: &mut dyn FnMut(&Progress),
) -> io::Result<Result<Vec<u8>, String>> {
    let Estimate { encoded: expected, label } = est;
    let stage = &cfg.stage;
    let report = progress::worth_showing(expected, progress::STAGED_RATE);
    let mut event = |stage: Stage| {
        if report {
            progress(&Progress { label: label.to_string(), done: 0, total: expected, stage });
        }
    };
    // No byte progress to give: the guest writes the archive by NFS.
    event(Stage::Unpacking);
    if let Err(why) = mount_stage(session, cfg, deadline)? {
        return Ok(Err(why));
    }
    let file = format!("g{}.cpio", token());
    let path = Path::new(stage).join(&file);
    let _unstage = Unstage(path.clone());
    let (out, code) =
        session.run(&stage_archive_command(&shell_quote(guest_dir), &shell_quote(name), &file), deadline)?;
    if code != 0 {
        let why = String::from_utf8_lossy(&out).trim().to_string();
        return Ok(Err(format!("the guest's cpio into {GUEST_STAGE}/{file} failed: {why}")));
    }
    // Not one for the tty to try: it would hit the same.
    archiving_said(&out, guest_dir, name)?;
    // The guest's close of the file has sent it all (NFS close-to-open).
    match std::fs::read(&path) {
        Ok(archive) if !archive.is_empty() => {
            let tar = cpio_to_tar(session, &archive, guest_dir, name, deadline)?;
            event(Stage::Done);
            Ok(Ok(tar))
        }
        Ok(_) => Ok(Err(format!("{} arrived empty", path.display()))),
        Err(e) => Ok(Err(format!("reading {}: {e}", path.display()))),
    }
}

/// `fetch_from_guest` up the tty: the guest's `cpio -o | uuencode`, decoded
/// here. What the guest says while archiving goes to a file, shown after
/// uuencode's `end`, where the decoding stops: on the tty it would land
/// among the encoded lines and be decoded as data.
fn fetch_via_tty(
    session: &mut Session,
    guest_dir: &str,
    name: &str,
    deadline: Instant,
    est: Estimate<'_>,
    progress: &mut dyn FnMut(&Progress),
) -> io::Result<Vec<u8>> {
    let Estimate { encoded: expected, label } = est;
    let report = progress::worth_showing(expected, progress::OUTBOUND_RATE);
    let mut out = Vec::new();
    let mut got = 0u64;
    if report {
        progress(&Progress { label: label.to_string(), done: 0, total: expected, stage: Stage::Sending });
    }
    let said = format!("/tmp/.g{}", token());
    session.run_to(
        &format!(
            "cd {} && (find {} -print | cpio -o | uuencode x) 2>{said}; cat {said}; rm -f {said}",
            shell_quote(guest_dir),
            shell_quote(name)
        ),
        deadline,
        None,
        &mut |chunk| {
            out.extend_from_slice(chunk);
            got += chunk.len() as u64;
            if report {
                // An estimate (a directory's size is a `du` guess): never
                // show 100% before the end.
                let done = got.min(expected.saturating_sub(1));
                progress(&Progress {
                    label: label.to_string(),
                    done,
                    total: expected,
                    stage: Stage::Sending,
                });
            }
        },
    )?;
    if report {
        progress(&Progress { label: label.to_string(), done: expected, total: expected, stage: Stage::Done });
    }
    // After the `end` line: what the guest said.
    let mut encoded = 0;
    for line in out.split_inclusive(|&b| b == b'\n') {
        encoded += line.len();
        if line.trim_ascii_end() == b"end" {
            break;
        }
    }
    archiving_said(&out[encoded..], guest_dir, name)?;
    let archive = uu::decode(&out[..encoded]);
    if archive.is_empty() {
        return Err(io::Error::other(format!("the guest's cpio of {guest_dir}/{name} produced nothing")));
    }
    cpio_to_tar(session, &archive, guest_dir, name, deadline)
}

/// Fail the fetch of `guest_dir/name` if its `find` and `cpio` said more
/// (in `said`) than cpio's count of blocks: `find` leaves out a path too
/// long for it, `cpio` a file it cannot read, and it exits 0 all the same.
fn archiving_said(said: &[u8], guest_dir: &str, name: &str) -> io::Result<()> {
    let text = String::from_utf8_lossy(said);
    // `find` ends its message with no newline, so cpio's count can follow
    // it on the same line: the text before the count.
    let before_count = |l: &str| -> Option<usize> {
        let count = l.strip_suffix(" blocks")?;
        let at = count.len() - count.trim_start_matches(|c: char| !c.is_ascii_digit()).len();
        (at < count.len() && count[at..].bytes().all(|b| b.is_ascii_digit())).then_some(at)
    };
    let said: Vec<&str> = text
        .lines()
        .map(|l| {
            let l = l.trim();
            before_count(l).map_or(l, |at| l[..at].trim())
        })
        .filter(|l| !l.is_empty())
        .collect();
    if said.is_empty() {
        return Ok(());
    }
    if said.iter().any(|l| l.contains(FIND_TOO_LONG)) {
        return Err(io::Error::other(format!(
            "{guest_dir}/{name}: holds a path too long for the guest's find (about 780 bytes), which left it out"
        )));
    }
    Err(io::Error::other(format!("{guest_dir}/{name}: the guest's archiving said: {}", said.join(" | "))))
}

/// The guest's cpio archive of `guest_dir/name` as a tar, warning of what
/// it held that `get` does not bring back. If it holds files with other
/// links, the guest lists their inode numbers, to tell which are links to
/// each other (`cpio::to_tar` says why its archive cannot).
fn cpio_to_tar(
    session: &mut Session,
    archive: &[u8],
    guest_dir: &str,
    name: &str,
    deadline: Instant,
) -> io::Result<Vec<u8>> {
    let members =
        super::cpio::read(archive).map_err(|e| io::Error::other(format!("{guest_dir}/{name}: {e}")))?;
    let mut inodes = Default::default();
    if super::cpio::has_links(&members) {
        let (dir, name) = (shell_quote(guest_dir), shell_quote(name));
        let (out, _) = session
            .run(&format!("cd {dir} && find {name} -type f -links +1 -exec ls -i {{}} \\;"), deadline)?;
        inodes = super::cpio::parse_inodes(&out);
    }
    let (tar, left_out) = super::cpio::to_tar(&members, &inodes);
    if !left_out.is_empty() {
        eprintln!("dockerix: not fetching {}: a socket, fifo or device", super::some_of(&left_out));
    }
    Ok(tar)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake_guest::FakeGuest;
    use crate::sync::place;
    use crate::testsupport::tmp_dir;

    fn soon() -> Instant {
        Instant::now() + std::time::Duration::from_secs(5)
    }

    fn est() -> Estimate<'static> {
        Estimate { encoded: 100, label: "fetching" }
    }

    /// A `Config` whose fetch stages through `stage` (created), with the fake
    /// guest's export mapped to the same directory.
    fn staged_cfg(guest: &FakeGuest, stage: &Path) -> Config {
        std::fs::create_dir_all(stage).unwrap();
        guest.state().stage = stage.to_path_buf();
        Config { stage: stage.to_string_lossy().into_owned(), ..guest.config() }
    }

    /// With the staging export there, a fetch comes back by NFS: the guest
    /// archives into it, and the archive is gone afterwards.
    #[test]
    fn a_fetch_comes_back_over_nfs_when_it_can() {
        let tmp = tmp_dir("fetch-staged");
        let stage = tmp.path().join("stage");
        let guest = FakeGuest::start();
        let cfg = staged_cfg(&guest, &stage);
        guest.state().files.insert("/wa/game.c".into(), b"int game;\n".to_vec());
        let mut s = guest.login();
        let tar = fetch_from_guest(&mut s, &cfg, "/wa", "game.c", soon(), est(), &mut |_| {}).unwrap();
        let got = place(&tar, "game.c", &tmp.path().join("got")).unwrap();
        assert_eq!(std::fs::read(got).unwrap(), b"int game;\n");
        let st = guest.state();
        assert!(st.commands.iter().any(|c| c.contains("| cpio -o > /tmp/.xfer/g")), "{:?}", st.commands);
        assert!(!st.commands.iter().any(|c| c.contains("uuencode")), "{:?}", st.commands);
        assert_eq!(std::fs::read_dir(&stage).unwrap().count(), 0, "the staged archive is removed");
    }

    /// A guest that cannot mount the export is fetched from up the tty.
    #[test]
    fn a_fetch_falls_back_to_the_tty() {
        let tmp = tmp_dir("fetch-unstaged");
        let stage = tmp.path().join("stage");
        let guest = FakeGuest::start();
        let cfg = staged_cfg(&guest, &stage);
        guest.state().stage_unmountable = true;
        guest.state().files.insert("/wa/game.c".into(), b"int game;\n".to_vec());
        let mut s = guest.login();
        let tar = fetch_from_guest(&mut s, &cfg, "/wa", "game.c", soon(), est(), &mut |_| {}).unwrap();
        let got = place(&tar, "game.c", &tmp.path().join("got")).unwrap();
        assert_eq!(std::fs::read(got).unwrap(), b"int game;\n");
        let st = guest.state();
        assert!(st.commands.iter().any(|c| c.contains("| uuencode x)")), "{:?}", st.commands);
        assert!(!st.commands.iter().any(|c| c.contains("> /tmp/.xfer/")), "{:?}", st.commands);
    }

    /// A fetch brings deep paths back whole, by NFS and up the tty; one
    /// with a path too deep for the guest's `find` fails, rather than come
    /// back without it.
    #[test]
    fn a_fetch_brings_long_paths_back() {
        let tmp = tmp_dir("fetch-long");
        let stage = tmp.path().join("stage");
        let guest = FakeGuest::start();
        let cfg = staged_cfg(&guest, &stage);
        let deep = format!("proj/{}/deep.c", ["d234567890"; 40].join("/"));
        guest.state().files.insert("/wa/proj/a.c".into(), b"int a;\n".to_vec());
        guest.state().files.insert(format!("/wa/{deep}"), b"int deep;\n".to_vec());
        guest.state().dirs.insert("/wa/proj/empty".into());
        let mut s = guest.login();
        for tty in [false, true] {
            guest.state().stage_unmountable = tty;
            let tar = fetch_from_guest(&mut s, &cfg, "/wa", "proj", soon(), est(), &mut |_| {}).unwrap();
            let got = place(&tar, "proj", &tmp.path().join(format!("got{tty}"))).unwrap();
            assert_eq!(std::fs::read(got.join(&deep[5..])).unwrap(), b"int deep;\n", "tty {tty}");
            assert!(got.join("empty").is_dir(), "tty {tty}");
        }
        assert_eq!(std::fs::read_dir(&stage).unwrap().count(), 0, "the staged archive is removed");

        let deeper = format!("proj/{}/deeper.c", ["d234567890"; 72].join("/"));
        guest.state().files.insert(format!("/wa/{deeper}"), b"int deeper;\n".to_vec());
        for tty in [false, true] {
            guest.state().stage_unmountable = tty;
            let e = fetch_from_guest(&mut s, &cfg, "/wa", "proj", soon(), est(), &mut |_| {}).unwrap_err();
            assert!(e.to_string().contains("too long for the guest's find"), "tty {tty}: {e}");
        }
    }

    #[test]
    fn archiving_said_more_than_its_blocks() {
        assert!(archiving_said(b"60 blocks\n", "/wa", "p").is_ok());
        assert!(archiving_said(b"\r\n1 blocks\r\n", "/wa", "p").is_ok());
        let e = archiving_said(b"Pathname too long60 blocks\n", "/wa", "p").unwrap_err().to_string();
        assert!(e.contains("too long for the guest's find"), "{e}");
        let e = archiving_said(b"Cannot open p/x\n3 blocks\n", "/wa", "p").unwrap_err().to_string();
        assert!(e.ends_with("said: Cannot open p/x"), "{e}");
    }
}
