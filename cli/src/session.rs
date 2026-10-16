//! A logged-in shell session over the telnet exec channel.
//!
//! One TCP connection == one guest login == one pty. RISC iX has only a
//! handful of ptys, and a login that is dropped without logging out leaves the
//! shell (and its pty) allocated until the guest reboots. So every path here
//! ends by asking the shell to `exit`, and callers must run a session to
//! completion rather than dropping it mid-command.
//!
//! Command results are framed with a per-command random sentinel that carries
//! the exit status, so we never have to recognise the shell prompt.

use std::io::{self, ErrorKind, Read};
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::telnet::Telnet;
use crate::uu;

/// Longest command line we will type at the guest shell. The 4.3BSD tty has
/// a 255-byte canonical input limit (TTYHOG); once a single line reaches it
/// with no newline the pty driver blocks the writer (telnetd) forever, the
/// shell sits idle, and the session is deadlocked. Longer commands are staged
/// into a script file instead (see `Session::run`).
const MAX_TTY_LINE: usize = 200;

pub struct Session {
    t: Telnet,
    residual: Vec<u8>,
    timing: Timing,
    /// A command run ahead of every command from now on (`guard`).
    guard: Option<String>,
}

/// How long a session waits at each step it does not get a deadline for.
/// The real guest needs seconds; the tests' fake guest answers in
/// milliseconds and uses `Timing::TEST`.
#[derive(Clone, Copy, Debug)]
pub struct Timing {
    /// Per-read socket timeout: how often a wait looks at its deadline (and
    /// at the interrupt flag).
    pub read: Duration,
    /// For the login banner and then the password prompt. Short: a stalled
    /// login is retried rather than waited out (see `connect_and_login`).
    pub prompt: Duration,
    /// After the password, for the shell prompt and the readiness token.
    pub shell: Duration,
    /// After sending ^C, before typing the marker line (see `run_to`).
    pub int_grace: Duration,
    /// Once the marker is typed, for it (or the sentinel) to come back.
    pub int_wait: Duration,
    /// After `exit`, for the guest to close the connection.
    pub logout: Duration,
}

impl Timing {
    pub const GUEST: Timing = Timing {
        read: Duration::from_millis(400),
        prompt: Duration::from_secs(10),
        shell: Duration::from_secs(45),
        int_grace: Duration::from_millis(300),
        int_wait: Duration::from_secs(5),
        logout: Duration::from_secs(3),
    };

    #[cfg(test)]
    pub const TEST: Timing = Timing {
        read: Duration::from_millis(5),
        prompt: Duration::from_millis(100),
        shell: Duration::from_millis(500),
        int_grace: Duration::from_millis(30),
        int_wait: Duration::from_millis(200),
        logout: Duration::from_millis(200),
    };
}

fn timeout(msg: &str) -> io::Error {
    io::Error::new(ErrorKind::TimedOut, msg.to_string())
}

/// A short token, unique enough between processes and over time: the
/// sentinels' mark (so a command's own output can't be mistaken for a
/// sentinel and vice versa), a `rix` run's id, and a sync epoch.
pub fn token() -> String {
    let ns = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos();
    let pid = std::process::id();
    format!("{:x}{:x}", pid, ns & 0xffff_ffff)
}

/// Where `needle` first occurs in `hay`; never for an empty needle.
pub fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || hay.len() < needle.len() {
        return None;
    }
    hay.windows(needle.len()).position(|w| w == needle)
}

/// How far `run_to` has got in interrupting the command: nothing yet, the
/// interrupt character sent (at that time; a grace period later, the marker
/// line is typed), or the marker typed to confirm the shell abandoned it.
#[derive(Clone, Copy)]
enum Interrupt {
    None,
    Sent(Instant),
    Marked,
}

/// Which needle `run_to` matched in the guest's output: the exit sentinel
/// that follows the command, or the interrupt marker.
#[derive(Clone, Copy)]
enum Found {
    Sentinel,
    Marker,
}

impl Session {
    /// Connect to the guest telnet port with a connect timeout.
    fn connect(addr: &str, tty: bool, cols: u16, rows: u16, timing: Timing) -> io::Result<Session> {
        let sa = addr
            .to_socket_addrs()?
            .next()
            .ok_or_else(|| io::Error::new(ErrorKind::InvalidInput, "no address"))?;
        let stream = TcpStream::connect_timeout(&sa, Duration::from_secs(10))?;
        stream.set_nodelay(true).ok();
        // A small send buffer, so that writing a large payload (the
        // work-area push) blocks on the guest actually draining it rather
        // than on the kernel absorbing it: that is what makes the bytes-sent
        // count a progress figure instead of a number that jumps to 100% and
        // then waits. Inbound to the guest runs at ~11 KB/s, so 4 KB of
        // buffering costs nothing. Best effort.
        let _ = rustix::net::sockopt::set_socket_send_buffer_size(&stream, 4096);
        // Short per-read timeout; overall deadlines are enforced by callers.
        stream.set_read_timeout(Some(timing.read))?;
        Ok(Session { t: Telnet::new(stream, tty, cols, rows), residual: Vec::new(), timing, guard: None })
    }

