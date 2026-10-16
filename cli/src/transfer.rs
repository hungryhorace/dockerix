//! `dockerix agent get|put` — the container half of `dockerix get`/`put`:
//! one path, explicitly, between the guest and the host.
//!
//! The work-area push is one-way (sync.rs), so this is how a finished build
//! product gets to the host (`get`), and — rarely — how a host file gets into
//! the guest somewhere the push does not cover (`put`). Both ride the same
//! tar-over-uuencode transport as the push, on a session of their own (a
//! deliberate transfer is rare enough that the 3–4 s login does not matter,
//! and it keeps the serve-mode pool untouched). There is no size cap: this
//! is what the user asked for, however big; large fetches say so on stderr.
//!
//! In NFS mode (`DOCKERIX_NFS=1`) the guest's work area *is* the host's, so
//! both still work but are plain copies, and neither touches the manifest:
//! it describes the guest-disc tree the push maintains, which the NFS mount
//! hides and which the next push in copy mode must still be able to trust.
//!
//! Paths on the container side are already resolved by the host wrapper:
//! guest paths are absolute, work-area paths are relative to the bind mount.
//! A destination or source outside the work area is streamed through
//! stdout/stdin as a tar and unpacked or packed by the wrapper itself
//! (host.rs), with the same helpers.

use std::io::{self, Read, Write};
use std::path::Path;
use std::time::{Duration, Instant};

use crate::config::Config;
use crate::exec::shell_quote;
use crate::progress::{self, Reporter, Style};
use crate::session::Session;
use crate::sync;

fn split(path: &str) -> (String, String) {
    let path = path.trim_end_matches('/');
    match path.rfind('/') {
        Some(0) => ("/".to_string(), path[1..].to_string()),
        Some(i) => (path[..i].to_string(), path[i + 1..].to_string()),
        None => (".".to_string(), path.to_string()),
    }
}

/// `-`: absent, `d`: directory, `f`: something else that exists.
fn guest_kind(session: &mut Session, path: &str, deadline: Instant) -> io::Result<char> {
    let q = shell_quote(path);
    let (out, _) = session.run(
        &format!("if test -d {q}; then echo d; elif test -r {q}; then echo f; else echo -; fi"),
        deadline,
    )?;
    Ok(String::from_utf8_lossy(&out).trim().chars().last().unwrap_or('-'))
}

fn login(cfg: &Config) -> io::Result<Session> {
    let (cols, rows) = crate::tty::window_size();
    Session::connect_and_login(&cfg.telnet_addr, false, cols, rows, &cfg.user, &cfg.pass)
}

/// A guest path as given: absolute, or relative to the guest's copy of the
/// host cwd (`DOCKERIX_SUBDIR`, set by the wrapper).
fn resolve_guest(cfg: &Config, path: &str) -> String {
    if path.starts_with('/') {
        path.to_string()
    } else {
        format!("{}/{}", cfg.guest_cwd(), path.trim_start_matches("./"))
    }
}

/// Where a work-area-relative path sits in the container.
fn in_workarea(cfg: &Config, rel: &str) -> std::path::PathBuf {
    Path::new(&cfg.container_workarea).join(rel)
}

/// `dockerix agent get [-r] <guest-path> [<rel-dest>]`: fetch the guest
/// path into the work area at `rel_dest` (cp semantics — an existing
/// directory receives it by name), recording it in the manifest as mirrored;
/// or, without a destination, write the tar to stdout for the wrapper.
pub fn get(cfg: &Config, guest: &str, rel_dest: Option<&str>, recursive: bool) -> io::Result<()> {
    get_to(cfg, guest, rel_dest, recursive, &mut io::stdout())
}

