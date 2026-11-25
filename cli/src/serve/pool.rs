//! The serve-mode pool of logged-in guest sessions: affinity by key, at
//! most `max` at once (logins in progress included), idle ones logged out
//! after a while, and the interrupt flags of the runs in progress.
//!
//! The invariant to keep: `live` counts every session that exists, idle or
//! in use, plus logins under way. `checkout` alone raises it, before a
//! login; `forget` alone lowers it.

use std::collections::{HashMap, VecDeque};
use std::io::{self, ErrorKind};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use super::activity::{Activity, Sessions};
use crate::config::Config;
use crate::session::{Session, Timing};

pub(super) struct Held {
    session: Session,
    key: Option<String>,
    idle_since: Instant,
}

pub(super) struct PoolState {
    pub(super) idle: VecDeque<Held>,
    /// Sessions that exist (idle or in use) plus logins in progress.
    pub(super) live: usize,
}

pub(super) struct Pool {
    cfg: Config,
    max: usize,
    idle_timeout: Duration,
    /// The waits for the pool's guest sessions.
    timing: Timing,
    state: Mutex<PoolState>,
    cv: Condvar,
    pub(super) sync_lock: Mutex<()>,
    /// In-progress runs by id, each with its interrupt flag.
    pub(super) runs: Mutex<HashMap<String, Arc<AtomicBool>>>,
    /// Aborts that arrived before their run registered, with arrival time.
    early_aborts: Mutex<HashMap<String, Instant>>,
    /// What is running in the guest, for `dockerix ps`.
    pub(super) activity: Activity,
}

/// How long an abort for an unknown run id is remembered.
const EARLY_ABORT_TTL: Duration = Duration::from_secs(30);

impl Pool {
    pub(super) fn new(cfg: Config, max: usize, idle_timeout: Duration, timing: Timing) -> Pool {
        Pool {
            cfg,
            max: max.max(1),
            idle_timeout,
            timing,
            state: Mutex::new(PoolState { idle: VecDeque::new(), live: 0 }),
            cv: Condvar::new(),
            sync_lock: Mutex::new(()),
            runs: Mutex::new(HashMap::new()),
            early_aborts: Mutex::new(HashMap::new()),
            activity: Activity::default(),
        }
    }

    /// The resolved settings the agent runs with (threaded into the push).
    pub(super) fn config(&self) -> &Config {
        &self.cfg
    }

    /// The held sessions now: in use (logins under way included), idle,
    /// and the limit.
    pub(super) fn sessions(&self) -> Sessions {
        let st = self.lock();
        Sessions { busy: st.live - st.idle.len(), idle: st.idle.len(), max: self.max }
    }

    /// Register a run; returns None if an abort already arrived for it.
    pub(super) fn register(&self, id: &str) -> Option<Arc<AtomicBool>> {
        let mut early = self.early_aborts.lock().unwrap_or_else(|p| p.into_inner());
        early.retain(|_, t| t.elapsed() < EARLY_ABORT_TTL);
        if early.remove(id).is_some() {
            return None;
        }
        let flag = Arc::new(AtomicBool::new(false));
        self.runs.lock().unwrap_or_else(|p| p.into_inner()).insert(id.to_string(), Arc::clone(&flag));
        Some(flag)
    }

    pub(super) fn unregister(&self, id: &str) {
        self.runs.lock().unwrap_or_else(|p| p.into_inner()).remove(id);
    }

    /// Flag the run `id` for interruption, or remember the request if it
    /// has not started yet. Returns whether a run was found.
    pub(super) fn abort(&self, id: &str) -> bool {
        let flag = self.runs.lock().unwrap_or_else(|p| p.into_inner()).get(id).cloned();
        if let Some(flag) = flag {
            flag.store(true, Ordering::SeqCst);
            self.wake_waiters();
            return true;
        }
        self.early_aborts.lock().unwrap_or_else(|p| p.into_inner()).insert(id.to_string(), Instant::now());
        false
    }

