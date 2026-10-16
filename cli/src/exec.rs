//! `dockerix agent exec` — run a command in the guest, or open a shell.

use std::io::{self, IsTerminal, Write};
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

use crate::config::{self, Config};
use crate::serve;
use crate::session::Session;
use crate::tty;

pub struct ExecOpts {
    /// Force tty allocation; if unset, inferred from the terminal.
    pub tty: Option<bool>,
    /// Command and args; empty means an interactive login shell.
    pub command: Vec<String>,
    /// Seconds before a non-interactive command is abandoned.
    pub timeout_secs: u64,
}

/// The bind-mount point in the container (fixed by `dockerix start`, but
/// overridable for tests).
pub fn container_workarea() -> String {
    config::container_workarea()
}

/// The marker `__rixmount` leaves in the guest disc's own work-area
/// directory before mounting the share over it: seen only while the share
/// is not mounted.
const UNMOUNTED: &str = ".dockerix-unmounted";

/// Where a fresh session writes the mount functions to source them (a
/// token follows, so sessions logging in together never share the file).
const MOUNT_FUNCTIONS: &str = "/tmp/.rixmnt";

/// The guest shell functions that mount the work area (see
/// `ensure_nfs_mount`). `__rixmount` mounts it unless the mount table has
/// it, first creating the mount point and the `UNMOUNTED` marker in it.
/// `__rixmnt`, run before every command, mounts it again if the marker
/// shows: one `test`, no process, while the share is mounted. Their status
/// is `mount`'s, so a failed remount fails the command. `grep` without -q:
/// the 1990 grep has no such flag.
fn mount_functions(guest: &str, server: &str, opts: &str) -> String {
    format!(
        "__rixmount() {{\n\
         \tif mount | grep ' on {guest} ' >/dev/null; then :; else\n\
         \t\ttest -d {guest} || mkdir {guest}\n\
         \t\ttouch {guest}/{UNMOUNTED}\n\
         \t\tmount -o {opts} {server}:/workarea {guest}\n\
         \tfi\n\
         }}\n\
         __rixmnt() {{\n\
         \tif test -f {guest}/{UNMOUNTED}; then __rixmount; else :; fi\n\
         }}\n"
    )
}

/// In NFS mode, make sure the guest has the work area mounted from the
/// in-container NFSv2 server, and give the session the functions that keep
/// it so (`mount_functions`): `__rixmnt` guards every command after
/// (`Session::guard`), so a held session whose mount has gone (a `umount`,
/// say) mounts it again rather than run in the guest disc's own directory,
/// and fails the command if it cannot. Called on
/// every fresh login; the mount itself happens only when the share is
/// absent (after a guest reboot, say). A no-op when NFS mode is off.
///
/// The functions are written to a file and sourced, so that they stay in
/// this shell. They are too long to type on one line, and `run` would
/// stage them into a script, which defines them in its own shell.
///
/// 8K read and write sizes: a large guest→host write fragments into several
/// SLiRP packets, which the bundled SLiRP reassembles — the path that carried
/// the ip_reass bug patches/arculator/0003 fixes. Without that patch, keep writes to
/// wsize=1024 (one packet) via DOCKERIX_NFS_OPTS.
///
/// `actimeo=1`: RISC iX keeps a file's attributes for up to 60 s, the more
/// the longer the file has gone unchanged, so a `make` (which stats, and
/// does not open) missed an edit saved on the host for 56–57 s; with it,
/// for 1 s at most (docs/how-it-works.md, "The live NFS mount"). A no-op
/// `make` of a mid-sized C tree costs 2.1 s with it against 1.7 s.
pub fn ensure_nfs_mount(cfg: &Config, session: &mut Session, deadline: Instant) -> io::Result<()> {
    if !cfg.nfs_enabled() {
        return Ok(());
    }
    let guest = &cfg.guest_workarea;
    let server = &cfg.nfs_server;
    let opts = &cfg.nfs_opts;
    let file = format!("{MOUNT_FUNCTIONS}{}", crate::session::token());
    let functions = mount_functions(guest, server, opts);
    session.feed("uudecode", &crate::uu::encode(&file, functions.as_bytes()), deadline)?;
    // `test -f` first: a `.` that cannot open its file makes the 1990 shell
    // abandon the line, and the command would hang to its deadline.
    let cmd = format!("test -f {file} && . {file} && rm -f {file} && __rixmount");
    let (out, code) = session.run(&cmd, deadline)?;
    if code != 0 {
        return Err(io::Error::other(format!(
            "guest NFS mount of {server}:/workarea at {guest} failed: {}",
            String::from_utf8_lossy(&out).trim()
        )));
    }
    session.guard("__rixmnt");
    Ok(())
}

