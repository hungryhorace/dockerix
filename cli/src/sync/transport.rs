//! Landing an archive in the guest — for the push and `put` — and the
//! packing and unpacking on this side. Down to the guest through the agent's
//! NFS staging export: the guest's `tar x` reads a tar staged there; when
//! staging is unavailable, the tty carries it instead, uuencoded to the
//! guest's `uudecode`. Fetching the other way, up from the guest for `get`,
//! is in `fetch`, which shares the staging helpers (`staging`, `mount_stage`,
//! `Unstage`, `GUEST_STAGE`) here. Used by the push and by `get`/`put`
//! (transfer.rs).
//!
//! Why stage: the telnet tty takes input at about 10 KB/s whatever reads it
//! (measured: `cat > /dev/null` 10 KB/s, plain TCP into the
//! guest 136 KB/s, NFS reads ~400 KB/s, the guest's own `uudecode` 235 KB/s),
//! so a tree full of sources took minutes down the tty; and up it, the
//! guest's `uuencode` and its tty output cost a get well over what an NFS
//! write does.
//!
//! Long paths: the guest's 1990 tar reads a POSIX ustar archive in full,
//! prefix field and all, so the push and `put` write ustar, which holds a
//! path of up to 254 bytes after the `./` we give it (`fits_ustar` has the
//! exact rule); the sweep and `pack` refuse a longer one by name. It unpacks
//! GNU tar's own long names and pax records as files instead (`@LongLink`,
//! `PaxHeader`), with the real name cut short. It writes only the old format,
//! though, whose names stop at 99 bytes, and leaves out a longer path with a
//! message and a 0 exit (measured). So `get` archives with the
//! guest's `cpio` instead (see `fetch`).

use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use crate::config::{self, Config};
use crate::exec::shell_quote;
use crate::progress::{self, Progress, Stage};
use crate::session::{Session, token};
use crate::uu;

/// The container directory transfers stage their archives in
/// (`DOCKERIX_STAGE`, else /xfer), which the agent exports to the guest
/// (agent.rs). Present only in the container, and there only once the agent
/// has started the NFS server. Short, as is `GUEST_STAGE`: the guest
/// commands that name them must fit the guest tty's line limit, or
/// `Session::run` sends them down the tty as a script.
pub fn stage_dir() -> String {
    config::stage_dir()
}

/// Where the guest mounts the staging export.
pub(super) const GUEST_STAGE: &str = "/tmp/.xfer";

/// Whether to try staging: the export is there (the agent made it) and it
/// is not turned off (`DOCKERIX_PUSH_TTY=1`).
pub(super) fn staging(cfg: &Config, stage: &str) -> bool {
    !cfg.push_tty && Path::new(stage).is_dir()
}

/// What the unpacking command says when the staged archive is not there.
const NOT_STAGED: &str = "__RIXNOSTAGE";

/// Land `tar` in the guest directory `guest_dir` (which must exist): staged
/// over NFS if the staging export is there and the guest can mount it, else
/// down the tty (`DOCKERIX_PUSH_TTY=1` forces that). Reports progress under
/// `label` if the transfer is long enough to be worth it.
pub fn extract_on_guest(
    session: &mut Session,
    cfg: &Config,
    guest_dir: &str,
    tar: &[u8],
    deadline: Instant,
    label: &str,
    progress: &mut dyn FnMut(&Progress),
) -> io::Result<()> {
    if staging(cfg, &cfg.stage) {
        match extract_staged(session, cfg, guest_dir, tar, deadline, label, progress)? {
            Staged::Done => return Ok(()),
            Staged::Unavailable(why) => {
                eprintln!("dockerix: NFS staging unavailable ({why}); pushing down the tty")
            }
        }
    }
    extract_via_tty(session, guest_dir, tar, deadline, label, progress)
}

/// What came of staging an archive.
#[derive(Debug, PartialEq)]
enum Staged {
    Done,
    /// The archive could not be staged or the guest cannot reach it: the
    /// tty can still carry it. (A guest `tar` that fails is an error.)
    Unavailable(String),
}

