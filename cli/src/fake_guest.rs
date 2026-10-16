//! A scripted stand-in for the guest's telnetd and Bourne shell, so the
//! tests can drive the exec channel (session.rs, exec.rs, serve.rs) without
//! the emulator.
//!
//! It listens on 127.0.0.1 and, per connection, behaves as RISC iX's
//! 4.3BSD telnetd, `login` and `sh` do as far as the client can tell:
//! option negotiation, the banner, `login:` and `Password:`, a `# ` prompt,
//! a tty that echoes until `stty -echo`. It then runs what the client
//! types: framed commands (`{ cmd ; }; __rc=$?; echo SENTINEL${__rc}`),
//! with or without an INT trap ahead, `uudecode` feeds into an in-memory
//! file system, staged scripts, ^C and `exit`. Each command goes first to
//! the test's handler, then to builtins for `echo` and the guest side of
//! the work-area push and of `get` and `put`.
//!
//! It is a model, not an emulator: it knows the shapes of the lines this
//! client sends, not Bourne shell syntax.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};

use crate::config::Config;
use crate::session::{Session, Timing};

pub const USER: &str = "root";
pub const PASSWORD: &str = "secret";

const IAC: u8 = 255;
const SB: u8 = 250;
const SE: u8 = 240;
const WILL: u8 = 251;
const DO: u8 = 253;
/// The tty interrupt character, ^C.
const INTR: u8 = 0x03;

/// What a command does when it runs.
pub enum Reply {
    /// Prints this (each newline becomes CR LF, as the tty makes it) and
    /// exits with this status.
    Done(String, i32),
    /// Runs until interrupted: ^C kills it and the shell abandons the rest
    /// of the line, sentinel included (unless the line traps INT).
    Hang,
    /// Ignores ^C and reads its stdin, swallowing whatever is typed next.
    /// Never finishes.
    Deaf,
    /// Ignores ^C and finishes after the delay.
    Slow(Duration, String, i32),
    /// The machine halts: the connection drops with no reply.
    Halt,
}

/// What the guest has seen and holds. Tests read it, and set the knobs.
#[derive(Default)]
pub struct State {
    pub connections: usize,
    pub logins: usize,
    /// Shells that ended with `exit`.
    pub logouts: usize,
    /// Shells whose connection closed without `exit`: on the real guest,
    /// each leaks a pty until reboot.
    pub dropped: usize,
    /// Times the machine was halted.
    pub halts: usize,
    /// Commands run, in order: framed ones by their inner command, and a
    /// staged script both as `sh PATH` and as what it holds.
    pub commands: Vec<String>,
    /// The longest line typed at the shell, uuencoded data excluded.
    pub longest_line: usize,
    pub files: BTreeMap<String, Vec<u8>>,
    pub dirs: BTreeSet<String>,
    /// Stall this many of the coming logins: the banner, then no password
    /// prompt (the real guest's behaviour that `connect_and_login` retries).
    pub stall: usize,
    /// Write output in pieces of this many bytes, a moment apart, so the
    /// client reads it in pieces. Set it after logging in: the login's
    /// readiness token must arrive with its newline.
    pub chunk: Option<usize>,
    /// The host-side staging directory (`Config::stage`) the guest's NFS
    /// export maps to: a staged `get` writes its archive here, and a staged
    /// push reads its archive from here. Set by a test that stages; unused
    /// when the transfer goes down the tty.
    pub stage: std::path::PathBuf,
    /// The push's staging export cannot be mounted (its NFS server down).
    pub stage_unmountable: bool,
    /// Sessions that sourced the work area's mount functions and mounted
    /// it (`exec::ensure_nfs_mount`).
    pub mounts: usize,
    open: Vec<TcpStream>,
}

type Handler = Box<dyn Fn(&str) -> Option<Reply> + Send + Sync>;

struct Shared {
    handler: Handler,
    state: Mutex<State>,
    stop: AtomicBool,
}

impl Shared {
    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }
}