/// `command`, run in the guest cwd (`cfg.guest_cwd`). Guarded with `test -d`
/// rather than a bare `cd … &&`: when `cd` fails in the guest's interactive
/// 1990 Bourne shell it abandons the rest of the line (the exit sentinel
/// included), and the command hangs until its timeout. The directory is
/// missing on a fresh disc until a pushed command creates it, or when the host
/// cwd has no guest counterpart.
pub(crate) fn in_guest_cwd(cfg: &Config, command: &str) -> String {
    let dir = cfg.guest_cwd();
    // Kept short: the whole line has to fit the guest tty's input limit
    // (MAX_TTY_LINE) or the command is staged into a script instead.
    format!(
        "if test -d {dir}; then cd {dir} && {command}; else echo 'rix: no such dir in guest: \
         {dir}' 1>&2; false; fi"
    )
}

/// Quote a list of arguments into a single /bin/sh command line.
fn join_command(args: &[String]) -> String {
    args.iter().map(|a| shell_quote(a)).collect::<Vec<_>>().join(" ")
}

pub(crate) fn shell_quote(s: &str) -> String {
    if !s.is_empty() && s.bytes().all(|b| b.is_ascii_alphanumeric() || b"@%_+=:,./-".contains(&b)) {
        return s.to_string();
    }
    // Single-quote, escaping embedded single quotes.
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for c in s.chars() {
        if c == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(c);
        }
    }
    out.push('\'');
    out
}

