//! The in-container NFSv2 server, and keeping it running.
//!
//! `rpcbind` (the portmapper), `rpc.mountd` and `rpc.nfsd` serve the
//! transfers' staging directory always (the push, `put` and `get` move their
//! archives through it, far faster than through the tty) and, with `--nfs`,
//! the live work-area mount. When one dies, transfers fall back to the tty,
//! 16 to 40 times slower, and commands in an NFS work area hang (that mount
//! is hard). So a keeper thread owns the two daemons: it restarts one that
//! exits, after a backoff that doubles while it keeps dying, and checks
//! rpcbind's registrations, restarting rpcbind (and then both daemons, so
//! they register again) when it stops answering, and a daemon whose
//! registration has gone. The guest's mounts survive a restart: its file
//! handles name files, not server state (patches/nfs-user-server, 0003).
//!
//! `check` is `dockerix agent nfs`: how each daemon is, asked afresh, for
//! `dockerix status`.

use std::collections::HashSet;
use std::io;
use std::net::SocketAddr;
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use rustix::process::{Pid, Signal, kill_process};

mod rpc;

use rpc::{IPPROTO_UDP, MOUNT_PROG, NFS_PROG, call, mappings, registrations};

/// The portmapper's address in the container.
const PORTMAPPER: &str = "127.0.0.1:111";

/// Bring up the NFSv2 server: the transfers' staging directory
/// (crate::sync::stage_dir, created empty here; transfers stage only when it
/// exists, and fall back to the tty if the guest then cannot mount it) and,
/// with `workarea` (`--nfs`), the live work-area mount. rpcbind must answer
/// first, so that the daemons can register; the keeper thread then starts
/// them and keeps them running for the container's life. Harmless when idle:
/// the guest only talks to it once it mounts, which the exec path drives.
/// Called before the emulator starts, so it clears the work area of what
/// the guest's client left there in its last life (`remove_silly_renames`).
pub fn start(workarea: bool) -> io::Result<()> {
    if workarea {
        remove_silly_renames(Path::new(&crate::exec::container_workarea()));
    }
    let stage = crate::sync::stage_dir();
    // Empty: an archive left by a push the container's last life cut short.
    let _ = std::fs::remove_dir_all(&stage);
    std::fs::create_dir_all(&stage)?;
    std::fs::write("/etc/exports", exports(&stage, workarea))?;
    start_rpcbind()?;
    let mut keeper = Keeper::new(daemons(), Policy::REAL);
    thread::spawn(move || keeper.run());
    Ok(())
}

/// Whether `name` is one RISC iX's NFS client gives a file it still holds
/// when the file is removed, renaming it rather than removing it ("silly
/// rename"): `.nfs` and four hex digits, upper case (a counter, least
/// significant digit first: `.nfs0F43`, `.nfs9F43`, `.nfsAF43`, as
/// seen). The client removes it when it lets go of the file, which
/// can take hours. Exactly that, so that a file of the user's own, such as
/// `.nfsrc`, is never taken for one.
pub fn silly_renamed(name: &str) -> bool {
    name.strip_prefix(".nfs").is_some_and(|tail| {
        tail.len() == 4 && tail.bytes().all(|b| b.is_ascii_digit() || (b'A'..=b'F').contains(&b))
    })
}

/// Remove what RISC iX's client left renamed (`silly_renamed`) under the
/// work area `root`, before the guest boots: no guest holds a file then, so
/// any left are orphans that the client would never remove. Regular files
/// only, symlinks not followed; each removal is logged. Errors are logged
/// and skipped: a tree that cannot be read is no reason not to start.
pub fn remove_silly_renames(root: &Path) {
    let mut dirs = vec![root.to_path_buf()];
    while let Some(dir) = dirs.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for ent in entries.flatten() {
            let Ok(kind) = ent.file_type() else { continue };
            let path = ent.path();
            if kind.is_dir() {
                dirs.push(path);
            } else if kind.is_file() && ent.file_name().to_str().is_some_and(silly_renamed) {
                let shown = path.strip_prefix(root).unwrap_or(&path).display();
                match std::fs::remove_file(&path) {
                    Ok(()) => eprintln!("NFS server: removed {shown}, left by the guest's NFS client"),
                    Err(e) => eprintln!("NFS server: could not remove {shown} ({e})"),
                }
            }
        }
    }
}