    pub(super) fn lock(&self) -> std::sync::MutexGuard<'_, PoolState> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Take a session: the client's own if it is idle, else an idle one no
    /// client has claimed, else a new login while under the limit, else the
    /// longest-idle session of another client (its shell state comes along;
    /// see the module comment), else wait for one to be returned. Sessions
    /// found dead are discarded on the way. Once `interrupt` is raised
    /// (Ctrl-C, `dockerix kill`, the client gone) it stops waiting, with
    /// `ErrorKind::Interrupted`; whoever raises it calls `wake_waiters`.
    pub(super) fn checkout(
        &self,
        key: Option<&str>,
        deadline: Instant,
        interrupt: Option<&AtomicBool>,
    ) -> io::Result<Session> {
        loop {
            let mut st = self.lock();
            loop {
                if interrupt.is_some_and(|f| f.load(Ordering::SeqCst)) {
                    // A checkin or discard may have woken this waiter rather
                    // than another: pass the wakeup on, or a session could
                    // sit idle while someone waits.
                    self.cv.notify_one();
                    return Err(io::Error::new(
                        ErrorKind::Interrupted,
                        "interrupted while waiting for a guest session",
                    ));
                }
                let pos = key
                    .and_then(|k| st.idle.iter().position(|h| h.key.as_deref() == Some(k)))
                    .or_else(|| st.idle.iter().position(|h| h.key.is_none()))
                    .or_else(|| if st.live >= self.max && !st.idle.is_empty() { Some(0) } else { None });
                let Some(pos) = pos else { break };
                let held = st.idle.remove(pos).expect("index from position");
                let mut session = held.session;
                if session.alive() {
                    return Ok(session);
                }
                // Dead (guest rebooted, shell exited): drop it and look again.
                self.forget(&mut st, 1);
                drop(st);
                eprintln!("exec: discarding a dead held session");
                st = self.lock();
            }
            if st.live < self.max {
                // Counted from before the login, so concurrent checkouts
                // cannot overshoot the limit; `forget` undoes it on failure.
                st.live += 1;
                drop(st);
                let (cols, rows) = (80, 24);
                match Session::connect_and_login_with(
                    &self.cfg.telnet_addr,
                    false,
                    cols,
                    rows,
                    &self.cfg.user,
                    &self.cfg.pass,
                    self.timing,
                ) {
                    Ok(mut s) => {
                        // In NFS mode a brand-new login (first command, or the
                        // first after a guest reboot) may need the share
                        // mounted; a no-op otherwise.
                        if let Err(e) = crate::exec::ensure_nfs_mount(&self.cfg, &mut s, deadline) {
                            self.forget(&mut self.lock(), 1);
                            return Err(e);
                        }
                        return Ok(s);
                    }
                    Err(e) => {
                        self.forget(&mut self.lock(), 1);
                        return Err(e);
                    }
                }
            }
            let now = Instant::now();
            if now >= deadline {
                return Err(io::Error::new(
                    ErrorKind::TimedOut,
                    "timed out waiting for a free guest session",
                ));
            }
            let (_st, _) = self.cv.wait_timeout(st, deadline - now).unwrap_or_else(|p| p.into_inner());
        }
    }

    /// Take `n` sessions out of the count of live ones: logged out, found
    /// dead, or a login that failed. The only place the count goes down;
    /// each place it frees may let a waiting checkout log in, so it wakes
    /// one waiter per place.
    fn forget(&self, st: &mut PoolState, n: usize) {
        st.live -= n;
        for _ in 0..n {
            self.cv.notify_one();
        }
    }

    /// Wake every waiting checkout, to look at its interrupt flag: called
    /// after raising one. Taking the lock first means a waiter is either
    /// still to look (and sees the flag) or already waiting (and wakes).
    pub(super) fn wake_waiters(&self) {
        let _st = self.lock();
        self.cv.notify_all();
    }

    pub(super) fn checkin(&self, session: Session, key: Option<String>) {
        let mut st = self.lock();
        st.idle.push_back(Held { session, key, idle_since: Instant::now() });
        drop(st);
        self.cv.notify_one();
    }

    /// The session is in an unknown state (error mid-command): log it out
    /// as best we can and forget it.
    pub(super) fn discard(&self, mut session: Session) {
        session.logout();
        self.forget(&mut self.lock(), 1);
    }

    /// Log out sessions idle for longer than the idle timeout.
    pub(super) fn reap(&self) {
        let mut expired = Vec::new();
        {
            let mut st = self.lock();
            let now = Instant::now();
            let mut i = 0;
            while i < st.idle.len() {
                if now.duration_since(st.idle[i].idle_since) >= self.idle_timeout {
                    expired.push(st.idle.remove(i).expect("index in range"));
                } else {
                    i += 1;
                }
            }
            self.forget(&mut st, expired.len());
        }
        for h in expired {
            let mut s = h.session;
            s.logout();
            eprintln!("exec: logged out an idle guest session");
        }
    }

