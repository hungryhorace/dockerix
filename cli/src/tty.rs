//! Interactive (tty) pass-through for the exec channel.
//!
//! Puts the local terminal in raw mode and shuttles bytes between it and the
//! guest login shell so full-screen/curses programs work. Window-size changes
//! are forwarded as telnet NAWS. When a command was launched with an exit
//! sentinel, the incoming stream is scanned for it so the sentinel line is
//! swallowed and its exit code returned.

use std::io::{self, ErrorKind, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use rustix::event::{PollFd, PollFlags, Timespec, poll};
use rustix::fd::BorrowedFd;
use rustix::fs::OFlags;
use rustix::io::Errno;
use rustix::termios::{self, OptionalActions, Termios};
use signal_hook::consts::SIGWINCH;

use crate::session::{Session, find, hold_back};

/// How long one `poll` waits before the loop checks for a resize.
const POLL_WAIT: Timespec = Timespec { tv_sec: 0, tv_nsec: 200_000_000 };

/// Restores the terminal on stdin to its saved state on drop.
struct RawGuard {
    orig: Termios,
}

impl RawGuard {
    /// Put the terminal on stdin in raw mode; None if stdin is not one.
    fn new() -> Option<RawGuard> {
        let fd = rustix::stdio::stdin();
        if !termios::isatty(fd) {
            return None;
        }
        let orig = termios::tcgetattr(fd).ok()?;
        let mut raw = orig.clone();
        raw.make_raw();
        termios::tcsetattr(fd, OptionalActions::Now, &raw).ok()?;
        Some(RawGuard { orig })
    }
}

impl Drop for RawGuard {
    fn drop(&mut self) {
        let _ = termios::tcsetattr(rustix::stdio::stdin(), OptionalActions::Now, &self.orig);
    }
}

/// Current terminal size (cols, rows), defaulting to 80x24 when unknown.
pub fn window_size() -> (u16, u16) {
    match termios::tcgetwinsize(rustix::stdio::stdout()) {
        Ok(ws) if ws.ws_col > 0 && ws.ws_row > 0 => (ws.ws_col, ws.ws_row),
        _ => (80, 24),
    }
}

/// Run an interactive session against an already-logged-in shell. If
/// `sentinel` is set, the stream is scanned for `<sentinel><digits>\n` to
/// capture the command's exit status; otherwise this is a plain shell and the
/// result is 0.
pub fn interactive(session: &mut Session, sentinel: Option<&str>) -> io::Result<i32> {
    let winch = Arc::new(AtomicBool::new(false));
    signal_hook::flag::register(SIGWINCH, Arc::clone(&winch))?;
    let _raw = RawGuard::new();
    let mut out = io::stdout().lock();
    let mut polls = 0u64;
    pump(session, sentinel, rustix::stdio::stdin(), &mut out, &winch, &mut polls)
}

/// The interactive pump: shuttle bytes between `input` and the guest, writing
/// the guest's output to `output`, until the guest closes (or, with a
/// sentinel, the command's exit line is seen). `winch` set means forward a
/// resize; `polls` counts `poll` calls, so a test can tell a quiet wait from
/// a spin. Taking the local ends as parameters (an `AsFd` to poll, a `Write`
/// to write) lets a test drive it over pipes against the fake guest, with
/// `interactive` passing stdin and stdout.
fn pump(
    session: &mut Session,
    sentinel: Option<&str>,
    input: BorrowedFd<'_>,
    output: &mut dyn Write,
    winch: &AtomicBool,
    polls: &mut u64,
) -> io::Result<i32> {
    session.telnet_mut().set_nonblocking(true)?;
    set_nonblocking(input)?;

    let sneedle = sentinel.map(|s| s.as_bytes().to_vec());
    let mut hold: Vec<u8> = Vec::new();
    let mut exit_code: i32 = 0;
    let mut inbuf = [0u8; 4096];
    // Once the local input reaches EOF it stays readable, so keep it out of
    // the poll set after that, or the loop would spin; we still drain the
    // guest to the end.
    let mut input_open = true;

    loop {
        if winch.swap(false, Ordering::SeqCst) {
            let (c, r) = window_size();
            let _ = session.telnet_mut().send_naws(c, r);
        }

        let (keys, guest) = {
            let mut fds: Vec<PollFd> = Vec::with_capacity(2);
            if input_open {
                fds.push(PollFd::new(&input, PollFlags::IN));
            }
            fds.push(PollFd::new(session.telnet_mut().get_ref(), PollFlags::IN));
            *polls += 1;
            match poll(&mut fds, Some(&POLL_WAIT)) {
                Ok(_) => {}
                Err(Errno::INTR) => continue, // SIGWINCH etc.
                Err(e) => return Err(e.into()),
            }
            let ready = PollFlags::IN | PollFlags::HUP;
            let keys = input_open && fds[0].revents().intersects(ready);
            let guest = fds[usize::from(input_open)].revents().intersects(ready);
            (keys, guest)
        };

        // Local keystrokes -> guest.
        if keys {
            match rustix::io::read(input, &mut inbuf[..]) {
                Ok(0) => {
                    // Local EOF (Ctrl-D handled by the guest; a closed input
                    // means we're done feeding it). Tell the guest, stop
                    // polling the input, and keep draining.
                    let _ = session.telnet_mut().get_ref().shutdown(std::net::Shutdown::Write);
                    input_open = false;
                }
                Ok(n) => session.telnet_mut().send(&inbuf[..n])?,
                Err(_) => {} // nothing to read after all (EAGAIN)
            }
        }

        // Guest output -> local terminal.
        if guest {
            match session.telnet_mut().read_raw(&mut inbuf) {
                Ok(0) => break, // guest closed the connection
                Ok(n) => {
                    let app = session.telnet_mut().decode_into(&inbuf[..n])?;
                    if app.is_empty() {
                        continue;
                    }
                    match &sneedle {
                        None => {
                            output.write_all(&app)?;
                            output.flush()?;
                        }
                        Some(needle) => {
                            hold.extend_from_slice(&app);
                            if let Some(pos) = find(&hold, needle) {
                                // Flush everything before the sentinel, then
                                // read its exit digits.
                                output.write_all(&hold[..pos])?;
                                output.flush()?;
                                exit_code = read_exit_code(session, &hold[pos + needle.len()..])?;
                                hold.clear(); // consumed — don't re-flush post-loop
                                break;
                            }
                            // Hold back only a tail that could be the start of
                            // a sentinel split across reads: the longest suffix
                            // that is a prefix of it. For ordinary output that
                            // is nothing, so a curses-style program's small
                            // updates reach the terminal whole, not 38 bytes
                            // short until the next write.
                            let keep = hold_back(&hold, needle);
                            if hold.len() > keep {
                                let cut = hold.len() - keep;
                                output.write_all(&hold[..cut])?;
                                output.flush()?;
                                hold.drain(..cut);
                            }
                        }
                    }
                }
                Err(e) if e.kind() == ErrorKind::WouldBlock => {}
                Err(e) => return Err(e),
            }
        }
    }

    // Flush any held-back bytes that weren't a sentinel.
    if sneedle.is_some() && !hold.is_empty() {
        output.write_all(&hold)?;
        output.flush()?;
    }
    Ok(exit_code)
}

/// After the sentinel, collect the exit digits up to a newline.
fn read_exit_code(session: &mut Session, already: &[u8]) -> io::Result<i32> {
    let mut digits: Vec<u8> = already.to_vec();
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut buf = [0u8; 256];
    while !digits.contains(&b'\n') && Instant::now() < deadline {
        match session.telnet_mut().read_raw(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                let app = session.telnet_mut().decode_into(&buf[..n])?;
                digits.extend_from_slice(&app);
            }
            Err(e) if e.kind() == ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(_) => break,
        }
    }
    let s: String = digits
        .iter()
        .take_while(|&&b| b != b'\n')
        .filter(|b| b.is_ascii_digit())
        .map(|&b| b as char)
        .collect();
    Ok(s.parse().unwrap_or(-1))
}