pub struct FakeGuest {
    addr: String,
    shared: Arc<Shared>,
}

impl FakeGuest {
    /// A guest that knows only the builtins.
    pub fn start() -> FakeGuest {
        FakeGuest::with(|_| None)
    }

    /// A guest whose commands go to `handler` first; `None` from it falls
    /// through to the builtins.
    pub fn with(handler: impl Fn(&str) -> Option<Reply> + Send + Sync + 'static) -> FakeGuest {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind the fake guest");
        let addr = listener.local_addr().expect("fake guest address").to_string();
        let shared = Arc::new(Shared {
            handler: Box::new(handler),
            state: Mutex::default(),
            stop: AtomicBool::new(false),
        });
        let accept = Arc::clone(&shared);
        thread::spawn(move || {
            for conn in listener.incoming() {
                if accept.stop.load(Ordering::SeqCst) {
                    break;
                }
                let Ok(stream) = conn else { continue };
                let shared = Arc::clone(&accept);
                thread::spawn(move || {
                    let _ = Conn::new(shared, stream).and_then(|mut c| c.run());
                });
            }
        });
        FakeGuest { addr, shared }
    }

    pub fn addr(&self) -> &str {
        &self.addr
    }

    pub fn config(&self) -> Config {
        Config {
            telnet_addr: self.addr.clone(),
            user: USER.into(),
            pass: PASSWORD.into(),
            ..Config::default()
        }
    }

    pub fn state(&self) -> MutexGuard<'_, State> {
        self.shared.state()
    }

    /// A logged-in non-tty session with the test timings.
    pub fn login(&self) -> Session {
        Session::connect_and_login_with(&self.addr, false, 80, 24, USER, PASSWORD, Timing::TEST)
            .expect("log in to the fake guest")
    }

    /// Wait up to a second for `cond` to hold of the state (for what the
    /// guest learns asynchronously, such as a client hanging up).
    pub fn wait(&self, cond: impl Fn(&State) -> bool) -> bool {
        let until = Instant::now() + Duration::from_secs(1);
        while Instant::now() < until {
            if cond(&self.state()) {
                return true;
            }
            thread::sleep(Duration::from_millis(2));
        }
        cond(&self.state())
    }

    /// Drop every connection, as a guest reboot would.
    pub fn hangup(&self) {
        for s in self.state().open.drain(..) {
            let _ = s.shutdown(Shutdown::Both);
        }
    }
}

impl Drop for FakeGuest {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::SeqCst);
        // Wake the accept loop so it sees the flag.
        let _ = TcpStream::connect(&self.addr);
        self.hangup();
    }
}

/// One connection: telnetd, login and then the shell.
struct Conn {
    shared: Arc<Shared>,
    stream: TcpStream,
    /// Bytes typed and not yet read, telnet commands removed.
    input: Vec<u8>,
    echo: bool,
    ps1: String,
    logged_out: bool,
}

impl Conn {
    fn new(shared: Arc<Shared>, stream: TcpStream) -> io::Result<Conn> {
        stream.set_nodelay(true)?;
        shared.state().open.push(stream.try_clone()?);
        Ok(Conn { shared, stream, input: Vec::new(), echo: true, ps1: String::new(), logged_out: false })
    }

    fn run(&mut self) -> io::Result<()> {
        let stall = {
            let mut st = self.shared.state();
            st.connections += 1;
            let stall = st.stall > 0;
            st.stall = st.stall.saturating_sub(1);
            stall
        };
        // As telnetd opens: ask for the terminal type, offer to echo and to
        // suppress go-aheads.
        self.stream.write_all(&[IAC, DO, 24, IAC, WILL, 1, IAC, WILL, 3])?;
        self.write("\n\nRISC iX (fake guest)\n\nlogin: ")?;
        loop {
            if self.line()?.is_none() {
                return Ok(());
            }
            if stall {
                // Never prompt; hold the connection until the client gives up.
                while self.fill()? {}
                return Ok(());
            }
            self.write("Password:")?;
            self.echo = false;
            let Some(pass) = self.line()? else { return Ok(()) };
            self.echo = true;
            if pass == PASSWORD {
                break;
            }
            self.write("\nLogin incorrect\nlogin: ")?;
        }
        self.shared.state().logins += 1;
        self.ps1 = "# ".into();
        self.write("\nLast login: Mon Oct 12 10:00:00 on ttyp0\nRISC iX release 1.21\n# ")?;
        let result = self.shell();
        if !self.logged_out {
            self.shared.state().dropped += 1;
        }
        result
    }