/// The exports, read-write: the transfers' staging directory always (the
/// push and `put` stage archives there for the guest, `get` has the guest
/// write them there), and the work area in NFS mode. all_squash with
/// anonuid 0 maps every request to container root, so the guest's root
/// writes keep the bind mount's ownership and it can read what the agent
/// staged; insecure because SLiRP's NAT rewrites the guest's reserved source
/// port to a high one, which the server would otherwise reject.
fn exports(stage: &str, workarea: bool) -> String {
    let opts = "rw,all_squash,anonuid=0,anongid=0,insecure";
    let mut out = format!("{stage} 127.0.0.1({opts})\n");
    if workarea {
        out.push_str(&format!("{} 127.0.0.1({opts})\n", crate::exec::container_workarea()));
    }
    out
}

/// The two daemons the keeper runs. -F keeps them in the foreground, so
/// they are its children; -r (re-export) turns off the check that takes the
/// bind mount (device major 0) for an NFS mount and refuses to export it.
fn daemons() -> Vec<Daemon> {
    vec![
        Daemon::new("rpc.mountd", &["/usr/local/sbin/rpc.mountd", "-F", "-r"], MOUNT_PROG),
        Daemon::new("rpc.nfsd", &["/usr/local/sbin/rpc.nfsd", "-F", "-r"], NFS_PROG),
    ]
}

/// Start rpcbind, first killing one that is there but not answering. `-w`
/// (warm start) daemonises once it is listening, so it answers when this
/// returns; it is not our child, and tini reaps it.
fn start_rpcbind() -> io::Result<()> {
    for pid in pids_of("rpcbind") {
        if let Some(pid) = Pid::from_raw(pid) {
            let _ = kill_process(pid, Signal::KILL);
        }
    }
    // Its lock and socket would make the new one think it is running.
    for leftover in ["/run/rpcbind.lock", "/run/rpcbind.sock"] {
        let _ = std::fs::remove_file(leftover);
    }
    if Command::new("/usr/sbin/rpcbind").arg("-w").status()?.success() {
        Ok(())
    } else {
        Err(io::Error::other("rpcbind failed to start"))
    }
}

/// How the keeper paces itself.
#[derive(Clone, Copy)]
struct Policy {
    /// The first wait before restarting a daemon that exited.
    backoff: Duration,
    /// The longest: the wait doubles while a daemon keeps dying.
    max_backoff: Duration,
    /// A daemon up this long has recovered: the next wait is the first again.
    steady: Duration,
    /// How long after starting a daemon to expect its registration.
    settle: Duration,
    /// How often to check rpcbind's registrations.
    check_every: Duration,
}

impl Policy {
    const REAL: Policy = Policy {
        backoff: Duration::from_secs(1),
        max_backoff: Duration::from_secs(60),
        steady: Duration::from_secs(60),
        settle: Duration::from_secs(5),
        check_every: Duration::from_secs(10),
    };
}

/// A daemon the keeper runs.
struct Daemon {
    name: &'static str,
    command: Vec<String>,
    /// The RPC program it registers with rpcbind.
    program: u32,
    child: Option<Child>,
    /// When it was last started, and whether it has been started before.
    started: Instant,
    restarting: bool,
    /// The wait before its next restart, should it exit.
    backoff: Option<Duration>,
    /// When to start it again; None while it runs.
    due: Option<Instant>,
}

impl Daemon {
    fn new(name: &'static str, command: &[&str], program: u32) -> Daemon {
        let command = command.iter().map(|s| s.to_string()).collect();
        let now = Instant::now();
        Daemon {
            name,
            command,
            program,
            child: None,
            started: now,
            restarting: false,
            backoff: None,
            due: Some(now),
        }
    }

    /// Stop it (it is misbehaving, not dead) and have it started again now.
    fn restart_now(&mut self, now: Instant) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        self.due = Some(now);
    }
}

/// Runs the daemons and restarts them (see the module doc).
struct Keeper {
    daemons: Vec<Daemon>,
    policy: Policy,
    last_check: Instant,
}

impl Keeper {
    fn new(daemons: Vec<Daemon>, policy: Policy) -> Keeper {
        Keeper { daemons, policy, last_check: Instant::now() }
    }

