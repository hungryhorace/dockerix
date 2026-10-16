//! Container-side agent subcommands: entrypoint, healthcheck wait, and halt.

use std::fs::TryLockError;
use std::io::{self, Write};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock};
use std::thread;
use std::time::{Duration, Instant};

use rustix::process::{Pid, Signal, kill_process};
use signal_hook::consts::{SIGINT, SIGTERM};

use crate::config::{self, Config};
use crate::session::{self, Session, Timing};

/// Set by SIGTERM or SIGINT (`install_signal_handlers`).
static STOP: LazyLock<Arc<AtomicBool>> = LazyLock::new(Default::default);

/// Healthcheck / boot-gate: succeed once the guest login banner is up, or fail
/// after `timeout_secs`. Exits 0 (up) or 1 (not yet).
pub fn wait(cfg: &Config, timeout_secs: u64) -> i32 {
    let deadline = Instant::now() + Duration::from_secs(timeout_secs);
    loop {
        if let Ok(true) = session::probe_banner(&cfg.telnet_addr, Duration::from_secs(3)) {
            return 0;
        }
        if Instant::now() >= deadline {
            return 1;
        }
        thread::sleep(Duration::from_secs(2));
    }
}

/// Cleanly shut the guest down over the exec channel. If the guest is
/// already gone (the port refuses, which a closed port reports at once),
/// that's success. Any other login failure (a login that keeps stalling, a
/// wrong password) is an error saying the disc may be unsynced; the callers
/// report it and stop the emulator regardless.
pub fn halt(cfg: &Config) -> io::Result<()> {
    halt_with(cfg, Timing::GUEST)
}

/// `halt` with the login's waits in `timing`.
fn halt_with(cfg: &Config, timing: Timing) -> io::Result<()> {
    let mut session = match Session::connect_and_login_with(
        &cfg.telnet_addr,
        false,
        80,
        24,
        &cfg.user,
        &cfg.pass,
        timing,
    ) {
        Ok(s) => s,
        Err(e) if e.kind() == io::ErrorKind::ConnectionRefused => return Ok(()),
        Err(e) => {
            return Err(io::Error::new(
                e.kind(),
                format!("could not log in ({e}); the disc may be left unsynced"),
            ));
        }
    };
    let deadline = Instant::now() + Duration::from_secs(30);
    // `halt` syncs and stops the machine; the connection drops when it does.
    session.send_and_wait_eof("sync; sync; halt", deadline)?;
    Ok(())
}

/// The container entrypoint: seed state, bring up the virtual display and
/// (unless `DOCKERIX_VNC=0`) noVNC, and launch Arculator. With no trailing command it runs the guest
/// persistently (until Arculator exits or SIGTERM); with a command it boots,
/// runs the command once, halts, and exits with the command's status — the
/// `docker run --rm dockerix <cmd>` one-shot path. Replaces the M1 shell
/// entrypoint.
pub fn entrypoint(cfg: &Config, command: Vec<String>) -> io::Result<i32> {
    std::env::set_current_dir("/opt/arculator")?;
    let config = cfg.config_name.clone();

    // Arculator opens the disc image read-write with no locking, so two
    // containers on the same /state would corrupt the ffs. Hold an exclusive
    // lock for the process lifetime; the fd (and thus the lock) is released
    // automatically even on `docker kill`.
    let _state_lock = lock_state()?;

    seed_state(&config)?;
    install_roms()?;
    install_signal_handlers()?;

    let mut arc = start_emulator(cfg, &config)?;

    if command.is_empty() { supervise(cfg, &mut arc) } else { one_shot(cfg, &mut arc, command) }
}

