//! Minimal telnet client layer over a TCP stream.
//!
//! RISC iX runs a stock 4.3BSD `telnetd`. On connect it negotiates a handful
//! of options (ECHO, SGA, and — if we let it — TTYPE/NAWS) before `login`
//! prints its banner. This layer answers that negotiation, strips IAC command
//! sequences out of the byte stream, and hands the caller only the application
//! bytes. It also escapes outbound 0xFF and can announce a window size for the
//! interactive (tty) path.
//!
//! The policy is deliberately asymmetric: we are the client of a remote shell,
//! so we want the *server* to echo and to suppress go-aheads (character-at-a-
//! time), and we refuse everything else unless we are in tty mode, where we
//! also advertise a terminal type and window size so curses programs work.

use std::collections::HashSet;
use std::io::{self, Read, Write};
use std::net::TcpStream;

// Telnet command bytes (RFC 854) and the options we care about (RFC 855+).
const IAC: u8 = 255;
const DONT: u8 = 254;
const DO: u8 = 253;
const WONT: u8 = 252;
const WILL: u8 = 251;
const SB: u8 = 250;
const SE: u8 = 240;

const OPT_ECHO: u8 = 1;
const OPT_SGA: u8 = 3;
const OPT_TTYPE: u8 = 24;
const OPT_NAWS: u8 = 31;

const TTYPE_IS: u8 = 0;
const TTYPE_SEND: u8 = 1;

enum State {
    Data,
    Iac,
    Neg(u8), // saw IAC WILL/WONT/DO/DONT, awaiting option byte
    Sb,      // inside subnegotiation, collecting until IAC SE
    SbIac,   // saw IAC while inside a subnegotiation
}

/// Generic over the stream so the protocol can be tested without a socket;
/// the real one is always a `TcpStream`.
pub struct Telnet<S = TcpStream> {
    stream: S,
    tty: bool,
    cols: u16,
    rows: u16,
    state: State,
    sb: Vec<u8>,
    // Only ever answer a given option once per command type, so we never enter
    // a negotiation loop with the peer.
    answered_do: HashSet<u8>,
    answered_will: HashSet<u8>,
}

impl<S: Read + Write> Telnet<S> {
    pub fn new(stream: S, tty: bool, cols: u16, rows: u16) -> Self {
        Telnet {
            stream,
            tty,
            cols,
            rows,
            state: State::Data,
            sb: Vec::new(),
            answered_do: HashSet::new(),
            answered_will: HashSet::new(),
        }
    }

    /// Read undecoded bytes straight from the socket (interactive poll loop).
    pub fn read_raw(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.stream.read(buf)
    }

    /// Send application bytes, escaping any literal 0xFF as IAC IAC.
    pub fn send(&mut self, data: &[u8]) -> io::Result<()> {
        let mut out = Vec::with_capacity(data.len());
        for &b in data {
            if b == IAC {
                out.push(IAC);
            }
            out.push(b);
        }
        trace("tx", &out);
        self.stream.write_all(&out)
    }

    /// Send a line to the remote login tty. Terminated with a bare CR: the
    /// guest tty runs in canonical mode with ICRNL, so CR maps to one newline
    /// — sending CR LF as well would inject a spurious empty line.
    pub fn send_line(&mut self, line: &str) -> io::Result<()> {
        self.send(line.as_bytes())?;
        trace("tx", b"\r");
        self.stream.write_all(b"\r")
    }

    /// Read a chunk and decode it. Returns the application bytes; an empty Vec
    /// means the chunk was pure negotiation. `Ok(None)` is EOF.
    pub fn read_app(&mut self) -> io::Result<Option<Vec<u8>>> {
        let mut buf = [0u8; 4096];
        let n = self.stream.read(&mut buf)?;
        if n == 0 {
            trace("rx", b"<EOF>");
            return Ok(None);
        }
        trace("rx", &buf[..n]);
        let mut app = Vec::with_capacity(n);
        let mut replies: Vec<u8> = Vec::new();
        for &b in &buf[..n] {
            self.step(b, &mut app, &mut replies);
        }
        if !replies.is_empty() {
            trace("tx", &replies);
            self.stream.write_all(&replies)?;
        }
        Ok(Some(app))
    }