/// Removes a staged archive when dropped, however the push went.
pub(super) struct Unstage(pub(super) PathBuf);

impl Drop for Unstage {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// The guest command that mounts the staging export, once: later transfers
/// find it mounted. Read-write, for `get`'s archives. Soft, with short
/// retries, so a server that has gone away sends the transfer through the
/// tty after a few seconds (the default retries took about 35) rather than
/// hanging it.
fn stage_mount_command(server: &str, stage: &str) -> String {
    let g = GUEST_STAGE;
    let opts = "soft,timeo=4,retrans=2";
    format!(
        "mount | grep {g} >/dev/null || {{ mkdir {g} 2>/dev/null; mount -o {opts} {server}:{stage} {g}; }}"
    )
}

/// The guest command that unpacks the staged `name` in `dir`, first saying
/// `NOT_STAGED` if it is not there (the `tar` then fails too, but the word
/// tells that apart from a `tar` that fails on an archive it has).
fn stage_unpack_command(dir: &str, name: &str) -> String {
    let path = format!("{GUEST_STAGE}/{name}");
    format!("test -f {path} || echo {NOT_STAGED}; cd {dir} && tar xf {path}")
}

/// Have the guest mount the staging export if it has not: Ok(Err(why)) if
/// it cannot.
pub(super) fn mount_stage(
    session: &mut Session,
    cfg: &Config,
    deadline: Instant,
) -> io::Result<Result<(), String>> {
    let (server, stage) = (&cfg.nfs_server, &cfg.stage);
    let (out, code) = session.run(&stage_mount_command(server, stage), deadline)?;
    if code != 0 {
        let why = String::from_utf8_lossy(&out).trim().to_string();
        return Ok(Err(format!("the guest cannot mount {server}:{stage}: {why}")));
    }
    Ok(Ok(()))
}

/// `extract_on_guest` by NFS: write the archive into the stage (`cfg.stage`),
/// have the guest mount the export if it has not, and `tar x` it from there.
fn extract_staged(
    session: &mut Session,
    cfg: &Config,
    guest_dir: &str,
    tar: &[u8],
    deadline: Instant,
    label: &str,
    progress: &mut dyn FnMut(&Progress),
) -> io::Result<Staged> {
    let name = format!("p{}.tar", token());
    let path = Path::new(&cfg.stage).join(&name);
    if let Err(e) = std::fs::write(&path, tar) {
        return Ok(Staged::Unavailable(format!("writing {}: {e}", path.display())));
    }
    let _unstage = Unstage(path);
    let total = tar.len() as u64;
    let report = progress::worth_showing(total, progress::STAGED_RATE);
    let mut event = |stage: Stage| {
        if report {
            progress(&Progress { label: label.to_string(), done: total, total, stage });
        }
    };
    // All the bytes are across: the guest reads them as it unpacks.
    event(Stage::Unpacking);
    if let Err(why) = mount_stage(session, cfg, deadline)? {
        return Ok(Staged::Unavailable(why));
    }
    let (out, code) = session.run(&stage_unpack_command(&shell_quote(guest_dir), &name), deadline)?;
    if String::from_utf8_lossy(&out).contains(NOT_STAGED) {
        return Ok(Staged::Unavailable(format!("the guest does not see {GUEST_STAGE}/{name}")));
    }
    if code != 0 {
        let text = String::from_utf8_lossy(&out);
        let lines: Vec<&str> = text.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
        return Err(io::Error::other(format!(
            "guest-side tar extract failed (staged over NFS, {total} bytes): {}",
            lines.join(" | ")
        )));
    }
    event(Stage::Done);
    Ok(Staged::Done)
}

/// `extract_on_guest` down the tty: uuencode the archive, `uudecode` and
/// `tar x` it on the guest.
fn extract_via_tty(
    session: &mut Session,
    guest_dir: &str,
    tar: &[u8],
    deadline: Instant,
    label: &str,
    progress: &mut dyn FnMut(&Progress),
) -> io::Result<()> {
    let encoded = uu::encode("wa.tar", tar);
    let dir = shell_quote(guest_dir);
    let total = encoded.len() as u64;
    let report = progress::worth_showing(total, progress::INBOUND_RATE);
    let mut event = |done: u64, stage: Stage| {
        if report {
            progress(&Progress { label: label.to_string(), done, total, stage });
        }
    };
    event(0, Stage::Sending);
    session.feed_with(&format!("cd {dir} && uudecode"), &encoded, deadline, &mut |sent, _| {
        event(sent, Stage::Sending)
    })?;
    event(total, Stage::Unpacking);
    // Report the size that arrived before extracting, so a failure can be
    // told apart as truncation/corruption in transit (the tty path has no
    // flow control of its own) rather than a bad archive.
    let (out, code) =
        session.run(&format!("cd {dir} && wc -c < wa.tar && tar xf wa.tar && rm -f wa.tar"), deadline)?;
    if code != 0 {
        return Err(io::Error::other(extract_failure(&out, tar.len())));
    }
    event(total, Stage::Done);
    Ok(())
}

/// The size of `bytes` of tar once uuencoded (4/3, plus a byte and a
/// newline per 45-byte line), to size a fetch's progress against.
pub fn encoded_size(bytes: u64) -> u64 {
    bytes.div_ceil(45) * 62 + 32
}

/// Archive one local path (`parent/name`; a directory comes with its
/// contents) with `-b 1`, as `tar_paths` does. Runs on the host too (the
/// wrapper's streamed `put`): `COPYFILE_DISABLE` stops macOS bsdtar adding
/// `._name` AppleDouble members, and plain ustar format keeps it from
/// emitting pax extended headers, which the guest's 1990 tar would unpack
/// as a `PaxHeader` directory. A path ustar cannot hold is refused here,
/// by name: bsdtar only warns of one, leaves it out and exits 0.
pub fn pack(parent: &Path, name: &str) -> io::Result<Vec<u8>> {
    let mut long = Vec::new();
    unarchivable(parent, name, &mut long);
    if !long.is_empty() {
        return Err(io::Error::other(format!(
            "{}: too long for the guest's tar: {}",
            parent.join(name).display(),
            super::some_of(&long)
        )));
    }
    let out = Command::new("tar")
        .env("COPYFILE_DISABLE", "1")
        .args(["cf", "-", "-b", "1", "--format", "ustar", "-C"])
        .arg(parent)
        .arg(format!("./{name}"))
        .output()?;
    if !out.status.success() {
        return Err(io::Error::other(format!(
            "tar of {} failed: {}",
            parent.join(name).display(),
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(out.stdout)
}

/// Unpack a single-path archive (as `fetch_from_guest`/`pack` make: one
/// top-level name, `name`) at `dest`, with `cp` semantics: an existing
/// directory at `dest` receives `name` inside it; otherwise `dest` is the
/// new path itself, replacing a file already there. Extracts into a scratch
/// directory beside the destination and renames, so a failed transfer leaves
/// no half-written target. Returns the path written.
pub fn place(tar: &[u8], name: &str, dest: &Path) -> io::Result<PathBuf> {
    let target = if dest.is_dir() { dest.join(name) } else { dest.to_path_buf() };
    let parent = match target.parent() {
        Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
        _ => PathBuf::from("."),
    };
    if !parent.is_dir() {
        return Err(io::Error::other(format!("{}: no such directory", parent.display())));
    }
    let scratch = parent.join(format!(".dockerix-get-{}", std::process::id()));
    std::fs::create_dir(&scratch)?;
    let result = (|| {
        untar_into(tar, &scratch.to_string_lossy())?;
        let unpacked = scratch.join(name);
        if !unpacked.exists() {
            return Err(io::Error::other(format!("archive did not contain {name}")));
        }
        if target.is_dir() {
            // `cp -r` onto an existing directory would merge; we do not.
            return Err(io::Error::other(format!("{}: is a directory", target.display())));
        }
        std::fs::rename(&unpacked, &target)?;
        Ok(target.clone())
    })();
    let _ = std::fs::remove_dir_all(&scratch);
    result
}

/// The contents of the one regular file in a single-member archive (as
/// `fetch_from_guest`/`pack` make for a file), for `dockerix get … -` to
/// write to stdout. The guest's `get` of a plain file sends exactly one
/// file member; a directory comes as `-r` and its tar goes out whole, not
/// through here. Errors if the archive holds no regular file.
pub fn one_file(tar: &[u8]) -> io::Result<Vec<u8>> {
    let mut i = 0;
    while i + 512 <= tar.len() {
        let header = &tar[i..i + 512];
        if header.iter().all(|&b| b == 0) {
            break;
        }
        let size = header_octal(&header[124..136]);
        let data = i + 512;
        // '0' and the old NUL both mean a regular file; anything else
        // (a directory, '5') is skipped.
        if matches!(header[156], b'0' | 0) {
            return Ok(tar[data..(data + size).min(tar.len())].to_vec());
        }
        i = data + size.div_ceil(512) * 512;
    }
    Err(io::Error::other("the guest sent no file (a directory needs -r)"))
}

/// A one-file ustar archive: a single regular-file member `name` holding
/// `data`, mode 0644, owner root, mtime now. For `dockerix put -`, which
/// takes a file's contents from stdin; the guest's `tar x` unpacks it. The
/// name must fit a ustar name field (the guest path's last component, so
/// short in practice).
pub fn archive_bytes(name: &str, data: &[u8]) -> io::Result<Vec<u8>> {
    if name.is_empty() || name.len() > 100 {
        return Err(io::Error::other(format!("{name:?}: not a usable file name for the guest")));
    }
    let mut header = [0u8; 512];
    header[..name.len()].copy_from_slice(name.as_bytes());
    put_octal(&mut header[100..108], 0o644); // mode
    put_octal(&mut header[108..116], 0); // uid
    put_octal(&mut header[116..124], 0); // gid
    put_octal(&mut header[124..136], data.len() as u64); // size
    let mtime = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
    put_octal(&mut header[136..148], mtime);
    header[156] = b'0'; // typeflag: regular file
    header[257..263].copy_from_slice(b"ustar\0");
    header[263..265].copy_from_slice(b"00");
    // The checksum is of the header with its own field read as spaces.
    header[148..156].copy_from_slice(b"        ");
    let sum: u32 = header.iter().map(|&b| b as u32).sum();
    // Six octal digits, a NUL, then a space, as tar writes it.
    header[148..156].copy_from_slice(format!("{sum:06o}\0 ").as_bytes());

    let mut out = Vec::with_capacity(512 + data.len().next_multiple_of(512) + 1024);
    out.extend_from_slice(&header);
    out.extend_from_slice(data);
    out.resize(512 + data.len().next_multiple_of(512), 0); // pad the data to a block
    out.resize(out.len() + 1024, 0); // two zero blocks end the archive
    Ok(out)
}

/// An octal header field: `value` in octal, zero-padded to one less than the
/// field width, then a NUL. Values beyond the field (a file over 8 GB in the
/// 12-byte size field) keep their low digits; nothing we write reaches that.
fn put_octal(field: &mut [u8], value: u64) {
    let digits = field.len() - 1;
    let s = format!("{value:0digits$o}");
    let s = &s[s.len().saturating_sub(digits)..];
    field[..s.len()].copy_from_slice(s.as_bytes());
    field[digits] = 0;
}

/// Read an octal header field (NUL- or space-padded), 0 if it does not parse.
fn header_octal(field: &[u8]) -> usize {
    let text = String::from_utf8_lossy(field);
    usize::from_str_radix(text.trim_matches(|c| c == '\0' || c == ' '), 8).unwrap_or(0)
}

fn extract_failure(out: &[u8], sent: usize) -> String {
    let text = String::from_utf8_lossy(out);
    let mut lines = text.lines().map(str::trim).filter(|l| !l.is_empty());
    let arrived = lines.next().and_then(|l| l.parse::<usize>().ok());
    let rest: Vec<&str> = lines.collect();
    let mut msg = String::from("guest-side tar extract failed");
    match arrived {
        Some(n) if n != sent => msg.push_str(&format!(" ({n} of {sent} bytes arrived)")),
        Some(_) => msg.push_str(&format!(" ({sent} bytes arrived intact)")),
        None => msg.push_str(" (no wa.tar was written)"),
    }
    if !rest.is_empty() {
        msg.push_str(": ");
        msg.push_str(&rest.join(" | "));
    }
    msg
}

/// Archive just `paths` (relative to `dir`), directories without their
/// contents; parents sort before children so the guest's `tar x` creates
/// them in order (it makes missing leading directories itself anyway).
/// `-b 1`: the default 10 KB blocking pads a one-file delta to 10 KB, and at
/// ~11 KB/s inbound that padding alone costs a second; the guest's tar reads
/// any blocking (it prints an informational `blocksize = N`). ustar, as
/// `pack`: the sweep has left out any path it cannot hold (`archivable`).
pub(super) fn tar_paths(dir: &str, paths: &[&str]) -> io::Result<Vec<u8>> {
    let mut child = Command::new("tar")
        .args(["cf", "-", "-b", "1", "--format", "ustar", "-C", dir, "--no-recursion", "--null", "-T", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let mut list = Vec::new();
    for p in paths {
        // `./` prefix: names starting with `-` are otherwise read as options.
        list.extend_from_slice(b"./");
        list.extend_from_slice(p.as_bytes());
        list.push(0);
    }
    child.stdin.take().expect("piped stdin").write_all(&list)?;
    let out = child.wait_with_output()?;
    if !out.status.success() {
        return Err(io::Error::other(format!(
            "tar of work area failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(out.stdout)
}

/// Whether `tar_paths` and `pack` can archive the path `rel` (a directory
/// if `is_dir`) so that the guest's tar unpacks it whole: as they name it,
/// `./rel`, with the trailing `/` a tar gives a directory.
pub(super) fn archivable(rel: &str, is_dir: bool) -> bool {
    fits_ustar(&format!("./{rel}{}", if is_dir { "/" } else { "" }))
}

/// Whether a ustar header holds the member name `name`: in its 100-byte
/// name field, or split at a `/` into its 155-byte prefix field and the
/// name field. GNU tar and bsdtar split by this rule (and refuse a name it
/// does not fit, GNU tar with an error, bsdtar with only a warning).
fn fits_ustar(name: &str) -> bool {
    let n = name.len();
    n <= 100
        || name
            .bytes()
            .enumerate()
            .any(|(i, b)| b == b'/' && (1..=155).contains(&i) && i + 2 <= n && n - i - 1 <= 100)
}

/// Add to `long` the paths under `parent` from `rel` down (`rel` itself
/// included) that `archivable` refuses. A directory refused is named alone,
/// without what it holds. One that cannot be read is left to the tar to
/// report.
fn unarchivable(parent: &Path, rel: &str, long: &mut Vec<String>) {
    let Ok(md) = std::fs::symlink_metadata(parent.join(rel)) else { return };
    if !archivable(rel, md.is_dir()) {
        long.push(rel.to_string());
    } else if md.is_dir()
        && let Ok(entries) = std::fs::read_dir(parent.join(rel))
    {
        for ent in entries.flatten() {
            unarchivable(parent, &format!("{rel}/{}", ent.file_name().to_string_lossy()), long);
        }
    }
}

fn untar_into(tar: &[u8], dir: &str) -> io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let mut child = Command::new("tar").args(["xf", "-", "-C", dir]).stdin(Stdio::piped()).spawn()?;
    child.stdin.take().expect("piped stdin").write_all(tar)?;
    let status = child.wait()?;
    if !status.success() {
        return Err(io::Error::other("host-side tar extract failed"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::fetch::stage_archive_command;
    use super::*;
    use crate::testsupport::tmp_dir;

    /// With the real paths, staging's guest commands fit the tty's line
    /// limit once `Session::run` has framed them (about 52 bytes of the
    /// 200), so they are typed, not sent down the tty as a script. The stage
    /// directory is the default (`Config::default().stage`, `/xfer`).
    #[test]
    fn staging_commands_fit_a_typed_line() {
        let name = format!("p{}.tar", "0".repeat(12));
        let stage = Config::default().stage;
        let lines = [
            stage_mount_command("10.0.2.2", &stage),
            stage_unpack_command("/workarea", &name),
            stage_unpack_command("/workarea/some/project/dir", &name),
            stage_archive_command("/workarea/some/project/dir", "demo-prof", &name),
        ];
        for line in lines {
            assert!(line.len() <= 140, "{} bytes: {line}", line.len());
        }
    }

    /// The member names in `tar`, as `rel` paths: without `./` or a
    /// directory's trailing `/`.
    fn listed(tar: &[u8]) -> Vec<String> {
        let mut child = Command::new("tar")
            .args(["tf", "-"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(tar).unwrap();
        let out = child.wait_with_output().unwrap();
        let text = String::from_utf8(out.stdout).unwrap();
        text.lines().map(|l| l.trim_start_matches("./").trim_end_matches('/').to_string()).collect()
    }

    /// `archivable` agrees with the tar here (bsdtar on a Mac, GNU tar in
    /// the container and on CI) at the edges of what ustar holds.
    #[test]
    fn archivable_agrees_with_tar() {
        let tmp = tmp_dir("ustar-edges");
        let dir = tmp.path();
        // 153 bytes: with the `./`, as much as the prefix field holds.
        let dirs = ["d234567890"; 14].join("/");
        let cases = [
            ("f".repeat(98), false, true),
            ("f".repeat(100), false, true),
            ("g".repeat(101), false, false),
            (format!("{dirs}/{}", "a".repeat(100)), false, true),
            (format!("{dirs}/{}", "b".repeat(101)), false, false),
            (format!("{dirs}1/{}", "a".repeat(100)), false, false),
            (format!("e/{}", "h".repeat(99)), true, true),
            (format!("i/{}", "h".repeat(100)), true, false),
        ];
        for (rel, is_dir, fits) in &cases {
            let path = dir.join(rel);
            if *is_dir {
                std::fs::create_dir_all(path).unwrap();
            } else {
                std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                std::fs::write(path, "x").unwrap();
            }
            assert_eq!(archivable(rel, *is_dir), *fits, "{} bytes: {rel}", rel.len());
            // GNU tar fails on a name it cannot hold; bsdtar leaves it out.
            let held = tar_paths(dir.to_str().unwrap(), &[rel]).is_ok_and(|t| listed(&t).contains(rel));
            assert_eq!(held, *fits, "the tar here, {} bytes: {rel}", rel.len());
        }
    }

    /// `put` refuses a tree holding a path ustar cannot, by name, rather
    /// than send it without (as bsdtar would).
    #[test]
    fn pack_refuses_a_path_too_long() {
        let tmp = tmp_dir("pack-long");
        let dir = tmp.path();
        let long = "g".repeat(101);
        std::fs::create_dir_all(dir.join("proj")).unwrap();
        std::fs::write(dir.join("proj/a.c"), "int a;").unwrap();
        std::fs::write(dir.join("proj").join(&long), "x").unwrap();
        let e = pack(dir, "proj").unwrap_err().to_string();
        assert!(e.contains("too long") && e.contains(&format!("proj/{long}")), "{e}");
        std::fs::remove_file(dir.join("proj").join(&long)).unwrap();
        assert!(pack(dir, "proj").is_ok());
    }

    #[test]
    fn pack_and_place_round_trip() {
        let tmp = tmp_dir("place");
        let dir = tmp.path();
        std::fs::create_dir_all(dir.join("src/proj/sub")).unwrap();
        std::fs::create_dir_all(dir.join("out")).unwrap();
        std::fs::write(dir.join("src/proj/a.txt"), "aaa").unwrap();
        std::fs::write(dir.join("src/proj/sub/b.txt"), "bb").unwrap();
        let tar = pack(&dir.join("src"), "proj").unwrap();
        assert_eq!(crate::transfer::tar_top_name(&tar).unwrap(), "proj");
        // Into an existing directory: by name.
        let p = place(&tar, "proj", &dir.join("out")).unwrap();
        assert_eq!(p, dir.join("out/proj"));
        assert_eq!(std::fs::read_to_string(p.join("sub/b.txt")).unwrap(), "bb");
        // As a new path: renamed.
        let p = place(&tar, "proj", &dir.join("out/renamed")).unwrap();
        assert_eq!(std::fs::read_to_string(p.join("a.txt")).unwrap(), "aaa");
        // Into a directory that already holds that name: refused, nothing
        // left behind (an existing directory as `dest` receives it inside,
        // cp-style, so out/proj as dest would make out/proj/proj).
        assert!(place(&tar, "proj", &dir.join("out")).is_err());
        assert!(!dir.join("out").join(format!(".dockerix-get-{}", std::process::id())).exists());
        // A file replaces an existing file.
        let ftar = pack(&dir.join("src/proj"), "a.txt").unwrap();
        std::fs::write(dir.join("out/old.txt"), "old").unwrap();
        place(&ftar, "a.txt", &dir.join("out/old.txt")).unwrap();
        assert_eq!(std::fs::read_to_string(dir.join("out/old.txt")).unwrap(), "aaa");
    }

    #[test]
    fn one_file_takes_the_file_out_of_a_packed_archive() {
        let tmp = tmp_dir("one-file");
        let dir = tmp.path();
        // Contents that cross a 512-byte block and end mid-block, so the
        // size field, not block counting, bounds what comes back.
        let body = "x".repeat(600);
        std::fs::write(dir.join("f"), &body).unwrap();
        let tar = pack(dir, "f").unwrap();
        assert_eq!(one_file(&tar).unwrap(), body.as_bytes());

        // An empty file is its own (empty) member.
        std::fs::write(dir.join("empty"), "").unwrap();
        assert!(one_file(&pack(dir, "empty").unwrap()).unwrap().is_empty());

        // A directory archive (no regular-file member) is refused.
        std::fs::create_dir(dir.join("d")).unwrap();
        let e = one_file(&pack(dir, "d").unwrap()).unwrap_err();
        assert!(e.to_string().contains("a directory needs -r"), "{e}");
    }

    #[test]
    fn archive_bytes_is_a_ustar_file_the_tools_read() {
        // Built from stdin bytes for `put -`: `tar tvf -` must see one
        // regular file of the given name, mode and size, and `one_file`
        // must give the bytes back.
        let data = b"hello from stdin\n".repeat(40); // > 512 bytes
        let tar = archive_bytes("payload", &data).unwrap();
        assert_eq!(one_file(&tar).unwrap(), data);
        let listing = verbose_listing(&tar);
        assert!(listing.contains(" payload"), "{listing}");
        assert!(listing.starts_with("-rw-r--r--"), "{listing}");
        assert!(listing.contains(&data.len().to_string()), "{listing}");
        // A name too long for a ustar name field is refused, not truncated.
        assert!(archive_bytes(&"n".repeat(101), b"x").is_err());
        assert!(archive_bytes("", b"x").is_err());
    }

    /// `tar tvf -` of `tar`, its first entry's line (permissions, owner,
    /// size, name).
    fn verbose_listing(tar: &[u8]) -> String {
        let mut child = Command::new("tar")
            .args(["tvf", "-"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(tar).unwrap();
        let out = child.wait_with_output().unwrap();
        assert!(out.status.success(), "tar rejected the archive");
        String::from_utf8_lossy(&out.stdout).lines().next().unwrap_or("").to_string()
    }
}