/// Bring up the display stack and launch Arculator; returns its child handle.
fn start_emulator(cfg: &Config, config: &str) -> io::Result<Child> {
    let display = cfg.display.clone();
    let screen = cfg.screen.clone();

    // A `docker restart` keeps the container's /tmp, so a stale X lock/socket
    // from the previous Xvfb makes the new one refuse to start ("Server is
    // already active for display"), taking Arculator down with it. Nothing
    // else runs in this container, so the leftovers are always ours.
    let num = display.trim_start_matches(':');
    let sock = format!("/tmp/.X11-unix/X{num}");
    let _ = std::fs::remove_file(format!("/tmp/.X{num}-lock"));
    let _ = std::fs::remove_file(&sock);

    let _xvfb = spawn("Xvfb", &[&display, "-screen", "0", &screen, "-nolisten", "tcp"])?;
    wait_for_path(&sock, Duration::from_secs(15));

    // The console (DOCKERIX_VNC=0 leaves it out; Arculator still needs Xvfb).
    // Quiet: in one-shot mode our stdout is the guest command's stdout, and
    // x11vnc prints "PORT=5900" there.
    if crate::config::vnc_enabled() {
        let _vnc = spawn_quiet(
            "x11vnc",
            &["-display", &display, "-forever", "-shared", "-nopw", "-quiet", "-bg", "-rfbport", "5900"],
        )?;
        let _ws = spawn_quiet("websockify", &["--web", "/usr/share/novnc", "6080", "localhost:5900"])?;
    }

    // The NFSv2 server: always, for the transfers' staging directory (the
    // push, `put` and `get` move their archives that way, far faster than
    // through the tty), and in NFS mode (`--nfs`) for the live work-area
    // mount too.
    // Started before the guest can reach it; idle until the guest mounts.
    // A thread keeps the daemons running (see nfs_server.rs).
    let nfs = crate::config::nfs_enabled();
    if cfg.nfs_requested && !nfs {
        eprintln!("NFS server: no work-area mount (no work area mounted)");
    }
    if let Err(e) = crate::nfs_server::start(nfs) {
        eprintln!("NFS server: not started ({e}); pushes go down the tty, and an NFS work area will fail");
    }

    // RFC 868 time server, so the guest's clock can be corrected from ours
    // after boot (see set_guest_clock and timeserver.rs). Harmless if unused.
    if cfg.set_clock {
        crate::timeserver::start();
    }

    // Optional X11 forwarding (DOCKERIX_X11=1). Guest X clients connect to the
    // SLiRP gateway (10.0.2.2:6000 = here); relay that on to the host's XQuartz
    // via host.docker.internal, where `dockerix x11` bridges to its socket.
    // The guest login exports DISPLAY (see session.rs). Idle until a client
    // connects.
    if crate::config::x11_enabled() {
        crate::relay::spawn(
            "0.0.0.0:6000",
            crate::relay::Target::Tcp("host.docker.internal:6000".to_string()),
        );
    }

    // Time the cold boot in the background (works for hand or auto boot).
    boot_timer(cfg.clone());

    let cfg_file = format!("configs/{config}.cfg");
    if Path::new(&cfg_file).exists() {
        // The machine profile (DOCKERIX_PROFILE): CPU, MEMC and RAM.
        let profile = crate::config::profile()?;
        let text = std::fs::read_to_string(&cfg_file)?;
        std::fs::write(&cfg_file, crate::config::apply_profile(&text, profile))?;
        eprintln!("Machine profile: {} ({})", profile.name(), profile.describe());
        spawn("./arculator", &[config])
    } else {
        eprintln!("No {cfg_file}; starting Arculator with its config selector");
        spawn("./arculator", &[])
    }
}

/// Persistent mode: run until Arculator exits; on SIGTERM, halt the guest
/// cleanly first so the filesystem is left consistent.
fn supervise(cfg: &Config, arc: &mut Child) -> io::Result<i32> {
    // Serve mode: hold logged-in guest sessions for `rix` (see serve.rs).
    // Listening before the guest has booted is fine: a request then fails at
    // login and says so.
    let server = match crate::serve::start(cfg) {
        Ok(s) => Some(s),
        Err(e) => {
            eprintln!("exec socket: not started ({e}); rix will log in per command");
            None
        }
    };
    // Once the guest is up, set its clock from ours and keep it synced so it
    // recovers after the emulator is frozen (host sleep). See clock_keeper.
    if cfg.set_clock {
        let cfg = cfg.clone();
        thread::spawn(move || clock_keeper(cfg));
    }
    loop {
        if let Some(status) = arc.try_wait()? {
            return Ok(exit_code_of(&status));
        }
        if STOP.swap(false, Ordering::SeqCst) {
            eprintln!("SIGTERM: halting RISC iX before stopping Arculator");
            if let Some(s) = &server {
                s.drain();
            }
            if let Err(e) = halt(cfg) {
                eprintln!("halt: {e}; stopping Arculator anyway");
            }
            return stop_emulator(arc);
        }
        thread::sleep(Duration::from_millis(200));
    }
}