    /// Decode a buffer that the caller already read from the socket (used by
    /// the interactive poll loop, which owns the fd). Any negotiation replies
    /// are written back to the socket; application bytes are returned.
    pub fn decode_into(&mut self, input: &[u8]) -> io::Result<Vec<u8>> {
        let mut app = Vec::with_capacity(input.len());
        let mut replies: Vec<u8> = Vec::new();
        for &b in input {
            self.step(b, &mut app, &mut replies);
        }
        if !replies.is_empty() {
            self.stream.write_all(&replies)?;
        }
        Ok(app)
    }

    fn step(&mut self, b: u8, app: &mut Vec<u8>, replies: &mut Vec<u8>) {
        match self.state {
            State::Data => {
                if b == IAC {
                    self.state = State::Iac;
                } else {
                    app.push(b);
                }
            }
            State::Iac => match b {
                IAC => {
                    app.push(IAC);
                    self.state = State::Data;
                }
                WILL | WONT | DO | DONT => self.state = State::Neg(b),
                SB => {
                    self.sb.clear();
                    self.state = State::Sb;
                }
                _ => self.state = State::Data, // GA, NOP, etc. — ignore
            },
            State::Neg(cmd) => {
                self.negotiate(cmd, b, replies);
                self.state = State::Data;
            }
            State::Sb => {
                if b == IAC {
                    self.state = State::SbIac;
                } else {
                    self.sb.push(b);
                }
            }
            State::SbIac => {
                if b == SE {
                    self.subnegotiation(replies);
                    self.state = State::Data;
                } else {
                    // IAC IAC inside SB is a literal 0xFF.
                    self.sb.push(b);
                    self.state = State::Sb;
                }
            }
        }
    }

    fn negotiate(&mut self, cmd: u8, opt: u8, replies: &mut Vec<u8>) {
        match cmd {
            DO => {
                if !self.answered_do.insert(opt) {
                    return;
                }
                match opt {
                    OPT_SGA => reply(replies, WILL, OPT_SGA),
                    OPT_TTYPE if self.tty => reply(replies, WILL, OPT_TTYPE),
                    OPT_NAWS if self.tty => {
                        reply(replies, WILL, OPT_NAWS);
                        self.push_naws(replies);
                    }
                    _ => reply(replies, WONT, opt),
                }
            }
            WILL => {
                if !self.answered_will.insert(opt) {
                    return;
                }
                match opt {
                    // Let the server echo and run character-at-a-time.
                    OPT_ECHO => reply(replies, DO, OPT_ECHO),
                    OPT_SGA => reply(replies, DO, OPT_SGA),
                    _ => reply(replies, DONT, opt),
                }
            }
            // Server refusing/denying — nothing to keep asking for.
            _ => {}
        }
    }

    fn subnegotiation(&mut self, replies: &mut Vec<u8>) {
        // Only TTYPE SEND needs a reply: answer with a terminal type.
        if self.sb.len() >= 2 && self.sb[0] == OPT_TTYPE && self.sb[1] == TTYPE_SEND {
            let term = guest_term();
            replies.extend_from_slice(&[IAC, SB, OPT_TTYPE, TTYPE_IS]);
            replies.extend_from_slice(term.as_bytes());
            replies.extend_from_slice(&[IAC, SE]);
        }
    }

    fn push_naws(&mut self, replies: &mut Vec<u8>) {
        let (c, r) = (self.cols, self.rows);
        replies.extend_from_slice(&[IAC, SB, OPT_NAWS]);
        for byte in [(c >> 8) as u8, c as u8, (r >> 8) as u8, r as u8] {
            if byte == IAC {
                replies.push(IAC);
            }
            replies.push(byte);
        }
        replies.extend_from_slice(&[IAC, SE]);
    }

    /// Announce a new window size mid-session (on SIGWINCH).
    pub fn send_naws(&mut self, cols: u16, rows: u16) -> io::Result<()> {
        self.cols = cols;
        self.rows = rows;
        let mut replies = Vec::new();
        self.push_naws(&mut replies);
        self.stream.write_all(&replies)
    }
}

