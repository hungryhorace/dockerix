//! Progress reporting for transfers that take more than a moment: the
//! work-area push (a fresh tree full of sources is minutes at ~11 KB/s),
//! `put`, and `get`.
//!
//! Producers (sync.rs, transfer.rs) decide from the byte count whether a
//! transfer is worth reporting at all — see `worth_showing` — and then emit
//! `Progress` events. Where the producer is the serve-mode agent, the events
//! cross the exec socket as `p` frames and `rix` renders them; otherwise the
//! producing process renders them itself. Either way the drawing is done by
//! `Reporter`, on stderr, in one of two styles:
//!
//! - **bar**: a single line redrawn in place with a percentage and an ETA,
//!   for a terminal;
//! - **plain**: one line when the transfer starts (with a size and time
//!   estimate) and one when it is done — `--plain`, or a non-terminal.
//!
//! The style is chosen on the host (the only place that knows whether stderr
//! is a terminal — inside the container it is always docker's pipe) and
//! passed down as `DOCKERIX_PROGRESS`: `bar`, `plain`, or `0` for nothing.

use std::io::{IsTerminal, Write};
use std::time::{Duration, Instant};

/// Guest-inbound bytes per second down the tty, roughly (measured).
pub const INBOUND_RATE: u64 = 11_000;
/// Bytes per second of an archive staged over NFS and unpacked (or packed)
/// by the guest, roughly (NFS reads ~400 KB/s, then `tar x`; measured).
pub const STAGED_RATE: u64 = 250_000;
/// Guest-outbound bytes per second, roughly.
pub const OUTBOUND_RATE: u64 = 90_000;

/// Report transfers expected to take longer than this.
const SHOW_ABOVE: Duration = Duration::from_secs(2);