/// `get`, writing the tar to `out` rather than stdout when there is no
/// destination.
fn get_to(
    cfg: &Config,
    guest: &str,
    rel_dest: Option<&str>,
    recursive: bool,
    out: &mut dyn Write,
) -> io::Result<()> {
    let deadline = Instant::now() + Duration::from_secs(3600);
    let guest = &resolve_guest(cfg, guest);
    let (dir, name) = split(guest);
    let mut session = login(cfg)?;
    let mut reporter = Reporter::new(Style::from_env());
    let result = (|| {
        // Size it for the progress report: `wc` for a file; for a directory
        // the sizes column of `ls -lR` summed (the guest's `du` counts
        // blocks in units of its own and undercounts by half).
        let size: u64 = match guest_kind(&mut session, guest, deadline)? {
            '-' => return Err(io::Error::other(format!("{guest}: no such file or directory in the guest"))),
            'd' if !recursive => return Err(io::Error::other(format!("{guest}: is a directory (use -r)"))),
            'd' => {
                let (out, _) = session.run(
                    &format!("ls -lR {} | awk '{{s+=$4}} END {{print s}}'", shell_quote(guest)),
                    deadline,
                )?;
                String::from_utf8_lossy(&out).trim().parse().unwrap_or(0)
            }
            _ => {
                let (out, _) = session.run(&format!("wc -c < {}", shell_quote(guest)), deadline)?;
                String::from_utf8_lossy(&out).trim().parse().unwrap_or(0)
            }
        };
        let label = format!("fetching {guest}, {}", progress::bytes(size));
        let est = sync::Estimate { encoded: sync::encoded_size(size + 1024), label: &label };
        sync::fetch_from_guest(&mut session, cfg, &dir, &name, deadline, est, &mut |p| reporter.show(p))
    })();
    session.logout();
    let tar = result?;
    match rel_dest {
        None => {
            out.write_all(&tar)?;
            out.flush()
        }
        Some(rel) => {
            let written = sync::place(&tar, &name, &in_workarea(cfg, rel))?;
            if let Ok(r) = written.strip_prefix(&cfg.container_workarea)
                && !cfg.nfs_enabled()
            {
                sync::record(cfg, &r.to_string_lossy(), false)?;
            }
            Ok(())
        }
    }
}

/// `dockerix agent put [-r] <guest-path> [<rel-src>]`: send the work-area
/// path `rel_src` (or a tar from stdin) to the guest at `guest` (cp
/// semantics). If source and destination are the same work-area path the
/// manifest records it as pushed, so the next push does not ship it again.
pub fn put(cfg: &Config, guest: &str, rel_src: Option<&str>, recursive: bool) -> io::Result<()> {
    let deadline = Instant::now() + Duration::from_secs(3600);
    let (tar, src_name) = match rel_src {
        Some(rel) => {
            let full = in_workarea(cfg, rel);
            let md = std::fs::symlink_metadata(&full)?;
            if md.is_dir() && !recursive {
                return Err(io::Error::other(format!("{}: is a directory (use -r)", full.display())));
            }
            let (parent, name) = split(&full.to_string_lossy());
            (sync::pack(Path::new(&parent), &name)?, name)
        }
        None => {
            let mut tar = Vec::new();
            io::stdin().read_to_end(&mut tar)?;
            let name = tar_top_name(&tar)?;
            (tar, name)
        }
    };
    let guest = &resolve_guest(cfg, guest);
    let mut session = login(cfg)?;
    let mut reporter = Reporter::new(Style::from_env());
    let label = format!("sending {src_name} to {guest}, {}", progress::bytes(tar.len() as u64));
    let result = (|| {
        // cp semantics on the guest: into an existing directory by name,
        // otherwise the path itself (the archive's member is renamed).
        let (guest_dir, final_name) = match guest_kind(&mut session, guest, deadline)? {
            'd' => (guest.trim_end_matches('/').to_string(), src_name.clone()),
            _ => split(guest),
        };
        if guest_kind(&mut session, &guest_dir, deadline)? != 'd' {
            return Err(io::Error::other(format!("{guest_dir}: no such directory in the guest")));
        }
        sync::extract_on_guest(&mut session, cfg, &guest_dir, &tar, deadline, &label, &mut |p| {
            reporter.show(p)
        })?;
        if final_name != src_name {
            let (out, code) = session.run(
                &format!(
                    "cd {} && mv -f {} {}",
                    shell_quote(&guest_dir),
                    shell_quote(&src_name),
                    shell_quote(&final_name)
                ),
                deadline,
            )?;
            if code != 0 {
                return Err(io::Error::other(format!(
                    "guest-side rename to {final_name} failed: {}",
                    String::from_utf8_lossy(&out).trim()
                )));
            }
        }
        Ok(format!("{guest_dir}/{final_name}"))
    })();
    session.logout();
    let landed = result?;
    if let Some(rel) = rel_src {
        let mirror = format!("{}/{}", cfg.guest_workarea, rel.trim_end_matches('/'));
        if landed == mirror && !cfg.nfs_enabled() {
            sync::record(cfg, rel.trim_end_matches('/'), true)?;
        }
    }
    Ok(())
}

