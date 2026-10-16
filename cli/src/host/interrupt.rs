//! Ctrl-C during a non-tty command. `docker exec` forwards no signals, so
//! the wrapper catches SIGINT itself, makes sure the docker client is gone,
//! and asks the agent to interrupt the guest command (host/rix.rs).

use std::io;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use rustix::process::{Pid, Signal, kill_process};
use signal_hook::consts::SIGINT;

use crate::docker::docker_missing;

/// Set by SIGINT once `install_sigint_flag` has run.
static INTERRUPTED: LazyLock<Arc<AtomicBool>> = LazyLock::new(Default::default);

/// Run the docker client, and if SIGINT arrives make sure it is gone (a
/// terminal Ctrl-C reaches it too, a `kill -INT` on us alone does not).
pub(super) fn wait_interruptible(mut cmd: Command) -> io::Result<std::process::ExitStatus> {
    let mut child = cmd.spawn().map_err(docker_missing)?;
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(status);
        }
        if INTERRUPTED.load(Ordering::SeqCst) {
            let _ = kill_process(Pid::from_child(&child), Signal::TERM);
            return child.wait();
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Catch SIGINT instead of dying: the docker client (same process group)
/// exits on it, `status()` returns, and we send the abort.
pub(super) fn install_sigint_flag() -> io::Result<()> {
    signal_hook::flag::register(SIGINT, Arc::clone(&INTERRUPTED))?;
    Ok(())
}

/// Whether SIGINT has arrived since `install_sigint_flag`.
pub(super) fn interrupted() -> bool {
    INTERRUPTED.load(Ordering::SeqCst)
}