/// Is a transfer of `bytes` at `rate` bytes/s long enough to report?
pub fn worth_showing(bytes: u64, rate: u64) -> bool {
    bytes >= rate * SHOW_ABOVE.as_secs()
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Stage {
    /// Bytes moving: `done` of `total`.
    Sending,
    /// All bytes across; the far side is unpacking (no byte progress).
    Unpacking,
    /// Finished.
    Done,
}

#[derive(Clone, Debug)]
pub struct Progress {
    pub label: String,
    pub done: u64,
    pub total: u64,
    pub stage: Stage,
}

impl Progress {
    /// Wire form for the exec socket: `done total stage label`.
    pub fn encode(&self) -> String {
        let s = match self.stage {
            Stage::Sending => 's',
            Stage::Unpacking => 'u',
            Stage::Done => 'd',
        };
        format!("{} {} {s} {}", self.done, self.total, self.label)
    }

    pub fn decode(text: &str) -> Option<Progress> {
        let mut f = text.splitn(4, ' ');
        let done = f.next()?.parse().ok()?;
        let total = f.next()?.parse().ok()?;
        let stage = match f.next()? {
            "s" => Stage::Sending,
            "u" => Stage::Unpacking,
            "d" => Stage::Done,
            _ => return None,
        };
        let label = f.next().unwrap_or("").to_string();
        Some(Progress { label, done, total, stage })
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Style {
    Bar,
    Plain,
    Off,
}

impl Style {
    /// `DOCKERIX_PROGRESS` if set (the host wrapper sets it); otherwise a bar
    /// on a terminal and plain lines elsewhere.
    pub fn from_env() -> Style {
        match crate::config::progress_style().as_deref() {
            Some("bar") => Style::Bar,
            Some("plain") => Style::Plain,
            Some("0" | "off" | "none") => Style::Off,
            _ if std::io::stderr().is_terminal() => Style::Bar,
            _ => Style::Plain,
        }
    }
}

/// Draws `Progress` events on stderr. One transfer at a time; a new label
/// starts a new one (after finishing the old).
pub struct Reporter {
    style: Style,
    current: Option<Active>,
}

struct Active {
    label: String,
    started: Instant,
    last_draw: Instant,
    drawn: bool,
}

impl Reporter {
    pub fn new(style: Style) -> Reporter {
        Reporter { style, current: None }
    }

    pub fn show(&mut self, p: &Progress) {
        if self.style == Style::Off {
            return;
        }
        let now = Instant::now();
        if self.current.as_ref().is_none_or(|a| a.label != p.label) {
            if let Some(old) = self.current.take() {
                self.end(&old, now);
            }
            let started = now;
            self.current = Some(Active { label: p.label.clone(), started, last_draw: started, drawn: false });
            if self.style == Style::Plain {
                // The label carries the payload size; `total` is the encoded
                // byte count the bar tracks, which the reader need not see.
                let secs = p.total / rate_guess(p).max(1);
                eprintln!("dockerix: {} (about {})", p.label, hms(secs));
            }
        }
        let Some(active) = self.current.as_mut() else { return };
        match p.stage {
            Stage::Done => {
                let a = self.current.take().expect("active");
                self.end(&a, now);
            }
            _ if self.style == Style::Bar => {
                // Throttle redraws; always draw the stage changes.
                if active.drawn
                    && p.stage == Stage::Sending
                    && now.duration_since(active.last_draw) < Duration::from_millis(100)
                {
                    return;
                }
                active.last_draw = now;
                active.drawn = true;
                let mut err = std::io::stderr().lock();
                let _ = write!(err, "\r\x1b[K{}", bar_line(p, now.duration_since(active.started)));
                let _ = err.flush();
            }
            _ => {}
        }
    }

    fn end(&self, a: &Active, now: Instant) {
        let took = hms(now.duration_since(a.started).as_secs());
        match self.style {
            Style::Bar => {
                let mut err = std::io::stderr().lock();
                let _ = writeln!(err, "\r\x1b[Kdockerix: {}: done in {took}", a.label);
            }
            Style::Plain => eprintln!("dockerix: {}: done in {took}", a.label),
            Style::Off => {}
        }
    }
}

impl Drop for Reporter {
    /// A transfer that errored out mid-bar leaves the cursor at the end of
    /// the bar line; step off it so the error lands on a line of its own.
    fn drop(&mut self) {
        if let Some(a) = self.current.take()
            && self.style == Style::Bar
            && a.drawn
        {
            let mut err = std::io::stderr().lock();
            let _ = writeln!(err, "\r\x1b[K");
        }
    }
}

/// Rate to base the plain-style estimate on: the label says which way the
/// bytes go, and a transfer that starts by unpacking was staged over NFS.
fn rate_guess(p: &Progress) -> u64 {
    if p.stage == Stage::Unpacking {
        STAGED_RATE
    } else if p.label.starts_with("fetching") {
        OUTBOUND_RATE
    } else {
        INBOUND_RATE
    }
}

fn bar_line(p: &Progress, elapsed: Duration) -> String {
    const WIDTH: usize = 24;
    let total = p.total.max(1);
    let done = p.done.min(total);
    let frac = done as f64 / total as f64;
    let filled = (frac * WIDTH as f64) as usize;
    let bar: String = (0..WIDTH).map(|i| if i < filled { '#' } else { '-' }).collect();
    let tail = match p.stage {
        // The guest at work: packing for a fetch, unpacking otherwise.
        Stage::Unpacking if p.label.starts_with("fetching") => "packing".to_string(),
        Stage::Unpacking => "unpacking".to_string(),
        _ => {
            let secs = elapsed.as_secs_f64();
            if done > 0 && secs > 0.5 {
                let rate = done as f64 / secs;
                let left = ((total - done) as f64 / rate) as u64;
                format!("{}/{} at {}/s, {} left", bytes(done), bytes(p.total), bytes(rate as u64), hms(left))
            } else {
                format!("{}/{}", bytes(done), bytes(p.total))
            }
        }
    };
    format!("dockerix: {} [{bar}] {:3.0}% {tail}", p.label, frac * 100.0)
}

pub fn bytes(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1} MB", n as f64 / 1e6)
    } else if n >= 1_000 {
        format!("{:.0} KB", n as f64 / 1e3)
    } else {
        format!("{n} B")
    }
}

fn hms(secs: u64) -> String {
    if secs >= 3600 {
        format!("{}:{:02}:{:02}", secs / 3600, secs % 3600 / 60, secs % 60)
    } else if secs >= 60 {
        format!("{}:{:02}", secs / 60, secs % 60)
    } else {
        format!("{secs} s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::var;
    use crate::testsupport::Env;

    #[test]
    fn style_from_env() {
        let mut env = Env::lock();
        for (value, want) in [
            ("bar", Style::Bar),
            ("plain", Style::Plain),
            ("0", Style::Off),
            ("off", Style::Off),
            ("none", Style::Off),
        ] {
            env.set(var::PROGRESS, value);
            assert_eq!(Style::from_env(), want, "{value}");
        }
        // Unset or unknown: decided by whether stderr is a terminal.
        let fallback = if std::io::stderr().is_terminal() { Style::Bar } else { Style::Plain };
        env.set(var::PROGRESS, "fancy");
        assert_eq!(Style::from_env(), fallback);
        env.unset(var::PROGRESS);
        assert_eq!(Style::from_env(), fallback);
    }

    #[test]
    fn wire_round_trip() {
        let p =
            Progress { label: "pushing 3 files, 40 KB".into(), done: 12, total: 40, stage: Stage::Sending };
        let back = Progress::decode(&p.encode()).unwrap();
        assert_eq!(back.label, p.label);
        assert_eq!((back.done, back.total, back.stage), (12, 40, Stage::Sending));
        assert_eq!(Progress::decode("1 2 d ").unwrap().stage, Stage::Done);
        assert!(Progress::decode("x 2 s l").is_none());
    }

    #[test]
    fn thresholds_and_formatting() {
        assert!(!worth_showing(20_000, INBOUND_RATE));
        assert!(worth_showing(30_000, INBOUND_RATE));
        assert_eq!(bytes(999), "999 B");
        assert_eq!(bytes(40_000), "40 KB");
        assert_eq!(bytes(1_234_567), "1.2 MB");
        assert_eq!(hms(59), "59 s");
        assert_eq!(hms(125), "2:05");
        assert_eq!(hms(3661), "1:01:01");
        let line = bar_line(
            &Progress { label: "pushing".into(), done: 50, total: 100, stage: Stage::Sending },
            Duration::from_secs(5),
        );
        assert!(line.contains(" 50% "), "{line}");
        assert!(line.contains("5 s left"), "{line}");
    }
}