/// One-shot mode: wait for the guest to boot, run the command, halt, and
/// return the command's exit status.
fn one_shot(cfg: &Config, arc: &mut Child, command: Vec<String>) -> io::Result<i32> {
    // Boot can take a while; the guest must reach a login banner hands-off,
    // which needs the headless auto-boot to be configured.
    if wait(cfg, cfg.boot_timeout) != 0 {
        let _ = stop_emulator(arc);
        return Err(io::Error::new(io::ErrorKind::TimedOut, "guest did not reach a login banner in time"));
    }

    // Correct the guest clock before the command runs, so any files it writes
    // are dated now rather than 1992. (One-shot is short-lived, so no periodic
    // re-sync is needed here.)
    set_guest_clock(cfg, true);

    let opts = crate::exec::ExecOpts { tty: None, command, timeout_secs: config::exec_timeout() };
    let code = crate::exec::run(cfg, opts)?;

    if let Err(e) = halt(cfg) {
        eprintln!("halt: {e}");
    }
    let _ = stop_emulator(arc);
    Ok(code)
}

fn boot_timer(cfg: Config) {
    thread::spawn(move || {
        let start = Instant::now();
        loop {
            if let Ok(true) = session::probe_banner(&cfg.telnet_addr, Duration::from_secs(3)) {
                eprintln!("cold boot: RISC iX login banner up after {}s", start.elapsed().as_secs());
                return;
            }
            thread::sleep(Duration::from_secs(2));
        }
    });
}

/// Take an exclusive lock on /state so only one container drives the disc.
fn lock_state() -> io::Result<std::fs::File> {
    std::fs::create_dir_all("/state")?;
    let f = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open("/state/.lock")?;
    match f.try_lock() {
        Ok(()) => Ok(f),
        Err(TryLockError::Error(e)) => Err(e),
        // Seen on Docker Desktop: a container SIGKILLed at the stop grace
        // deadline left its flock held inside the VM's file server, so the
        // next start failed here with nothing else running.
        Err(TryLockError::WouldBlock) => Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            "/state is already in use by another container (exclusive lock on \
             /state/.lock). If no other dockerix container is running, the \
             lock is stale: delete .lock in the state directory and retry",
        )),
    }
}

/// The user-supplied ROMs and pristine disc image, bind-mounted read-only.
const ASSETS: &str = "/assets";

/// Copy the user-supplied ROMs from /assets/roms over Arculator's `roms/`
/// (which it resolves relative to its binary), alongside the upstream tree's
/// `roms.txt` placeholders and podule ROMs.
fn install_roms() -> io::Result<()> {
    let src = Path::new(ASSETS).join("roms");
    if !src.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!(
                "{} not found: the RISC OS ROMs are user-supplied. Mount the assets \
                 directory at {ASSETS} (dockerix start mounts DOCKERIX_ASSETS, default \
                 ~/.dockerix/assets; see README.md, \"What you need\")",
                src.display()
            ),
        ));
    }
    copy_tree(&src, Path::new("roms"))
}

/// Recursive copy, overwriting. Dotfiles (a Mac's .DS_Store) are skipped:
/// Arculator loads every file in a ROM directory, in name order.
fn copy_tree(src: &Path, dst: &Path) -> io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        if entry.file_name().to_string_lossy().starts_with('.') {
            continue;
        }
        let (from, to) = (entry.path(), dst.join(entry.file_name()));
        if from.is_dir() {
            copy_tree(&from, &to)?;
        } else {
            std::fs::copy(&from, &to)?;
        }
    }
    Ok(())
}

