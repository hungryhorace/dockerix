//! The commands that run in the guest through `rix` in the container:
//! `login`, `exec`, `run` and `sync`, with the host cwd mapped to the guest's
//! copy of it and the settings `rix` reads passed in its environment.

use std::io::{self, IsTerminal};
use std::path::{Path, PathBuf};
use std::process::Stdio;

use super::interrupt::{install_sigint_flag, interrupted, wait_interruptible};
use super::*;
use crate::docker::{docker, docker_missing};
use crate::session::token;

/// `dockerix login [-w DIR]`: interactive root shell in the guest, in `DIR`
/// (else the `start -w` directory, else /workarea — `rix` applies those
/// defaults in the container).
pub fn login(workdir: Option<String>, plain: bool) -> io::Result<i32> {
    let workdir = workdir.as_deref().map(guest_dir).transpose()?;
    require_running()?;
    exec_in_container(true, None, workdir.as_deref(), &RixOpts::new(Push::Default, plain), &[])
}

/// `dockerix sync`: push the work area to the guest and nothing else. Every
/// command already pushes first, so this is `dockerix exec true` with the sync
/// forced on and the cwd mapping skipped (the push covers the whole tree,
/// so it works from anywhere). For someone editing on the host and building
/// in a login shell, where no command of ours runs to carry the push.
/// `clear` (`--clear`) first deletes the guest work area, guest build
/// products included, and starts a new manifest: a full push from scratch.
/// `hash` (`--hash`) compares file contents, not just sizes and times.
pub fn sync(clear: bool, hash: bool, plain: bool) -> io::Result<i32> {
    let container = require_running()?;
    if workarea_root(&container).is_none() {
        return Err(io::Error::other(NO_WORKAREA));
    }
    let opts = RixOpts { hash, ..RixOpts::new(Push::Forced { clear }, plain) };
    exec_in_container(false, None, Some(WORKAREA_MOUNT), &opts, &["true".to_string()])
}

/// The options of `dockerix exec` that `run` shares.
#[derive(Clone, Debug, Default)]
pub struct ExecOptions {
    /// Guest directory (`-w`).
    pub workdir: Option<String>,
    /// Compare file contents in the push (`--hash`).
    pub hash: bool,
    /// Seconds before the command is abandoned, 0 for no limit (`--timeout`).
    pub timeout: Option<u64>,
}

/// `dockerix exec [-t] [--no-sync] [--hash] [--timeout SECS] [-w DIR]
/// <command…>`: run in the running container, in `DIR`, else the `start -w`
/// directory, else the guest's copy of the cwd. With no command, a login
/// shell.
pub fn exec(
    tty: bool,
    no_sync: bool,
    options: ExecOptions,
    command: Vec<String>,
    plain: bool,
) -> io::Result<i32> {
    if command.is_empty() {
        return login(options.workdir, plain);
    }
    let workdir = options.workdir.as_deref().map(guest_dir).transpose()?;
    let container = require_running()?;
    let push = if no_sync { Push::Skip } else { Push::Default };
    let opts = RixOpts { hash: options.hash, timeout: options.timeout, ..RixOpts::new(push, plain) };
    exec_prepared(tty, &opts, workdir, &command, &container)
}