    /// Connect and log in, retrying on a stalled login. Observed:
    /// intermittently the guest sends the banner but never the password
    /// prompt (or never even reaches `login`), strictly alternating with
    /// good attempts, while a plain telnet client always gets through. The
    /// stalled attempt itself clears the condition, so dropping the
    /// connection and reconnecting works; a wrong password is not retried.
    pub fn connect_and_login(
        addr: &str,
        tty: bool,
        cols: u16,
        rows: u16,
        user: &str,
        pass: &str,
    ) -> io::Result<Session> {
        Session::connect_and_login_with(addr, tty, cols, rows, user, pass, Timing::GUEST)
    }

    /// `connect_and_login` with the waits in `timing`.
    pub fn connect_and_login_with(
        addr: &str,
        tty: bool,
        cols: u16,
        rows: u16,
        user: &str,
        pass: &str,
        timing: Timing,
    ) -> io::Result<Session> {
        const ATTEMPTS: u32 = 4;
        let mut last = None;
        for attempt in 1..=ATTEMPTS {
            let mut session = Session::connect(addr, tty, cols, rows, timing)?;
            match session.login(user, pass) {
                Ok(()) => return Ok(session),
                Err(e) if e.kind() == ErrorKind::PermissionDenied => return Err(e),
                Err(e) => {
                    if attempt < ATTEMPTS {
                        eprintln!("dockerix: login stalled ({e}); retrying ({attempt}/{ATTEMPTS})");
                    }
                    last = Some(e);
                }
            }
            // `session` drops here, closing the stalled connection.
        }
        Err(last.unwrap_or_else(|| io::Error::new(ErrorKind::TimedOut, "login failed")))
    }

    pub fn telnet_mut(&mut self) -> &mut Telnet {
        &mut self.t
    }

    /// Run `guard` ahead of every command from now on, `run`'s, `run_to`'s
    /// and `launch`'s, and the command only if it succeeds (its status is
    /// then the command's). It goes on the typed line even when the command
    /// is staged into a script, so it runs in the login shell: it can be a
    /// shell function defined there, which the script's `sh` would not know.
    pub fn guard(&mut self, guard: &str) {
        self.guard = Some(guard.to_string());
    }

    /// Read application bytes until `needle` appears or the deadline passes.
    /// Returns everything up to and including `needle`; bytes after it are kept
    /// for the next read.
    fn read_until(&mut self, needle: &[u8], deadline: Instant) -> io::Result<Vec<u8>> {
        let mut acc = std::mem::take(&mut self.residual);
        if let Some(pos) = find(&acc, needle) {
            let cut = pos + needle.len();
            self.residual = acc.split_off(cut);
            return Ok(acc);
        }
        loop {
            if Instant::now() >= deadline {
                self.residual = acc;
                return Err(timeout(&format!("timed out waiting for {:?}", String::from_utf8_lossy(needle))));
            }
            match self.t.read_app() {
                Ok(None) => {
                    self.residual = acc;
                    return Err(io::Error::new(ErrorKind::UnexpectedEof, "connection closed"));
                }
                Ok(Some(bytes)) => {
                    if bytes.is_empty() {
                        continue;
                    }
                    let search_from = acc.len().saturating_sub(needle.len() - 1);
                    acc.extend_from_slice(&bytes);
                    if let Some(rel) = find(&acc[search_from..], needle) {
                        let pos = search_from + rel;
                        let cut = pos + needle.len();
                        self.residual = acc.split_off(cut);
                        return Ok(acc);
                    }
                }
                Err(e) if e.kind() == ErrorKind::WouldBlock || e.kind() == ErrorKind::TimedOut => continue,
                Err(e) => return Err(e),
            }
        }
    }