    fn shell(&mut self) -> io::Result<()> {
        while let Some(line) = self.line()? {
            {
                let mut st = self.shared.state();
                st.longest_line = st.longest_line.max(line.len());
            }
            if !self.execute(&line)? || self.logged_out {
                return Ok(());
            }
            let ps1 = self.ps1.clone();
            self.write(&ps1)?;
        }
        Ok(())
    }

    /// Run one typed line. False once the shell has exited.
    fn execute(&mut self, line: &str) -> io::Result<bool> {
        // An INT trap ahead of the framing (a tty launch): ^C still ends the
        // command, but the shell runs the trap and goes on with the line,
        // `$?` 130, instead of abandoning it.
        let (trapped, line) = match line.strip_prefix("trap : 2; ") {
            Some(rest) => (true, rest),
            None => (false, line),
        };
        // A framed command: `{ cmd ; }; __rc=$?; epilogue`.
        if let Some((inner, epilogue)) = line.strip_prefix("{ ").and_then(|l| l.split_once(" ; }; __rc=$?; "))
        {
            // The work area's mount guard (`exec::ensure_nfs_mount`): the
            // share is always mounted here.
            let inner = match inner.strip_prefix("__rixmnt && ") {
                Some(rest) => {
                    self.shared.state().commands.push("__rixmnt".into());
                    rest
                }
                None => inner,
            };
            let inner = self.tty_settings(inner);
            let rc = match self.command(inner)? {
                Some(rc) => rc,
                None if trapped && !self.logged_out => 130,
                None => return Ok(true), // interrupted: the rest of the line is abandoned
            };
            for part in epilogue.split("; ") {
                if part == "exit" {
                    return self.logout();
                } else if let Some(path) = part.strip_prefix("rm -f ") {
                    self.shared.state().files.remove(path);
                } else if part == "echo" {
                    self.write("\n")?;
                } else if let Some(text) = part.strip_prefix("echo ") {
                    self.write(&format!("{}\n", text.replace("${__rc}", &rc.to_string())))?;
                }
            }
            return Ok(true);
        }
        // A feed: `[cd DIR && ]uudecode; echo MARKER`, then the data.
        if let Some((cmd, marker)) = line.rsplit_once("; echo ")
            && cmd.ends_with("uudecode")
            && marker.starts_with("__RIXFEED")
        {
            self.shared.state().commands.push(cmd.to_string());
            self.uudecode(cmd)?;
            self.write(&format!("{marker}\n"))?;
            return Ok(true);
        }
        // Anything else (the login's setup line, the interrupt marker,
        // `exit`): simple commands separated by `; `.
        for part in line.split("; ") {
            let part = part.trim().trim_end_matches(" 2>/dev/null");
            match part {
                "exit" => return self.logout(),
                "stty -echo" => self.echo = false,
                "stty echo" => self.echo = true,
                _ if part.starts_with("PS1=") => self.ps1 = words(&part[4..]).pop().unwrap_or_default(),
                // Tty and shell settings the model has no use for.
                _ if ["stty ", "PS2=", "DISPLAY=", "export ", "cd "].iter().any(|p| part.starts_with(p)) => {}
                _ => {
                    if self.command(part)?.is_none() {
                        break;
                    }
                }
            }
        }
        Ok(true)
    }

    fn logout(&mut self) -> io::Result<bool> {
        self.shared.state().logouts += 1;
        self.logged_out = true;
        let _ = self.stream.shutdown(Shutdown::Both);
        Ok(false)
    }

