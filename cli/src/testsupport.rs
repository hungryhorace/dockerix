//! Helpers shared by the unit tests.

use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard};

use tempfile::TempDir;

/// Serialises the tests that read or change the process environment: cargo
/// runs tests on parallel threads and the environment is process-wide.
static ENV_LOCK: Mutex<()> = Mutex::new(());

/// Exclusive use of the process environment for one test. Variables set or
/// removed through it are put back as they were when it drops, so a test
/// neither sees the developer's `DOCKERIX_*` settings nor leaks its own.
///
/// Changing the environment is `unsafe` because another thread may be
/// reading it outside std's own lock, through C's `getenv`. Here the other
/// threads are tests, and the code under test reads the environment only
/// through `std::env` (and `Command`, which takes the same lock); its
/// addresses are IP literals, so no resolver runs to read it from C.
pub struct Env {
    saved: Vec<(String, Option<OsString>)>,
    _lock: MutexGuard<'static, ()>,
}

#[allow(unsafe_code)] // set_var/remove_var: see above
impl Env {
    /// Take the lock. A test that panicked while holding it poisons it;
    /// the environment is restored by then, so carry on.
    pub fn lock() -> Env {
        let lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        Env { saved: Vec::new(), _lock: lock }
    }

    pub fn set(&mut self, key: &str, value: &str) -> &mut Env {
        self.save(key);
        // SAFETY: see `Env`.
        unsafe { std::env::set_var(key, value) };
        self
    }

    pub fn unset(&mut self, key: &str) -> &mut Env {
        self.save(key);
        // SAFETY: see `Env`.
        unsafe { std::env::remove_var(key) };
        self
    }

    fn save(&mut self, key: &str) {
        if !self.saved.iter().any(|(k, _)| k == key) {
            self.saved.push((key.to_string(), std::env::var_os(key)));
        }
    }
}

#[allow(unsafe_code)] // set_var/remove_var: see `Env`
impl Drop for Env {
    fn drop(&mut self) {
        for (key, value) in self.saved.drain(..) {
            // SAFETY: see `Env`; the lock is still held here.
            match value {
                Some(v) => unsafe { std::env::set_var(&key, v) },
                None => unsafe { std::env::remove_var(&key) },
            }
        }
    }
}

/// A fresh, empty directory for one test, named for it. Removed with
/// everything in it when the result drops, pass or fail.
pub fn tmp_dir(tag: &str) -> TempDir {
    tempfile::Builder::new()
        .prefix(&format!("dockerix-test-{tag}-"))
        .tempdir()
        .expect("create a test directory")
}

/// A work area under a temp dir (`hello.c`, `sub/x.h` and an empty
/// directory). Returns the temp dir and the work-area path; the tests build
/// a `Config` pointing a push at it (manifest and stage beside it), rather
/// than the environment. The temp dir goes when the first value drops.
pub fn work_area(tag: &str) -> (TempDir, PathBuf) {
    let tmp = tmp_dir(tag);
    let wa = tmp.path().join("workarea");
    std::fs::create_dir_all(wa.join("sub")).unwrap();
    std::fs::create_dir_all(wa.join("empty")).unwrap();
    std::fs::write(wa.join("hello.c"), "int main() { return 0; }\n").unwrap();
    std::fs::write(wa.join("sub/x.h"), "#define X 1\n").unwrap();
    (tmp, wa)
}