    /// Log everything out (before the guest is halted).
    pub(super) fn drain(&self) {
        let held: Vec<Held> = self.lock().idle.drain(..).collect();
        for h in held {
            let mut s = h.session;
            s.logout();
            self.forget(&mut self.lock(), 1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake_guest::FakeGuest;
    use crate::serve::tests::{pool, soon};
    use std::thread;

    /// Which connection a session is: its local port.
    fn port(s: &mut Session) -> u16 {
        s.telnet_mut().get_ref().local_addr().unwrap().port()
    }

    #[test]
    fn a_key_gets_back_its_own_session() {
        let guest = FakeGuest::start();
        let pool = pool(&guest, 2, Duration::from_secs(600));
        let mut a = pool.checkout(Some("a"), soon(), None).unwrap();
        let pa = port(&mut a);
        pool.checkin(a, Some("a".into()));
        // A new client logs in afresh while the pool has room.
        let mut b = pool.checkout(Some("b"), soon(), None).unwrap();
        assert_ne!(port(&mut b), pa);
        pool.checkin(b, Some("b".into()));
        let mut a = pool.checkout(Some("a"), soon(), None).unwrap();
        assert_eq!(port(&mut a), pa);
        assert_eq!(guest.state().logins, 2);
    }

    #[test]
    fn an_unclaimed_session_is_used_before_a_new_login() {
        let guest = FakeGuest::start();
        let pool = pool(&guest, 2, Duration::from_secs(600));
        let mut s = pool.checkout(None, soon(), None).unwrap();
        let p = port(&mut s);
        pool.checkin(s, None);
        let mut s = pool.checkout(Some("x"), soon(), None).unwrap();
        assert_eq!(port(&mut s), p);
        assert_eq!(guest.state().logins, 1);
    }

    #[test]
    fn at_the_limit_the_longest_idle_session_is_borrowed() {
        let guest = FakeGuest::start();
        let pool = pool(&guest, 2, Duration::from_secs(600));
        let mut a = pool.checkout(Some("a"), soon(), None).unwrap();
        let b = pool.checkout(Some("b"), soon(), None).unwrap();
        let pa = port(&mut a);
        pool.checkin(a, Some("a".into()));
        pool.checkin(b, Some("b".into()));
        let mut c = pool.checkout(Some("c"), soon(), None).unwrap();
        assert_eq!(port(&mut c), pa);
        assert_eq!(guest.state().logins, 2);
    }

    #[test]
    fn at_the_limit_with_none_idle_checkout_waits() {
        let guest = FakeGuest::start();
        let pool = pool(&guest, 1, Duration::from_secs(600));
        let held = pool.checkout(None, soon(), None).unwrap();
        let e = pool.checkout(None, Instant::now() + Duration::from_millis(30), None).err().unwrap();
        assert_eq!(e.kind(), ErrorKind::TimedOut);
        let returner = Arc::clone(&pool);
        let t = thread::spawn(move || {
            thread::sleep(Duration::from_millis(20));
            returner.checkin(held, None);
        });
        assert!(pool.checkout(None, soon(), None).is_ok());
        t.join().unwrap();
        assert_eq!(guest.state().logins, 1);
    }

    #[test]
    fn an_interrupt_stops_a_waiting_checkout() {
        let guest = FakeGuest::start();
        let pool = pool(&guest, 1, Duration::from_secs(600));
        let _held = pool.checkout(None, soon(), None).unwrap();
        let flag = Arc::new(AtomicBool::new(false));
        let (p, f) = (Arc::clone(&pool), Arc::clone(&flag));
        let waiter =
            thread::spawn(move || p.checkout(None, Instant::now() + Duration::from_secs(60), Some(&f)));
        thread::sleep(Duration::from_millis(20));
        let raised = Instant::now();
        flag.store(true, Ordering::SeqCst);
        pool.wake_waiters();
        let e = waiter.join().unwrap().err().unwrap();
        assert_eq!(e.kind(), ErrorKind::Interrupted);
        assert!(raised.elapsed() < Duration::from_secs(5), "waited {:?}", raised.elapsed());
        // Raised before it ever waits, it does not wait at all.
        let e = pool.checkout(None, Instant::now() + Duration::from_secs(60), Some(&flag)).err().unwrap();
        assert_eq!(e.kind(), ErrorKind::Interrupted);
        assert_eq!(guest.state().logins, 1);
    }

    /// The checkin's one wakeup may reach the interrupted waiter rather than
    /// the other: it must pass it on, not leave the session idle while the
    /// other waits out its deadline.
    #[test]
    fn an_interrupted_waiter_passes_the_wakeup_on() {
        let guest = FakeGuest::start();
        let pool = pool(&guest, 1, Duration::from_secs(600));
        let held = pool.checkout(None, soon(), None).unwrap();
        let flag = Arc::new(AtomicBool::new(false));
        let long = Instant::now() + Duration::from_secs(10);
        let (p, f) = (Arc::clone(&pool), Arc::clone(&flag));
        let quitter = thread::spawn(move || p.checkout(None, long, Some(&f)));
        // Waiting first, it is the likelier to get the wakeup.
        thread::sleep(Duration::from_millis(20));
        let p = Arc::clone(&pool);
        let other = thread::spawn(move || p.checkout(None, long, None));
        thread::sleep(Duration::from_millis(20));
        // Raised without a wake_waiters: only the checkin wakes anyone.
        flag.store(true, Ordering::SeqCst);
        let returned = Instant::now();
        pool.checkin(held, None);
        assert!(other.join().unwrap().is_ok());
        assert!(returned.elapsed() < Duration::from_secs(5), "waited {:?}", returned.elapsed());
        pool.wake_waiters();
        assert_eq!(quitter.join().unwrap().err().unwrap().kind(), ErrorKind::Interrupted);
    }

    #[test]
    fn a_discard_lets_a_waiting_checkout_log_in() {
        let guest = FakeGuest::start();
        let pool = pool(&guest, 1, Duration::from_secs(600));
        let held = pool.checkout(None, soon(), None).unwrap();
        let discarder = Arc::clone(&pool);
        let t = thread::spawn(move || {
            thread::sleep(Duration::from_millis(20));
            discarder.discard(held);
        });
        assert!(pool.checkout(None, soon(), None).is_ok());
        t.join().unwrap();
        let st = guest.state();
        assert_eq!((st.logins, st.logouts), (2, 1));
        assert_eq!(pool.lock().live, 1);
    }

    #[test]
    fn a_dead_session_is_discarded() {
        let guest = FakeGuest::start();
        let pool = pool(&guest, 1, Duration::from_secs(600));
        let mut s = pool.checkout(Some("a"), soon(), None).unwrap();
        guest.hangup();
        // The close reaches the client a moment after the guest makes it.
        let until = soon();
        while s.alive() {
            assert!(Instant::now() < until, "the hangup never arrived");
            thread::sleep(Duration::from_millis(1));
        }
        pool.checkin(s, Some("a".into()));
        let _s = pool.checkout(Some("a"), soon(), None).unwrap();
        assert_eq!(guest.state().logins, 2);
        assert_eq!(pool.lock().live, 1);
    }

    #[test]
    fn idle_sessions_are_reaped_and_discards_logged_out() {
        let guest = FakeGuest::start();
        let pool = pool(&guest, 2, Duration::from_millis(20));
        let old = pool.checkout(None, soon(), None).unwrap();
        let busy = pool.checkout(None, soon(), None).unwrap();
        pool.checkin(old, None);
        thread::sleep(Duration::from_millis(30));
        pool.reap();
        assert_eq!(guest.state().logouts, 1);
        assert_eq!(pool.lock().live, 1);
        // Too fresh to reap.
        let fresh = Pool::new(guest.config(), 1, Duration::from_secs(600), Timing::TEST);
        let s = fresh.checkout(None, soon(), None).unwrap();
        fresh.checkin(s, None);
        fresh.reap();
        assert_eq!(guest.state().logouts, 1);
        // A session in an unknown state is logged out and forgotten.
        pool.discard(busy);
        assert_eq!(guest.state().logouts, 2);
        assert_eq!(pool.lock().live, 0);
        fresh.drain();
        assert_eq!(guest.state().logouts, 3);
        assert_eq!(fresh.lock().live, 0);
    }

    #[test]
    fn an_early_abort_is_remembered() {
        let guest = FakeGuest::start();
        let pool = pool(&guest, 1, Duration::from_secs(600));
        assert!(!pool.abort("r1"));
        assert!(pool.register("r1").is_none());
        // Remembered once only, and only for that id.
        let flag = pool.register("r2").unwrap();
        assert!(pool.abort("r2"));
        assert!(flag.load(Ordering::SeqCst));
        pool.unregister("r2");
        assert!(!pool.abort("r2"));
    }
}
