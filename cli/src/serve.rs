//! Serve mode: the persistent agent keeps logged-in guest sessions and runs
//! `rix` commands on them.
//!
//! Every non-tty `rix` command used to telnet in, log in (banner, password,
//! prompt, tty setup: 3–4s) and log out again. Now the entrypoint agent holds
//! a small pool of logged-in non-tty sessions and listens on a Unix socket
//! inside the container; `rix` sends the command there and the reply streams
//! back, so a command costs the guest's execution time plus milliseconds.
//!
//! Rules: a session runs one command at a time; a client that gives an
//! affinity key (`DOCKERIX_SESSION`, the host terminal by default) gets the
//! session it used last, so shell variables set by one command are visible to
//! the next (a new client gets a fresh login while the pool has room, and
//! only at the limit borrows another client's idle session, state and all);
//! the pool (`DOCKERIX_SESSIONS`, default 2) lets a second client proceed
//! while a long build holds a session; the work-area push stays per-command
//! and single-writer (pushes are serialised across sessions). A session idle
//! for `DOCKERIX_SESSION_IDLE` seconds (default 600) is logged out to free
//! its guest pty and re-established on demand.
//!
//! Wire format, both directions: frames of one type byte, a big-endian u32
//! length and the payload. Request: `c` command, `s` sync ("0"/"1"), `m` sync
//! size ceiling, `h` sync by content hash ("0"/"1"), `w` wipe the guest work
//! area first ("1"; `dockerix sync --clear`), `t` timeout seconds, `k`
//! affinity key, `i` run id, `d` the command as given and `r` its guest
//! directory (for `dockerix ps`), then `g` (go). Reply: `o` output bytes (any
//! number), `e` a message for stderr, `p` a transfer progress event
//! (progress.rs), `x` the exit status (last frame).
//!
//! Two more requests are a single frame each. `l` (list) asks what is running
//! in the guest; the reply is the `dockerix ps` table as `o` (with payload
//! `count`, just how many), then `x`. `n` (notice) carries
//! `phase\tdir\tcommand` from a `rix` that runs a session of its own (`exec
//! -t`, `login`, a direct command): the server lists it until that `rix`
//! closes the connection, and replies nothing.
//!
//! Interrupting: `docker exec` does not forward signals, so Ctrl-C on the host
//! only kills the docker client and the command would run on. The host
//! wrapper therefore tags each run with an id (`DOCKERIX_RUN_ID`) and on
//! SIGINT sends a second request, a single `a` frame with that id: the server
//! flags the run and `Session::run_to` sends the guest the interrupt
//! character. An abort for a run that has not registered yet is remembered
//! briefly, so a very early Ctrl-C works too. The client socket closing
//! mid-command (the in-container `rix` killed) is treated as an abort as well.
//!
//! This file has the wire format, the client and the server; the pool of
//! held sessions, and the run ids' interrupt flags, are in `pool`.

use std::io::{self, ErrorKind, Read, Write};
use std::net::Shutdown;
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use crate::config::{self, Config};
use crate::exec;
use crate::session::Timing;

mod activity;
mod pool;

pub use activity::Phase;
use pool::Pool;

/// A non-tty command to run in the guest.
pub struct Request {
    /// The full shell command line (already `cd`'d to the guest cwd).
    pub command: String,
    /// Push the work area (as a delta) before the command.
    pub sync: bool,
    /// Push size ceiling in bytes.
    pub sync_max: usize,
    /// Compare file contents, not just size and mtime (`DOCKERIX_SYNC_HASH`).
    pub sync_hash: bool,
    /// Empty the guest work area and start a new manifest first
    /// (`dockerix sync --clear`).
    pub sync_clear: bool,
    /// Seconds before the command is abandoned.
    pub timeout_secs: u64,
    /// Session affinity key, if the client wants one.
    pub key: Option<String>,
    /// Run id, so the client can ask for the command to be interrupted.
    pub id: Option<String>,
    /// The command as given, without the `cd` wrapper (`dockerix ps`).
    pub display: Option<String>,
    /// The guest directory it runs in (`dockerix ps`).
    pub dir: Option<String>,
}