/// `dockerix run [--workarea DIR] [-w DIR] <command…>`: the one-shot form
/// for CI. Start the emulator sharing `--workarea`, run the command, then
/// stop and remove the container. Refuses if the container is already
/// running: that is someone's live session, and its work area may not be
/// the one asked for. The exit status is the command's, or `stop`'s if the
/// command succeeded but the halt failed.
pub fn run(
    workarea: Option<PathBuf>,
    profile: Option<Profile>,
    settings: &AgentSettings,
    options: ExecOptions,
    command: Vec<String>,
    plain: bool,
) -> io::Result<i32> {
    let workdir = options.workdir.as_deref().map(guest_dir).transpose()?;
    if let State::Running { .. } = inspect_state()? {
        return Err(io::Error::other(format!(
            "{CONTAINER} is already running; use dockerix exec, or dockerix stop first"
        )));
    }
    // No console: nobody watches a one-shot run's screen.
    ensure_running(workarea, None, false, profile, settings)?;
    // Report a failure to run the command now, before the stop's messages,
    // and stop regardless.
    let opts = RixOpts { hash: options.hash, timeout: options.timeout, ..RixOpts::new(Push::Default, plain) };
    let code = require_running()
        .and_then(|c| exec_prepared(false, &opts, workdir, &command, &c))
        .unwrap_or_else(|e| {
            eprintln!("dockerix: {e}");
            FAILURE
        });
    match stop() {
        Ok(_) => Ok(code),
        // A command that succeeded takes the halt's failure as its result.
        Err(e) if code == 0 => Err(e),
        Err(e) => {
            eprintln!("dockerix: {e}");
            Ok(code)
        }
    }
}

/// The part of `exec` after the container is up: map the cwd and run, from
/// the container's snapshot (no further inspect).
fn exec_prepared(
    tty: bool,
    opts: &RixOpts,
    workdir: Option<String>,
    command: &[String],
    container: &Container,
) -> io::Result<i32> {
    // The cwd mapping only matters when no working directory was fixed: an
    // explicit one (here or at start) is used as given, from anywhere.
    let fixed = workdir.is_some() || container.value(var::WORKDIR).is_some();
    let subdir = if fixed { None } else { map_cwd(workarea_root(container).as_deref())? };
    exec_in_container(tty, subdir.as_deref(), workdir.as_deref(), opts, command)
}

pub(super) const NO_WORKAREA: &str =
    "no work area: the container was started without one (dockerix stop && dockerix start --workarea DIR)";

/// The work area the container was started with (canonical, so symlinks
/// such as /tmp on macOS don't defeat path comparisons), or None if it was
/// started without one. From the container's snapshot: no inspect.
pub(super) fn workarea_root(container: &Container) -> Option<PathBuf> {
    container.mount(WORKAREA_MOUNT).map(|root| std::fs::canonicalize(&root).unwrap_or(root))
}

/// `path` relative to the work-area root ("" at the root), or None if it is
/// outside or there is no work area (`root` None). The path need not exist
/// yet: its parent must.
pub(super) fn workarea_rel(root: Option<&Path>, path: &Path) -> io::Result<Option<String>> {
    let Some(root) = root else {
        return Ok(None);
    };
    let canon = match std::fs::canonicalize(path) {
        Ok(p) => p,
        Err(_) => {
            let parent = path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
            let name =
                path.file_name().ok_or_else(|| io::Error::other(format!("{}: bad path", path.display())))?;
            std::fs::canonicalize(parent)?.join(name)
        }
    };
    Ok(canon.strip_prefix(root).ok().map(|r| r.to_string_lossy().into_owned()))
}

/// Map the host cwd into the guest: it must be inside the work area the
/// container was started with (`root`). Returns the path relative to the
/// work-area root ("" at the root), or None if there is no work area
/// (commands then run in the guest's `/`).
pub(super) fn map_cwd(root: Option<&Path>) -> io::Result<Option<String>> {
    let Some(root) = root else {
        return Ok(None);
    };
    let cwd = std::env::current_dir()?;
    let rel = workarea_rel(Some(root), &cwd)?.ok_or_else(|| {
        io::Error::other(format!(
            "the current directory is outside the work area.\n  cwd:       {}\n  work area: {}\n\
             The work area is fixed when the container starts. Either cd into it, or restart \
             with this directory exported:\n  dockerix stop && dockerix start --workarea .",
            cwd.display(),
            root.display()
        ))
    })?;
    // Guest paths go into a `cd` on a 1990 /bin/sh; keep them boring.
    if rel.bytes().any(|b| !(b.is_ascii_alphanumeric() || b"/_.+-".contains(&b))) {
        return Err(io::Error::other(format!(
            "work-area path {rel:?} has characters the guest shell can't take; use plain names"
        )));
    }
    Ok(Some(rel))
}