impl Telnet<TcpStream> {
    pub fn get_ref(&self) -> &TcpStream {
        &self.stream
    }

    pub fn set_nonblocking(&self, nb: bool) -> io::Result<()> {
        self.stream.set_nonblocking(nb)
    }
}

fn reply(replies: &mut Vec<u8>, cmd: u8, opt: u8) {
    replies.extend_from_slice(&[IAC, cmd, opt]);
}

/// Terminal type to advertise to the guest. RISC iX's 1990 termcap knows
/// `xterm` and `vt100` but not modern names like `xterm-256color`, and with an
/// unknown TERM vi refuses visual mode. So collapse any xterm variant to plain
/// `xterm` and fall back to `vt100` for everything else. `DOCKERIX_TERM`
/// overrides (e.g. `xterms` for the small-screen entry).
pub fn guest_term() -> String {
    if let Some(t) = crate::config::term() {
        return t;
    }
    match std::env::var("TERM") {
        Ok(t) if t.starts_with("xterm") => "xterm".into(),
        Ok(t) if t == "vt100" || t == "vt102" => t,
        _ => "vt100".into(),
    }
}

/// Debug aid: with `DOCKERIX_TELNET_TRACE=1`, dump every raw chunk on the wire
/// to stderr, escaped (IAC sequences show as \xff\xfd\x18 etc.).
fn trace(dir: &str, bytes: &[u8]) {
    if !crate::config::telnet_trace() {
        return;
    }
    let mut s = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        match b {
            0x20..=0x7e => s.push(b as char),
            b'\r' => s.push_str("\\r"),
            b'\n' => s.push_str("\\n"),
            _ => s.push_str(&format!("\\x{b:02x}")),
        }
    }
    eprintln!("telnet {dir}: {s}");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::var;
    use crate::testsupport::Env;

    /// A stream that has nothing to read and keeps what is written: the
    /// replies the client sends back to the server.
    #[derive(Default)]
    struct Wire(Vec<u8>);

    impl Read for Wire {
        fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
            Ok(0)
        }
    }

    impl Write for Wire {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn client(tty: bool) -> Telnet<Wire> {
        Telnet::new(Wire::default(), tty, 80, 24)
    }

    /// Decode `input`, returning the application bytes and the replies sent.
    fn feed(t: &mut Telnet<Wire>, input: &[u8]) -> (Vec<u8>, Vec<u8>) {
        let app = t.decode_into(input).unwrap();
        (app, std::mem::take(&mut t.stream.0))
    }

    #[test]
    fn iac_iac_is_a_literal_ff() {
        let mut t = client(false);
        assert_eq!(feed(&mut t, &[b'a', IAC, IAC, b'b']), (vec![b'a', 0xff, b'b'], vec![]));
    }

    #[test]
    fn commands_and_state_survive_a_chunk_boundary() {
        let mut t = client(false);
        // IAC at the end of one read, the rest of the command in the next.
        assert_eq!(feed(&mut t, &[b'x', IAC]), (b"x".to_vec(), vec![]));
        assert_eq!(feed(&mut t, &[WILL, OPT_ECHO, b'y']), (b"y".to_vec(), vec![IAC, DO, OPT_ECHO]));
        // GA and NOP are dropped without a reply.
        assert_eq!(feed(&mut t, &[IAC, 249, IAC, 241, b'z']), (b"z".to_vec(), vec![]));
    }

    #[test]
    fn subnegotiation_is_stripped_with_an_embedded_iac() {
        let mut t = client(false);
        let (app, replies) = feed(&mut t, &[b'a', IAC, SB, 99, IAC, IAC, 1, IAC, SE, b'b']);
        assert_eq!((app, replies), (b"ab".to_vec(), vec![]));
        assert_eq!(t.sb, vec![99, 0xff, 1]);
    }

    #[test]
    fn ttype_send_is_answered_with_the_guest_term() {
        let mut env = Env::lock();
        env.set(var::TERM, "xterms");
        let mut t = client(true);
        let (app, replies) = feed(&mut t, &[IAC, SB, OPT_TTYPE, TTYPE_SEND, IAC, SE]);
        assert!(app.is_empty());
        let mut want = vec![IAC, SB, OPT_TTYPE, TTYPE_IS];
        want.extend_from_slice(b"xterms");
        want.extend_from_slice(&[IAC, SE]);
        assert_eq!(replies, want);
    }

    #[test]
    fn naws_escapes_ff_bytes() {
        // 255 columns and 511 rows put a 0xFF in both halves.
        let mut t = Telnet::new(Wire::default(), true, 255, 511);
        let (_, replies) = feed(&mut t, &[IAC, DO, OPT_NAWS]);
        assert_eq!(
            replies,
            vec![IAC, WILL, OPT_NAWS, IAC, SB, OPT_NAWS, 0, 0xff, 0xff, 1, 0xff, 0xff, IAC, SE]
        );
        t.send_naws(80, 24).unwrap();
        assert_eq!(t.stream.0, vec![IAC, SB, OPT_NAWS, 0, 80, 0, 24, IAC, SE]);
    }

    #[test]
    fn each_option_is_answered_once() {
        let mut t = client(false);
        let (_, replies) =
            feed(&mut t, &[IAC, DO, OPT_SGA, IAC, DO, OPT_SGA, IAC, WILL, OPT_ECHO, IAC, WILL, OPT_ECHO]);
        assert_eq!(replies, vec![IAC, WILL, OPT_SGA, IAC, DO, OPT_ECHO]);
        // Refusals from the server need no answer at all.
        assert_eq!(feed(&mut t, &[IAC, WONT, OPT_ECHO, IAC, DONT, OPT_SGA]), (vec![], vec![]));
    }

    #[test]
    fn non_tty_refuses_ttype_and_naws() {
        let mut t = client(false);
        let (_, replies) = feed(
            &mut t,
            &[IAC, DO, OPT_TTYPE, IAC, DO, OPT_NAWS, IAC, DO, 5, IAC, WILL, OPT_SGA, IAC, WILL, 5],
        );
        assert_eq!(
            replies,
            vec![IAC, WONT, OPT_TTYPE, IAC, WONT, OPT_NAWS, IAC, WONT, 5, IAC, DO, OPT_SGA, IAC, DONT, 5]
        );
    }

    #[test]
    fn tty_offers_ttype_and_naws() {
        let mut t = client(true);
        let (_, replies) = feed(&mut t, &[IAC, DO, OPT_TTYPE, IAC, DO, OPT_NAWS]);
        assert_eq!(
            replies,
            vec![IAC, WILL, OPT_TTYPE, IAC, WILL, OPT_NAWS, IAC, SB, OPT_NAWS, 0, 80, 0, 24, IAC, SE]
        );
    }

    #[test]
    fn send_escapes_ff_and_lines_end_in_a_bare_cr() {
        let mut t = client(false);
        t.send(&[1, 0xff, 2]).unwrap();
        t.send_line("ls").unwrap();
        assert_eq!(t.stream.0, vec![1, 0xff, 0xff, 2, b'l', b's', b'\r']);
    }

    #[test]
    fn guest_term_collapses_to_what_the_guest_knows() {
        let mut env = Env::lock();
        env.unset(var::TERM);
        for (term, want) in [
            (Some("xterm-256color"), "xterm"),
            (Some("xterm"), "xterm"),
            (Some("vt102"), "vt102"),
            (Some("vt100"), "vt100"),
            (Some("screen"), "vt100"),
            (None, "vt100"),
        ] {
            match term {
                Some(t) => env.set("TERM", t),
                None => env.unset("TERM"),
            };
            assert_eq!(guest_term(), want, "TERM={term:?}");
        }
        env.set("TERM", "xterm").set(var::TERM, "xterms");
        assert_eq!(guest_term(), "xterms");
        // An empty override is no override.
        env.set(var::TERM, "");
        assert_eq!(guest_term(), "xterm");
    }
}