    /// Read until any one of several needles matches; returns (index, data).
    fn read_until_any(&mut self, needles: &[&[u8]], deadline: Instant) -> io::Result<(usize, Vec<u8>)> {
        let mut acc = std::mem::take(&mut self.residual);
        loop {
            for (i, n) in needles.iter().enumerate() {
                if let Some(pos) = find(&acc, n) {
                    let cut = pos + n.len();
                    self.residual = acc.split_off(cut);
                    return Ok((i, acc));
                }
            }
            if Instant::now() >= deadline {
                self.residual = acc;
                return Err(timeout("timed out waiting for prompt"));
            }
            match self.t.read_app() {
                Ok(None) => {
                    self.residual = acc;
                    return Err(io::Error::new(ErrorKind::UnexpectedEof, "connection closed"));
                }
                Ok(Some(bytes)) => acc.extend_from_slice(&bytes),
                Err(e) if e.kind() == ErrorKind::WouldBlock || e.kind() == ErrorKind::TimedOut => {}
                Err(e) => return Err(e),
            }
        }
    }

    /// Log in as `user` and put the shell in a known clean state (no echo, no
    /// prompt), confirmed by a readiness token.
    pub fn login(&mut self, user: &str, pass: &str) -> io::Result<()> {
        // The banner and password prompt normally arrive within a second or
        // two. Occasionally the guest never answers one of them (see
        // `connect_and_login`), so keep these short; the retry loop there is
        // the recovery path, not a long wait.
        let prompt_deadline = Instant::now() + self.timing.prompt;
        // Wait for the "login:" banner. Match on the suffix so case/leading
        // text don't matter.
        self.read_until(b"ogin:", prompt_deadline)?;
        self.t.send_line(user)?;
        self.read_until(b"assword:", prompt_deadline)?;
        self.t.send_line(pass)?;
        let deadline = Instant::now() + self.timing.shell;

        // Wait for the shell prompt before sending anything. `login` flushes the
        // tty input queue when it execs the shell, so a command sent too early
        // is silently discarded. A wrong password prints "Login incorrect" and
        // never reaches a prompt. (Match the prompt, not "login:" — the motd's
        // "Last login:" line contains it on a *successful* login.)
        let (which, _banner) = self.read_until_any(&[b"# ", b"$ ", b"ncorrect"], deadline)?;
        if which == 2 {
            return Err(io::Error::new(ErrorKind::PermissionDenied, "login failed"));
        }
        // Now the shell is ready. Put it in a known clean state and confirm with
        // a readiness token. Echo is still on for this first command, so split
        // the token in the source (`RDY""…`) — the echoed command line then
        // can't be mistaken for the token's real output.
        let m = token();
        let token = format!("__RIXRDY{m}__");
        let split = format!("__RIXRDY\"\"{m}__");
        // Under X11 forwarding, export DISPLAY once here so it carries to every
        // command in this (possibly held) session; the container relay carries
        // it out to XQuartz. Bourne sh: assign then `export` separately.
        let display = if crate::config::x11_enabled() {
            format!("DISPLAY={}; export DISPLAY; ", crate::config::x11_display())
        } else {
            String::new()
        };
        let setup = format!("{display}stty -echo 2>/dev/null; PS1=; PS2=; echo {split}");
        self.t.send_line(&setup)?;
        self.read_until(token.as_bytes(), deadline)?;
        // Drop the trailing newline/empty-prompt after the token so it doesn't
        // show up as a leading blank line in the first command's output.
        self.residual.clear();
        Ok(())
    }

    /// Guest-side tty setup for an interactive session. RISC iX's telnetd
    /// ignores NAWS, so the pty is 0x0 and vi refuses visual mode with the
    /// xterm termcap entry ("Don't know enough about your terminal"); set the
    /// size with stty. The erase char is already DEL (what terminals send for
    /// Backspace) but the crt modes are off, so an erase deletes the character
    /// without visibly removing it — turn them on. Assumes `login` ran (echo
    /// off), so nothing is echoed.
    fn tty_setup(cols: u16, rows: u16) -> String {
        format!("stty rows {rows} columns {cols} crtbs crterase crtkill")
    }

    /// Turn the `login`ed shell back into a normal interactive one: tty size
    /// set, echo on, prompt restored, then `cd dir`. The shell prints the
    /// prompt afterwards. The `cd` goes last: when it fails the 1990 Bourne
    /// shell abandons the rest of the line, so nothing important may follow
    /// it — the user just sees its error and stays in $HOME.
    pub fn restore_interactive(&mut self, cols: u16, rows: u16, prompt: &str, dir: &str) -> io::Result<()> {
        let line = format!("{}; stty echo; PS1='{prompt}'; cd {dir}", Self::tty_setup(cols, rows));
        self.t.send_line(&line)
    }