/// How transfer progress should be drawn, decided here because only the
/// host side knows whether stderr is a terminal (inside the container it is
/// always docker's pipe). Options first: `--quiet` turns it off (`0`) and
/// `--plain` gives one-line notices. Then an explicit `DOCKERIX_PROGRESS`
/// passes straight through. Then `plain` (`DOCKERIX_PLAIN=1`) or a
/// non-terminal gives notices, and a terminal gets a bar.
pub(super) fn progress_style(plain: bool) -> String {
    if globals().quiet {
        return "0".to_string();
    }
    if globals().plain {
        return "plain".to_string();
    }
    if let Ok(v) = std::env::var(var::PROGRESS) {
        return v;
    }
    if plain || !io::stderr().is_terminal() { "plain" } else { "bar" }.to_string()
}

/// How `rix` runs a command, as the host-side command was told.
#[derive(Clone, Copy, Debug)]
pub(super) struct RixOpts {
    pub(super) push: Push,
    /// One-line progress notices (`--plain` or `DOCKERIX_PLAIN=1`).
    pub(super) plain: bool,
    /// Compare file contents in the push (`--hash`); else as
    /// `DOCKERIX_SYNC_HASH` says.
    pub(super) hash: bool,
    /// Seconds before the command is abandoned, 0 for no limit
    /// (`--timeout`); else as `DOCKERIX_EXEC_TIMEOUT` says.
    pub(super) timeout: Option<u64>,
}

impl RixOpts {
    pub(super) fn new(push: Push, plain: bool) -> RixOpts {
        RixOpts { push, plain, hash: false, timeout: None }
    }
}

/// The `KEY=value` settings `rix` gets for a command: where it runs, the
/// push, the progress style, the timeout, and the tuning knobs passed
/// through from the host's environment (options winning over them).
pub(super) fn rix_env(subdir: Option<&str>, workdir: Option<&str>, opts: &RixOpts) -> Vec<String> {
    let (push, plain) = (opts.push, opts.plain);
    let mut env = Vec::new();
    if let Some(sub) = subdir {
        env.push(format!("{}={sub}", var::SUBDIR));
    }
    if let Some(dir) = workdir {
        env.push(format!("{}={dir}", var::WORKDIR));
    }
    match push {
        // Passed through with the other knobs below.
        Push::Default => {}
        Push::Skip => env.push(format!("{}=0", var::SYNC)),
        // On, whatever the host's DOCKERIX_SYNC says: it is not passed.
        Push::Forced { clear } => {
            if clear {
                env.push(format!("{}=1", var::SYNC_CLEAR));
            }
        }
    }
    if let Ok(t) = std::env::var("TERM") {
        env.push(format!("TERM={t}"));
    }
    env.push(format!("{}={}", var::PROGRESS, progress_style(plain)));
    // Options, each in place of the variable it stands for.
    if opts.hash {
        env.push(format!("{}=1", var::SYNC_HASH));
    }
    if let Some(secs) = opts.timeout {
        env.push(format!("{}={secs}", var::EXEC_TIMEOUT));
    }
    // Tuning knobs `rix` reads inside the container; pass them through so
    // `DOCKERIX_SYNC=0 dockerix exec make` means what it says. One the
    // command line has set is not passed (a repeated `-e` would be
    // ambiguous).
    let sync = if push == Push::Default { Some(var::SYNC) } else { None };
    let hash = (!opts.hash).then_some(var::SYNC_HASH);
    let timeout = opts.timeout.is_none().then_some(var::EXEC_TIMEOUT);
    let knobs = [var::SYNC_MAX, var::TERM, var::TELNET_TRACE, var::DIRECT];
    for key in sync.into_iter().chain(hash).chain(timeout).chain(knobs) {
        if let Ok(v) = std::env::var(key) {
            env.push(format!("{key}={v}"));
        }
    }
    env
}