impl Request {
    /// The request for `command` (as the user gave it): the shell line is
    /// wrapped to the guest cwd, the push settings come from `cfg` (resolved
    /// once from the environment at `rix` entry), and the timeout, affinity
    /// key and run id from the environment — the same timeout source
    /// (`config::exec_timeout`) the agent builds its `ExecOpts` from. Both the
    /// non-tty and the tty path build one, so the push settings live in one
    /// place.
    pub fn from_cfg(cfg: &Config, command: &str) -> Request {
        Request {
            command: exec::in_guest_cwd(cfg, command),
            sync: cfg.sync_enabled(),
            sync_max: cfg.sync_max,
            sync_hash: cfg.sync_hash,
            sync_clear: cfg.sync_clear,
            timeout_secs: config::exec_timeout(),
            key: config::session_key(),
            id: config::run_id(),
            display: Some(command.to_string()),
            dir: Some(cfg.guest_cwd()),
        }
    }

    /// The work-area push settings carried in this request.
    pub fn push_opts(&self) -> crate::sync::PushOpts {
        crate::sync::PushOpts { max_bytes: self.sync_max, hash: self.sync_hash, clear: self.sync_clear }
    }
}

/// A request read off the socket.
enum Incoming {
    /// A command to run.
    Run(Request),
    /// Interrupt the run with this id.
    Abort(String),
    /// List what is running: the table, or (`count`) just how many.
    List { count: bool },
    /// A `rix` running a session of its own, listed until it hangs up.
    Notice(activity::Entry),
}

pub fn socket_path() -> String {
    config::socket_path()
}

// ---- framing -------------------------------------------------------------

fn write_frame(w: &mut dyn Write, kind: u8, payload: &[u8]) -> io::Result<()> {
    let mut hdr = [0u8; 5];
    hdr[0] = kind;
    hdr[1..].copy_from_slice(&(payload.len() as u32).to_be_bytes());
    w.write_all(&hdr)?;
    w.write_all(payload)?;
    w.flush()
}

