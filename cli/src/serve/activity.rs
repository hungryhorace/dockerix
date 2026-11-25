//! What dockerix is running in the guest, for `dockerix ps`: the commands
//! the agent runs on its held sessions, and the sessions `rix` opens itself
//! (`exec -t`, `login`, a direct command), which it announces to the agent
//! for as long as they last. Not the guest's processes: only what was
//! asked of it through dockerix.
//!
//! Each entry carries the pid of the in-container process that asked for
//! it (the `rix` a `docker exec` started), so an entry can be named, and
//! how to stop it, for `dockerix kill`.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Where a piece of guest work has got to.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Phase {
    /// A command waiting for a free held session.
    Waiting,
    /// A command whose work-area push is under way.
    Pushing,
    /// A command running in the guest.
    Running,
    /// `exec -t`: a command on a terminal of its own.
    Tty,
    /// `login`: an interactive shell.
    Login,
}

impl Phase {
    /// How `dockerix ps` shows it, and how an announcing `rix` names it.
    pub fn word(self) -> &'static str {
        match self {
            Phase::Waiting => "waiting",
            Phase::Pushing => "pushing",
            Phase::Running => "running",
            Phase::Tty => "tty",
            Phase::Login => "login",
        }
    }

    /// The phase an announcing `rix` names (`announce` in serve.rs).
    pub fn from_word(word: &str) -> Option<Phase> {
        [Phase::Waiting, Phase::Pushing, Phase::Running, Phase::Tty, Phase::Login]
            .into_iter()
            .find(|p| p.word() == word)
    }
}

/// One piece of guest work.
#[derive(Clone, Debug, PartialEq)]
pub struct Entry {
    /// The command as given (no `cd` wrapper); empty for a login shell.
    pub command: String,
    /// The guest directory it runs in.
    pub dir: String,
    pub phase: Phase,
    pub started: Instant,
    /// The in-container process that asked for it, from the socket's peer
    /// credentials; None where the system does not say (anywhere but
    /// Linux, so only in tests on another host).
    pub pid: Option<u32>,
}

impl Entry {
    /// The command as `dockerix ps` and `dockerix kill` name it.
    pub fn shown(&self) -> &str {
        if self.command.is_empty() { "(login shell)" } else { &self.command }
    }
}

/// How `dockerix kill` stops a piece of work.
#[derive(Clone, Debug)]
pub enum Stop {
    /// A command on a held session: raise its interrupt flag, so the guest
    /// gets ^C (and the session is dropped if the command ignores it).
    Interrupt(Arc<AtomicBool>),
    /// A session a `rix` runs itself: hang up that `rix` (SIGHUP). Its
    /// telnet connection closing makes the guest hang up the shell and what
    /// it runs.
    HangUp,
}

/// What `Activity::kill` found under a pid.
#[derive(Clone, Debug, PartialEq)]
pub enum Killed {
    /// A command on a held session, its interrupt flag now raised.
    Interrupted(Entry),
    /// A session of the `rix` with that pid, for the caller to hang up.
    HangUp(Entry),
}

/// One entry, how to stop it, and whether `dockerix kill` has.
struct Record {
    entry: Entry,
    stop: Stop,
    killed: bool,
}

/// The last record id given out, and the records by id.
type Records = (u64, BTreeMap<u64, Record>);

/// The registry. Entries are listed in the order they began.
#[derive(Default)]
pub struct Activity {
    records: Mutex<Records>,
}

impl Activity {
    fn lock(&self) -> std::sync::MutexGuard<'_, Records> {
        self.records.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Record `entry`, stopped by `stop`, until the returned guard is
    /// dropped.
    pub fn track(&self, entry: Entry, stop: Stop) -> Tracked<'_> {
        let mut st = self.lock();
        st.0 += 1;
        let id = st.0;
        st.1.insert(id, Record { entry, stop, killed: false });
        Tracked { activity: self, id }
    }

    pub fn list(&self) -> Vec<Entry> {
        self.lock().1.values().map(|r| r.entry.clone()).collect()
    }

    /// `dockerix kill pid`: mark every entry of that pid killed and raise
    /// the interrupt flags of those that have one. The rest come back for
    /// the caller to hang up. Empty if `dockerix ps` lists no such pid.
    pub fn kill(&self, pid: u32) -> Vec<Killed> {
        let mut st = self.lock();
        st.1.values_mut()
            .filter(|r| r.entry.pid == Some(pid))
            .map(|r| {
                r.killed = true;
                match &r.stop {
                    Stop::Interrupt(flag) => {
                        flag.store(true, Ordering::SeqCst);
                        Killed::Interrupted(r.entry.clone())
                    }
                    Stop::HangUp => Killed::HangUp(r.entry.clone()),
                }
            })
            .collect()
    }
}

/// An entry in the registry, removed when this is dropped.
pub struct Tracked<'a> {
    activity: &'a Activity,
    id: u64,
}

impl Tracked<'_> {
    pub fn set_phase(&self, phase: Phase) {
        if let Some(r) = self.activity.lock().1.get_mut(&self.id) {
            r.entry.phase = phase;
        }
    }

    /// Whether `dockerix kill` has stopped this entry (rather than Ctrl-C,
    /// or the client going away).
    pub fn killed(&self) -> bool {
        self.activity.lock().1.get(&self.id).is_some_and(|r| r.killed)
    }
}

impl Drop for Tracked<'_> {
    fn drop(&mut self) {
        self.activity.lock().1.remove(&self.id);
    }
}