    fn run(&mut self) -> ! {
        loop {
            let now = Instant::now();
            self.tick(now);
            if now.duration_since(self.last_check) >= self.policy.check_every {
                self.last_check = now;
                let registered = registrations(PORTMAPPER.parse().expect("an address"));
                self.check_registrations(now, registered.ok(), || {
                    if let Err(e) = start_rpcbind() {
                        eprintln!("NFS server: {e}");
                    }
                });
            }
            thread::sleep(Duration::from_secs(1));
        }
    }

    /// Note daemons that have exited, and start those that are due.
    fn tick(&mut self, now: Instant) {
        let policy = self.policy;
        for d in &mut self.daemons {
            if let Some(child) = &mut d.child {
                let Ok(Some(status)) = child.try_wait() else { continue };
                d.child = None;
                // One that ran steadily has recovered; one that keeps
                // dying waits longer each time.
                let wait = match d.backoff {
                    Some(last) if now.duration_since(d.started) < policy.steady => {
                        (last * 2).min(policy.max_backoff)
                    }
                    _ => policy.backoff,
                };
                d.backoff = Some(wait);
                d.due = Some(now + wait);
                let (name, how, secs) = (d.name, exited(status), wait.as_secs());
                eprintln!("NFS server: {name} {how}; starting it again in {secs} s");
            }
            if d.due.is_some_and(|due| now >= due) {
                match Command::new(&d.command[0])
                    .args(&d.command[1..])
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .spawn()
                {
                    Ok(child) => {
                        if d.restarting {
                            eprintln!("NFS server: started {} again", d.name);
                        }
                        (d.child, d.started, d.restarting, d.due) = (Some(child), now, true, None);
                    }
                    Err(e) => {
                        let wait = d.backoff.map_or(policy.backoff, |b| (b * 2).min(policy.max_backoff));
                        (d.backoff, d.due) = (Some(wait), Some(now + wait));
                        let (name, secs) = (d.name, wait.as_secs());
                        eprintln!("NFS server: could not start {name} ({e}); trying again in {secs} s");
                    }
                }
            }
        }
    }

    /// Act on rpcbind's registrations: `registered`, the programs it has,
    /// or None when it is not answering. Then rpcbind is started again
    /// (`restart_rpcbind`) and both daemons after it, to register with
    /// the new one; a daemon whose program has gone is restarted alone.
    fn check_registrations(
        &mut self,
        now: Instant,
        registered: Option<HashSet<u32>>,
        restart_rpcbind: impl FnOnce(),
    ) {
        let Some(registered) = registered else {
            eprintln!("NFS server: rpcbind is not answering; starting it again, and the daemons after it");
            restart_rpcbind();
            for d in &mut self.daemons {
                d.restart_now(now);
            }
            self.tick(now);
            return;
        };
        for d in &mut self.daemons {
            let settled = d.child.is_some() && now.duration_since(d.started) >= self.policy.settle;
            if settled && !registered.contains(&d.program) {
                eprintln!("NFS server: {} is not registered with rpcbind; starting it again", d.name);
                d.restart_now(now);
            }
        }
        self.tick(now);
    }
}

/// How a daemon ended, for the log.
fn exited(status: ExitStatus) -> String {
    use std::os::unix::process::ExitStatusExt;
    match (status.code(), status.signal()) {
        (Some(code), _) => format!("exited {code}"),
        (None, Some(sig)) => format!("died on signal {sig}"),
        (None, None) => "exited".to_string(),
    }
}

/// The pids of live processes named `comm` (as /proc/PID/stat has it; not
/// zombies).
fn pids_of(comm: &str) -> Vec<i32> {
    let Ok(entries) = std::fs::read_dir("/proc") else { return Vec::new() };
    entries
        .flatten()
        .filter_map(|e| {
            let pid: i32 = e.file_name().to_str()?.parse().ok()?;
            let stat = std::fs::read_to_string(e.path().join("stat")).ok()?;
            let (name, state) = stat.split_once(" (")?.1.rsplit_once(") ")?;
            (name == comm && !state.starts_with(['Z', 'X'])).then_some(pid)
        })
        .collect()
}