fn set_nonblocking(fd: BorrowedFd<'_>) -> io::Result<()> {
    let flags = rustix::fs::fcntl_getfl(fd)?;
    rustix::fs::fcntl_setfl(fd, flags | OFlags::NONBLOCK)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake_guest::{FakeGuest, Reply};
    use rustix::fd::AsFd;

    /// Run `pump` against `session` with `input` as the local end and a
    /// captured output; returns the exit code, what was written, and how many
    /// `poll`s it took.
    fn drive(session: &mut Session, sentinel: Option<&str>, input: BorrowedFd<'_>) -> (i32, Vec<u8>, u64) {
        let mut out = Vec::new();
        let winch = AtomicBool::new(false);
        let mut polls = 0u64;
        let code = pump(session, sentinel, input, &mut out, &winch, &mut polls).unwrap();
        (code, out, polls)
    }

    #[test]
    fn a_launched_commands_output_and_status_come_back() {
        let guest = FakeGuest::with(|cmd| (cmd == "say").then(|| Reply::Done("hello world\n".into(), 7)));
        let mut s = guest.login();
        let token = s.launch("say", 80, 24).unwrap();
        // `_w` stays open, so the local end never signals input or EOF.
        let (r, _w) = rustix::pipe::pipe().unwrap();
        let (code, out, _) = drive(&mut s, Some(&token), r.as_fd());
        assert_eq!(code, 7);
        assert!(String::from_utf8_lossy(&out).contains("hello world"), "{out:?}");
    }

    #[test]
    fn a_typed_ctrl_c_reaches_the_guest() {
        let guest = FakeGuest::with(|cmd| (cmd == "hold").then_some(Reply::Hang));
        let mut s = guest.login();
        let token = s.launch("hold", 80, 24).unwrap();
        let (r, w) = rustix::pipe::pipe().unwrap();
        rustix::io::write(&w, &[0x03]).unwrap(); // ^C
        // The command hangs until the ^C; the launch's `trap` then reports 130.
        let (code, _, _) = drive(&mut s, Some(&token), r.as_fd());
        assert_eq!(code, 130);
    }

    #[test]
    fn local_eof_does_not_spin() {
        let guest = FakeGuest::with(|cmd| (cmd == "hold").then_some(Reply::Hang));
        let mut s = guest.login();
        let token = s.launch("hold", 80, 24).unwrap();
        let (r, w) = rustix::pipe::pipe().unwrap();
        drop(w); // local EOF before the guest says anything
        let mut out = Vec::new();
        let winch = AtomicBool::new(false);
        let mut polls = 0u64;
        std::thread::scope(|sc| {
            // The guest stays up and silent; close it after a beat so the pump
            // returns. A spin would rack up polls while it waits.
            sc.spawn(|| {
                std::thread::sleep(Duration::from_millis(80));
                guest.hangup();
            });
            let _ = pump(&mut s, Some(&token), r.as_fd(), &mut out, &winch, &mut polls);
        });
        assert!(polls < 10, "pump spun after EOF: {polls} polls");
    }
}