/// The held sessions, for the line under the table: how many are running
/// commands, how many are idle, and the most there may be.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Sessions {
    pub busy: usize,
    pub idle: usize,
    pub max: usize,
}

/// `m:ss`, or `h:mm:ss` from an hour.
fn elapsed(d: Duration) -> String {
    let s = d.as_secs();
    if s >= 3600 {
        format!("{}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60)
    } else {
        format!("{}:{:02}", s / 60, s % 60)
    }
}

/// The `dockerix ps` table, as of `now`.
pub fn render(entries: &[Entry], sessions: Sessions, now: Instant) -> String {
    let mut out = String::new();
    if entries.is_empty() {
        out.push_str("nothing running in the guest\n");
    } else {
        let rows: Vec<[String; 5]> = entries
            .iter()
            .map(|e| {
                [
                    e.pid.map_or_else(|| "-".to_string(), |p| p.to_string()),
                    e.phase.word().to_string(),
                    elapsed(now.saturating_duration_since(e.started)),
                    e.dir.clone(),
                    e.shown().to_string(),
                ]
            })
            .collect();
        let head = ["PID", "STATE", "TIME", "DIR", "COMMAND"];
        let width = |i: usize| rows.iter().map(|r| r[i].len()).chain([head[i].len()]).max().unwrap_or(0);
        let (w0, w1, w2, w3) = (width(0), width(1), width(2), width(3));
        for r in std::iter::once(head.map(str::to_string)).chain(rows) {
            // PID and TIME are right-aligned, as in ps(1), so the digits
            // line up.
            let _ = writeln!(out, "{:>w0$}  {:<w1$}  {:>w2$}  {:<w3$}  {}", r[0], r[1], r[2], r[3], r[4]);
        }
    }
    let _ = writeln!(
        out,
        "held sessions: {} busy, {} idle, at most {}",
        sessions.busy, sessions.idle, sessions.max
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(command: &str, phase: Phase, started: Instant) -> Entry {
        Entry { command: command.into(), dir: "/workarea/game".into(), phase, started, pid: Some(4321) }
    }

    #[test]
    fn entries_last_as_long_as_their_guard() {
        let a = Activity::default();
        let now = Instant::now();
        let make = a.track(entry("make", Phase::Waiting, now), Stop::HangUp);
        {
            let _ls = a.track(entry("ls", Phase::Running, now), Stop::HangUp);
            assert_eq!(a.list().len(), 2);
        }
        make.set_phase(Phase::Pushing);
        assert_eq!(a.list(), vec![entry("make", Phase::Pushing, now)]);
        drop(make);
        assert!(a.list().is_empty());
    }

    #[test]
    fn a_kill_interrupts_runs_and_hands_back_sessions_to_hang_up() {
        let a = Activity::default();
        let now = Instant::now();
        let flag = Arc::new(AtomicBool::new(false));
        let make = a.track(entry("make", Phase::Running, now), Stop::Interrupt(Arc::clone(&flag)));
        let login = a.track(Entry { pid: Some(87), ..entry("", Phase::Login, now) }, Stop::HangUp);
        let other = Arc::new(AtomicBool::new(false));
        let ls = a.track(
            Entry { pid: Some(99), ..entry("ls", Phase::Running, now) },
            Stop::Interrupt(Arc::clone(&other)),
        );

        assert!(a.kill(5).is_empty(), "no such pid");
        assert!(!make.killed() && !flag.load(Ordering::SeqCst));

        assert_eq!(a.kill(4321), vec![Killed::Interrupted(entry("make", Phase::Running, now))]);
        assert!(make.killed() && flag.load(Ordering::SeqCst));
        assert_eq!(a.kill(87), vec![Killed::HangUp(Entry { pid: Some(87), ..entry("", Phase::Login, now) })]);
        assert!(login.killed());
        assert!(!ls.killed() && !other.load(Ordering::SeqCst), "only the pid named");
        assert_eq!(a.list().len(), 3, "listed until each guard goes");
    }

    #[test]
    fn phases_round_trip_through_their_words() {
        for p in [Phase::Waiting, Phase::Pushing, Phase::Running, Phase::Tty, Phase::Login] {
            assert_eq!(Phase::from_word(p.word()), Some(p));
        }
        assert_eq!(Phase::from_word("asleep"), None);
    }

    #[test]
    fn times_read_as_a_clock() {
        assert_eq!(elapsed(Duration::from_secs(4)), "0:04");
        assert_eq!(elapsed(Duration::from_secs(754)), "12:34");
        assert_eq!(elapsed(Duration::from_secs(3600 + 62)), "1:01:02");
    }

    #[test]
    fn the_table_lines_up() {
        let now = Instant::now();
        let entries = [
            entry("make", Phase::Running, now - Duration::from_secs(754)),
            Entry { dir: "/".into(), pid: Some(87), ..entry("", Phase::Login, now - Duration::from_secs(4)) },
            Entry { pid: None, ..entry("ls", Phase::Waiting, now) },
        ];
        let sessions = Sessions { busy: 1, idle: 0, max: 2 };
        assert_eq!(
            render(&entries, sessions, now),
            " PID  STATE     TIME  DIR             COMMAND\n\
             4321  running  12:34  /workarea/game  make\n  \
               87  login     0:04  /               (login shell)\n   \
                -  waiting   0:00  /workarea/game  ls\n\
             held sessions: 1 busy, 0 idle, at most 2\n"
        );
        assert_eq!(
            render(&[], Sessions { busy: 0, idle: 2, max: 2 }, now),
            "nothing running in the guest\nheld sessions: 0 busy, 2 idle, at most 2\n"
        );
    }
}