fn read_frame(r: &mut dyn Read) -> io::Result<Option<(u8, Vec<u8>)>> {
    let mut hdr = [0u8; 5];
    match r.read_exact(&mut hdr) {
        Ok(()) => {}
        Err(e) if e.kind() == ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let len = u32::from_be_bytes([hdr[1], hdr[2], hdr[3], hdr[4]]) as usize;
    if len > 64 * 1024 * 1024 {
        return Err(io::Error::new(ErrorKind::InvalidData, "frame too large"));
    }
    let mut payload = vec![0u8; len];
    r.read_exact(&mut payload)?;
    Ok(Some((hdr[0], payload)))
}

// ---- client --------------------------------------------------------------

/// Send `req` to the server at `path`, relay its output to stdout and its
/// messages to stderr, and return the guest exit status. `NotFound` /
/// `ConnectionRefused` mean there is no server (the caller falls back).
pub fn client_run(path: &str, req: &Request) -> io::Result<i32> {
    let mut s = UnixStream::connect(path)?;
    write_frame(&mut s, b'c', req.command.as_bytes())?;
    write_frame(&mut s, b's', if req.sync { b"1" } else { b"0" })?;
    write_frame(&mut s, b'm', req.sync_max.to_string().as_bytes())?;
    write_frame(&mut s, b'h', if req.sync_hash { b"1" } else { b"0" })?;
    if req.sync_clear {
        write_frame(&mut s, b'w', b"1")?;
    }
    write_frame(&mut s, b't', req.timeout_secs.to_string().as_bytes())?;
    if let Some(k) = &req.key {
        write_frame(&mut s, b'k', k.as_bytes())?;
    }
    if let Some(id) = &req.id {
        write_frame(&mut s, b'i', id.as_bytes())?;
    }
    if let Some(d) = &req.display {
        write_frame(&mut s, b'd', d.as_bytes())?;
    }
    if let Some(r) = &req.dir {
        write_frame(&mut s, b'r', r.as_bytes())?;
    }
    write_frame(&mut s, b'g', b"")?;

    let stdout = io::stdout();
    let mut out = stdout.lock();
    let mut reporter = crate::progress::Reporter::new(crate::progress::Style::from_env());
    loop {
        match read_frame(&mut s)? {
            None => {
                return Err(io::Error::new(
                    ErrorKind::UnexpectedEof,
                    "the agent closed the exec socket before the command finished",
                ));
            }
            Some((b'o', data)) => {
                out.write_all(&data)?;
                out.flush()?;
            }
            Some((b'e', msg)) => eprintln!("{}", String::from_utf8_lossy(&msg)),
            Some((b'p', ev)) => {
                if let Some(p) = crate::progress::Progress::decode(&String::from_utf8_lossy(&ev)) {
                    reporter.show(&p);
                }
            }
            Some((b'x', code)) => {
                return Ok(String::from_utf8_lossy(&code).trim().parse().unwrap_or(125));
            }
            Some((k, _)) => {
                return Err(io::Error::new(
                    ErrorKind::InvalidData,
                    format!("unexpected frame {:?} from the agent", k as char),
                ));
            }
        }
    }
}

/// Ask the server to interrupt the run tagged `id`. Returns once the server
/// has acknowledged (it acts on the flag asynchronously).
pub fn client_abort(path: &str, id: &str) -> io::Result<()> {
    let mut s = UnixStream::connect(path)?;
    write_frame(&mut s, b'a', id.as_bytes())?;
    loop {
        match read_frame(&mut s)? {
            None | Some((b'x', _)) => return Ok(()),
            Some((b'e', msg)) => eprintln!("{}", String::from_utf8_lossy(&msg)),
            Some(_) => {}
        }
    }
}

/// The `dockerix ps` table from the server at `path`, or with `count` just
/// the number of things running, on a line of its own.
pub fn client_list(path: &str, count: bool) -> io::Result<String> {
    let mut s = UnixStream::connect(path)?;
    write_frame(&mut s, b'l', if count { b"count" } else { b"" })?;
    let mut table = Vec::new();
    loop {
        match read_frame(&mut s)? {
            Some((b'o', data)) => table.extend_from_slice(&data),
            Some((b'x', _)) => return Ok(String::from_utf8_lossy(&table).into_owned()),
            Some((b'e', msg)) => return Err(io::Error::other(String::from_utf8_lossy(&msg).into_owned())),
            Some(_) => {}
            None => {
                return Err(io::Error::new(ErrorKind::UnexpectedEof, "the agent closed the exec socket"));
            }
        }
    }
}

/// Tell the server at `path` about a session this `rix` runs itself, so
/// `dockerix ps` lists it. It stays listed while the returned connection is
/// open: hold it for the session's life. None when there is no server (a
/// one-shot run before the agent is up, say): nothing to tell.
pub fn announce(path: &str, phase: Phase, dir: &str, command: &str) -> Option<UnixStream> {
    let mut s = UnixStream::connect(path).ok()?;
    let notice = format!("{}\t{dir}\t{command}", phase.word());
    write_frame(&mut s, b'n', notice.as_bytes()).ok()?;
    Some(s)
}

// ---- server --------------------------------------------------------------

/// Handle to the running server, for the entrypoint to drain at shutdown.
pub struct Server {
    pool: Arc<Pool>,
}

impl Server {
    /// Log every held session out (best effort, bounded by the sessions'
    /// own logout timeouts). Sessions in use are left to their commands.
    pub fn drain(&self) {
        self.pool.drain();
    }
}

/// Start listening on the exec socket in background threads.
pub fn start(cfg: &Config) -> io::Result<Server> {
    let path = socket_path();
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path)?;
    let pool = Arc::new(Pool::new(
        cfg.clone(),
        config::sessions(),
        Duration::from_secs(config::session_idle_secs()),
        Timing::GUEST,
    ));

    let reaper = Arc::clone(&pool);
    thread::spawn(move || {
        loop {
            thread::sleep(Duration::from_secs(15));
            reaper.reap();
        }
    });

    let acceptor = Arc::clone(&pool);
    thread::spawn(move || {
        for conn in listener.incoming() {
            match conn {
                Ok(stream) => {
                    let pool = Arc::clone(&acceptor);
                    thread::spawn(move || serve_one(pool, stream));
                }
                Err(e) => {
                    eprintln!("exec socket: accept failed: {e}");
                    thread::sleep(Duration::from_millis(200));
                }
            }
        }
    });
    Ok(Server { pool })
}