/// Whether `rix` pushes the work area before the command.
#[derive(Clone, Copy, PartialEq, Debug)]
pub(super) enum Push {
    /// As `DOCKERIX_SYNC` on the host says (on unless `0`).
    Default,
    /// Not this time (`exec --no-sync`).
    Skip,
    /// Always, whatever `DOCKERIX_SYNC` says, and with `clear` the guest
    /// work area emptied first (`dockerix sync [--clear]`).
    Forced { clear: bool },
}

/// `docker exec [-it] [-e DOCKERIX_SUBDIR=…] [-e DOCKERIX_WORKDIR=…]
/// dockerix-vm rix [-t] <command…>`, stdio inherited so a tty passes straight
/// through. Returns the guest exit code. Without `workdir` the container's
/// own `DOCKERIX_WORKDIR` (from `start -w`) applies.
pub(super) fn exec_in_container(
    tty: bool,
    subdir: Option<&str>,
    workdir: Option<&str>,
    opts: &RixOpts,
    command: &[String],
) -> io::Result<i32> {
    let mut cmd = docker();
    cmd.arg("exec");
    // Only allocate a docker tty when we actually have one; `-it` on a
    // non-terminal stdin makes docker complain and exit.
    let have_terminal = io::stdin().is_terminal() && io::stdout().is_terminal();
    if tty && !have_terminal {
        return Err(io::Error::other("-t needs a terminal on stdin and stdout"));
    }
    if tty {
        cmd.arg("-it");
    }
    for setting in rix_env(subdir, workdir, opts) {
        cmd.arg("-e").arg(setting);
    }
    // Session affinity for serve mode: commands from the same host terminal
    // reuse the same held guest session, so shell state carries over.
    if let Some(key) = session_key() {
        cmd.arg("-e").arg(format!("{}={key}", var::SESSION));
    }
    // Ctrl-C: `docker exec` forwards no signals, so tag non-tty runs with an
    // id and, if we are interrupted, ask the agent to interrupt the guest
    // command (`rix --abort`). In tty mode ^C travels down the pty itself.
    let run_id = if !tty && !command.is_empty() { Some(token()) } else { None };
    if let Some(id) = &run_id {
        cmd.arg("-e").arg(format!("{}={id}", var::RUN_ID));
        install_sigint_flag()?;
    }
    cmd.arg(CONTAINER).arg("rix");
    if tty {
        cmd.arg("-t");
    }
    if !command.is_empty() {
        cmd.arg("--").args(command);
    }
    let status =
        if run_id.is_some() { wait_interruptible(cmd) } else { cmd.status().map_err(docker_missing) };
    if let Some(id) = &run_id
        && interrupted()
    {
        let mut abort = docker();
        abort.args(["exec", CONTAINER, "rix", "--abort", id]);
        abort.stdout(Stdio::null());
        match abort.status() {
            Ok(s) if s.success() => eprintln!("dockerix: interrupted the guest command"),
            _ => eprintln!("dockerix: could not reach the agent to interrupt the guest command"),
        }
        return Ok(130);
    }
    Ok(child_code(status?, FAILURE))
}

