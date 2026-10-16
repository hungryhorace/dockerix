//! host-side convenience commands (`dockerix start|stop|restart|status|
//! console|login|exec|run|sync|get|put|x11|build|reset`).
//!
//! Everything here is a short sequence of `docker` commands (docker.rs).
//! Nothing in this module runs inside the container. This file has the
//! container's life: start, stop, restart, status, the console, X11 and
//! reset. The rest:
//!
//! - `rix`: commands run in the guest through `rix` (`login`, `exec`,
//!   `run`, `sync`), and mapping the host cwd into the guest;
//! - `transfer`: `get` and `put`;
//! - `image`: the image built from the embedded context, and `build`;
//! - `spec`: the `docker run` arguments, and reading them back;
//! - `interrupt`: Ctrl-C during a command.
//!
//! Every command returns its exit status, or an error that `main` reports
//! as `dockerix: …` with exit status `FAILURE`.

use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use crate::config::{self, Profile, var};
use crate::docker::{
    CONTAINER, Container, State, docker, docker_missing, docker_output, docker_quiet, inspect, inspect_state,
};
use crate::relay;

mod image;
mod interrupt;
mod rix;
mod spec;
mod transfer;

pub use image::build;
pub use rix::{ExecOptions, exec, login, run, sync};
pub use spec::AgentSettings;
pub use transfer::{get, put};

use image::{ensure_image, warn_if_stale};
use spec::{RunSpec, current_spec, merge_env, run_container, start_env};

/// The bind-mount point of the work area inside the container.
const WORKAREA_MOUNT: &str = "/workarea";
/// The bind-mount point of the persistent guest disc inside the container.
const STATE_MOUNT: &str = "/state";
/// The bind-mount point of the user-supplied ROMs and disc image.
const ASSETS_MOUNT: &str = "/assets";
/// The ROM set the baked machine config uses, under the assets' `roms/`.
const ROM_SET: &str = "riscos311";

/// Exit status used for wrapper-side failures (docker missing, cwd outside the
/// work area, …), distinct from any guest exit code. Same as `rix` uses.
pub const FAILURE: i32 = 125;

/// The options that apply to every command (`--state`, `--assets`,
/// `--image`, `--quiet`, `--plain`). Each wins over its environment
/// variables.
#[derive(Debug, Default)]
pub struct Globals {
    pub state: Option<PathBuf>,
    pub assets: Option<PathBuf>,
    pub image: Option<String>,
    /// No transfer progress at all.
    pub quiet: bool,
    /// `--plain` itself (the `plain` arguments also count `DOCKERIX_PLAIN`).
    pub plain: bool,
}

static GLOBALS: OnceLock<Globals> = OnceLock::new();

/// Record the global options, once, before any command runs.
pub fn set_globals(globals: Globals) {
    let _ = GLOBALS.set(globals);
}

fn globals() -> &'static Globals {
    GLOBALS.get_or_init(Globals::default)
}

/// The exit status a `docker` child leaves us with: its own, or 128 + the
/// signal that killed it, as a shell reports it (`dockerix exec ls | head`
/// ends with the child's SIGPIPE, which is no failure of ours); `otherwise`
/// if it has neither.
fn child_code(status: std::process::ExitStatus, otherwise: i32) -> i32 {
    use std::os::unix::process::ExitStatusExt;
    status.code().or_else(|| status.signal().map(|sig| 128 + sig)).unwrap_or(otherwise)
}

/// `~/.dockerix/<name>`: the default home of the state and assets directories.
pub(crate) fn dockerix_home(name: &str) -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    Path::new(&home).join(".dockerix").join(name)
}

/// The guest-state directory mounted at /state: `--state`, else
/// `DOCKERIX_STATE`, else `~/.dockerix/state`.
fn state_dir() -> PathBuf {
    globals().state.clone().or_else(config::state).unwrap_or_else(|| dockerix_home("state"))
}

/// The user-supplied ROMs and disc image, mounted read-only at /assets:
/// `--assets`, else `DOCKERIX_ASSETS`, else `~/.dockerix/assets`.
fn assets_dir() -> PathBuf {
    globals().assets.clone().or_else(config::assets).unwrap_or_else(|| dockerix_home("assets"))
}

/// Check the assets before starting, so a missing ROM fails here rather than
/// in the container (or after a first image build). The disc image is needed
/// only to seed a state directory that has no disc yet.
fn check_assets(assets: &Path, state: &Path) -> io::Result<()> {
    let missing = |what: String| {
        io::Error::other(format!(
            "{what}: the RISC OS ROMs and RISC iX disc image are user-supplied; put them in \
             {} or name another directory with --assets (see README.md, \"What you need\")",
            assets.display()
        ))
    };
    if !assets.is_dir() {
        return Err(missing(format!("no assets directory {}", assets.display())));
    }
    let roms = assets.join("roms").join(ROM_SET);
    let has_rom = std::fs::read_dir(&roms).is_ok_and(|d| {
        d.flatten().any(|e| {
            let name = e.file_name();
            let name = name.to_string_lossy();
            !name.starts_with('.') && name != "roms.txt" && e.path().is_file()
        })
    });
    if !has_rom {
        return Err(missing(format!("no RISC OS 3.11 ROM in {}", roms.display())));
    }
    let disc = assets.join("images").join("hdimage.hdf");
    if !state.join("hdimage.hdf").exists() && !disc.is_file() {
        return Err(missing(format!("no RISC iX disc image {}", disc.display())));
    }
    Ok(())
}