pub fn run(cfg: &Config, opts: ExecOpts) -> io::Result<i32> {
    let want_tty = opts.tty.unwrap_or_else(|| io::stdin().is_terminal() && io::stdout().is_terminal());
    let (cols, rows) = tty::window_size();

    // Interactive login shell: no command given.
    if opts.command.is_empty() {
        let dir = cfg.login_dir();
        let _listed = serve::announce(&serve::socket_path(), serve::Phase::Login, &dir, "");
        let mut session =
            Session::connect_and_login(&cfg.telnet_addr, want_tty, cols, rows, &cfg.user, &cfg.pass)?;
        or_logout(&mut session, |session| {
            ensure_nfs_mount(cfg, session, Instant::now() + Duration::from_secs(30))?;
            let prompt = if cfg.user == "root" { "# " } else { "$ " };
            // Start in the fixed working directory, else the work area: that
            // is where the files are, and `cd /workarea` on every login got
            // old.
            session.restore_interactive(cols, rows, prompt, &dir)
        })?;
        // The motd/"Last login" banner read during login is deliberately not
        // replayed: this is the usual way to get a shell, and the RISC iX
        // prologue (kernel id, X/Open notice) is just noise every time.
        let code = tty::interactive(&mut session, None)?;
        return Ok(code);
    }

    if cfg.sync_clear && !cfg.sync_enabled() {
        // Above all in NFS mode, where the guest work area is the host's own
        // directory: clearing it would delete the user's files.
        return Err(io::Error::other(
            "nothing to clear: the work area is not pushed (NFS mode, or no work area)",
        ));
    }

    let command = join_command(&opts.command);

    if want_tty {
        // Same delta push as non-tty mode, on our own session: the agent's
        // pool can't carry a tty. The manifest's file lock keeps this push
        // and the agent's from interleaving. The push happens before
        // `launch` turns echo back on, while the shell is still in the clean
        // post-login state the push's framing relies on.
        let _listed = serve::announce(&serve::socket_path(), serve::Phase::Tty, &cfg.guest_cwd(), &command);
        let mut session =
            Session::connect_and_login(&cfg.telnet_addr, true, cols, rows, &cfg.user, &cfg.pass)?;
        let deadline = Instant::now() + Duration::from_secs(opts.timeout_secs);
        // The same `Request` the non-tty path builds, for the one push block;
        // on a tty we run it ourselves rather than send it to the agent.
        let request = serve::Request::from_cfg(cfg, &command);
        let token = or_logout(&mut session, |session| {
            ensure_nfs_mount(cfg, session, deadline)?;
            if request.sync {
                let mut reporter = crate::progress::Reporter::new(crate::progress::Style::from_env());
                let mut output = Output {
                    sink: &mut |_: &[u8]| {},
                    warn: &mut |m: &str| eprintln!("{m}"),
                    progress: &mut |p| reporter.show(p),
                    started: &mut || {},
                };
                push_workarea(session, cfg, &request.push_opts(), deadline, &mut output)?;
            }
            session.launch(&request.command, cols, rows)
        })?;
        let code = tty::interactive(&mut session, Some(&token))?;
        return Ok(code);
    }

    // Non-tty: the persistent agent holds logged-in sessions (serve mode), so
    // ask it to run the command and stream the output back; that skips the
    // 3–4s login. Without a server (one-shot mode, or DOCKERIX_DIRECT=1) do
    // the same thing over a session of our own.
    let request = serve::Request::from_cfg(cfg, &command);
    if !cfg.direct {
        match serve::client_run(&serve::socket_path(), &request) {
            Ok(code) => return Ok(code),
            Err(e) if e.kind() == io::ErrorKind::NotFound || e.kind() == io::ErrorKind::ConnectionRefused => {
            }
            Err(e) => return Err(e),
        }
    }

    // A direct command (DOCKERIX_DIRECT=1) still shows in `dockerix ps`
    // when the agent is up.
    let _listed = serve::announce(&serve::socket_path(), serve::Phase::Running, &cfg.guest_cwd(), &command);
    let mut session = Session::connect_and_login(&cfg.telnet_addr, false, cols, rows, &cfg.user, &cfg.pass)?;
    let deadline = Instant::now() + Duration::from_secs(opts.timeout_secs);
    or_logout(&mut session, |session| ensure_nfs_mount(cfg, session, deadline))?;
    // Run the whole exchange, then log out unconditionally — even on error — so
    // the guest pty is always freed cleanly, not just on the happy path.
    let stdout = io::stdout();
    let mut out = stdout.lock();
    let mut sink = |chunk: &[u8]| {
        let _ = out.write_all(chunk);
        let _ = out.flush();
    };
    let mut reporter = crate::progress::Reporter::new(crate::progress::Style::from_env());
    let mut output = Output {
        sink: &mut sink,
        warn: &mut |m| eprintln!("{m}"),
        progress: &mut |p| reporter.show(p),
        started: &mut || {},
    };
    let result = run_command(cfg, &mut session, &request, deadline, None, None, &mut output);
    session.logout();
    result
}

/// Run `prepare` on a freshly logged-in session; if it fails, log out
/// before returning the error, so the guest pty is freed (see session.rs).
fn or_logout<T>(session: &mut Session, prepare: impl FnOnce(&mut Session) -> io::Result<T>) -> io::Result<T> {
    let result = prepare(session);
    if result.is_err() {
        session.logout();
    }
    result
}

/// The non-tty command exchange on a logged-in session: the delta push of
/// the work area, then the command (output streamed to `sink`). Nothing is
/// pulled back: guest products stay on the guest until `dockerix get`.
/// `out.warn` receives messages for the user's stderr and `out.progress`
/// the push's transfer events (progress.rs). `sync_lock`, when given,
/// is held during the push so concurrent sessions never push (and touch the
/// manifest) at once; the command itself runs unlocked. `interrupt` is polled
/// during the command only (never mid-sync): see `Session::run_to`. The
/// session is left logged in; on error the caller must treat it as unusable.
pub fn run_command(
    cfg: &Config,
    session: &mut Session,
    req: &serve::Request,
    deadline: Instant,
    sync_lock: Option<&Mutex<()>>,
    interrupt: Option<&AtomicBool>,
    out: &mut Output<'_>,
) -> io::Result<i32> {
    if !req.sync {
        // Same working directory as the synced path, so `./hello` means the
        // same thing whether or not the push ran.
        (out.started)();
        return session.run_to(&req.command, deadline, interrupt, out.sink);
    }
    {
        let _g = sync_lock.map(|m| m.lock().unwrap_or_else(|p| p.into_inner()));
        push_workarea(session, cfg, &req.push_opts(), deadline, out)?;
    }
    // Run in the shared dir so relative paths match the host's view. The
    // subdirectory exists after the push (the delta carries new directories,
    // empty ones included).
    (out.started)();
    session.run_to(&req.command, deadline, interrupt, out.sink)
}