    /// Launch a command on an interactive tty, framed so the pass-through can
    /// recover its exit code, then `exit` so the pty is freed. Returns the
    /// sentinel token to scan the output stream for. Assumes `login` (clean,
    /// no-echo) has run so the launch line itself isn't echoed.
    ///
    /// The user's ^C reaches the guest as a byte and the tty makes it SIGINT
    /// for the whole line. Untrapped, the 1990 Bourne shell then abandons the
    /// rest of the line — sentinel and `exit` included — and the pass-through
    /// would wait at an invisible prompt. `trap : 2` keeps the shell on the
    /// line (the epilogue reports the command's 130 and exits), while the
    /// command itself still gets the default SIGINT, since a trap with a
    /// command, unlike `trap '' 2`, is not inherited. A program that reads ^C
    /// itself (vi) keeps it; the line goes on when it exits.
    pub fn launch(&mut self, command: &str, cols: u16, rows: u16) -> io::Result<String> {
        let m = token();
        let token = format!("__RIXEXIT{m}__");
        // See `run` re: echo instead of printf. `exit` frees the pty afterwards.
        let deadline = Instant::now() + Duration::from_secs(30);
        let line = self.framed(
            "trap : 2; ",
            &format!("{}; stty echo; {command}", Self::tty_setup(cols, rows)),
            &format!("echo; echo {token}${{__rc}}; exit"),
            deadline,
        )?;
        self.t.send_line(&line)?;
        Ok(token)
    }

    /// Build `prologue{ command ; }; __rc=$?; epilogue` as a single shell
    /// line, so the epilogue can report `$__rc`; with a `guard`, `{ guard &&
    /// command ; }`. If that would exceed the
    /// tty's input limit, stage `prologue command` into a script on the
    /// guest (uuencoded over the channel, like the work-area sync) and run
    /// that instead, so the typed line stays short; the script is removed
    /// after its status is captured. The prologue goes in the script as well
    /// as on the line: the script's `sh` needs `launch`'s trap as much as the
    /// login shell does (untrapped, it gets the ^C too, and after a vi that
    /// caught it and quit cleanly it reported 208 rather than vi's 0).
    fn framed(
        &mut self,
        prologue: &str,
        command: &str,
        epilogue: &str,
        deadline: Instant,
    ) -> io::Result<String> {
        let guard = self.guard.as_deref().map(|g| format!("{g} && ")).unwrap_or_default();
        let direct = format!("{prologue}{{ {guard}{command} ; }}; __rc=$?; {epilogue}");
        if direct.len() <= MAX_TTY_LINE {
            return Ok(direct);
        }
        let path = format!("/tmp/.rix{}", token());
        let script = format!("{prologue}{command}\n");
        self.feed("uudecode", &uu::encode(&path, script.as_bytes()), deadline)?;
        Ok(format!("{prologue}{{ {guard}sh {path} ; }}; __rc=$?; rm -f {path}; {epilogue}"))
    }

    /// Run a command, returning its combined output and exit status. Echo is
    /// off by this point, so the only framing is our exit sentinel.
    pub fn run(&mut self, command: &str, deadline: Instant) -> io::Result<(Vec<u8>, i32)> {
        let mut out = Vec::new();
        let code = self.run_to(command, deadline, None, &mut |chunk| out.extend_from_slice(chunk))?;
        Ok((out, code))
    }