// ---------------------------------------------------------------- commands

/// `dockerix start [--workarea DIR] [-w GUESTDIR] [--no-vnc] [--profile P]
/// [--nfs] [--x11] …`: bring the container up in the background and wait
/// for the guest to boot. Idempotent. `workdir` is recorded in the
/// container's environment (`DOCKERIX_WORKDIR`) so later `login`s and
/// commands start there without being told. `vnc` false leaves out the
/// noVNC console. `profile` picks the emulated machine (`DOCKERIX_PROFILE`),
/// and `settings` the agent's start-time settings.
pub fn start(
    workarea: Option<PathBuf>,
    workdir: Option<String>,
    vnc: bool,
    profile: Option<Profile>,
    settings: &AgentSettings,
) -> io::Result<i32> {
    let workdir = workdir.as_deref().map(guest_dir).transpose()?;
    ensure_running(workarea, workdir, vnc, profile, settings)?;
    Ok(0)
}

/// Bring the container up and wait for the guest to boot. Returns whether
/// this call started it (false: it was already running).
fn ensure_running(
    workarea: Option<PathBuf>,
    workdir: Option<String>,
    vnc: bool,
    profile: Option<Profile>,
    settings: &AgentSettings,
) -> io::Result<bool> {
    let mut started = false;
    let snapshot = inspect()?;
    if let Some(State::Running { health, image, .. }) = snapshot.as_ref().map(Container::state) {
        let container = snapshot.as_ref().expect("running: inspect returned a container");
        let other_profile = profile.is_some_and(|p| p != container_profile(container));
        if workarea.is_some() || workdir.is_some() || !vnc || other_profile || settings.any() {
            eprintln!(
                "dockerix: {CONTAINER} is already running; the start options are fixed at \
                 start (dockerix restart with them, or dockerix stop first, to change them)"
            );
        }
        // Not restarted here: that would end someone's live session.
        warn_if_stale(&image);
        if health == "healthy" {
            return Ok(false);
        }
    } else {
        // Absent or stopped: start fresh.
        started = true;
        let (assets, state) = (assets_dir(), state_dir());
        check_assets(&assets, &state)?;
        // Bind mounts need absolute paths (`-v` takes a relative one for
        // a volume name). Docker would create a missing state directory,
        // but owned by root on Linux.
        std::fs::create_dir_all(&state)
            .map_err(|e| io::Error::other(format!("state directory {}: {e}", state.display())))?;
        // No work area unless one is named.
        let workarea = workarea
            .or_else(config::workarea)
            .map(|dir| {
                std::fs::canonicalize(&dir)
                    .map_err(|e| io::Error::other(format!("work area {}: {e}", dir.display())))
            })
            .transpose()?;
        let env = start_env(settings);
        let (image, local) = ensure_image()?;
        let spec = RunSpec {
            image,
            local,
            state: std::fs::canonicalize(&state)?,
            assets: std::fs::canonicalize(&assets)?,
            workarea,
            workdir,
            vnc,
            profile,
            env,
        };
        run_container(&spec)?;
    }
    wait_healthy(Duration::from_secs(600))?;
    Ok(started)
}

/// For the commands that work in an existing session (`exec`, `login`,
/// `sync`, `get`, `put`): refuse if the container is not up, rather than
/// start one, and wait out a boot still in progress. Returns the snapshot
/// so the caller reads the work area and `-w` from it with no second
/// inspect; its fields do not change over a boot.
fn require_running() -> io::Result<Container> {
    let not_running = || {
        io::Error::other(format!(
            "{CONTAINER} is not running (dockerix start, or dockerix run for a one-shot)"
        ))
    };
    let container = inspect()?.ok_or_else(not_running)?;
    let State::Running { health, image, .. } = container.state() else {
        return Err(not_running());
    };
    warn_if_stale(&image);
    // Already booted: no second inspect. Otherwise poll it out.
    if health != "healthy" {
        wait_healthy(Duration::from_secs(600))?;
    }
    Ok(container)
}

/// Poll the health check until the guest is booted.
fn wait_healthy(limit: Duration) -> io::Result<()> {
    let deadline = Instant::now() + limit;
    let mut said = false;
    loop {
        match inspect_state()? {
            State::Running { health, .. } if health == "healthy" => {
                if said {
                    eprintln!();
                }
                return Ok(());
            }
            State::Running { health, .. } if health == "unhealthy" => {
                return Err(io::Error::other(format!(
                    "{CONTAINER} is running but unhealthy (guest not answering); see `docker logs {CONTAINER}`"
                )));
            }
            State::Running { .. } => {}
            other => {
                return Err(io::Error::other(format!(
                    "{CONTAINER} stopped while booting ({other:?}); see `docker logs {CONTAINER}`"
                )));
            }
        }
        if Instant::now() >= deadline {
            return Err(io::Error::other("timed out waiting for the guest to boot"));
        }
        if !said {
            eprint!("dockerix: waiting for RISC iX to boot (about a minute)");
            said = true;
        } else {
            eprint!(".");
        }
        let _ = io::stderr().flush();
        std::thread::sleep(Duration::from_secs(3));
    }
}

/// `dockerix stop`: halt the guest cleanly (the entrypoint does that on
/// SIGTERM) and remove the container.
pub fn stop() -> io::Result<i32> {
    match inspect_state()? {
        State::Absent => eprintln!("dockerix: {CONTAINER} is not running"),
        state => stop_container(&state)?,
    }
    Ok(0)
}