    /// Skip the tty settings a launch puts ahead of its command
    /// (`stty rows … crtkill; stty echo; cmd`), applying the echo.
    fn tty_settings<'a>(&mut self, mut text: &'a str) -> &'a str {
        while let Some((first, rest)) = text.split_once("; ")
            && first.starts_with("stty ")
        {
            self.echo = self.echo || first == "stty echo";
            text = rest;
        }
        text
    }

    /// Whether a ^C has been typed (taking it if so), looking at what has
    /// already arrived without waiting for more.
    fn take_intr(&mut self) -> io::Result<bool> {
        self.stream.set_nonblocking(true)?;
        let _ = self.fill();
        self.stream.set_nonblocking(false)?;
        let Some(i) = self.input.iter().position(|&b| b == INTR) else { return Ok(false) };
        self.input.drain(..=i);
        Ok(true)
    }

    /// Run a command; its exit status, or None if ^C ended it.
    fn command(&mut self, cmd: &str) -> io::Result<Option<i32>> {
        self.shared.state().commands.push(cmd.to_string());
        // A staged script: run what it holds. The script's `sh` gets the ^C
        // too: one that traps INT goes on (to its end: the command was its
        // last), one that doesn't dies of it, even when the command caught
        // the ^C itself and is still running.
        if let Some(path) = cmd.strip_prefix("sh ") {
            let script = self.shared.state().files.get(path).cloned();
            let Some(script) = script else {
                self.write(&format!("{path}: cannot open\n"))?;
                return Ok(Some(2));
            };
            let text = String::from_utf8_lossy(&script).trim_end_matches('\n').to_string();
            let (trapped, body) = match text.strip_prefix("trap : 2; ") {
                Some(rest) => (true, rest),
                None => (false, text.as_str()),
            };
            let body = self.tty_settings(body);
            let rc = self.command(body)?;
            if self.logged_out {
                return Ok(None);
            }
            return Ok(match (trapped, rc) {
                (true, rc) => Some(rc.unwrap_or(130)),
                (false, None) => None,
                (false, Some(rc)) => (!self.take_intr()?).then_some(rc),
            });
        }
        let reply = (self.shared.handler)(cmd).unwrap_or_else(|| self.builtin(cmd));
        match reply {
            Reply::Done(out, rc) => {
                self.write(&out)?;
                Ok(Some(rc))
            }
            Reply::Hang => loop {
                if let Some(i) = self.input.iter().position(|&b| b == INTR) {
                    // SIGINT; the tty flushes what was typed before it.
                    self.input.drain(..=i);
                    return Ok(None);
                }
                if !self.fill()? {
                    return Ok(None);
                }
            },
            Reply::Deaf => {
                while self.fill()? {
                    self.input.clear();
                }
                Ok(None)
            }
            Reply::Halt => {
                self.shared.state().halts += 1;
                // The shell goes with the machine; no pty is left behind.
                self.logged_out = true;
                let _ = self.stream.shutdown(Shutdown::Both);
                Ok(None)
            }
            Reply::Slow(delay, out, rc) => {
                thread::sleep(delay);
                self.write(&out)?;
                Ok(Some(rc))
            }
        }
    }

    /// The commands the client itself sends: `echo`, and the guest side of
    /// the work-area push (sync.rs).
    fn builtin(&self, cmd: &str) -> Reply {
        let done = |out: String, rc| Reply::Done(out, rc);
        let mut st = self.shared.state();
        // The push's first step: make the root, read the epoch marker.
        if let (Some(rest), Some(marker)) = (cmd.strip_prefix("test -d "), between(cmd, "if test -f ", ";")) {
            st.dirs.insert(rest.split(' ').next().unwrap_or_default().to_string());
            let out = st.files.get(marker).map(|m| String::from_utf8_lossy(m).into_owned());
            return done(out.unwrap_or_else(|| "-\n".into()), 0);
        }
        // Stamping the epoch.
        if let Some((text, path)) = cmd.strip_prefix("echo ").and_then(|r| r.split_once(" > ")) {
            st.files.insert(path.to_string(), format!("{text}\n").into_bytes());
            return done(String::new(), 0);
        }
        if let Some(text) = cmd.strip_prefix("echo ") {
            return done(format!("{}\n", text.replace("\"\"", "")), 0);
        }
        // What a path is, for `get` and `put`: `if test -d P; then echo d;
        // elif test -r P; then echo f; else echo -; fi`.
        if let Some(path) = between(cmd, "if test -d ", "; then echo d;") {
            let path = words(path).pop().unwrap_or_default();
            let kind = if is_dir(&st, &path) {
                "d"
            } else if st.files.contains_key(&path) {
                "f"
            } else {
                "-"
            };
            return done(format!("{kind}\n"), 0);
        }
        // Sizing a file for `get`: `wc -c < P`.
        if let Some(path) = cmd.strip_prefix("wc -c < ") {
            let path = words(path).pop().unwrap_or_default();
            return match st.files.get(&path) {
                Some(data) => done(format!("{:8}\n", data.len()), 0),
                None => done(format!("{path}: cannot open\n"), 1),
            };
        }
        // Sizing a directory for `get`: `ls -lR P | awk '{s+=$4} END {print s}'`.
        if let Some(path) = between(cmd, "ls -lR ", " | awk ") {
            let prefix = format!("{}/", words(path).pop().unwrap_or_default());
            let total: usize =
                st.files.iter().filter(|(p, _)| p.starts_with(&prefix)).map(|(_, d)| d.len()).sum();
            return done(format!("{total}\n"), 0);
        }
        // Renaming what `put` unpacked: `cd DIR && mv -f FROM TO`.
        if let Some((dir, names)) = cmd.strip_prefix("cd ").and_then(|r| r.split_once(" && mv -f ")) {
            let dir = words(dir).pop().unwrap_or_default();
            let [from, to] = <[String; 2]>::try_from(words(names)).expect("mv -f FROM TO");
            let (from, to) = (format!("{dir}/{from}"), format!("{dir}/{to}"));
            let moved = |p: &str| {
                (p == from || p.starts_with(&format!("{from}/"))).then(|| format!("{to}{}", &p[from.len()..]))
            };
            let files: Vec<_> = st.files.keys().filter_map(|p| Some((p.clone(), moved(p)?))).collect();
            let dirs: Vec<_> = st.dirs.iter().filter_map(|p| Some((p.clone(), moved(p)?))).collect();
            if files.is_empty() && dirs.is_empty() {
                return done(format!("mv: {from}: No such file or directory\n"), 1);
            }
            for (old, new) in files {
                let data = st.files.remove(&old).unwrap_or_default();
                st.files.insert(new, data);
            }
            for (old, new) in dirs {
                st.dirs.remove(&old);
                st.dirs.insert(new);
            }
            return done(String::new(), 0);
        }
        // Sourcing the work area's mount functions, and mounting it:
        // `test -f FILE && . FILE && rm -f FILE && __rixmount`.
        if let (Some(file), true) = (between(cmd, "test -f ", " && . "), cmd.ends_with(" && __rixmount")) {
            if st.files.remove(file).is_none() {
                return done(String::new(), 1);
            }
            st.mounts += 1;
            return done(String::new(), 0);
        }
        // Clearing the work area (`sync --clear`).
        if cmd.starts_with("cd /; if mount | grep ") {
            if let Some(dir) = between(cmd, "rm -rf ", ";") {
                let prefix = format!("{dir}/");
                st.files.retain(|p, _| !p.starts_with(&prefix));
                st.dirs.retain(|p| p != dir && !p.starts_with(&prefix));
            }
            return done(String::new(), 0);
        }
        // Mounting the transfers' staging export: the directory the test set
        // as the host side of the export (`State::stage`, matching `cfg.stage`).
        let stage = st.stage.clone();
        if cmd.starts_with("mount | grep /tmp/.xfer ") {
            if st.stage_unmountable {
                return done("mount: RPC: Timed out\n".into(), 1);
            }
            return done(String::new(), 0);
        }
        // Archiving for `get`: `cd DIR && find NAME -print | cpio -o`, into
        // the export (`> /tmp/.xfer/FILE`) or up the tty (`(… | uuencode x)
        // 2>SAID; cat SAID; rm -f SAID`, what it said after the encoding).
        if let (Some(rest), Some(name)) =
            (cmd.strip_prefix("cd "), between(cmd, "find ", " -print | cpio -o"))
        {
            let dir = words(rest.split(" && ").next().unwrap_or_default()).pop().unwrap_or_default();
            let (archive, said) = cpio(&st, &dir, &words(name).pop().unwrap_or_default());
            if let Some(file) = cmd.split("> /tmp/.xfer/").nth(1) {
                std::fs::write(stage.join(file), archive).expect("the test's stage directory");
                return done(said, 0);
            }
            let mut out = String::from_utf8(crate::uu::encode("x", &archive)).expect("uuencoded text");
            out.push_str(&said);
            return done(out, 0);
        }
        // Unpacking a staged archive, after saying if it is not there:
        // `test -f /tmp/.xfer/NAME || echo WORD; cd DIR && tar xf /tmp/.xfer/NAME`.
        if let (Some(name), Some(dir)) =
            (between(cmd, "test -f /tmp/.xfer/", " "), between(cmd, "; cd ", " && "))
        {
            let dir = words(dir).pop().unwrap_or_default();
            let Ok(tar) = std::fs::read(stage.join(name)) else {
                let said = between(cmd, "|| echo ", ";").unwrap_or_default();
                return done(format!("{said}\ntar: /tmp/.xfer/{name}: cannot open\n"), 1);
            };
            for (entry, data) in untar(&tar) {
                let full = format!("{dir}/{entry}");
                match data {
                    Some(d) => {
                        st.files.insert(full, d);
                    }
                    None => {
                        st.dirs.insert(full);
                    }
                }
            }
            return done(String::new(), 0);
        }
        // Unpacking a pushed archive.
        let unpack = " && wc -c < wa.tar && tar xf wa.tar && rm -f wa.tar";
        if let Some(dir) = cmd.strip_prefix("cd ").and_then(|r| r.strip_suffix(unpack)) {
            let dir = words(dir).pop().unwrap_or_default();
            let path = format!("{dir}/wa.tar");
            let Some(tar) = st.files.remove(&path) else {
                return done(format!("{path}: cannot open\n"), 1);
            };
            for (name, data) in untar(&tar) {
                let full = format!("{dir}/{name}");
                match data {
                    Some(d) => {
                        st.files.insert(full, d);
                    }
                    None => {
                        st.dirs.insert(full);
                    }
                }
            }
            return done(format!("{:8}\n", tar.len()), 0);
        }
        // Deleting what the host no longer has: `cd DIR && rm -f ./a …;
        // rmdir ./d 2>/dev/null; true`.
        if let (Some(rest), true) = (cmd.strip_prefix("cd "), cmd.ends_with("; true")) {
            let dir = rest.split([' ', ';']).next().unwrap_or_default();
            for word in words(rest) {
                if let Some(p) = word.trim_end_matches(';').strip_prefix("./") {
                    let full = format!("{dir}/{p}");
                    st.files.remove(&full);
                    st.dirs.remove(&full);
                }
            }
            return done(String::new(), 0);
        }
        match cmd {
            "true" | ":" | "sync" => done(String::new(), 0),
            "halt" => Reply::Halt,
            "false" => done(String::new(), 1),
            _ => done(format!("sh: {cmd}: not found\n"), 127),
        }
    }

    /// Take a feed's uuencoded data, up to `end`, into the file it names
    /// (relative to the `cd` before `uudecode`, if any).
    fn uudecode(&mut self, cmd: &str) -> io::Result<()> {
        let dir = cmd.strip_prefix("cd ").and_then(|r| r.split_once(" && ")).map(|(d, _)| d.to_string());
        let mut text = String::new();
        let mut name = None;
        while let Some(line) = self.line()? {
            if let Some(rest) = line.strip_prefix("begin ") {
                name = rest.split(' ').nth(1).map(str::to_string);
            }
            text.push_str(&line);
            text.push('\n');
            if line == "end" {
                break;
            }
        }
        if let Some(name) = name {
            let path = match (name.starts_with('/'), dir) {
                (false, Some(d)) => format!("{}/{name}", words(&d).pop().unwrap_or_default()),
                _ => name,
            };
            self.shared.state().files.insert(path, crate::uu::decode(text.as_bytes()));
        }
        Ok(())
    }

    /// The next line typed, without its end; None at EOF. A ^C discards
    /// what was typed before it on the line, as the tty does.
    fn line(&mut self) -> io::Result<Option<String>> {
        loop {
            if let Some(i) = self.input.iter().position(|&b| b == b'\r' || b == b'\n') {
                let raw: Vec<u8> = self.input.drain(..=i).collect();
                let mut text = &raw[..i];
                if let Some(j) = text.iter().rposition(|&b| b == INTR) {
                    text = &text[j + 1..];
                }
                if text.is_empty() {
                    continue;
                }
                let line = String::from_utf8_lossy(text).into_owned();
                if self.echo {
                    self.write(&format!("{line}\n"))?;
                }
                return Ok(Some(line));
            }
            if !self.fill()? {
                return Ok(None);
            }
        }
    }

    /// Read more from the client, dropping telnet commands (the client
    /// writes each of its replies whole). False at EOF.
    fn fill(&mut self) -> io::Result<bool> {
        let mut buf = [0u8; 4096];
        let n = match self.stream.read(&mut buf) {
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => return Ok(true),
            Err(_) => 0,
        };
        let mut i = 0;
        while i < n {
            match (buf[i], buf.get(i + 1)) {
                (IAC, Some(&IAC)) => {
                    self.input.push(IAC);
                    i += 2;
                }
                (IAC, Some(&SB)) => {
                    let end = buf[i..n].windows(2).position(|w| w == [IAC, SE]).map_or(n, |p| i + p + 2);
                    i = end;
                }
                (IAC, _) => i += 3,
                (b, _) => {
                    self.input.push(b);
                    i += 1;
                }
            }
        }
        Ok(n > 0)
    }

    fn write(&mut self, text: &str) -> io::Result<()> {
        let bytes = text.replace('\n', "\r\n").into_bytes();
        let chunk = self.shared.state().chunk;
        match chunk {
            None => self.stream.write_all(&bytes),
            Some(n) => {
                for piece in bytes.chunks(n.max(1)) {
                    self.stream.write_all(piece)?;
                    thread::sleep(Duration::from_millis(1));
                }
                Ok(())
            }
        }
    }
}