/// Push the work area to the guest as a delta, the one place `sync::push` is
/// called: the tty branch and `run_command` both come here (the latter with
/// the manifest lock held). Warnings go to `out.warn`, transfer events to
/// `out.progress`; `out.sink` and `out.started` are untouched.
pub fn push_workarea(
    session: &mut Session,
    cfg: &Config,
    opts: &crate::sync::PushOpts,
    deadline: Instant,
    out: &mut Output<'_>,
) -> io::Result<()> {
    for msg in crate::sync::push(session, cfg, opts, deadline, out.progress)? {
        (out.warn)(&msg);
    }
    Ok(())
}

/// Where a command's results go: its output, messages for the user's
/// stderr, and transfer progress events.
pub struct Output<'a> {
    pub sink: &'a mut dyn FnMut(&[u8]),
    pub warn: &'a mut dyn FnMut(&str),
    pub progress: &'a mut dyn FnMut(&crate::progress::Progress),
    /// Called as the command itself starts, after any push.
    pub started: &'a mut dyn FnMut(),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake_guest::{FakeGuest, Reply};
    use crate::testsupport::work_area;

    /// A work area under a temp dir and a `Config` whose push targets it:
    /// the manifest and stage beside it, the fake guest's login settings,
    /// and a stage directory that does not exist (so the push goes down the
    /// tty) until a test makes it. Returns the temp dir, the work-area path
    /// and the `Config`.
    fn push_cfg(guest: &FakeGuest, tag: &str) -> (tempfile::TempDir, std::path::PathBuf, Config) {
        let (tmp, wa) = work_area(tag);
        let cfg = Config {
            container_workarea: wa.to_string_lossy().into_owned(),
            sync_manifest: tmp.path().join("state/manifest"),
            stage: tmp.path().join("stage").to_string_lossy().into_owned(),
            ..guest.config()
        };
        (tmp, wa, cfg)
    }

    #[test]
    fn shell_quote_leaves_plain_words_alone() {
        for word in ["make", "hello.c", "-o", "/usr/src/a_b+c", "x=1", "user@host:dir,2%"] {
            assert_eq!(shell_quote(word), word);
        }
        assert_eq!(shell_quote(""), "''");
        assert_eq!(shell_quote("a b"), "'a b'");
        assert_eq!(shell_quote("it's"), "'it'\\''s'");
        assert_eq!(join_command(&["echo".into(), "a b".into(), "$HOME".into()]), "echo 'a b' '$HOME'");
    }

    #[test]
    fn shell_quote_survives_a_real_shell() {
        // The guest's 1990 Bourne shell has the same single-quote rules as
        // any /bin/sh, so the host's is a fair judge.
        for word in [
            "",
            "a b",
            "it's",
            "''",
            "$HOME `id` $(id)",
            "*?[a]",
            "a\\b",
            "tab\there",
            "new\nline",
            "!#;&|<>",
        ] {
            let out = std::process::Command::new("/bin/sh")
                .arg("-c")
                .arg(format!("printf %s {}", shell_quote(word)))
                .output()
                .unwrap();
            assert_eq!(String::from_utf8(out.stdout).unwrap(), word);
        }
    }

    /// A `Config` with a work area mounted and nothing else fixed; the guest
    /// cwd is resolved off it (empty subdir/workdir resolve to None at
    /// `from_env`, so the field cases alone are exercised here).
    #[test]
    fn guest_cwd_follows_the_subdirectory() {
        let mut cfg = Config { guest_workarea: "/workarea".into(), ..Config::default() };
        assert_eq!(cfg.guest_cwd(), "/workarea");
        cfg.subdir = Some("/games/rogue/".into());
        assert_eq!(cfg.guest_cwd(), "/workarea/games/rogue");
        cfg.guest_workarea = "/usr/users/w".into();
        assert_eq!(cfg.guest_cwd(), "/usr/users/w/games/rogue");
    }

    #[test]
    fn guest_cwd_prefers_a_fixed_workdir() {
        let mut cfg = Config {
            guest_workarea: "/workarea".into(),
            subdir: Some("games".into()),
            workdir: Some("/usr/src".into()),
            ..Config::default()
        };
        assert_eq!(cfg.guest_cwd(), "/usr/src");
        // No fixed working directory: back to the work area plus subdir.
        cfg.workdir = None;
        assert_eq!(cfg.guest_cwd(), "/workarea/games");
    }

    #[test]
    fn in_guest_cwd_guards_the_cd() {
        let cfg = Config { workdir: Some("/usr/src".into()), ..Config::default() };
        assert_eq!(
            in_guest_cwd(&cfg, "make all"),
            "if test -d /usr/src; then cd /usr/src && make all; \
             else echo 'rix: no such dir in guest: /usr/src' 1>&2; false; fi"
        );
    }

    /// The functions a session sources to keep the work area mounted. Run
    /// for real by the guest's shell (checked on the scratch guest); here,
    /// their text.
    #[test]
    fn mount_functions_mount_once_and_again() {
        let expected = "\
__rixmount() {
\tif mount | grep ' on /workarea ' >/dev/null; then :; else
\t\ttest -d /workarea || mkdir /workarea
\t\ttouch /workarea/.dockerix-unmounted
\t\tmount -o intr 10.0.2.2:/workarea /workarea
\tfi
}
__rixmnt() {
\tif test -f /workarea/.dockerix-unmounted; then __rixmount; else :; fi
}
";
        assert_eq!(mount_functions("/workarea", "10.0.2.2", "intr"), expected);
    }

    /// A fresh session in NFS mode sources the functions, with a line short
    /// enough to type (so they stay in its shell), and mounts; every command
    /// after, typed or staged, is guarded by the mount check.
    #[test]
    fn a_fresh_session_gets_the_mount_functions() {
        let guest = FakeGuest::start();
        let cfg = Config { nfs_requested: true, ..guest.config() };
        let mut s = guest.login();
        let deadline = Instant::now() + Duration::from_secs(5);
        ensure_nfs_mount(&cfg, &mut s, deadline).unwrap();
        {
            let st = guest.state();
            let sourced = st.commands.iter().find(|c| c.contains("&& . /tmp/.rixmnt")).expect("sourced");
            assert!(sourced.ends_with("&& __rixmount"), "{sourced}");
            assert!(st.longest_line <= 200, "{}", st.longest_line);
            assert_eq!(st.mounts, 1);
            assert!(!st.files.keys().any(|p| p.starts_with("/tmp/.rixmnt")), "removed once sourced");
        }
        let before = guest.state().commands.len();
        let long = format!("echo {}", "x".repeat(250));
        assert_eq!(s.run("echo short", deadline).unwrap(), (b"short\n".to_vec(), 0));
        assert_eq!(s.run(&long, deadline).unwrap().1, 0);
        let st = guest.state();
        let after: Vec<&str> = st.commands[before..].iter().map(|c| c.split(' ').next().unwrap()).collect();
        // The staged one: the guard on the typed line, ahead of the script.
        assert_eq!(after, ["__rixmnt", "echo", "uudecode", "__rixmnt", "sh", "echo"]);
    }

    fn request(command: &str, sync: bool) -> serve::Request {
        serve::Request {
            command: command.into(),
            sync,
            sync_max: 1 << 20,
            sync_hash: false,
            sync_clear: false,
            timeout_secs: 5,
            key: None,
            id: None,
            display: None,
            dir: None,
        }
    }

    /// `run_command` on `session` with `cfg`: the status, output, warnings.
    fn run(cfg: &Config, session: &mut Session, req: &serve::Request) -> (i32, String, Vec<String>) {
        let (mut out, mut warnings) = (Vec::new(), Vec::new());
        let mut sink = |c: &[u8]| out.extend_from_slice(c);
        let mut warn = |m: &str| warnings.push(m.to_string());
        let mut output =
            Output { sink: &mut sink, warn: &mut warn, progress: &mut |_| {}, started: &mut || {} };
        let deadline = Instant::now() + Duration::from_secs(5);
        let code = run_command(cfg, session, req, deadline, None, None, &mut output).unwrap();
        (code, String::from_utf8(out).unwrap(), warnings)
    }

    fn compiler() -> FakeGuest {
        FakeGuest::with(|cmd| (cmd == "cc hello.c").then(|| Reply::Done("compiled\n".into(), 0)))
    }

    fn guest_file(guest: &FakeGuest, path: &str) -> Option<String> {
        guest.state().files.get(path).map(|b| String::from_utf8_lossy(b).into_owned())
    }

    #[test]
    fn run_command_pushes_then_runs() {
        let guest = compiler();
        let (_tmp, wa, cfg) = push_cfg(&guest, "exec-push");
        let mut s = guest.login();

        let (code, out, warnings) = run(&cfg, &mut s, &request("cc hello.c", true));
        assert_eq!((code, out.as_str()), (0, "compiled\n"));
        assert!(warnings.iter().any(|w| w.contains("full work-area push")), "{warnings:?}");
        assert_eq!(guest_file(&guest, "/workarea/hello.c").as_deref(), Some("int main() { return 0; }\n"));
        assert_eq!(guest_file(&guest, "/workarea/sub/x.h").as_deref(), Some("#define X 1\n"));
        assert!(guest.state().dirs.contains("/workarea/empty"));
        assert!(guest_file(&guest, "/workarea/.dockerix-epoch").is_some());
        assert!(guest_file(&guest, "/workarea/wa.tar").is_none());
        assert_eq!(guest.state().commands.last().map(String::as_str), Some("cc hello.c"));

        // The next push is a delta: the edit and the deletion, no warning.
        // (A new size: within the same second, size is all that changes;
        // see `DOCKERIX_SYNC_HASH`.)
        std::fs::write(wa.join("hello.c"), "int main() { return 10; }\n").unwrap();
        std::fs::remove_file(wa.join("sub/x.h")).unwrap();
        let before = guest.state().commands.len();
        let (code, _, warnings) = run(&cfg, &mut s, &request("cc hello.c", true));
        assert_eq!(code, 0);
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(guest_file(&guest, "/workarea/hello.c").as_deref(), Some("int main() { return 10; }\n"));
        assert!(guest_file(&guest, "/workarea/sub/x.h").is_none());
        let st = guest.state();
        let pushed: Vec<_> = st.commands[before..].iter().filter(|c| c.ends_with("uudecode")).collect();
        assert_eq!(pushed.len(), 1, "{:?}", &st.commands[before..]);
    }

    /// With the staging export there, the push goes by NFS: the guest
    /// mounts it and unpacks the staged archive; nothing goes down the tty,
    /// and the archive is gone afterwards.
    #[test]
    fn the_push_stages_over_nfs_when_it_can() {
        let guest = compiler();
        let (_tmp, _wa, cfg) = push_cfg(&guest, "exec-staged");
        let stage = std::path::PathBuf::from(&cfg.stage);
        std::fs::create_dir_all(&stage).unwrap();
        guest.state().stage = stage.clone();
        let mut s = guest.login();
        let (code, out, _) = run(&cfg, &mut s, &request("cc hello.c", true));
        assert_eq!((code, out.as_str()), (0, "compiled\n"));
        assert_eq!(guest_file(&guest, "/workarea/hello.c").as_deref(), Some("int main() { return 0; }\n"));
        assert_eq!(guest_file(&guest, "/workarea/sub/x.h").as_deref(), Some("#define X 1\n"));
        let st = guest.state();
        assert!(st.commands.iter().any(|c| c.contains("tar xf /tmp/.xfer/p")), "{:?}", st.commands);
        // The archive went by NFS, not `cd DIR && uudecode` (a bare
        // `uudecode` stages a long command line as a script: the test's
        // stage path is long, the real one short).
        assert!(!st.commands.iter().any(|c| c.ends_with("&& uudecode")), "{:?}", st.commands);
        assert_eq!(std::fs::read_dir(&stage).unwrap().count(), 0, "the staged archive is removed");
    }

    /// A path too long for a tar header's name field arrives whole (in
    /// ustar's prefix field, which the guest's tar reads), not cut short
    /// after a `@LongLink` file; one too long for ustar is left out, with
    /// a warning naming it.
    #[test]
    fn the_push_carries_long_paths_whole() {
        let guest = compiler();
        let (_tmp, wa, cfg) = push_cfg(&guest, "exec-long");
        let deep = format!("{}/deep.c", ["d234567890"; 14].join("/"));
        let long = "g".repeat(101);
        std::fs::create_dir_all(wa.join(&deep).parent().unwrap()).unwrap();
        std::fs::write(wa.join(&deep), "int deep;\n").unwrap();
        std::fs::write(wa.join(&long), "x").unwrap();
        let mut s = guest.login();
        let (code, _, warnings) = run(&cfg, &mut s, &request("cc hello.c", true));
        assert_eq!(code, 0);
        assert_eq!(guest_file(&guest, &format!("/workarea/{deep}")).as_deref(), Some("int deep;\n"));
        let st = guest.state();
        assert!(
            !st.files.keys().any(|p| p.contains("LongLink") || p.contains(&long)),
            "{:?}",
            st.files.keys()
        );
        assert!(warnings.iter().any(|w| w.contains("too long") && w.contains(&long)), "{warnings:?}");
    }

    /// A guest that cannot mount the staging export still gets the push,
    /// down the tty; so does one told to (`DOCKERIX_PUSH_TTY=1`).
    #[test]
    fn the_push_falls_back_to_the_tty() {
        let guest = compiler();
        let (tmp, _wa, cfg) = push_cfg(&guest, "exec-unstaged");
        let stage = std::path::PathBuf::from(&cfg.stage);
        std::fs::create_dir_all(&stage).unwrap();
        guest.state().stage = stage.clone();
        guest.state().stage_unmountable = true;
        let mut s = guest.login();
        assert_eq!(run(&cfg, &mut s, &request("cc hello.c", true)).0, 0);
        assert_eq!(guest_file(&guest, "/workarea/hello.c").as_deref(), Some("int main() { return 0; }\n"));
        assert!(guest.state().commands.iter().any(|c| c.ends_with("&& uudecode")));
        assert_eq!(std::fs::read_dir(&stage).unwrap().count(), 0);

        guest.state().stage_unmountable = false;
        // Told to push over the tty even though staging is available now.
        let tty_cfg = Config { push_tty: true, ..cfg.clone() };
        // A new size: within the same second, size is all that changes.
        std::fs::write(tmp.path().join("workarea/hello.c"), "int main() { return 70; }\n").unwrap();
        let before = guest.state().commands.len();
        assert_eq!(run(&tty_cfg, &mut s, &request("cc hello.c", true)).0, 0);
        let st = guest.state();
        assert!(
            !st.commands[before..].iter().any(|c| c.contains("/tmp/.xfer")),
            "{:?}",
            &st.commands[before..]
        );
        assert!(st.commands[before..].iter().any(|c| c.ends_with("&& uudecode")));
    }

    #[test]
    fn run_command_without_sync_only_runs() {
        let guest = compiler();
        let (_tmp, _wa, cfg) = push_cfg(&guest, "exec-nosync");
        let mut s = guest.login();
        assert_eq!(run(&cfg, &mut s, &request("cc hello.c", false)), (0, "compiled\n".into(), vec![]));
        assert_eq!(guest.state().commands[1..], ["cc hello.c"]);
        assert!(guest.state().files.is_empty());
    }

    #[test]
    fn run_command_clear_starts_the_guest_tree_over() {
        let guest = compiler();
        let (_tmp, _wa, cfg) = push_cfg(&guest, "exec-clear");
        guest.state().files.insert("/workarea/stale.o".into(), b"old".to_vec());
        let mut s = guest.login();
        let req = serve::Request { sync_clear: true, ..request("cc hello.c", true) };
        let (code, _, warnings) = run(&cfg, &mut s, &req);
        assert_eq!(code, 0);
        assert!(warnings.iter().any(|w| w.contains("cleared")), "{warnings:?}");
        assert!(guest_file(&guest, "/workarea/stale.o").is_none());
        assert!(guest_file(&guest, "/workarea/hello.c").is_some());
    }

    #[test]
    fn clear_is_refused_in_nfs_mode() {
        let guest = compiler();
        let cfg = Config { nfs_requested: true, sync_clear: true, ..guest.config() };
        let opts =
            ExecOpts { tty: Some(false), command: vec!["cc".into(), "hello.c".into()], timeout_secs: 5 };
        let e = super::run(&cfg, opts).unwrap_err();
        assert!(e.to_string().starts_with("nothing to clear"), "{e}");
        assert_eq!(guest.state().connections, 0);
    }

    #[test]
    fn a_failed_push_still_logs_out() {
        let guest = FakeGuest::with(|cmd| {
            cmd.ends_with("tar xf wa.tar && rm -f wa.tar")
                .then(|| Reply::Done("     512\ntar: bad\n".into(), 1))
        });
        let (_tmp, _wa, cfg) = push_cfg(&guest, "exec-fail");
        let cfg = Config { direct: true, ..cfg };
        let opts =
            ExecOpts { tty: Some(false), command: vec!["cc".into(), "hello.c".into()], timeout_secs: 5 };
        let e = super::run(&cfg, opts).unwrap_err();
        assert!(e.to_string().starts_with("guest-side tar extract failed"), "{e}");
        let st = guest.state();
        assert_eq!((st.logins, st.logouts, st.dropped), (1, 1, 0));
    }

    fn failing_unpack() -> FakeGuest {
        FakeGuest::with(|cmd| {
            cmd.ends_with("tar xf wa.tar && rm -f wa.tar").then(|| Reply::Done("tar: bad\n".into(), 1))
        })
    }

    #[test]
    fn a_failed_push_before_a_tty_command_still_logs_out() {
        let guest = failing_unpack();
        let (_tmp, _wa, cfg) = push_cfg(&guest, "exec-tty-fail");
        let opts =
            ExecOpts { tty: Some(true), command: vec!["vi".into(), "hello.c".into()], timeout_secs: 5 };
        let e = super::run(&cfg, opts).unwrap_err();
        assert!(e.to_string().starts_with("guest-side tar extract failed"), "{e}");
        assert!(guest.wait(|st| st.logouts + st.dropped == 1));
        let st = guest.state();
        assert_eq!((st.logins, st.logouts, st.dropped), (1, 1, 0));
    }

    #[test]
    fn a_failed_nfs_mount_before_a_login_shell_still_logs_out() {
        let guest = FakeGuest::with(|cmd| {
            cmd.ends_with("&& __rixmount").then(|| Reply::Done("mount: RPC: timed out\n".into(), 1))
        });
        let cfg = Config { nfs_requested: true, ..guest.config() };
        let opts = ExecOpts { tty: Some(false), command: vec![], timeout_secs: 5 };
        let e = super::run(&cfg, opts).unwrap_err();
        assert!(e.to_string().contains("guest NFS mount"), "{e}");
        assert!(guest.wait(|st| st.logouts + st.dropped == 1));
        let st = guest.state();
        assert_eq!((st.logins, st.logouts, st.dropped), (1, 1, 0));
    }

    #[test]
    fn a_failed_nfs_mount_before_a_direct_command_still_logs_out() {
        let guest = FakeGuest::with(|cmd| {
            cmd.ends_with("&& __rixmount").then(|| Reply::Done("mount: RPC: timed out\n".into(), 1))
        });
        let cfg = Config { nfs_requested: true, direct: true, ..guest.config() };
        let opts =
            ExecOpts { tty: Some(false), command: vec!["cc".into(), "hello.c".into()], timeout_secs: 5 };
        let e = super::run(&cfg, opts).unwrap_err();
        assert!(e.to_string().contains("guest NFS mount"), "{e}");
        assert!(guest.wait(|st| st.logouts + st.dropped == 1));
        let st = guest.state();
        assert_eq!((st.logins, st.logouts, st.dropped), (1, 1, 0));
    }
}