/// Halt and remove the existing container, in `state`.
fn stop_container(state: &State) -> io::Result<()> {
    if matches!(state, State::Running { .. }) {
        eprintln!("dockerix: stopping {CONTAINER} (halting RISC iX)…");
        match docker_quiet(&["stop", CONTAINER])? {
            0 => {}
            code => return Err(io::Error::other(format!("docker stop failed (exit {code})"))),
        }
    }
    match docker_quiet(&["rm", CONTAINER])? {
        0 => Ok(()),
        code => Err(io::Error::other(format!("docker rm failed (exit {code})"))),
    }
}

/// `dockerix restart`: recreate the container from this dockerix's image
/// with the options the current one was started with: its work area, state
/// and assets, `-w`, `--no-vnc`, `--profile` and `START_ENV` settings. A
/// setting given now wins over the old one, as an option (`--x11`, `--state
/// DIR`, …) or a `START_ENV` variable on the host, so `dockerix restart
/// --x11` turns X11 on and keeps the rest. The image is built before the
/// old container stops, so a long or failed build leaves the session
/// running.
pub fn restart(settings: &AgentSettings) -> io::Result<i32> {
    let container =
        inspect()?.ok_or_else(|| io::Error::other(format!("{CONTAINER} is not running (dockerix start)")))?;
    let mut spec = current_spec(&container)?;
    merge_env(&mut spec.env, start_env(settings));
    if globals().state.is_some() {
        let dir = state_dir();
        std::fs::create_dir_all(&dir)
            .map_err(|e| io::Error::other(format!("state directory {}: {e}", dir.display())))?;
        spec.state = std::fs::canonicalize(&dir)?;
    }
    if globals().assets.is_some() {
        check_assets(&assets_dir(), &spec.state)?;
        spec.assets = std::fs::canonicalize(assets_dir())?;
    }
    let (image, local) = ensure_image()?;
    (spec.image, spec.local) = (image, local);
    stop_container(&container.state())?;
    run_container(&spec)?;
    wait_healthy(Duration::from_secs(600))?;
    Ok(0)
}

/// `dockerix status`: is it running, is it booted, and how it was started.
/// With `verbose`, also ask the guest (whether the NFS share is mounted).
/// Exit 0 when booted, 1 when running but not yet booted, 2 when not
/// running.
pub fn status(verbose: bool) -> io::Result<i32> {
    let mut out = Vec::new();
    let code = status_to(&mut out, verbose)?;
    crate::stdout::write_bytes(&out);
    Ok(code)
}

/// `status`, writing its report to `w` so a test can read it back. The
/// SIGPIPE-quiet stdout is the caller's (`status`); the stale-image warning
/// still goes to stderr.
fn status_to(w: &mut impl Write, verbose: bool) -> io::Result<i32> {
    let Some(c) = inspect()? else {
        writeln!(w, "not running")?;
        return Ok(2);
    };
    Ok(match c.state() {
        // `inspect` returned a container, so never `Absent`.
        State::Absent | State::Stopped(_) => {
            let State::Stopped(s) = c.state() else { unreachable!() };
            writeln!(w, "not running (container {s})")?;
            2
        }
        State::Running { health, started, image } => {
            let wa = c.mount(WORKAREA_MOUNT);
            let (word, code) = match health.as_str() {
                "healthy" => ("running, RISC iX booted", 0),
                "starting" => ("running, RISC iX booting", 1),
                "unhealthy" => ("running, RISC iX not answering", 1),
                _ => ("running", 0),
            };
            writeln!(w, "{word}")?;
            writeln!(w, "  container  {CONTAINER}, started {started}")?;
            writeln!(w, "  image      {image}")?;
            let set = |key: &str| c.value(key);
            let nfs = set(var::NFS).as_deref() == Some("1");
            // Only worth a `docker exec` when the mount is meant to be live,
            // or asked for.
            let live = nfs && wa.is_some();
            let daemons = (live || verbose).then(nfs_daemons).flatten();
            let server = live.then(|| daemons.as_ref().map(|d| d.iter().all(|(_, s)| s == "up"))).flatten();
            match &wa {
                Some(wa) => writeln!(w, "  work area  {}", wa.display())?,
                None => writeln!(w, "  work area  none")?,
            }
            if let Some(line) = sharing(nfs, wa.is_some(), server) {
                writeln!(w, "  sharing    {line}")?;
            }
            if verbose && live {
                let guest_wa = set(var::GUEST_WORKAREA).unwrap_or_else(|| WORKAREA_MOUNT.to_string());
                writeln!(w, "  in guest   {}", guest_mount(guest_mount_table().as_deref(), &guest_wa))?;
            }
            if verbose {
                writeln!(w, "  nfs        {}", nfs_line(daemons.as_deref()))?;
            }
            writeln!(w, "  workdir    {}", workdir_line(set(var::WORKDIR).as_deref(), wa.is_some()))?;
            for (label, dest) in [("state", STATE_MOUNT), ("assets", ASSETS_MOUNT)] {
                let source = c.mount(dest);
                writeln!(w, "  {label:<9}  {}", source.map_or("none".into(), |p| p.display().to_string()))?;
            }
            match docker_output(&["port", CONTAINER, "6080"]) {
                Ok(Some(p)) => writeln!(
                    w,
                    "  console    http://{}/vnc.html?autoconnect=1",
                    p.replace("0.0.0.0", "localhost")
                )?,
                _ => writeln!(w, "  console    none (started with --no-vnc)")?,
            }
            let profile = container_profile(&c);
            writeln!(w, "  machine    {} ({})", profile.name(), profile.describe())?;
            writeln!(w, "  x11        {}", x11_line(set(var::X11).as_deref() == Some("1")))?;
            writeln!(w, "  guest      {}", activity_line(running_count()))?;
            warn_if_stale(&image);
            code
        }
    })
}