/// The top-level name in a single-path archive (`pack`'s `./name`).
pub fn tar_top_name(tar: &[u8]) -> io::Result<String> {
    if tar.len() < 512 {
        return Err(io::Error::other("empty archive on stdin"));
    }
    let raw = &tar[..100];
    let end = raw.iter().position(|&b| b == 0).unwrap_or(100);
    let name =
        String::from_utf8_lossy(&raw[..end]).trim_end_matches('/').trim_start_matches("./").to_string();
    let top = name.split('/').next().unwrap_or("").to_string();
    if top.is_empty() {
        return Err(io::Error::other("archive on stdin has no member name"));
    }
    Ok(top)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake_guest::FakeGuest;
    use crate::testsupport::{tmp_dir, work_area};

    fn pair(dir: &str, name: &str) -> (String, String) {
        (dir.to_string(), name.to_string())
    }

    #[test]
    fn split_into_directory_and_name() {
        assert_eq!(split("/usr/src/hello.c"), pair("/usr/src", "hello.c"));
        assert_eq!(split("/vmunix"), pair("/", "vmunix"));
        assert_eq!(split("hello.c"), pair(".", "hello.c"));
        assert_eq!(split("games/rogue/"), pair("games", "rogue"));
    }

    /// A 512-byte tar header block with `name` in the name field.
    fn header(name: &[u8]) -> Vec<u8> {
        let mut block = vec![0u8; 512];
        block[..name.len()].copy_from_slice(name);
        block
    }

    #[test]
    fn tar_top_name_takes_the_first_path_component() {
        assert_eq!(tar_top_name(&header(b"./hello.c")).unwrap(), "hello.c");
        assert_eq!(tar_top_name(&header(b"./games/")).unwrap(), "games");
        assert_eq!(tar_top_name(&header(b"games/rogue/main.c")).unwrap(), "games");
        // A name filling all 100 bytes has no terminating NUL.
        assert_eq!(tar_top_name(&header(&[b'n'; 100])).unwrap(), "n".repeat(100));
    }

    #[test]
    fn tar_top_name_refuses_short_or_nameless_archives() {
        assert!(tar_top_name(b"").is_err());
        assert!(tar_top_name(&header(b"./hello.c")[..511]).is_err());
        assert!(tar_top_name(&header(b"")).is_err());
    }

    /// A `Config` whose transfers target a work area under a temp dir: the
    /// manifest and stage beside it, the fake guest's login settings.
    fn transfer_cfg(guest: &FakeGuest, tmp: &tempfile::TempDir, wa: &Path) -> Config {
        Config {
            container_workarea: wa.to_string_lossy().into_owned(),
            sync_manifest: tmp.path().join("state/manifest"),
            stage: tmp.path().join("stage").to_string_lossy().into_owned(),
            ..guest.config()
        }
    }

    /// A delta push of the work area on a session of its own: the warnings,
    /// and the commands it sent. One command (making the root and reading
    /// the epoch) means it shipped and deleted nothing.
    fn push(guest: &FakeGuest, cfg: &Config) -> (Vec<String>, Vec<String>) {
        let mut s = guest.login();
        let before = guest.state().commands.len();
        let opts = sync::PushOpts { max_bytes: 1 << 20, hash: false, clear: false };
        let deadline = Instant::now() + Duration::from_secs(5);
        let warnings = sync::push(&mut s, cfg, &opts, deadline, &mut |_| {}).unwrap();
        let sent = guest.state().commands[before..].to_vec();
        s.logout();
        (warnings, sent)
    }

    /// The manifest as saved.
    fn manifest(cfg: &Config) -> String {
        std::fs::read_to_string(&cfg.sync_manifest).unwrap()
    }

    /// The manifest's tag for `rel`: lower case for owned, upper for
    /// mirrored; `None` if it is not recorded.
    fn tag(cfg: &Config, rel: &str) -> Option<char> {
        manifest(cfg)
            .lines()
            .skip(1)
            .find(|l| l.splitn(5, ' ').nth(4) == Some(rel))
            .and_then(|l| l.chars().next())
    }

    fn guest_file(guest: &FakeGuest, path: &str) -> Option<String> {
        guest.state().files.get(path).map(|b| String::from_utf8_lossy(b).into_owned())
    }

    /// A pushed work area, a guest that has built two products in it, and the
    /// `Config` whose transfers target the work area.
    fn built(tag: &str) -> (tempfile::TempDir, std::path::PathBuf, FakeGuest, Config) {
        let (tmp, wa) = work_area(tag);
        let guest = FakeGuest::start();
        let cfg = transfer_cfg(&guest, &tmp, &wa);
        let (warnings, _) = push(&guest, &cfg);
        assert!(warnings.iter().any(|w| w.contains("full work-area push")), "{warnings:?}");
        let mut st = guest.state();
        st.files.insert("/workarea/hello.o".into(), b"object code\n".to_vec());
        st.files.insert("/workarea/a.out".into(), b"executable\n".to_vec());
        drop(st);
        (tmp, wa, guest, cfg)
    }

    /// What `get` brings into the work area is the guest's: the push leaves
    /// it alone, and deleting it on the host deletes nothing on the guest.
    /// Edited on the host, it is pushed and becomes the push's.
    #[test]
    fn get_into_the_work_area_mirrors_until_edited() {
        let (_tmp, wa, guest, cfg) = built("get-mirror");
        get(&cfg, "/workarea/hello.o", Some("hello.o"), false).unwrap();
        get(&cfg, "/workarea/a.out", Some("a.out"), false).unwrap();
        assert_eq!(std::fs::read_to_string(wa.join("hello.o")).unwrap(), "object code\n");
        assert_eq!(std::fs::read_to_string(wa.join("a.out")).unwrap(), "executable\n");
        assert_eq!(
            (tag(&cfg, "hello.o"), tag(&cfg, "a.out"), tag(&cfg, "hello.c")),
            (Some('F'), Some('F'), Some('f'))
        );

        let (warnings, sent) = push(&guest, &cfg);
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(sent.len(), 1, "{sent:?}");

        std::fs::remove_file(wa.join("hello.o")).unwrap();
        let (_, sent) = push(&guest, &cfg);
        assert_eq!(sent.len(), 1, "{sent:?}");
        assert_eq!(guest_file(&guest, "/workarea/hello.o").as_deref(), Some("object code\n"));
        assert_eq!(tag(&cfg, "hello.o"), None);

        // A new size: within the same second, size is all that changes.
        std::fs::write(wa.join("a.out"), "patched on the host\n").unwrap();
        let (_, sent) = push(&guest, &cfg);
        assert!(sent.iter().any(|c| c.ends_with("uudecode")), "{sent:?}");
        assert_eq!(guest_file(&guest, "/workarea/a.out").as_deref(), Some("patched on the host\n"));
        assert_eq!(tag(&cfg, "a.out"), Some('f'));
    }

    /// Without a destination the tar goes to the writer (stdout) for the
    /// wrapper to unpack; a directory needs `-r`.
    #[test]
    fn get_without_a_destination_writes_the_tar() {
        // A guest path given absolute, no work area involved: the fake
        // guest's default Config (a `/xfer` stage that does not exist) sends
        // it up the tty.
        let guest = FakeGuest::start();
        let cfg = guest.config();
        let mut st = guest.state();
        st.files.insert("/usr/src/game/main.c".into(), b"int main;\n".to_vec());
        st.files.insert("/usr/src/game/lib/util.c".into(), b"int util;\n".to_vec());
        drop(st);

        let mut tar = Vec::new();
        get_to(&cfg, "/usr/src/game", None, true, &mut tar).unwrap();
        let dest = tmp_dir("get-tar-dest");
        sync::place(&tar, "game", dest.path()).unwrap();
        let read = |p: &str| std::fs::read_to_string(dest.path().join(p)).unwrap();
        assert_eq!(
            (read("game/main.c"), read("game/lib/util.c")),
            ("int main;\n".into(), "int util;\n".into())
        );

        let mut tar = Vec::new();
        let e = get_to(&cfg, "/usr/src/game", None, false, &mut tar).unwrap_err();
        assert_eq!(e.to_string(), "/usr/src/game: is a directory (use -r)");
        assert!(tar.is_empty());
        assert!(guest.wait(|st| st.logouts == 2));
        let st = guest.state();
        assert_eq!((st.logins, st.logouts, st.dropped), (2, 2, 0));
    }

    /// `put` of a work-area path to its own place on the guest does the
    /// push's work, so the push need not do it again.
    #[test]
    fn put_to_the_same_path_is_recorded_as_pushed() {
        let (_tmp, wa, guest, cfg) = built("put-same");
        std::fs::write(wa.join("new.c"), "int x;\n").unwrap();
        put(&cfg, "/workarea/new.c", Some("new.c"), false).unwrap();
        assert_eq!(guest_file(&guest, "/workarea/new.c").as_deref(), Some("int x;\n"));
        assert_eq!(tag(&cfg, "new.c"), Some('f'));
        let (_, sent) = push(&guest, &cfg);
        assert_eq!(sent.len(), 1, "{sent:?}");
    }

    /// `put` elsewhere renames the member to the path given and leaves the
    /// manifest alone; a missing guest directory is named.
    #[test]
    fn put_elsewhere_renames_and_leaves_the_manifest() {
        let (_tmp, _wa, guest, cfg) = built("put-elsewhere");
        guest.state().dirs.insert("/usr/src".into());
        let before = manifest(&cfg);
        put(&cfg, "/usr/src/renamed.c", Some("hello.c"), false).unwrap();
        assert_eq!(guest_file(&guest, "/usr/src/renamed.c").as_deref(), Some("int main() { return 0; }\n"));
        assert_eq!(guest_file(&guest, "/usr/src/hello.c"), None);
        assert!(guest.state().commands.iter().any(|c| c == "cd /usr/src && mv -f hello.c renamed.c"));
        assert_eq!(manifest(&cfg), before);

        let e = put(&cfg, "/nowhere/x.c", Some("hello.c"), false).unwrap_err();
        assert_eq!(e.to_string(), "/nowhere: no such directory in the guest");
        assert_eq!(manifest(&cfg), before);
    }

    /// In NFS mode the guest's work area is the host's: neither transfer
    /// touches the manifest, which describes the guest disc's own tree.
    #[test]
    fn nfs_mode_transfers_leave_the_manifest_alone() {
        // The initial push in `built` is in copy mode (not NFS); the transfers
        // then run with NFS on, where neither touches the manifest.
        let (_tmp, wa, guest, cfg) = built("transfer-nfs");
        let nfs_cfg = Config { nfs_requested: true, ..cfg.clone() };
        let before = manifest(&cfg);
        get(&nfs_cfg, "/workarea/hello.o", Some("hello.o"), false).unwrap();
        assert_eq!(std::fs::read_to_string(wa.join("hello.o")).unwrap(), "object code\n");
        std::fs::write(wa.join("new.c"), "int x;\n").unwrap();
        put(&nfs_cfg, "/workarea/new.c", Some("new.c"), false).unwrap();
        assert_eq!(guest_file(&guest, "/workarea/new.c").as_deref(), Some("int x;\n"));
        assert_eq!(manifest(&cfg), before);
    }
}