/// How a daemon is, for `check`.
#[derive(Debug, PartialEq)]
enum Health {
    Up,
    /// No live process.
    Down,
    /// Running, but not in rpcbind's registrations.
    Unregistered,
    /// Registered, but no answer to a NULL call.
    NotAnswering,
}

impl Health {
    fn word(&self) -> &'static str {
        match self {
            Health::Up => "up",
            Health::Down => "down",
            Health::Unregistered => "unregistered",
            Health::NotAnswering => "not-answering",
        }
    }
}

/// `dockerix agent nfs`: a line `NAME STATE` for rpcbind, rpc.mountd and
/// rpc.nfsd, each asked afresh (its process, its registration, a NULL
/// call). STATE is `up`, `down`, `unregistered` or `not-answering`. It
/// exits 0 having checked, whatever it found, for the host to read.
pub fn check() {
    let portmapper: SocketAddr = PORTMAPPER.parse().expect("an address");
    let maps = mappings(portmapper);
    let rpcbind = match (&maps, pids_of("rpcbind").is_empty()) {
        (_, true) => Health::Down,
        (Err(_), false) => Health::NotAnswering,
        (Ok(_), false) => Health::Up,
    };
    outln!("rpcbind {}", rpcbind.word());
    for (name, program, version) in [("rpc.mountd", MOUNT_PROG, 1), ("rpc.nfsd", NFS_PROG, 2)] {
        let port = maps.as_ref().ok().and_then(|maps| {
            maps.iter().find(|m| (m.program, m.version, m.protocol) == (program, version, IPPROTO_UDP))
        });
        let health = match (pids_of(name).is_empty(), port) {
            (true, _) => Health::Down,
            (false, None) => Health::Unregistered,
            (false, Some(m)) => {
                let addr = SocketAddr::from(([127, 0, 0, 1], m.port as u16));
                match call(addr, program, version, 0, Duration::from_secs(2)) {
                    Ok(_) => Health::Up,
                    Err(_) => Health::NotAnswering,
                }
            }
        };
        outln!("{name} {}", health.word());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testsupport::tmp_dir;

    #[test]
    fn knows_the_clients_names_and_no_others() {
        for name in [".nfs0F43", ".nfs9F43", ".nfsAF43", ".nfsFE43", ".nfs0000"] {
            assert!(silly_renamed(name), "{name}");
        }
        let near = [".nfsrc", ".nfs0f43", ".nfs0F4", ".nfs0F433", ".nfs0G43", "nfs0F43", ".nfs", "x.nfs0F43"];
        for name in near {
            assert!(!silly_renamed(name), "{name}");
        }
    }

    /// Removes the client's leftovers anywhere in the tree, and nothing
    /// else: not a directory of that name, a symlink, or a near miss.
    #[test]
    fn removes_only_the_clients_leftovers() {
        let tmp = tmp_dir("silly");
        let root = tmp.path();
        std::fs::create_dir_all(root.join("src/deep/.nfsAB12")).unwrap();
        for f in [".nfs0F43", "src/deep/.nfsFE43", "src/.nfsrc", "src/main.c", "src/.nfs0f43"] {
            std::fs::write(root.join(f), "x").unwrap();
        }
        std::os::unix::fs::symlink("main.c", root.join("src/.nfs1234")).unwrap();
        remove_silly_renames(root);
        assert!(!root.join(".nfs0F43").exists() && !root.join("src/deep/.nfsFE43").exists());
        for kept in ["src/.nfsrc", "src/main.c", "src/.nfs0f43", "src/deep/.nfsAB12"] {
            assert!(root.join(kept).exists(), "{kept}");
        }
        assert!(root.join("src/.nfs1234").symlink_metadata().is_ok(), "the symlink");
    }

    /// A policy for tests: short waits, so time can be stepped through.
    const FAST: Policy = Policy {
        backoff: Duration::from_millis(100),
        max_backoff: Duration::from_millis(400),
        steady: Duration::from_secs(10),
        settle: Duration::from_millis(50),
        check_every: Duration::from_secs(10),
    };

    fn daemon(name: &'static str, script: &str, program: u32) -> Daemon {
        Daemon::new(name, &["sh", "-c", script], program)
    }

    /// Tick until `d`'s child has been reaped, or give up.
    fn until_exited(k: &mut Keeper, now: Instant) {
        for _ in 0..200 {
            k.tick(now);
            if k.daemons[0].child.is_none() {
                return;
            }
            thread::sleep(Duration::from_millis(10));
        }
        panic!("the daemon did not exit");
    }

    #[test]
    fn a_daemon_that_keeps_dying_waits_longer_each_time() {
        let mut k = Keeper::new(vec![daemon("dies", "exit 3", 1)], FAST);
        let mut now = Instant::now();
        k.tick(now);
        assert!(k.daemons[0].child.is_some(), "started at once");
        let mut waits = Vec::new();
        for _ in 0..5 {
            until_exited(&mut k, now);
            let due = k.daemons[0].due.expect("a restart is due");
            waits.push(due - now);
            k.tick(due - Duration::from_millis(1));
            assert!(k.daemons[0].child.is_none(), "not before it is due");
            now = due;
            k.tick(now);
            assert!(k.daemons[0].child.is_some(), "started again when due");
        }
        let ms: Vec<u128> = waits.iter().map(Duration::as_millis).collect();
        assert_eq!(ms, [100, 200, 400, 400, 400]);
    }

    #[test]
    fn a_daemon_that_ran_steadily_starts_again_soon() {
        let mut k = Keeper::new(vec![daemon("steady", "sleep 30", 1)], FAST);
        let start = Instant::now();
        k.tick(start);
        k.daemons[0].backoff = Some(FAST.max_backoff);
        // Killed long after it settled down.
        k.daemons[0].child.as_mut().unwrap().kill().unwrap();
        let later = start + FAST.steady + Duration::from_secs(1);
        until_exited(&mut k, later);
        assert_eq!(k.daemons[0].due, Some(later + FAST.backoff));
    }

    #[test]
    fn a_daemon_that_cannot_start_is_tried_again() {
        let mut k = Keeper::new(vec![Daemon::new("missing", &["/nonexistent/daemon"], 1)], FAST);
        let now = Instant::now();
        k.tick(now);
        assert!(k.daemons[0].child.is_none());
        assert_eq!(k.daemons[0].due, Some(now + FAST.backoff));
    }

    #[test]
    fn a_lost_registration_restarts_its_daemon() {
        let mut k = Keeper::new(vec![daemon("mountd", "sleep 30", 5), daemon("nfsd", "sleep 30", 3)], FAST);
        let start = Instant::now();
        k.tick(start);
        let pids: Vec<u32> = k.daemons.iter().map(|d| d.child.as_ref().unwrap().id()).collect();
        // Before it has had time to register: left alone.
        k.check_registrations(start, Some(HashSet::from([5])), || panic!("rpcbind is answering"));
        assert_eq!(k.daemons[1].child.as_ref().unwrap().id(), pids[1]);
        let later = start + FAST.settle;
        k.check_registrations(later, Some(HashSet::from([5])), || panic!("rpcbind is answering"));
        assert_eq!(k.daemons[0].child.as_ref().unwrap().id(), pids[0], "registered, so left alone");
        assert_ne!(k.daemons[1].child.as_ref().unwrap().id(), pids[1], "started again");
        // rpcbind gone: it is started again, and both daemons after it.
        let pids: Vec<u32> = k.daemons.iter().map(|d| d.child.as_ref().unwrap().id()).collect();
        let mut restarted = false;
        k.check_registrations(later, None, || restarted = true);
        assert!(restarted);
        for (d, pid) in k.daemons.iter().zip(pids) {
            assert_ne!(d.child.as_ref().unwrap().id(), pid, "{}", d.name);
        }
        for d in &mut k.daemons {
            d.restart_now(later);
        }
    }

    #[test]
    fn finds_live_processes_by_name() {
        // procfs is Linux's; a Mac has none, and finds nothing.
        if Path::new("/proc/self/stat").exists() {
            let mut child = Command::new("sleep").arg("30").spawn().unwrap();
            assert!(pids_of("sleep").contains(&(child.id() as i32)));
            child.kill().unwrap();
            child.wait().unwrap();
            assert!(!pids_of("sleep").contains(&(child.id() as i32)));
        } else {
            assert!(pids_of("sleep").is_empty());
        }
    }
}