/// The container's NFS daemons and their states (`up`, `down`,
/// `unregistered`, `not-answering`), as `dockerix agent nfs` finds them;
/// None if it could not say (an image from before it).
fn nfs_daemons() -> Option<Vec<(String, String)>> {
    let out = docker_output(&["exec", CONTAINER, "dockerix", "agent", "nfs"]).ok()??;
    let daemons: Vec<_> =
        out.lines().filter_map(|l| l.split_once(' ')).map(|(n, s)| (n.to_string(), s.to_string())).collect();
    (!daemons.is_empty()).then_some(daemons)
}

/// The NFS server, for `status -v`: up, or which daemons are not.
fn nfs_line(daemons: Option<&[(String, String)]>) -> String {
    let Some(daemons) = daemons else {
        return "could not ask the agent".to_string();
    };
    let down: Vec<String> = daemons
        .iter()
        .filter(|(_, s)| s != "up")
        .map(|(n, s)| format!("{n} {}", s.replace('-', " ")))
        .collect();
    if down.is_empty() {
        let names: Vec<&str> = daemons.iter().map(|(n, _)| n.as_str()).collect();
        format!("server up ({})", names.join(", "))
    } else {
        let them = if down.len() == 1 { "it" } else { "them" };
        format!("{}: the agent restarts {them}; transfers use the tty meanwhile", down.join(", "))
    }
}

/// How the work area reaches the guest, for `status`: the live NFS mount
/// when the container was started with `--nfs` and a work area (as
/// `config::nfs_enabled` decides inside it), else the push before each
/// command. `server` is whether the NFS daemons are all up, asked only when
/// the mount is meant to be live. No line without a work area, unless NFS was
/// asked for, since then the flag has silently done nothing.
fn sharing(nfs: bool, workarea: bool, server: Option<bool>) -> Option<&'static str> {
    match (nfs, workarea, server) {
        (true, true, Some(false)) => Some("live NFS mount, but the NFS server is down (status -v)"),
        (true, true, _) => Some("live NFS mount (--nfs)"),
        (true, false, _) => Some("none: --nfs needs a work area"),
        (false, true, _) => Some("pushed before each command"),
        (false, false, _) => None,
    }
}

/// Where commands and logins start, for `status`: the `start -w` directory,
/// else the guest's copy of the host cwd (`/workarea` for a login), else `/`
/// without a work area.
fn workdir_line(workdir: Option<&str>, workarea: bool) -> String {
    match (workdir, workarea) {
        (Some(dir), _) => format!("{dir} (start -w)"),
        (None, true) => format!("follows the host cwd ({WORKAREA_MOUNT} for a login)"),
        (None, false) => "/".to_string(),
    }
}

fn x11_line(on: bool) -> &'static str {
    if on { "on (--x11; `dockerix x11` on the host relays to XQuartz)" } else { "off" }
}

/// How many things dockerix is running in the guest, as the agent counts
/// them (`dockerix ps`); None if it could not say (still booting).
fn running_count() -> Option<usize> {
    let out = docker_output(&["exec", CONTAINER, "dockerix", "agent", "ps", "--count"]).ok()??;
    out.trim().parse().ok()
}

fn activity_line(count: Option<usize>) -> String {
    match count {
        None => "not answering yet".to_string(),
        Some(0) => "idle".to_string(),
        Some(1) => "1 thing running (dockerix ps)".to_string(),
        Some(n) => format!("{n} things running (dockerix ps)"),
    }
}

/// The guest's mount table, from a command on one of the agent's held
/// sessions (no push, run from `/`); None if the guest did not answer in
/// time.
fn guest_mount_table() -> Option<String> {
    let (no_push, from_root) = (format!("{}=0", var::SYNC), format!("{}=/", var::WORKDIR));
    let args =
        ["exec", "-e", &no_push, "-e", &from_root, CONTAINER, "dockerix", "agent", "exec", "--timeout", "30"];
    docker_output(&[&args[..], &["mount"]].concat()).ok()?
}

/// Whether the guest's mount table has the NFS share at `guest_wa`.
fn guest_mount(table: Option<&str>, guest_wa: &str) -> String {
    let Some(table) = table else {
        return "could not ask the guest".to_string();
    };
    let on = format!(" on {guest_wa} ");
    match table.lines().find(|l| l.contains(&on)) {
        Some(line) if line.contains("type nfs") => format!("NFS share mounted: {}", line.trim()),
        Some(line) => format!("NOT the NFS share at {guest_wa}: {}", line.trim()),
        None => format!(
            "NOT mounted at {guest_wa}: commands on held sessions see the guest disc's copy \
             (a new login mounts it)"
        ),
    }
}

/// `dockerix ps`: what dockerix is running in the guest, as the agent
/// lists it.
pub fn ps() -> io::Result<i32> {
    match inspect_state()? {
        State::Running { .. } => {}
        State::Absent | State::Stopped(_) => {
            return Err(io::Error::other(format!("{CONTAINER} is not running (dockerix start)")));
        }
    }
    let status =
        docker().args(["exec", CONTAINER, "dockerix", "agent", "ps"]).status().map_err(docker_missing)?;
    Ok(child_code(status, FAILURE))
}

