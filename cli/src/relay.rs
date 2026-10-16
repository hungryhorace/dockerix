//! Byte-transparent TCP relay, used for X11 forwarding — the same code runs on
//! the host and in the container.
//!
//! An X client in the guest wants to reach a real X server (XQuartz on the
//! host), but there are two gaps. The guest only ever sees the container: SLiRP
//! maps the outside world to the `10.0.2.2` gateway, so a guest connection to
//! `10.0.2.2:6000` lands on the *container*, not the host. And XQuartz listens
//! on a unix socket the container cannot touch. So the hop is bridged twice by
//! this same relay:
//!
//!   guest (`DISPLAY=10.0.2.2:0`)
//!     -> SLiRP -> container:  `relay 6000 tcp:host.docker.internal:6000`
//!     -> host:                 `relay 6000 unix:/tmp/.X11-unix/X0`  (XQuartz)
//!
//! It is deliberately dumb: accept a TCP connection, open the target, and copy
//! bytes both ways until either side closes. X's own protocol and any
//! authentication ride through untouched. See docs/how-it-works.md, "X11
//! forwarding".

use std::io::{self, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::thread;

/// Where each accepted connection is forwarded.
pub enum Target {
    /// A `host:port` reached over TCP (the container -> host hop).
    Tcp(String),
    /// A unix-domain socket path (the host -> XQuartz hop).
    Unix(PathBuf),
}

/// Parse a `--to` value: `tcp:HOST:PORT` or `unix:/path`.
pub fn parse_target(s: &str) -> Result<Target, String> {
    match s.split_once(':') {
        Some(("tcp", rest)) if !rest.is_empty() => Ok(Target::Tcp(rest.to_string())),
        Some(("unix", rest)) if !rest.is_empty() => Ok(Target::Unix(PathBuf::from(rest))),
        _ => Err(format!("target must be `tcp:HOST:PORT` or `unix:/path` (got {s:?})")),
    }
}

fn describe(target: &Target) -> String {
    match target {
        Target::Tcp(hp) => format!("tcp:{hp}"),
        Target::Unix(p) => format!("unix:{}", p.display()),
    }
}

/// Bind `listen` (a `host:port`, e.g. `0.0.0.0:6000`) and forward every
/// connection to `target`. Blocks, running the accept loop on this thread; a
/// failed target connection drops just that client, not the listener.
pub fn serve(listen: &str, target: Target) -> io::Result<()> {
    let listener = TcpListener::bind(listen)?;
    eprintln!("relay: {listen} -> {}", describe(&target));
    for conn in listener.incoming() {
        let client = match conn {
            Ok(c) => c,
            Err(e) => {
                eprintln!("relay: accept failed: {e}");
                continue;
            }
        };
        match Stream::connect(&target) {
            Ok(far) => couple(Stream::Tcp(client), far),
            Err(e) => eprintln!("relay: cannot reach {}: {e}", describe(&target)),
        }
    }
    Ok(())
}

/// Start `serve` on a background thread, for the in-container auto-start under
/// `DOCKERIX_X11` (used like `timeserver::start`). Best effort: if the bind
/// fails it logs and the thread exits, leaving the guest without a display
/// rather than taking anything else down.
pub fn spawn(listen: &'static str, target: Target) {
    thread::spawn(move || {
        if let Err(e) = serve(listen, target) {
            eprintln!("relay: not started ({e}); X11 forwarding will not work");
        }
    });
}

/// Wire two connected streams together with a copy thread each way. When one
/// direction hits EOF it half-closes the peers so the other unblocks and the
/// connection tears down cleanly.
fn couple(near: Stream, far: Stream) {
    let (near_w, far_r) = match (near.try_clone(), far.try_clone()) {
        (Ok(nw), Ok(fr)) => (nw, fr),
        _ => return, // clone failed: drop the connection rather than half-wire it
    };
    thread::spawn(move || pipe(near, far)); // client -> target
    thread::spawn(move || pipe(far_r, near_w)); // target -> client
}

fn pipe(mut src: Stream, mut dst: Stream) {
    let _ = io::copy(&mut src, &mut dst);
    dst.shutdown(Shutdown::Write);
    src.shutdown(Shutdown::Read);
}

/// A TCP or unix stream, so one relay handles both hops.
enum Stream {
    Tcp(TcpStream),
    Unix(UnixStream),
}

impl Stream {
    fn connect(target: &Target) -> io::Result<Stream> {
        Ok(match target {
            Target::Tcp(hp) => Stream::Tcp(TcpStream::connect(hp)?),
            Target::Unix(p) => Stream::Unix(UnixStream::connect(p)?),
        })
    }

    fn try_clone(&self) -> io::Result<Stream> {
        Ok(match self {
            Stream::Tcp(s) => Stream::Tcp(s.try_clone()?),
            Stream::Unix(s) => Stream::Unix(s.try_clone()?),
        })
    }

    fn shutdown(&self, how: Shutdown) {
        let _ = match self {
            Stream::Tcp(s) => s.shutdown(how),
            Stream::Unix(s) => s.shutdown(how),
        };
    }
}

impl Read for Stream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Stream::Tcp(s) => s.read(buf),
            Stream::Unix(s) => s.read(buf),
        }
    }
}

impl Write for Stream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            Stream::Tcp(s) => s.write(buf),
            Stream::Unix(s) => s.write(buf),
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        match self {
            Stream::Tcp(s) => s.flush(),
            Stream::Unix(s) => s.flush(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn parse_target_accepts_tcp_and_unix() {
        assert!(matches!(parse_target("tcp:host.docker.internal:6000"),
            Ok(Target::Tcp(hp)) if hp == "host.docker.internal:6000"));
        assert!(matches!(parse_target("unix:/tmp/.X11-unix/X0"),
            Ok(Target::Unix(p)) if p == Path::new("/tmp/.X11-unix/X0")));
        for bad in ["", "tcp:", "unix:", "udp:host:6000", "host:6000", "/tmp/x"] {
            assert!(parse_target(bad).is_err(), "{bad:?}");
        }
    }
}