    /// Like `run`, but hands output to `sink` as it arrives (CRs stripped),
    /// holding back only the few bytes that could be the start of the exit
    /// sentinel. Returns the exit status.
    ///
    /// If `interrupt` becomes true while the command runs, the guest gets the
    /// tty interrupt character (^C: SIGINT to the foreground process group,
    /// exactly what a user at the console would send). The 1990 Bourne shell
    /// then abandons the rest of the line — the sentinel never comes — so a
    /// moment later we type a marker line of our own: seeing it means the
    /// shell is back at its prompt and the result is 130; seeing the original
    /// sentinel means the command finished regardless (we still drain our
    /// marker). Neither within a few seconds means the command ignored SIGINT
    /// and swallowed the marker: the error leaves the caller to drop the
    /// session, whose hangup kills the command the old-fashioned way.
    pub fn run_to(
        &mut self,
        command: &str,
        deadline: Instant,
        interrupt: Option<&AtomicBool>,
        sink: &mut dyn FnMut(&[u8]),
    ) -> io::Result<i32> {
        let m = token();
        let sentinel = format!("__RIXEXIT{m}__");
        let marker = format!("__RIXINT{m}__");
        // Run the command, then emit the sentinel with $? straight after its
        // output (no separating newline, so the output streams unaltered and
        // nothing has to be stripped). RISC iX (1990 4.3BSD) has no
        // printf(1), so use echo.
        let line = self.framed("", command, &format!("echo {sentinel}${{__rc}}"), deadline)?;
        self.t.send_line(&line)?;

        let needle = sentinel.as_bytes();
        let mneedle = marker.as_bytes();
        let mut int = Interrupt::None;
        let mut deadline = deadline;
        let mut acc = std::mem::take(&mut self.residual);
        loop {
            let sentinel_at = find(&acc, needle).map(|p| (p, Found::Sentinel));
            // The marker is only in the output once we have typed it.
            let marker_at = matches!(int, Interrupt::Marked).then(|| find(&acc, mneedle)).flatten();
            let found = match (sentinel_at, marker_at) {
                (Some((p, _)), Some(q)) if q < p => Some((q, Found::Marker)),
                (None, Some(q)) => Some((q, Found::Marker)),
                (sentinel, _) => sentinel,
            };
            if let Some((pos, which)) = found {
                let len = match which {
                    Found::Sentinel => needle.len(),
                    Found::Marker => mneedle.len(),
                };
                let rest = acc.split_off(pos + len);
                self.residual = rest;
                acc.truncate(pos);
                let data = strip_cr(&acc);
                if !data.is_empty() {
                    sink(&data);
                }
                if let Found::Marker = which {
                    // Our marker: the shell abandoned the line after ^C.
                    self.read_until(b"\n", deadline)?;
                    return Ok(130);
                }
                // The exit digits follow the sentinel, up to the next newline.
                let tail = self.read_until(b"\n", deadline)?;
                let code: i32 = String::from_utf8_lossy(&tail).trim().parse().unwrap_or(-1);
                if let Interrupt::Marked = int {
                    // The shell will run our marker line next; drain it.
                    self.read_until(mneedle, deadline)?;
                    self.read_until(b"\n", deadline)?;
                }
                return Ok(code);
            }
            // Flush everything that cannot be the start of the sentinel (or
            // of our marker, once typed).
            let marker_hold = if matches!(int, Interrupt::Marked) { hold_back(&acc, mneedle) } else { 0 };
            let hold = hold_back(&acc, needle).max(marker_hold);
            let flush = acc.len() - hold;
            if flush > 0 {
                let data = strip_cr(&acc[..flush]);
                if !data.is_empty() {
                    sink(&data);
                }
                acc.drain(..flush);
            }
            let now = Instant::now();
            if matches!(int, Interrupt::None) && interrupt.map(|f| f.load(Ordering::SeqCst)).unwrap_or(false)
            {
                self.t.send(&[0x03])?;
                int = Interrupt::Sent(now);
            } else if let Interrupt::Sent(at) = int
                && now.duration_since(at) >= self.timing.int_grace
            {
                self.t.send_line(&format!("echo {marker}"))?;
                int = Interrupt::Marked;
                deadline = deadline.min(now + self.timing.int_wait);
            }
            if now >= deadline {
                self.residual = acc;
                return Err(timeout(if matches!(int, Interrupt::None) {
                    "timed out waiting for the command to finish"
                } else {
                    "the command did not stop on interrupt"
                }));
            }
            match self.t.read_app() {
                Ok(None) => {
                    self.residual = acc;
                    return Err(io::Error::new(ErrorKind::UnexpectedEof, "connection closed"));
                }
                Ok(Some(bytes)) => acc.extend_from_slice(&bytes),
                Err(e) if e.kind() == ErrorKind::WouldBlock || e.kind() == ErrorKind::TimedOut => {}
                Err(e) => return Err(e),
            }
        }
    }

    /// Cheap liveness check for a session that has been idle: without a
    /// round trip to the guest, see whether the connection has been closed
    /// (or has unexpected bytes waiting, which a held session should never
    /// have). Returns false if the session must be discarded.
    pub fn alive(&mut self) -> bool {
        if self.t.set_nonblocking(true).is_err() {
            return false;
        }
        let mut buf = [0u8; 256];
        let r = self.t.read_raw(&mut buf);
        let _ = self.t.set_nonblocking(false);
        match r {
            // Nothing waiting: still connected and quiet.
            Err(e) if e.kind() == ErrorKind::WouldBlock => true,
            // Closed (0), stray bytes, or a socket error.
            _ => false,
        }
    }