/// `dockerix console`: open the noVNC console in the browser.
pub fn console() -> io::Result<i32> {
    let Some(port) = docker_output(&["port", CONTAINER, "6080"])? else {
        return Err(io::Error::other(match inspect_state() {
            Ok(State::Running { .. }) => format!(
                "{CONTAINER} was started with --no-vnc, so has no console \
                 (dockerix stop && dockerix start to get one)"
            ),
            _ => format!("{CONTAINER} is not running (dockerix start)"),
        }));
    };
    let host = port.replace("0.0.0.0", "localhost");
    let url = format!("http://{host}/vnc.html?autoconnect=1");
    let opener = if cfg!(target_os = "macos") { "open" } else { "xdg-open" };
    if !Command::new(opener).arg(&url).status().is_ok_and(|s| s.success()) {
        outln!("{url}");
    }
    Ok(0)
}

/// The machine profile the container was started with (`DOCKERIX_PROFILE`
/// in its environment; `fast` when unset or unreadable), from the snapshot.
fn container_profile(c: &Container) -> Profile {
    c.value(var::PROFILE).and_then(|v| Profile::from_name(&v)).unwrap_or(Profile::Fast)
}

/// A `-w` argument as a guest path: absolute as given, otherwise relative to
/// the guest work area. Same character rules as the cwd mapping, since it
/// goes into a `cd` on a 1990 /bin/sh.
fn guest_dir(spec: &str) -> io::Result<String> {
    let spec = spec.trim_end_matches('/');
    if spec.is_empty() {
        return Ok("/".to_string());
    }
    if spec.bytes().any(|b| !(b.is_ascii_alphanumeric() || b"/_.+-".contains(&b))) {
        return Err(io::Error::other(format!(
            "guest directory {spec:?} has characters the guest shell can't take; use plain names"
        )));
    }
    Ok(if spec.starts_with('/') {
        spec.to_string()
    } else {
        format!("{WORKAREA_MOUNT}/{}", spec.trim_start_matches("./"))
    })
}

/// `dockerix x11 [--port N] [--socket PATH]`: the host side of X11 forwarding.
/// Runs a foreground relay from a local TCP port to XQuartz's unix socket, so
/// guest X clients (which reach the container relay via SLiRP) display here.
/// The container must have been started with `--x11` — that starts the
/// matching relay inside it and sets the guest DISPLAY. Ctrl-C to stop.
pub fn x11(port: u16, socket: &str) -> io::Result<i32> {
    if !matches!(inspect_state()?, State::Running { .. }) {
        return Err(io::Error::other(format!(
            "{CONTAINER} is not running; start it with X11 on:  dockerix start --x11"
        )));
    }
    // The container relay and the guest DISPLAY only exist if the container was
    // started with --x11; forwarding to a container that has neither would
    // silently do nothing, so check and say so.
    match docker_output(&["exec", CONTAINER, "printenv", var::X11]) {
        Ok(Some(v)) if v == "1" => {}
        _ => eprintln!(
            "dockerix: warning: {CONTAINER} was not started with --x11, so it has no\n\
             relay and the guest DISPLAY is unset. Restart it:\n\
             \x20 dockerix restart --x11"
        ),
    }
    if !Path::new(socket).exists() {
        eprintln!("dockerix: warning: {socket} does not exist — is XQuartz running?");
    }
    let display = crate::config::x11_display();
    eprintln!("dockerix: X11 relay :{port} -> XQuartz ({socket})");
    eprintln!("  Allow local clients once, in an XQuartz terminal:  xhost +local:");
    eprintln!("  The guest already has DISPLAY={display}; try:  dockerix exec xclock");
    eprintln!("  Ctrl-C to stop.");
    relay::serve(&format!("0.0.0.0:{port}"), relay::Target::Unix(PathBuf::from(socket)))?;
    Ok(0)
}

/// `dockerix reset [--yes]`: discard the guest disc (and CMOS) so the next
/// start re-seeds them from the pristine assets. Refuses while running.
pub fn reset(yes: bool) -> io::Result<i32> {
    let snapshot = inspect()?;
    if let Some(c) = &snapshot
        && matches!(c.state(), State::Running { .. })
    {
        return Err(io::Error::other(format!("{CONTAINER} is running; `dockerix stop` first")));
    }
    // A directory named with --state; else the one a stopped container was
    // actually started with; else the one start would use.
    let state = match (&globals().state, snapshot.and_then(|c| c.mount(STATE_MOUNT))) {
        (Some(p), _) => p.clone(),
        (None, Some(p)) => p,
        _ => state_dir(),
    };
    let present = reset_targets(&state);
    if present.is_empty() {
        outln!("nothing to reset in {}", state.display());
        return Ok(0);
    }
    outln!("This discards the guest disc, CMOS and sync manifest in {}:", state.display());
    for p in &present {
        outln!("  {}", p.display());
    }
    outln!(
        "The next start re-seeds them from the assets' disc image and the image's\n\
         CMOS, patches the RISC iX network configuration into the disc copy and\n\
         RISC OS reconfigures itself from hostfs/!Boot — see docs/how-it-works.md."
    );
    if !yes {
        if !io::stdin().is_terminal() {
            return Err(io::Error::other("refusing to reset without --yes when not interactive"));
        }
        out!("Type 'reset' to continue: ");
        let mut line = String::new();
        if io::stdin().read_line(&mut line).is_err() || line.trim() != "reset" {
            eprintln!("dockerix: cancelled");
            return Ok(1);
        }
    }
    for p in &present {
        std::fs::remove_file(p).map_err(|e| io::Error::other(format!("remove {}: {e}", p.display())))?;
    }
    outln!("reset done");
    Ok(0)
}

