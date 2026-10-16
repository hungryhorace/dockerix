//! What dockerix is running in the guest, for `dockerix ps`: the commands
//! the agent runs on its held sessions, and the sessions `rix` opens itself
//! (`exec -t`, `login`, a direct command), which it announces to the agent
//! for as long as they last. Not the guest's processes: only what was
//! asked of it through dockerix.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::sync::Mutex;
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
}

/// The registry. Entries are listed in the order they began.
#[derive(Default)]
pub struct Activity {
    entries: Mutex<(u64, BTreeMap<u64, Entry>)>,
}

impl Activity {
    fn lock(&self) -> std::sync::MutexGuard<'_, (u64, BTreeMap<u64, Entry>)> {
        self.entries.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Record `entry` until the returned guard is dropped.
    pub fn track(&self, entry: Entry) -> Tracked<'_> {
        let mut st = self.lock();
        st.0 += 1;
        let id = st.0;
        st.1.insert(id, entry);
        Tracked { activity: self, id }
    }

    pub fn list(&self) -> Vec<Entry> {
        self.lock().1.values().cloned().collect()
    }
}

/// An entry in the registry, removed when this is dropped.
pub struct Tracked<'a> {
    activity: &'a Activity,
    id: u64,
}

impl Tracked<'_> {
    pub fn set_phase(&self, phase: Phase) {
        if let Some(e) = self.activity.lock().1.get_mut(&self.id) {
            e.phase = phase;
        }
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
        let rows: Vec<[String; 4]> = entries
            .iter()
            .map(|e| {
                let command =
                    if e.command.is_empty() { "(login shell)".to_string() } else { e.command.clone() };
                [
                    e.phase.word().to_string(),
                    elapsed(now.saturating_duration_since(e.started)),
                    e.dir.clone(),
                    command,
                ]
            })
            .collect();
        let head = ["STATE", "TIME", "DIR", "COMMAND"];
        let width = |i: usize| rows.iter().map(|r| r[i].len()).chain([head[i].len()]).max().unwrap_or(0);
        let (w0, w1, w2) = (width(0), width(1), width(2));
        for r in std::iter::once(head.map(str::to_string)).chain(rows) {
            // TIME is right-aligned, so the minutes line up.
            let _ = writeln!(out, "{:<w0$}  {:>w1$}  {:<w2$}  {}", r[0], r[1], r[2], r[3]);
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
        Entry { command: command.into(), dir: "/workarea/game".into(), phase, started }
    }

    #[test]
    fn entries_last_as_long_as_their_guard() {
        let a = Activity::default();
        let now = Instant::now();
        let make = a.track(entry("make", Phase::Waiting, now));
        {
            let _ls = a.track(entry("ls", Phase::Running, now));
            assert_eq!(a.list().len(), 2);
        }
        make.set_phase(Phase::Pushing);
        assert_eq!(a.list(), vec![entry("make", Phase::Pushing, now)]);
        drop(make);
        assert!(a.list().is_empty());
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
            Entry { dir: "/".into(), ..entry("", Phase::Login, now - Duration::from_secs(4)) },
        ];
        let sessions = Sessions { busy: 1, idle: 0, max: 2 };
        assert_eq!(
            render(&entries, sessions, now),
            "STATE     TIME  DIR             COMMAND\n\
             running  12:34  /workarea/game  make\n\
             login     0:04  /               (login shell)\n\
             held sessions: 1 busy, 0 idle, at most 2\n"
        );
        assert_eq!(
            render(&[], Sessions { busy: 0, idle: 2, max: 2 }, now),
            "nothing running in the guest\nheld sessions: 0 busy, 2 idle, at most 2\n"
        );
    }
}