/// Whether the guest has `path` as a directory: `/`, one it was given, or
/// one implied by something in it.
fn is_dir(st: &State, path: &str) -> bool {
    let prefix = format!("{}/", path.trim_end_matches('/'));
    prefix == "/"
        || st.dirs.contains(path)
        || st.dirs.iter().any(|d| d.starts_with(&prefix))
        || st.files.keys().any(|f| f.starts_with(&prefix))
}

/// The text between `start` and the next `end` in `s`.
fn between<'a>(s: &'a str, start: &str, end: &str) -> Option<&'a str> {
    let rest = &s[s.find(start)? + start.len()..];
    Some(&rest[..rest.find(end).unwrap_or(rest.len())])
}

/// Split `s` into shell words, honouring single quotes (as `shell_quote`
/// writes them, `'\''` included).
fn words(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut word = String::new();
    let mut quoted = false;
    let mut any = false;
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        match c {
            '\'' => {
                quoted = !quoted;
                any = true;
            }
            '\\' if !quoted => {
                if let Some(n) = chars.next() {
                    word.push(n);
                    any = true;
                }
            }
            c if c.is_whitespace() && !quoted => {
                if any {
                    out.push(std::mem::take(&mut word));
                    any = false;
                }
            }
            c => {
                word.push(c);
                any = true;
            }
        }
    }
    if any {
        out.push(word);
    }
    out
}