/// `DOCKERIX_SESSION` if set, else the name of the terminal on stdin.
pub(super) fn session_key() -> Option<String> {
    // Set but empty means no affinity, not the terminal's name.
    if let Ok(k) = std::env::var(var::SESSION) {
        return if k.is_empty() { None } else { Some(k) };
    }
    let name = rustix::termios::ttyname(rustix::stdio::stdin(), Vec::new()).ok()?;
    Some(name.to_string_lossy().trim_start_matches("/dev/").to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testsupport::Env;

    /// `rix_env` with the host's environment as given (and nothing else of
    /// ours set).
    fn rix_env_with(env: &mut Env, host: &[(&str, &str)], push: Push, plain: bool) -> Vec<String> {
        rix_env_opts(env, host, &RixOpts::new(push, plain))
    }

    fn rix_env_opts(env: &mut Env, host: &[(&str, &str)], opts: &RixOpts) -> Vec<String> {
        for key in config::var::ALL.iter().chain(&["TERM"]) {
            env.unset(key);
        }
        for (k, v) in host {
            env.set(k, v);
        }
        rix_env(None, None, opts)
    }

    #[test]
    fn rix_env_options_win_over_the_macs_variables() {
        let mut env = Env::lock();
        let opts = RixOpts { hash: true, timeout: Some(0), ..RixOpts::new(Push::Default, false) };
        let host = [(var::SYNC_HASH, "0"), (var::EXEC_TIMEOUT, "60")];
        let got = rix_env_opts(&mut env, &host, &opts);
        let set = |key: &str| got.iter().filter(|e| e.starts_with(&format!("{key}="))).collect::<Vec<_>>();
        // Each once, as the option says: a second `-e` would be ambiguous.
        assert_eq!(set(var::SYNC_HASH), [&format!("{}=1", var::SYNC_HASH)]);
        assert_eq!(set(var::EXEC_TIMEOUT), [&format!("{}=0", var::EXEC_TIMEOUT)]);
        // Without the options, the host's variables pass through.
        let got = rix_env_opts(&mut env, &host, &RixOpts::new(Push::Default, false));
        assert!(got.contains(&format!("{}=0", var::SYNC_HASH)), "{got:?}");
        assert!(got.contains(&format!("{}=60", var::EXEC_TIMEOUT)), "{got:?}");
    }

    #[test]
    fn rix_env_carries_the_push_decision() {
        let mut env = Env::lock();
        let sync = |v: &str| format!("{}={v}", var::SYNC);
        // Default: the host's DOCKERIX_SYNC passes through, if set.
        let got = rix_env_with(&mut env, &[(var::SYNC, "0")], Push::Default, false);
        assert!(got.contains(&sync("0")), "{got:?}");
        let got = rix_env_with(&mut env, &[], Push::Default, false);
        assert!(!got.iter().any(|e| e.starts_with(var::SYNC)), "{got:?}");
        // --no-sync wins over the host's setting.
        let got = rix_env_with(&mut env, &[(var::SYNC, "1")], Push::Skip, false);
        assert!(got.contains(&sync("0")) && !got.contains(&sync("1")), "{got:?}");
        // `dockerix sync` pushes even with DOCKERIX_SYNC=0 on the host.
        let got = rix_env_with(&mut env, &[(var::SYNC, "0")], Push::Forced { clear: false }, false);
        assert!(!got.iter().any(|e| e.starts_with(var::SYNC)), "{got:?}");
        let got = rix_env_with(&mut env, &[], Push::Forced { clear: true }, false);
        assert_eq!(
            got.iter().filter(|e| e.starts_with(var::SYNC)).collect::<Vec<_>>(),
            [&format!("{}=1", var::SYNC_CLEAR)]
        );
    }

    #[test]
    fn rix_env_progress_and_pass_through() {
        let mut env = Env::lock();
        let progress = |v: &str| format!("{}={v}", var::PROGRESS);
        let got = rix_env_with(&mut env, &[(var::SYNC_HASH, "1"), ("TERM", "xterm")], Push::Default, true);
        assert!(got.contains(&progress("plain")), "{got:?}");
        assert!(got.contains(&format!("{}=1", var::SYNC_HASH)), "{got:?}");
        assert!(got.contains(&"TERM=xterm".to_string()), "{got:?}");
        // An explicit DOCKERIX_PROGRESS beats DOCKERIX_PLAIN (the --plain
        // and --quiet options beat both; see progress_style).
        let got = rix_env_with(&mut env, &[(var::PROGRESS, "0")], Push::Default, true);
        assert!(got.contains(&progress("0")) && !got.contains(&progress("plain")), "{got:?}");
    }
}