fn seed_state(config: &str) -> io::Result<()> {
    std::fs::create_dir_all("/state")?;
    if !Path::new("/state/hdimage.hdf").exists() {
        eprintln!("Seeding /state/hdimage.hdf from pristine image");
        let pristine = Path::new(ASSETS).join("images/hdimage.hdf");
        if !pristine.exists() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!(
                    "{} not found: the RISC iX disc image is user-supplied. Mount the \
                     assets directory at {ASSETS} (dockerix start mounts DOCKERIX_ASSETS, \
                     default ~/.dockerix/assets; see README.md, \"What you need\")",
                    pristine.display()
                ),
            ));
        }
        std::fs::copy(pristine, "/state/hdimage.hdf")?;
        match crate::discpatch::apply(Path::new("/state/hdimage.hdf"))? {
            crate::discpatch::Outcome::Patched => {
                eprintln!("Applied the RISC iX network configuration to the disc copy")
            }
            crate::discpatch::Outcome::AlreadyPatched => {
                eprintln!("Disc image already carries the RISC iX network configuration")
            }
            crate::discpatch::Outcome::NotPristine(what) => eprintln!(
                "Disc image is not the known pristine one ({what} not found); \
                 left as is. Configure the RISC iX network by hand: docs/how-it-works.md, \
                 \"A fresh disc image\""
            ),
        }
    }
    // Arculator names the CMOS after the config and ROM family, opens it
    // read-write, and persists the emulated RTC into it. That is a problem for
    // the clock-sync: RISC iX writes the hardware clock whenever the time is
    // set (settimeofday calls resettodr), and the year round-trips through the
    // PCF8583's two-bit year with a +2 error, so a persisted CMOS would boot
    // two years further ahead every time the clock is corrected (seen as 2028
    // dates on the boot-touched dirs). So keep a pristine copy of the committed
    // CMOS and reset the working copy from it on every boot. The committed
    // CMOS is already fully configured and boots hands-off (see the HostFS-boot
    // finding), so this is a clean boot each time; the running clock is then
    // corrected by rdate without the RTC corruption accumulating. (The disc
    // image in /state still persists — only the CMOS is reset.)
    let cmos = format!("cmos/{config}.riscos3_old.cmos.bin");
    let cmos_path = Path::new(&cmos);
    let pristine = format!("{cmos}.pristine");
    let pristine_path = Path::new(&pristine);
    let is_symlink = cmos_path.symlink_metadata().map(|m| m.file_type().is_symlink()).unwrap_or(false);
    // Preserve the committed CMOS the first time we see it (a real file, before
    // it is replaced with the symlink). On a fresh container it is the image's
    // copy; the sibling survives a `docker restart` in the container layer.
    if !pristine_path.exists() && cmos_path.exists() && !is_symlink {
        std::fs::copy(cmos_path, pristine_path)?;
    }
    // Reset the working CMOS from pristine each boot (fall back to seeding from
    // the committed file if the pristine copy is somehow missing).
    if pristine_path.exists() {
        std::fs::copy(pristine_path, "/state/cmos.bin")?;
    } else if !Path::new("/state/cmos.bin").exists() && cmos_path.exists() && !is_symlink {
        std::fs::copy(cmos_path, "/state/cmos.bin")?;
    }
    // Point Arculator's CMOS filename at the persistent copy.
    if cmos_path.exists() || is_symlink {
        let _ = std::fs::remove_file(cmos_path);
    }
    std::os::unix::fs::symlink("/state/cmos.bin", cmos_path)?;
    Ok(())
}

fn install_signal_handlers() -> io::Result<()> {
    for sig in [SIGTERM, SIGINT] {
        signal_hook::flag::register(sig, Arc::clone(&STOP))?;
    }
    Ok(())
}

/// Stop Arculator and return its exit code. SIGTERM (SDL turns it into
/// SDL_QUIT) makes Arculator stop the emulation thread and close the disc
/// image — that's the flush we need — but it then opens its config-selection
/// dialog and waits for a click that never comes. So after a short grace
/// period SIGKILL it; by then the disc file is already closed.
fn stop_emulator(child: &mut Child) -> io::Result<i32> {
    let _ = kill_process(Pid::from_child(child), Signal::TERM);
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if let Some(status) = child.try_wait()? {
            return Ok(exit_code_of(&status));
        }
        thread::sleep(Duration::from_millis(200));
    }
    eprintln!("Arculator still running after SIGTERM (config dialog); killing it");
    let _ = child.kill(); // SIGKILL
    let status = child.wait()?;
    Ok(exit_code_of(&status))
}

/// Map a child's exit status to a shell-style code. A signal death (e.g.
/// Arculator segfaulting at startup) used to be reported as 0, which made a
/// crashed emulator look like a clean exit; report it as 128+signal and say so.
fn exit_code_of(status: &std::process::ExitStatus) -> i32 {
    use std::os::unix::process::ExitStatusExt;
    match (status.code(), status.signal()) {
        (Some(c), _) => c,
        (None, Some(sig)) => {
            eprintln!("Arculator died on signal {sig}");
            128 + sig
        }
        (None, None) => 1,
    }
}