/// The guest's `find NAME -print | cpio -o` in `dir`, of the files and
/// directories `st` holds: the archive, and what `find` and `cpio` say. As
/// the real ones, `find` leaves out a path of over about 780 bytes, saying
/// so with no newline, and cpio counts its blocks.
fn cpio(st: &State, dir: &str, name: &str) -> (Vec<u8>, String) {
    let prefix = format!("{dir}/{name}");
    let within = |p: &str| p == prefix || p.starts_with(&format!("{prefix}/"));
    let rel = |p: &str| p[dir.len() + 1..].to_string();
    // Each directory, given or implied by a file in it, and each file.
    let mut entries: BTreeMap<String, Option<&Vec<u8>>> = BTreeMap::new();
    for (path, data) in st.files.iter().filter(|(p, _)| within(p)) {
        let path = rel(path);
        for (j, _) in path.match_indices('/') {
            entries.insert(path[..j].to_string(), None);
        }
        entries.insert(path, Some(data));
    }
    for d in st.dirs.iter().filter(|d| within(d)) {
        entries.insert(rel(d), None);
    }
    let mut said = String::new();
    if entries.is_empty() {
        said.push_str(&format!("find: {name}: No such file or directory\n"));
    }
    let mut members = Vec::new();
    for (ino, (path, data)) in entries.into_iter().enumerate() {
        if path.len() > 780 {
            said.push_str("Pathname too long");
            continue;
        }
        let (mode, data) = match data {
            Some(d) => (0o100644, d.clone()),
            None => (0o040755, Vec::new()),
        };
        members.push(crate::sync::CpioMember {
            name: path.into_bytes(),
            mode,
            uid: 0,
            gid: 0,
            mtime: 1_700_000_000,
            dev: 0xc00,
            ino: ino as u16 + 1,
            nlink: 1,
            data,
        });
    }
    let archive = crate::sync::write_cpio(&members);
    said.push_str(&format!("{} blocks\n", archive.len().div_ceil(512)));
    (archive, said)
}

