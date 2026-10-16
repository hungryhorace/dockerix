//! RFC 868 (TIME protocol) server on TCP 37, so the guest's `rdate` can set
//! its clock from the container's.
//!
//! RISC iX boots believing it is 1992: the emulated RTC's century is wrong and
//! its `date(1)` takes a two-digit year (`1900 + yy`), so it cannot be nudged
//! past 1999 from the shell. `rdate` can — it reads a 32-bit count of seconds
//! since 1900 from this server and calls `settimeofday`. RISC iX's own clock
//! is good to 2038 (verified: 2000, the 2000-02-29 leap day, the present
//! and 2038 all render correctly), and RFC 868's unsigned seconds-since-1900
//! last until 2036, so the current date is comfortably in range.
//!
//! Only the guest reaches this port, over SLiRP (10.0.2.2:37); it is not a
//! published container port.

use std::io::Write;
use std::net::{TcpListener, TcpStream};
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

/// Seconds between 1900-01-01 and the Unix epoch (1970-01-01).
const RFC868_EPOCH_OFFSET: u64 = 2_208_988_800;

/// Bind TCP 37 and answer every connection with the current time. Best effort:
/// if the bind fails (port taken, no privilege) it logs and returns, leaving
/// the guest on its 1992 RTC default. Returns once listening (the accept loop
/// runs on its own thread).
pub fn start() {
    let listener = match TcpListener::bind(("0.0.0.0", 37)) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("time server: not started ({e}); guest clock stays at its RTC default");
            return;
        }
    };
    thread::spawn(move || {
        for conn in listener.incoming() {
            match conn {
                Ok(stream) => respond(stream),
                Err(e) => eprintln!("time server: accept failed: {e}"),
            }
        }
    });
}

/// Write the 4-byte big-endian seconds-since-1900 and close. RFC 868 sends the
/// value and nothing else; `rdate` reads exactly four bytes.
fn respond(mut stream: TcpStream) {
    let unix = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let secs1900 = (unix + RFC868_EPOCH_OFFSET) as u32;
    let _ = stream.write_all(&secs1900.to_be_bytes());
}