/// The files under the state directory `state` that `dockerix reset`
/// removes, of those that exist: the guest disc, its CMOS, the container's
/// lock, and the sync manifest with its lock and any half-written copy.
fn reset_targets(state: &Path) -> Vec<PathBuf> {
    let guest = ["hdimage.hdf", "cmos.bin", ".lock"].map(|f| state.join(f));
    let sync = crate::sync::manifest_files(&state.join("sync/manifest"));
    guest.into_iter().chain(sync).filter(|p| p.exists()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::docker::fake::{self, Container};
    use crate::testsupport::{Env, tmp_dir};

    fn put(path: &Path) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, b"x").unwrap();
    }

    fn s(x: &str) -> String {
        x.to_string()
    }

    /// A running, booted container with a work area, state and assets, the
    /// live NFS mount on, and the agent answering.
    fn running() -> Container {
        Container {
            status: s("running"),
            health: s("healthy"),
            started: s("2026-10-15T06:00:00Z"),
            image: s("dockerix:testimage"),
            image_id: s("sha256:img"),
            env: vec![(s(var::NFS), s("1"))],
            image_env: vec![(s("PATH"), s("/usr/bin")), (s(var::NFS), s("0"))],
            mounts: vec![
                (s(WORKAREA_MOUNT), s("/home/me/wa")),
                (s(STATE_MOUNT), s("/s")),
                (s(ASSETS_MOUNT), s("/a")),
            ],
            port6080: Some(s("0.0.0.0:6080")),
            agent: vec![
                (s("nfs"), s("rpcbind up\nrpc.mountd up\nrpc.nfsd up\n")),
                (s("ps"), s("0")),
                (s("exec"), s("10.0.2.2:/workarea on /workarea type nfs (rw)\n")),
            ],
        }
    }

    /// The `docker` calls recorded, joined with spaces, oldest first.
    fn calls() -> Vec<String> {
        fake::calls().into_iter().map(|c| c.join(" ")).collect()
    }

    /// Clear the `START_ENV` variables so a test reads only what its fixture
    /// and settings put there, and point the image at the fixture's so the
    /// stale-image warning stays quiet.
    fn env() -> Env {
        let mut env = Env::lock();
        env.set(var::IMAGE, "dockerix:testimage");
        for k in [var::NFS, var::SET_CLOCK, var::CLOCK_INTERVAL, var::X11, var::SESSIONS] {
            env.unset(k);
        }
        env
    }

    #[test]
    fn status_without_a_container_says_not_running() {
        let _env = env();
        let _fake = fake::install(None);
        let mut out = Vec::new();
        assert_eq!(status_to(&mut out, false).unwrap(), 2);
        assert_eq!(String::from_utf8(out).unwrap(), "not running\n");

        let _fake = fake::install(Some(Container { status: s("exited"), ..Container::default() }));
        let mut out = Vec::new();
        assert_eq!(status_to(&mut out, false).unwrap(), 2);
        assert_eq!(String::from_utf8(out).unwrap(), "not running (container exited)\n");
    }

    #[test]
    fn status_running_reports_how_it_was_started() {
        let _env = env();
        let _fake = fake::install(Some(running()));
        let mut out = Vec::new();
        assert_eq!(status_to(&mut out, false).unwrap(), 0);
        let text = String::from_utf8(out).unwrap();
        assert!(text.starts_with("running, RISC iX booted\n"), "{text}");
        assert!(text.contains("container  dockerix-vm, started 2026-10-15T06:00:00Z"), "{text}");
        assert!(text.contains("image      dockerix:testimage"), "{text}");
        assert!(text.contains("work area  /home/me/wa"), "{text}");
        assert!(text.contains("sharing    live NFS mount (--nfs)"), "{text}");
        assert!(text.contains("state      /s"), "{text}");
        assert!(text.contains("assets     /a"), "{text}");
        assert!(text.contains("console    http://localhost:6080/vnc.html?autoconnect=1"), "{text}");
        assert!(text.contains("machine    fast"), "{text}");
        assert!(text.contains("x11        off"), "{text}");
        assert!(text.contains("guest      idle"), "{text}");
        // No `in guest` or `nfs` line without -v.
        assert!(!text.contains("in guest"), "{text}");
        assert!(!text.contains("\n  nfs "), "{text}");
    }

    #[test]
    fn status_verbose_asks_the_guest_about_the_mount() {
        let _env = env();
        let _fake = fake::install(Some(running()));
        let mut out = Vec::new();
        assert_eq!(status_to(&mut out, true).unwrap(), 0);
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("in guest   NFS share mounted:"), "{text}");
        assert!(text.contains("nfs        server up (rpcbind, rpc.mountd, rpc.nfsd)"), "{text}");
        // The verbose lines come from a `docker exec … agent exec … mount`.
        assert!(calls().iter().any(|c| c.contains("agent exec") && c.ends_with("mount")), "{:?}", calls());
    }

    #[test]
    fn start_on_a_running_container_warns_and_does_not_run() {
        let _env = env();
        let _fake = fake::install(Some(running()));
        // A setting given while it is up cannot take effect: warn, don't recreate.
        let settings = AgentSettings { nfs: Some(true), ..AgentSettings::default() };
        assert_eq!(start(None, None, true, None, &settings).unwrap(), 0);
        assert!(!calls().iter().any(|c| c.starts_with("run ")), "{:?}", calls());
    }

    #[test]
    fn restart_recreates_with_the_old_options_and_the_given_one() {
        let mut env = env();
        // Started with the live mount on, VNC off, an r140 machine and a -w.
        env.unset(var::WORKDIR);
        let container = Container {
            env: vec![
                (s(var::NFS), s("1")),
                (s(var::X11), s("0")),
                (s(var::SET_CLOCK), s("1")),
                (s(var::VNC), s("0")),
                (s(var::PROFILE), s("r140")),
                (s(var::WORKDIR), s("/workarea/game")),
            ],
            image_env: vec![
                (s("PATH"), s("/usr/bin")),
                (s(var::NFS), s("0")),
                (s(var::X11), s("0")),
                (s(var::SET_CLOCK), s("1")),
            ],
            ..running()
        };
        let _fake = fake::install(Some(container));
        // --x11 given now: it replaces the old X11=0, the rest is kept.
        let settings = AgentSettings { x11: Some(true), ..AgentSettings::default() };
        assert_eq!(restart(&settings).unwrap(), 0);
        let calls = calls();
        let run = calls.iter().find(|c| c.starts_with("run ")).unwrap_or_else(|| panic!("{calls:?}"));
        assert!(calls.contains(&s("stop dockerix-vm")), "{calls:?}");
        assert!(calls.contains(&s("rm dockerix-vm")), "{calls:?}");
        // Recreated from the same mounts, machine, -w and --no-vnc.
        assert!(run.contains("-v /s:/state"), "{run}");
        assert!(run.contains("-v /a:/assets:ro"), "{run}");
        assert!(run.contains("-v /home/me/wa:/workarea"), "{run}");
        assert!(run.contains("-e DOCKERIX_VNC=0"), "{run}");
        assert!(run.contains("-e DOCKERIX_PROFILE=r140"), "{run}");
        assert!(run.contains("-e DOCKERIX_WORKDIR=/workarea/game"), "{run}");
        // The old START_ENV setting kept, the given one in place of its default.
        assert!(run.contains("-e DOCKERIX_NFS=1"), "{run}");
        assert!(run.contains("-e DOCKERIX_X11=1"), "{run}");
        // A named image is not pulled, and it ends the line.
        assert!(!run.contains("--pull"), "{run}");
        assert!(run.ends_with("dockerix:testimage"), "{run}");
    }

    #[test]
    fn stop_matches_what_is_there() {
        let _env = env();
        // Absent: nothing to stop or remove.
        let _fake = fake::install(None);
        assert_eq!(stop().unwrap(), 0);
        assert!(!calls().iter().any(|c| c.starts_with("stop ") || c.starts_with("rm ")), "{:?}", calls());

        // Stopped: remove it, but no `docker stop` (it is not running).
        let _fake = fake::install(Some(Container { status: s("exited"), ..Container::default() }));
        assert_eq!(stop().unwrap(), 0);
        assert!(!calls().iter().any(|c| c.starts_with("stop ")), "{:?}", calls());
        assert!(calls().contains(&s("rm dockerix-vm")), "{:?}", calls());

        // Running: halt it, then remove it.
        let _fake = fake::install(Some(running()));
        assert_eq!(stop().unwrap(), 0);
        assert!(calls().contains(&s("stop dockerix-vm")), "{:?}", calls());
        assert!(calls().contains(&s("rm dockerix-vm")), "{:?}", calls());
    }

    #[test]
    fn reset_refuses_while_running() {
        let _env = env();
        let _fake = fake::install(Some(running()));
        let err = reset(true).unwrap_err().to_string();
        assert!(err.contains("is running"), "{err}");
    }

    #[test]
    fn guest_dir_maps_relative_paths_into_the_work_area() {
        assert_eq!(guest_dir("").unwrap(), "/");
        assert_eq!(guest_dir("/").unwrap(), "/");
        assert_eq!(guest_dir("/usr/src/").unwrap(), "/usr/src");
        assert_eq!(guest_dir("games").unwrap(), format!("{WORKAREA_MOUNT}/games"));
        assert_eq!(guest_dir("./games/rogue-1.0").unwrap(), format!("{WORKAREA_MOUNT}/games/rogue-1.0"));
        for bad in ["a b", "$HOME", "x;rm", "it's", "caf\u{e9}"] {
            assert!(guest_dir(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn sharing_says_how_the_work_area_reaches_the_guest() {
        assert_eq!(sharing(true, true, Some(true)), Some("live NFS mount (--nfs)"));
        assert_eq!(sharing(true, true, None), Some("live NFS mount (--nfs)"));
        assert_eq!(
            sharing(true, true, Some(false)),
            Some("live NFS mount, but the NFS server is down (status -v)")
        );
        assert_eq!(sharing(true, false, None), Some("none: --nfs needs a work area"));
        assert_eq!(sharing(false, true, None), Some("pushed before each command"));
        assert_eq!(sharing(false, false, None), None);
    }

    #[test]
    fn nfs_line_names_the_daemons_that_are_not_up() {
        let daemons = |states: [&str; 3]| -> Vec<(String, String)> {
            let names = ["rpcbind", "rpc.mountd", "rpc.nfsd"];
            names.into_iter().zip(states).map(|(n, s)| (n.into(), s.into())).collect()
        };
        assert_eq!(nfs_line(Some(&daemons(["up"; 3]))), "server up (rpcbind, rpc.mountd, rpc.nfsd)");
        assert_eq!(
            nfs_line(Some(&daemons(["up", "unregistered", "not-answering"]))),
            "rpc.mountd unregistered, rpc.nfsd not answering: the agent restarts them; transfers use the tty meanwhile"
        );
        let one = nfs_line(Some(&daemons(["up", "up", "down"])));
        assert!(one.starts_with("rpc.nfsd down: the agent restarts it;"), "{one}");
        assert_eq!(nfs_line(None), "could not ask the agent");
    }

    #[test]
    fn child_code_reports_a_signal_as_a_shell_does() {
        use std::os::unix::process::ExitStatusExt;
        use std::process::ExitStatus;
        // Raw wait statuses: an exit code sits in the second byte, a
        // terminating signal in the first.
        assert_eq!(child_code(ExitStatus::from_raw(3 << 8), FAILURE), 3);
        assert_eq!(child_code(ExitStatus::from_raw(0), FAILURE), 0);
        assert_eq!(child_code(ExitStatus::from_raw(13), FAILURE), crate::stdout::BROKEN_PIPE_EXIT);
        assert_eq!(child_code(ExitStatus::from_raw(9), FAILURE), 137);
    }

    #[test]
    fn status_lines_read_plainly() {
        assert_eq!(workdir_line(Some("/usr/src"), true), "/usr/src (start -w)");
        assert_eq!(workdir_line(None, true), "follows the host cwd (/workarea for a login)");
        assert_eq!(workdir_line(None, false), "/");
        assert_eq!(x11_line(false), "off");
        assert!(x11_line(true).starts_with("on"));
        assert_eq!(activity_line(None), "not answering yet");
        assert_eq!(activity_line(Some(0)), "idle");
        assert_eq!(activity_line(Some(1)), "1 thing running (dockerix ps)");
        assert_eq!(activity_line(Some(3)), "3 things running (dockerix ps)");
    }

    #[test]
    fn guest_mount_finds_the_share_in_the_mount_table() {
        let mounted = "/dev/sd0a on / type 4.3 (rw)\n\
                       10.0.2.2:/workarea on /workarea type nfs (intr,rsize=8192,wsize=8192,rw)\n";
        assert_eq!(
            guest_mount(Some(mounted), "/workarea"),
            "NFS share mounted: 10.0.2.2:/workarea on /workarea type nfs (intr,rsize=8192,wsize=8192,rw)"
        );
        assert!(guest_mount(Some("/dev/sd0a on / type 4.3 (rw)\n"), "/workarea").starts_with("NOT mounted"));
        let disc = "/dev/sd0a on / type 4.3 (rw)\n/dev/sd0d on /workarea type 4.3 (rw)\n";
        assert!(guest_mount(Some(disc), "/workarea").starts_with("NOT the NFS share"));
        // A path that only starts the same is not the work area.
        let other = "10.0.2.2:/workarea on /workarea2 type nfs (rw)\n";
        assert!(guest_mount(Some(other), "/workarea").starts_with("NOT mounted"));
        assert_eq!(guest_mount(None, "/workarea"), "could not ask the guest");
    }

    #[test]
    fn check_assets_wants_roms_and_a_disc_to_seed() {
        let tmp = tmp_dir("assets");
        let dir = tmp.path();
        let (assets, state) = (dir.join("assets"), dir.join("state"));
        let err = |a: &Path, s: &Path| check_assets(a, s).unwrap_err().to_string();
        assert!(err(&assets, &state).contains("no assets directory"));
        // Neither the upstream placeholder nor a Finder file counts as a ROM.
        put(&assets.join("roms/riscos311/roms.txt"));
        put(&assets.join("roms/riscos311/.DS_Store"));
        assert!(err(&assets, &state).contains("no RISC OS 3.11 ROM"));
        put(&assets.join("roms/riscos311/IC24.ROM"));
        assert!(err(&assets, &state).contains("no RISC iX disc image"));
        // A seeded state directory no longer needs the pristine disc.
        put(&state.join("hdimage.hdf"));
        check_assets(&assets, &state).unwrap();
        std::fs::remove_file(state.join("hdimage.hdf")).unwrap();
        put(&assets.join("images/hdimage.hdf"));
        check_assets(&assets, &state).unwrap();
    }

    #[test]
    fn reset_removes_the_manifest_with_its_lock_and_temp_file() {
        let tmp = tmp_dir("reset");
        let state = tmp.path();
        for f in [
            "hdimage.hdf",
            "cmos.bin",
            "sync/manifest",
            "sync/manifest.lock",
            "sync/manifest.tmp",
            "assets.txt",
        ] {
            put(&state.join(f));
        }
        let mut got = reset_targets(state);
        got.sort();
        let mut want: Vec<PathBuf> =
            ["cmos.bin", "hdimage.hdf", "sync/manifest", "sync/manifest.lock", "sync/manifest.tmp"]
                .iter()
                .map(|f| state.join(f))
                .collect();
        want.sort();
        assert_eq!(got, want);
        // A lock or temp file alone still counts: reset is not a no-op then.
        let tmp = tmp_dir("reset-lock-only");
        let state = tmp.path();
        put(&state.join("sync/manifest.lock"));
        assert_eq!(reset_targets(state), [state.join("sync/manifest.lock")]);
    }
}