/// The members of a tar: names without `./`, and the contents of files
/// (None for directories). Reads it as the guest's 1990 tar does: ustar's
/// prefix field, but not GNU long names or pax records, which it takes for
/// files (`././@LongLink`, `PaxHeader/…`) before a member whose name is
/// cut short.
fn untar(tar: &[u8]) -> Vec<(String, Option<Vec<u8>>)> {
    let mut out = Vec::new();
    let mut i = 0;
    while i + 512 <= tar.len() {
        let h = &tar[i..i + 512];
        if h.iter().all(|&b| b == 0) {
            break;
        }
        let field = |a: usize, b: usize| {
            let f = &h[a..b];
            String::from_utf8_lossy(&f[..f.iter().position(|&c| c == 0).unwrap_or(f.len())]).into_owned()
        };
        let size = usize::from_str_radix(field(124, 136).trim(), 8).unwrap_or(0);
        let data = tar[(i + 512).min(tar.len())..(i + 512 + size).min(tar.len())].to_vec();
        i += 512 + size.div_ceil(512) * 512;
        let mut name = field(0, 100);
        // POSIX ustar only: GNU's own format keeps other fields there.
        if &h[257..263] == b"ustar\0" && h[345] != 0 {
            name = format!("{}/{name}", field(345, 500));
        }
        let name = name.trim_start_matches("./").trim_end_matches('/').to_string();
        if !name.is_empty() && name != "." {
            out.push((name, if h[156] == b'5' { None } else { Some(data) }));
        }
    }
    out
}