fn read_request(stream: &mut UnixStream) -> io::Result<Incoming> {
    let mut req = Request {
        command: String::new(),
        sync: true,
        sync_max: crate::sync::max_bytes_from_env(),
        sync_hash: crate::sync::hash_enabled(),
        sync_clear: false,
        timeout_secs: 3600,
        key: None,
        id: None,
        display: None,
        dir: None,
    };
    loop {
        let Some((kind, payload)) = read_frame(stream)? else {
            return Err(io::Error::new(ErrorKind::UnexpectedEof, "client hung up"));
        };
        let text = String::from_utf8_lossy(&payload).into_owned();
        match kind {
            b'c' => req.command = text,
            b's' => req.sync = text != "0",
            b'm' => req.sync_max = text.parse().unwrap_or(req.sync_max),
            b'h' => req.sync_hash = text == "1",
            b'w' => req.sync_clear = text == "1",
            b't' => req.timeout_secs = text.parse().map(config::clamp_timeout).unwrap_or(req.timeout_secs),
            b'k' => req.key = Some(text),
            b'i' => req.id = Some(text),
            b'd' => req.display = Some(text),
            b'r' => req.dir = Some(text),
            b'g' => return Ok(Incoming::Run(req)),
            b'a' => return Ok(Incoming::Abort(text)),
            b'l' => return Ok(Incoming::List { count: text == "count" }),
            b'n' => return notice(&text).map(Incoming::Notice),
            other => {
                return Err(io::Error::new(
                    ErrorKind::InvalidData,
                    format!("unexpected request frame {:?}", other as char),
                ));
            }
        }
    }
}

/// An `n` frame's `phase\tdir\tcommand`, as an entry starting now.
fn notice(text: &str) -> io::Result<activity::Entry> {
    let mut f = text.splitn(3, '\t');
    let (phase, dir, command) = (f.next().unwrap_or(""), f.next(), f.next());
    match (Phase::from_word(phase), dir, command) {
        (Some(phase), Some(dir), Some(command)) => Ok(activity::Entry {
            command: command.to_string(),
            dir: dir.to_string(),
            phase,
            started: Instant::now(),
        }),
        _ => Err(io::Error::new(ErrorKind::InvalidData, format!("bad notice {text:?}"))),
    }
}