fn spawn(cmd: &str, args: &[&str]) -> io::Result<Child> {
    Command::new(cmd).args(args).spawn()
}

/// Set the guest clock from the container's by running its `rdate` against our
/// RFC 868 server (see timeserver.rs). RISC iX boots as 1992 and its `date(1)`
/// cannot reach a post-1999 year; rdate can. Best effort: a failure leaves the
/// clock as it was. The server address is the SLiRP gateway
/// (`DOCKERIX_TIME_SERVER`, default 10.0.2.2).
///
/// `announce` logs the routine success and a login failure; the periodic
/// caller passes false so a re-sync (and a login that fails because the guest
/// is mid-reboot) stays quiet. Unexpected rdate errors are always logged.
fn set_guest_clock(cfg: &Config, announce: bool) {
    if !cfg.set_clock {
        return;
    }
    let server = &cfg.time_server;
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut session = match Session::connect_and_login(&cfg.telnet_addr, false, 80, 24, &cfg.user, &cfg.pass)
    {
        Ok(s) => s,
        Err(e) => {
            if announce {
                eprintln!("clock set: guest login failed ({e}); leaving the clock as is");
            }
            return;
        }
    };
    match session.run(&format!("rdate {server}"), deadline) {
        Ok((_out, 0)) => {
            if announce {
                eprintln!("guest clock set from container (rdate {server})");
            }
        }
        Ok((out, code)) => {
            eprintln!("clock set: rdate exited {code}: {}", String::from_utf8_lossy(&out).trim())
        }
        Err(e) => eprintln!("clock set: rdate failed ({e})"),
    }
    session.logout();
}

/// Background loop: set the guest clock once the guest is up, then keep
/// re-syncing every `DOCKERIX_CLOCK_INTERVAL` seconds (default 300). The
/// re-sync matters because the guest clock free-runs on emulated timer ticks
/// and stops advancing whenever the emulator is frozen — a host sleep leaves
/// it behind by the sleep's length until the next sync. The periodic login
/// gates on guest availability, so this also re-corrects the clock a few
/// minutes after a guest reboot.
fn clock_keeper(cfg: Config) {
    let interval = Duration::from_secs(cfg.clock_interval);
    loop {
        if let Ok(true) = session::probe_banner(&cfg.telnet_addr, Duration::from_secs(3)) {
            break;
        }
        thread::sleep(Duration::from_secs(2));
    }
    set_guest_clock(&cfg, true);
    loop {
        thread::sleep(interval);
        set_guest_clock(&cfg, false);
    }
}

fn spawn_quiet(cmd: &str, args: &[&str]) -> io::Result<Child> {
    Command::new(cmd).args(args).stdout(Stdio::null()).stderr(Stdio::null()).spawn()
}

fn wait_for_path(path: &str, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    while !Path::new(path).exists() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(100));
    }
}

// Small helper so callers can flush before exit if needed.
pub fn flush_stdout() {
    let _ = io::stdout().flush();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake_guest::FakeGuest;

    #[test]
    fn halt_logs_in_once_and_halts() {
        let guest = FakeGuest::start();
        halt(&guest.config()).unwrap();
        let st = guest.state();
        assert_eq!((st.connections, st.logins, st.halts), (1, 1, 1));
        assert_eq!(st.commands[1..], ["sync", "sync", "halt"]);
    }

    #[test]
    fn halt_with_nothing_listening_is_done() {
        // A port that was just free.
        let addr = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().to_string();
        let cfg = Config { telnet_addr: addr, user: "root".into(), pass: "x".into(), ..Config::default() };
        let started = Instant::now();
        halt(&cfg).unwrap();
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn halt_says_when_it_could_not_log_in() {
        // The guest is up but never prompts: the halt cannot sync the disc,
        // and must say so rather than pass for a guest already gone.
        let guest = FakeGuest::start();
        guest.state().stall = 10;
        let e = halt_with(&guest.config(), Timing::TEST).unwrap_err();
        assert!(e.to_string().starts_with("could not log in ("), "{e}");
        assert!(e.to_string().ends_with("); the disc may be left unsynced"), "{e}");
        let st = guest.state();
        assert_eq!((st.connections, st.logins, st.halts), (4, 0, 0));
    }
}