    /// Send a line and drain until the guest closes the connection. Used for
    /// `halt`, which never returns a prompt. Returns Ok once EOF is seen or the
    /// deadline passes.
    pub fn send_and_wait_eof(&mut self, line: &str, deadline: Instant) -> io::Result<()> {
        self.t.send_line(line)?;
        while Instant::now() < deadline {
            match self.t.read_app() {
                Ok(None) => return Ok(()),
                Ok(Some(_)) => {}
                Err(e) if e.kind() == ErrorKind::WouldBlock || e.kind() == ErrorKind::TimedOut => {}
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    /// Run a guest command that consumes `data` on its stdin (e.g. `uudecode`),
    /// streaming the data over the tty and waiting for a completion marker.
    /// The command must stay silent while reading — the guest can't send to us
    /// while we're not reading it. Echo is off (post-login), so the streamed
    /// data isn't echoed back.
    pub fn feed(&mut self, command: &str, data: &[u8], deadline: Instant) -> io::Result<()> {
        self.feed_with(command, data, deadline, &mut |_, _| {})
    }

    /// `feed`, reporting `(sent, total)` bytes as the data goes down the
    /// tty (in small writes, so the figure follows the guest's consumption —
    /// see `connect`).
    pub fn feed_with(
        &mut self,
        command: &str,
        data: &[u8],
        deadline: Instant,
        progress: &mut dyn FnMut(u64, u64),
    ) -> io::Result<()> {
        let m = token();
        let marker = format!("__RIXFEED{m}__");
        self.t.send_line(&format!("{command}; echo {marker}"))?;
        let total = data.len() as u64;
        let mut sent = 0u64;
        for chunk in data.chunks(2048) {
            self.t.send(chunk)?;
            sent += chunk.len() as u64;
            progress(sent, total);
        }
        self.read_until(marker.as_bytes(), deadline)?;
        // And the marker's own newline, or it leaks into the next output.
        self.read_until(b"\n", deadline)?;
        Ok(())
    }

    /// Log out cleanly, freeing the guest pty. Best effort.
    pub fn logout(&mut self) {
        let _ = self.t.send_line("exit");
        // Drain briefly so the shell actually processes the exit before we drop
        // the socket.
        let deadline = Instant::now() + self.timing.logout;
        while Instant::now() < deadline {
            match self.t.read_app() {
                Ok(None) => break,
                Ok(Some(_)) => {}
                Err(_) => break,
            }
        }
    }
}

/// How many trailing bytes of `acc` to hold back while streaming: the longest
/// suffix that is a prefix of `needle`.
pub fn hold_back(acc: &[u8], needle: &[u8]) -> usize {
    (1..=needle.len().min(acc.len())).rev().find(|&n| acc.ends_with(&needle[..n])).unwrap_or(0)
}

/// Telnet delivers guest newlines as CR LF; collapse to LF for captured text.
fn strip_cr(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len());
    for &b in data {
        if b != b'\r' && b != 0 {
            out.push(b);
        }
    }
    out
}

/// Read the login banner without logging in — used by the healthcheck. Returns
/// true if the guest sent any bytes (telnetd + login are alive) within the
/// timeout. Closes promptly so no pty is held.
pub fn probe_banner(addr: &str, wait: Duration) -> io::Result<bool> {
    let sa = addr
        .to_socket_addrs()?
        .next()
        .ok_or_else(|| io::Error::new(ErrorKind::InvalidInput, "no address"))?;
    let stream = TcpStream::connect_timeout(&sa, wait)?;
    stream.set_read_timeout(Some(wait))?;
    let mut s = stream;
    let mut buf = [0u8; 64];
    match s.read(&mut buf) {
        Ok(n) if n > 0 => Ok(true),
        _ => Ok(false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake_guest::{FakeGuest, PASSWORD, Reply, USER};
    use std::sync::Arc;
    use std::thread;

    #[test]
    fn hold_back_keeps_only_a_sentinel_prefix() {
        let n = b"__RIXEXITabc__";
        assert_eq!(hold_back(b"one\r\n", n), 0);
        assert_eq!(hold_back(b"one", n), 0);
        assert_eq!(hold_back(b"one\r\n__RIX", n), 5);
        assert_eq!(hold_back(b"one\n_", n), 1);
        assert_eq!(hold_back(b"x__RIXEXITabc_", n), 13);
    }

    fn login(guest: &FakeGuest, pass: &str) -> io::Result<Session> {
        Session::connect_and_login_with(guest.addr(), false, 80, 24, USER, pass, Timing::TEST)
    }

    fn run(session: &mut Session, command: &str) -> (String, i32) {
        let (out, code) = session.run(command, Instant::now() + Duration::from_secs(2)).unwrap();
        (String::from_utf8(out).unwrap(), code)
    }

    /// A guest where `slow`, `hang` and `deaf` behave as their names say
    /// and `fail` exits 3.
    fn scripted() -> FakeGuest {
        FakeGuest::with(|cmd| match cmd {
            "fail" => Some(Reply::Done("oops\n".into(), 3)),
            "hang" => Some(Reply::Hang),
            "deaf" => Some(Reply::Deaf),
            "slow" => Some(Reply::Slow(Duration::from_millis(80), "done\n".into(), 0)),
            _ => None,
        })
    }

    #[test]
    fn login_leaves_a_clean_shell() {
        let guest = FakeGuest::start();
        let mut s = login(&guest, PASSWORD).unwrap();
        // No echo of the command, no prompt, no stray newline from the
        // readiness token: just the output.
        assert_eq!(run(&mut s, "echo hello"), ("hello\n".into(), 0));
        assert_eq!(run(&mut s, "false"), (String::new(), 1));
        let st = guest.state();
        assert_eq!((st.connections, st.logins), (1, 1));
        assert!(st.commands[0].starts_with("echo __RIXRDY\"\""), "{:?}", st.commands);
        assert_eq!(st.commands[1..], ["echo hello", "false"]);
    }

    #[test]
    fn wrong_password_is_not_retried() {
        let guest = FakeGuest::start();
        let e = login(&guest, "wrong").err().unwrap();
        assert_eq!(e.kind(), ErrorKind::PermissionDenied);
        assert_eq!(guest.state().connections, 1);
    }

    #[test]
    fn a_stalled_login_is_retried() {
        let guest = FakeGuest::start();
        guest.state().stall = 2;
        let mut s = login(&guest, PASSWORD).unwrap();
        assert_eq!(run(&mut s, "echo third time"), ("third time\n".into(), 0));
        let st = guest.state();
        assert_eq!((st.connections, st.logins), (3, 1));
    }

    #[test]
    fn a_login_that_always_stalls_gives_up() {
        let guest = FakeGuest::start();
        guest.state().stall = 10;
        let e = login(&guest, PASSWORD).err().unwrap();
        assert_eq!(e.kind(), ErrorKind::TimedOut);
        assert_eq!(guest.state().connections, 4);
    }

    #[test]
    fn output_and_sentinel_split_across_reads() {
        let guest = scripted();
        let mut s = guest.login();
        guest.state().chunk = Some(3);
        assert_eq!(run(&mut s, "echo one two three"), ("one two three\n".into(), 0));
        assert_eq!(run(&mut s, "fail"), ("oops\n".into(), 3));
        assert_eq!(run(&mut s, "echo after"), ("after\n".into(), 0));
    }

    #[test]
    fn run_to_streams_and_holds_back_only_a_sentinel_prefix() {
        // Output that looks like the start of a sentinel must still arrive,
        // whole and in order, and arrive in pieces as it is written.
        let guest = FakeGuest::with(|cmd| {
            (cmd == "tricky").then(|| Reply::Done("a__RIX b__RIXEXIT c__\nend".into(), 0))
        });
        let mut s = guest.login();
        guest.state().chunk = Some(4);
        let mut chunks = Vec::new();
        let code = s
            .run_to("tricky", Instant::now() + Duration::from_secs(2), None, &mut |c| chunks.push(c.to_vec()))
            .unwrap();
        assert_eq!(code, 0);
        assert_eq!(String::from_utf8(chunks.concat()).unwrap(), "a__RIX b__RIXEXIT c__\nend");
        assert!(chunks.len() > 1, "{chunks:?}");
    }

    #[test]
    fn a_long_command_is_staged_through_a_script() {
        let guest = FakeGuest::start();
        let mut s = guest.login();
        let word = "x".repeat(300);
        assert_eq!(run(&mut s, &format!("echo {word}")), (format!("{word}\n"), 0));
        let st = guest.state();
        // Nothing near the tty's 255-byte input limit was typed, and the
        // script was removed afterwards.
        assert!(st.longest_line <= MAX_TTY_LINE, "{}", st.longest_line);
        assert!(st.commands.iter().any(|c| c.starts_with("sh /tmp/.rix")), "{:?}", st.commands);
        assert!(!st.files.keys().any(|p| p.starts_with("/tmp/.rix")), "{:?}", st.files.keys());
    }

    #[test]
    fn an_interrupt_stops_the_command() {
        let guest = scripted();
        let mut s = guest.login();
        let flag = Arc::new(AtomicBool::new(false));
        let setter = Arc::clone(&flag);
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(20));
            setter.store(true, Ordering::SeqCst);
        });
        let started = Instant::now();
        let code =
            s.run_to("hang", Instant::now() + Duration::from_secs(2), Some(&flag), &mut |_| {}).unwrap();
        assert_eq!(code, 130);
        assert!(started.elapsed() < Duration::from_millis(500));
        // The marker was consumed: the session is clean for the next command.
        assert_eq!(run(&mut s, "echo next"), ("next\n".into(), 0));
    }

    #[test]
    fn a_command_that_finishes_despite_the_interrupt_keeps_its_status() {
        let guest = scripted();
        let mut s = guest.login();
        let flag = AtomicBool::new(true);
        let mut out = Vec::new();
        let code = s
            .run_to("slow", Instant::now() + Duration::from_secs(2), Some(&flag), &mut |c| {
                out.extend_from_slice(c)
            })
            .unwrap();
        assert_eq!((String::from_utf8(out).unwrap(), code), ("done\n".into(), 0));
        assert_eq!(run(&mut s, "echo next"), ("next\n".into(), 0));
    }

    #[test]
    fn a_command_that_ignores_the_interrupt_times_out() {
        let guest = scripted();
        let mut s = guest.login();
        let flag = AtomicBool::new(true);
        let started = Instant::now();
        let e =
            s.run_to("deaf", Instant::now() + Duration::from_secs(5), Some(&flag), &mut |_| {}).unwrap_err();
        assert_eq!(e.kind(), ErrorKind::TimedOut);
        assert_eq!(e.to_string(), "the command did not stop on interrupt");
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    /// What the guest sends until it hangs up or two seconds pass, telnet
    /// commands decoded away and CRs dropped.
    fn drain(session: &mut Session) -> String {
        let until = Instant::now() + Duration::from_secs(2);
        let mut out = Vec::new();
        let mut buf = [0u8; 4096];
        while Instant::now() < until {
            match session.telnet_mut().read_raw(&mut buf) {
                Ok(0) => break,
                Ok(n) => out.extend_from_slice(&session.telnet_mut().decode_into(&buf[..n]).unwrap()),
                Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {}
                Err(_) => break,
            }
        }
        String::from_utf8_lossy(&out).replace('\r', "")
    }

    #[test]
    fn a_tty_command_reports_its_status_and_logs_out() {
        let guest = scripted();
        let mut s = guest.login();
        let token = s.launch("fail", 80, 24).unwrap();
        assert!(drain(&mut s).ends_with(&format!("oops\n\n{token}3\n")));
        let st = guest.state();
        assert_eq!((st.logouts, st.dropped), (1, 0));
    }

    /// A guest where `hang …` runs until ^C and `catch …` is a vi: it takes
    /// the ^C itself and finishes later with status 0.
    fn interruptible() -> FakeGuest {
        FakeGuest::with(|cmd| match cmd.split(' ').next() {
            Some("hang") => Some(Reply::Hang),
            Some("catch") => Some(Reply::Slow(Duration::from_millis(100), "done\n".into(), 0)),
            _ => None,
        })
    }

    /// Launch `command` on a tty, type ^C as the pass-through would, and
    /// return what follows and the sentinel.
    fn launch_and_interrupt(guest: &FakeGuest, command: &str) -> (String, String) {
        let mut s = guest.login();
        let token = s.launch(command, 80, 24).unwrap();
        s.telnet_mut().send(b"\x03").unwrap();
        (drain(&mut s), token)
    }

    #[test]
    fn a_ctrl_c_in_a_tty_command_still_reaches_the_sentinel() {
        // Short, and long enough to be staged through a script, as a real
        // launch with its tty settings and `cd` guard nearly always is.
        for command in ["hang".to_string(), format!("hang {}", "x".repeat(250))] {
            let guest = interruptible();
            let (out, token) = launch_and_interrupt(&guest, &command);
            // Untrapped, the shell would abandon the line here: no sentinel,
            // no `exit`, and the pass-through would wait for ever.
            assert!(out.ends_with(&format!("\n{token}130\n")), "{out:?}");
            let st = guest.state();
            assert_eq!((st.logouts, st.dropped), (1, 0));
            assert!(st.longest_line <= MAX_TTY_LINE, "{}", st.longest_line);
        }
    }

    #[test]
    fn a_ctrl_c_caught_by_a_staged_tty_command_keeps_its_status() {
        // vi takes the ^C, the user quits, the status is vi's: the script's
        // `sh` must not die of the ^C underneath it.
        let guest = interruptible();
        let (out, token) = launch_and_interrupt(&guest, &format!("catch {}", "x".repeat(250)));
        assert!(out.ends_with(&format!("done\n\n{token}0\n")), "{out:?}");
        assert!(guest.state().commands.iter().any(|c| c.starts_with("sh /tmp/.rix")));
    }

    #[test]
    fn logout_exits_the_shell() {
        let guest = FakeGuest::start();
        let mut s = guest.login();
        s.logout();
        let st = guest.state();
        assert_eq!((st.logouts, st.dropped), (1, 0));
    }

    #[test]
    fn alive_notices_a_hangup() {
        let guest = FakeGuest::start();
        let mut s = guest.login();
        assert!(s.alive());
        guest.hangup();
        // The close reaches the client a moment after the guest makes it.
        let until = Instant::now() + Duration::from_secs(1);
        while s.alive() {
            assert!(Instant::now() < until, "the hangup never arrived");
            thread::sleep(Duration::from_millis(1));
        }
        assert!(guest.wait(|st| st.dropped == 1));
    }
}