fn serve_one(pool: Arc<Pool>, mut stream: UnixStream) {
    let req = match read_request(&mut stream) {
        Ok(Incoming::Run(r)) => r,
        Ok(Incoming::Abort(id)) => {
            if !pool.abort(&id) {
                eprintln!("exec: interrupt for run {id} before it started; remembered");
            }
            let _ = write_frame(&mut stream, b'x', b"0");
            return;
        }
        Ok(Incoming::List { count }) => {
            let entries = pool.activity.list();
            let reply = if count {
                format!("{}\n", entries.len())
            } else {
                activity::render(&entries, pool.sessions(), Instant::now())
            };
            let _ = write_frame(&mut stream, b'o', reply.as_bytes());
            let _ = write_frame(&mut stream, b'x', b"0");
            return;
        }
        Ok(Incoming::Notice(entry)) => {
            // Listed until the announcing `rix` hangs up (or dies).
            let _tracked = pool.activity.track(entry);
            let mut buf = [0u8; 64];
            while matches!(stream.read(&mut buf), Ok(n) if n > 0) {}
            return;
        }
        Err(e) => {
            let _ = write_frame(&mut stream, b'e', format!("rix: bad request: {e}").as_bytes());
            let _ = write_frame(&mut stream, b'x', b"125");
            return;
        }
    };
    let interrupt = match &req.id {
        Some(id) => match pool.register(id) {
            Some(flag) => flag,
            None => {
                let _ = write_frame(&mut stream, b'e', b"rix: interrupted before the command started");
                let _ = write_frame(&mut stream, b'x', b"130");
                return;
            }
        },
        None => Arc::new(AtomicBool::new(false)),
    };
    // The client closing its socket mid-command (the in-container `rix`
    // killed) also counts as an interrupt. Reads on the clone unblock when
    // serve_one shuts the socket down at the end.
    if let Ok(mut watch) = stream.try_clone() {
        let flag = Arc::clone(&interrupt);
        thread::spawn(move || {
            let mut buf = [0u8; 64];
            loop {
                match watch.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {}
                }
            }
            flag.store(true, Ordering::SeqCst);
        });
    }
    let deadline = Instant::now() + Duration::from_secs(req.timeout_secs);
    let tracked = pool.activity.track(activity::Entry {
        command: req.display.clone().unwrap_or_else(|| req.command.clone()),
        dir: req.dir.clone().unwrap_or_default(),
        phase: Phase::Waiting,
        started: Instant::now(),
    });
    let result = (|| {
        let mut session = pool.checkout(req.key.as_deref(), deadline)?;
        tracked.set_phase(if req.sync { Phase::Pushing } else { Phase::Running });
        // If the client goes away mid-command, keep going: the session must
        // reach a known state to stay usable, so socket writes are best effort.
        let mut out = stream.try_clone().ok();
        let mut sink = |chunk: &[u8]| {
            if let Some(o) = out.as_mut() {
                let _ = write_frame(o, b'o', chunk);
            }
        };
        let mut err = stream.try_clone().ok();
        let mut warn = |msg: &str| {
            if let Some(o) = err.as_mut() {
                let _ = write_frame(o, b'e', msg.as_bytes());
            }
        };
        let mut prog = stream.try_clone().ok();
        let mut progress = |p: &crate::progress::Progress| {
            if let Some(o) = prog.as_mut() {
                let _ = write_frame(o, b'p', p.encode().as_bytes());
            }
        };
        let mut started = || tracked.set_phase(Phase::Running);
        let mut output =
            exec::Output { sink: &mut sink, warn: &mut warn, progress: &mut progress, started: &mut started };
        let r = exec::run_command(
            pool.config(),
            &mut session,
            &req,
            deadline,
            Some(&pool.sync_lock),
            Some(&interrupt),
            &mut output,
        );
        match r {
            Ok(code) => {
                pool.checkin(session, req.key.clone());
                Ok(code)
            }
            Err(e) => {
                pool.discard(session);
                Err(e)
            }
        }
    })();
    drop(tracked);
    if let Some(id) = &req.id {
        pool.unregister(id);
    }
    match result {
        Ok(code) => {
            let _ = write_frame(&mut stream, b'x', code.to_string().as_bytes());
        }
        Err(e) => {
            let _ = write_frame(&mut stream, b'e', format!("rix: {e}").as_bytes());
            let _ = write_frame(&mut stream, b'x', b"125");
        }
    }
    let _ = stream.shutdown(Shutdown::Both);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake_guest::{FakeGuest, Reply};
    use crate::testsupport::tmp_dir;

    fn frames(list: &[(u8, &[u8])]) -> Vec<u8> {
        let mut buf = Vec::new();
        for (kind, payload) in list {
            write_frame(&mut buf, *kind, payload).unwrap();
        }
        buf
    }

    /// `read_request` on the far end of a socket the frames were written to
    /// and then closed.
    fn incoming(list: &[(u8, &[u8])]) -> io::Result<Incoming> {
        let (mut client, mut server) = UnixStream::pair().unwrap();
        client.write_all(&frames(list)).unwrap();
        drop(client);
        read_request(&mut server)
    }

    /// As `incoming`, for frames that make a command to run.
    fn request(list: &[(u8, &[u8])]) -> io::Result<Request> {
        incoming(list).map(|i| match i {
            Incoming::Run(r) => r,
            _ => panic!("not a run request"),
        })
    }

    #[test]
    fn frames_round_trip() {
        let big = vec![7u8; 100_000];
        let buf = frames(&[(b'o', b"hello"), (b'g', b""), (b'o', &big)]);
        assert_eq!(&buf[..5], &[b'o', 0, 0, 0, 5]);
        let mut r = &buf[..];
        assert_eq!(read_frame(&mut r).unwrap(), Some((b'o', b"hello".to_vec())));
        assert_eq!(read_frame(&mut r).unwrap(), Some((b'g', vec![])));
        assert_eq!(read_frame(&mut r).unwrap(), Some((b'o', big)));
        assert_eq!(read_frame(&mut r).unwrap(), None);
    }

    #[test]
    fn short_reads_at_eof() {
        // A partial header is a clean end of stream; a partial payload is not.
        let mut r = &[b'o', 0, 0][..];
        assert_eq!(read_frame(&mut r).unwrap(), None);
        let mut r = &[b'o', 0, 0, 0, 5, b'h', b'i'][..];
        assert_eq!(read_frame(&mut r).unwrap_err().kind(), ErrorKind::UnexpectedEof);
    }

    #[test]
    fn frames_over_64_mb_are_refused() {
        let len = (64 * 1024 * 1024 + 1) as u32;
        let mut hdr = vec![b'o'];
        hdr.extend_from_slice(&len.to_be_bytes());
        let mut r = &hdr[..];
        assert_eq!(read_frame(&mut r).unwrap_err().kind(), ErrorKind::InvalidData);
    }

    #[test]
    fn request_reads_every_field() {
        let req = request(&[
            (b'c', b"cd /workarea && make"),
            (b's', b"0"),
            (b'm', b"12345"),
            (b'h', b"1"),
            (b'w', b"1"),
            (b't', b"60"),
            (b'k', b"ttys003"),
            (b'i', b"run-1"),
            (b'd', b"make"),
            (b'r', b"/workarea"),
            (b'g', b""),
        ])
        .unwrap();
        assert_eq!(req.display.as_deref(), Some("make"));
        assert_eq!(req.dir.as_deref(), Some("/workarea"));
        assert_eq!(req.command, "cd /workarea && make");
        assert!(!req.sync);
        assert_eq!(req.sync_max, 12345);
        assert!(req.sync_hash);
        assert!(req.sync_clear);
        assert_eq!(req.timeout_secs, 60);
        assert_eq!(req.key.as_deref(), Some("ttys003"));
        assert_eq!(req.id.as_deref(), Some("run-1"));
    }

    #[test]
    fn request_defaults_and_bad_numbers() {
        let req = request(&[(b'c', b"ls"), (b'm', b"lots"), (b't', b"soon"), (b'g', b"")]).unwrap();
        assert!(req.sync);
        assert!(!req.sync_clear);
        assert_eq!(req.sync_max, crate::sync::max_bytes_from_env());
        assert_eq!(req.sync_hash, crate::sync::hash_enabled());
        assert_eq!(req.timeout_secs, 3600);
        assert_eq!((req.key, req.id), (None, None));
    }

    #[test]
    fn request_timeouts_are_clamped() {
        // Unclamped, `serve_one` adds the timeout to an `Instant`: a huge
        // one panics there, and 0 (no limit) would time out at once.
        for t in [u64::MAX.to_string(), "0".into()] {
            let req = request(&[(b'c', b"ls"), (b't', t.as_bytes()), (b'g', b"")]).unwrap();
            assert_eq!(req.timeout_secs, config::NO_LIMIT_SECS, "t = {t}");
            let _ = Instant::now() + Duration::from_secs(req.timeout_secs);
        }
    }

    #[test]
    fn single_frame_requests() {
        assert!(matches!(incoming(&[(b'a', b"run-7")]), Ok(Incoming::Abort(id)) if id == "run-7"));
        assert!(matches!(incoming(&[(b'l', b"")]), Ok(Incoming::List { count: false })));
        assert!(matches!(incoming(&[(b'l', b"count")]), Ok(Incoming::List { count: true })));
        match incoming(&[(b'n', b"tty\t/workarea/game\tvi a\tb.c")]) {
            Ok(Incoming::Notice(e)) => {
                assert_eq!(
                    (e.phase, e.dir.as_str(), e.command.as_str()),
                    (Phase::Tty, "/workarea/game", "vi a\tb.c")
                )
            }
            _ => panic!("not a notice"),
        }
        assert_eq!(incoming(&[(b'n', b"login\t/")]).err().unwrap().kind(), ErrorKind::InvalidData);
        assert_eq!(incoming(&[(b'n', b"dozing\t/\tx")]).err().unwrap().kind(), ErrorKind::InvalidData);
    }

    /// A served command is listed while it runs, by the name it was given;
    /// a notice is listed until its connection closes; `l` returns the table.
    #[test]
    fn the_list_shows_runs_and_notices() {
        let guest = hanging();
        let pool = pool(&guest, 2, Duration::from_secs(600));
        let run = [
            (b'c', &b"hang"[..]),
            (b's', b"0"),
            (b'i', b"r3"),
            (b'd', b"make all"),
            (b'r', b"/wa"),
            (b'g', b""),
        ];
        let replies = serve(&pool, &run, |_| {
            wait_registered(&pool, "r3");
            assert!(guest.wait(|st| st.commands.last().map(String::as_str) == Some("hang")));
            let (mut note, server) = UnixStream::pair().unwrap();
            let p = Arc::clone(&pool);
            let noticed = thread::spawn(move || serve_one(p, server));
            write_frame(&mut note, b'n', b"login\t/\t").unwrap();
            let until = soon();
            while pool.activity.list().len() < 2 {
                assert!(Instant::now() < until, "the notice was never listed");
                thread::sleep(Duration::from_millis(2));
            }
            let table = serve(&pool, &[(b'l', b"")], |_| {});
            let text = &table[0].1;
            assert!(text.contains("running") && text.contains("/wa") && text.contains("make all"), "{text}");
            assert!(text.contains("login") && text.contains("(login shell)"), "{text}");
            assert!(text.contains("held sessions: 1 busy, 0 idle, at most 2"), "{text}");
            assert_eq!(serve(&pool, &[(b'l', b"count")], |_| {})[0].1, "2\n");
            drop(note);
            noticed.join().unwrap();
            assert_eq!(pool.activity.list().len(), 1);
            serve(&pool, &[(b'a', b"r3")], |_| {});
        });
        assert_eq!(replies.last(), Some(&(b'x', "130".into())));
        assert!(pool.activity.list().is_empty());
    }

    #[test]
    fn eof_before_go_and_unknown_frames_are_errors() {
        let e = request(&[(b'c', b"ls"), (b's', b"1")]).err().unwrap();
        assert_eq!(e.kind(), ErrorKind::UnexpectedEof);
        let e = request(&[(b'c', b"ls"), (b'z', b"")]).err().unwrap();
        assert_eq!(e.kind(), ErrorKind::InvalidData);
    }

    // ---- helpers for the pool, here and in pool.rs's tests -------------

    pub(super) fn pool(guest: &FakeGuest, max: usize, idle: Duration) -> Arc<Pool> {
        Arc::new(Pool::new(guest.config(), max, idle, Timing::TEST))
    }

    /// A pool whose agent runs with `cfg` (for a push targeting a test work
    /// area); checkout mounts no NFS share, since `cfg` requests none.
    pub(super) fn pool_with(cfg: Config, max: usize, idle: Duration) -> Arc<Pool> {
        Arc::new(Pool::new(cfg, max, idle, Timing::TEST))
    }

    pub(super) fn soon() -> Instant {
        Instant::now() + Duration::from_secs(2)
    }

    // ---- whole requests through serve_one --------------------------------

    /// Send `list` to `serve_one` on `pool`, call `during` with the client
    /// end of the socket, and return the reply frames up to `x`.
    fn serve(
        pool: &Arc<Pool>,
        list: &[(u8, &[u8])],
        during: impl FnOnce(&mut UnixStream),
    ) -> Vec<(u8, String)> {
        let (mut client, server) = UnixStream::pair().unwrap();
        let p = Arc::clone(pool);
        let t = thread::spawn(move || serve_one(p, server));
        client.write_all(&frames(list)).unwrap();
        during(&mut client);
        let mut replies = Vec::new();
        while let Some((kind, payload)) = read_frame(&mut client).unwrap() {
            replies.push((kind, String::from_utf8_lossy(&payload).into_owned()));
            if kind == b'x' {
                break;
            }
        }
        t.join().unwrap();
        replies
    }

    fn hanging() -> FakeGuest {
        FakeGuest::with(|cmd| match cmd {
            "make" => Some(Reply::Done("built\n".into(), 0)),
            "hang" => Some(Reply::Hang),
            _ => None,
        })
    }

    fn wait_registered(pool: &Pool, id: &str) {
        let until = soon();
        while !pool.runs.lock().unwrap().contains_key(id) {
            assert!(Instant::now() < until, "run {id} never registered");
            thread::sleep(Duration::from_millis(2));
        }
    }

    #[test]
    fn a_request_runs_and_returns_its_session() {
        let guest = hanging();
        let pool = pool(&guest, 1, Duration::from_secs(600));
        let replies = serve(&pool, &[(b'c', b"make"), (b's', b"0"), (b'g', b"")], |_| {});
        assert_eq!(replies, [(b'o', "built\n".into()), (b'x', "0".into())]);
        assert_eq!(pool.lock().idle.len(), 1);
    }

    #[test]
    fn an_abort_interrupts_a_running_request() {
        let guest = hanging();
        let pool = pool(&guest, 1, Duration::from_secs(600));
        let replies = serve(&pool, &[(b'c', b"hang"), (b's', b"0"), (b'i', b"r9"), (b'g', b"")], |_| {
            wait_registered(&pool, "r9");
            // The abort arrives as a request of its own.
            assert_eq!(serve(&pool, &[(b'a', b"r9")], |_| {}), [(b'x', "0".into())]);
        });
        assert_eq!(replies.last(), Some(&(b'x', "130".into())));
        // Interrupted cleanly, the session goes back to the pool.
        assert_eq!(pool.lock().idle.len(), 1);
    }

    #[test]
    fn an_abort_before_the_request_stops_it_starting() {
        let guest = hanging();
        let pool = pool(&guest, 1, Duration::from_secs(600));
        assert_eq!(serve(&pool, &[(b'a', b"r5")], |_| {}), [(b'x', "0".into())]);
        let replies = serve(&pool, &[(b'c', b"make"), (b's', b"0"), (b'i', b"r5"), (b'g', b"")], |_| {});
        assert_eq!(replies.last(), Some(&(b'x', "130".into())));
        assert_eq!(guest.state().connections, 0);
    }

    #[test]
    fn the_client_hanging_up_interrupts_the_command() {
        let guest = hanging();
        let pool = pool(&guest, 1, Duration::from_secs(600));
        let replies = serve(&pool, &[(b'c', b"hang"), (b's', b"0"), (b'g', b"")], |client| {
            assert!(guest.wait(|st| st.commands.last().map(String::as_str) == Some("hang")));
            client.shutdown(Shutdown::Write).unwrap();
        });
        assert_eq!(replies.last(), Some(&(b'x', "130".into())));
    }

    #[test]
    fn a_failed_request_discards_and_logs_out_its_session() {
        let tmp = tmp_dir("serve-fail");
        let dir = tmp.path();
        std::fs::create_dir_all(dir.join("wa")).unwrap();
        std::fs::write(dir.join("wa/hello.c"), "int x;\n").unwrap();
        let guest = FakeGuest::with(|cmd| {
            cmd.ends_with("tar xf wa.tar && rm -f wa.tar").then(|| Reply::Done("tar: bad\n".into(), 1))
        });
        // The agent runs with a push targeting the test work area (manifest
        // beside it); the staged archive goes down the tty (no stage dir).
        let cfg = Config {
            container_workarea: dir.join("wa").to_string_lossy().into_owned(),
            sync_manifest: dir.join("manifest"),
            stage: dir.join("stage").to_string_lossy().into_owned(),
            ..guest.config()
        };
        let pool = pool_with(cfg, 1, Duration::from_secs(600));
        let replies = serve(&pool, &[(b'c', b"make"), (b's', b"1"), (b'g', b"")], |_| {});
        assert_eq!(replies.last(), Some(&(b'x', "125".into())));
        assert!(replies.iter().any(|(k, m)| *k == b'e' && m.contains("tar extract failed")), "{replies:?}");
        let st = guest.state();
        assert_eq!((st.logouts, st.dropped), (1, 0));
        assert_eq!(pool.lock().live, 0);
    }
}
