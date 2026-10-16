//! The `docker` CLI as the host side drives it: running it, and reading the
//! container's state, mounts and environment. Everything goes through the
//! CLI so it follows whatever Docker context is active.

use std::ffi::OsString;
use std::io;
use std::path::PathBuf;
use std::process::{Command, Stdio};

/// The name of the container `dockerix start` runs.
pub const CONTAINER: &str = "dockerix-vm";

pub fn docker() -> Command {
    let mut c = Command::new("docker");
    // Docker Desktop appends "What's next: Try Docker Debug…" hints to exec
    // and friends; they'd land in the middle of guest output.
    c.env("DOCKER_CLI_HINTS", "false");
    c
}

/// `docker …` with stderr inherited and stdout (container IDs and names)
/// discarded; Ok(exit code).
pub fn docker_quiet(args: &[&str]) -> io::Result<i32> {
    #[cfg(test)]
    if let Some((code, _)) = fake::intercept(args) {
        return Ok(code);
    }
    let status = docker().args(args).stdout(Stdio::null()).status().map_err(docker_missing)?;
    Ok(status.code().unwrap_or(1))
}

/// `docker …` for the lifecycle commands that take an owned argument list
/// (`run`), with stdout discarded and stderr inherited; Ok(exit code). The
/// one place a `docker run` is spawned, so a test can replace it.
pub fn docker_run(args: &[OsString]) -> io::Result<i32> {
    #[cfg(test)]
    {
        let argv: Vec<String> = args.iter().map(|a| a.to_string_lossy().into_owned()).collect();
        let refs: Vec<&str> = argv.iter().map(String::as_str).collect();
        if let Some((code, _)) = fake::intercept(&refs) {
            return Ok(code);
        }
    }
    let status = docker().args(args).stdout(Stdio::null()).status().map_err(docker_missing)?;
    Ok(status.code().unwrap_or(1))
}

pub fn docker_missing(e: io::Error) -> io::Error {
    if e.kind() == io::ErrorKind::NotFound { io::Error::other("the `docker` CLI is not on PATH") } else { e }
}

/// Run `docker …` capturing stdout; Ok(None) if docker exited non-zero.
pub fn docker_output(args: &[&str]) -> io::Result<Option<String>> {
    #[cfg(test)]
    if let Some((code, out)) = fake::intercept(args) {
        return Ok((code == 0).then(|| out.trim().to_string()));
    }
    let out = docker().args(args).stderr(Stdio::null()).output().map_err(docker_missing)?;
    if !out.status.success() {
        return Ok(None);
    }
    Ok(Some(String::from_utf8_lossy(&out.stdout).trim().to_string()))
}

/// Container state as Docker reports it.
#[derive(Debug, PartialEq)]
pub enum State {
    /// No container of that name exists.
    Absent,
    /// Exists but is not running (created, exited, …).
    Stopped(String),
    /// Running; the health string is `starting`, `healthy` or `unhealthy`,
    /// and `image` the one it was started from, as named then.
    Running { health: String, started: String, image: String },
}

/// A container's inspectable state, read in one `docker inspect`: the state
/// line, the environment as `KEY=value` lines, and the bind mounts by
/// destination. This is the one place the container is inspected, so a
/// command reads it once and takes its state, settings, work area and mounts
/// from the snapshot rather than inspecting again.
#[derive(Debug, Default, PartialEq)]
pub struct Container {
    status: String,
    health: String,
    started: String,
    /// `.Config.Image`, as the container was started from.
    image: String,
    /// `.Image`, the image ID that tag pointed at then.
    image_id: String,
    /// The environment, as `KEY=value` lines (the `ENV_FORMAT` layout).
    env: String,
    /// Bind mounts as (destination, source).
    mounts: Vec<(String, String)>,
}

/// The combined `inspect` template read into a `Container`: a first line of
/// `status`, health, started, image and image ID (tab-separated), then the
/// environment as `E`-tagged lines and the mounts as `M`-tagged lines. The
/// `\t` and `\n` are real characters (Rust escapes), which docker emits
/// verbatim.
const SNAPSHOT_FORMAT: &str = concat!(
    "{{.State.Status}}\t",
    "{{if .State.Health}}{{.State.Health.Status}}{{else}}none{{end}}\t",
    "{{.State.StartedAt}}\t{{.Config.Image}}\t{{.Image}}\n",
    "{{range .Config.Env}}E\t{{.}}\n{{end}}",
    "{{range .Mounts}}M\t{{.Destination}}\t{{.Source}}\n{{end}}",
);

/// Inspect the container in one `docker inspect`; None if it does not exist.
pub fn inspect() -> io::Result<Option<Container>> {
    Ok(docker_output(&["inspect", "--format", SNAPSHOT_FORMAT, CONTAINER])?.map(|out| parse_container(&out)))
}

/// The `Container` from one `SNAPSHOT_FORMAT` reading.
fn parse_container(out: &str) -> Container {
    let mut lines = out.lines();
    let mut head = lines.next().unwrap_or("").split('\t');
    let mut next = || head.next().unwrap_or("").to_string();
    let (status, health, started, image, image_id) = (next(), next(), next(), next(), next());
    let mut env = String::new();
    let mut mounts = Vec::new();
    for line in lines {
        if let Some(e) = line.strip_prefix("E\t") {
            env.push_str(e);
            env.push('\n');
        } else if let Some(m) = line.strip_prefix("M\t")
            && let Some((dest, source)) = m.split_once('\t')
        {
            mounts.push((dest.to_string(), source.to_string()));
        }
    }
    Container { status, health, started, image, image_id, env, mounts }
}

impl Container {
    /// The state as the lifecycle commands match on it. Never `Absent`: that
    /// is `inspect` returning None.
    pub fn state(&self) -> State {
        if self.status == "running" {
            State::Running {
                health: self.health.clone(),
                started: self.started.clone(),
                image: self.image.clone(),
            }
        } else {
            State::Stopped(self.status.clone())
        }
    }

    /// `.Image`, the image ID, for reading the image's own environment.
    pub fn image_id(&self) -> &str {
        &self.image_id
    }

    /// The environment as `KEY=value` lines.
    pub fn env(&self) -> &str {
        &self.env
    }

    /// A non-empty variable from the container's environment.
    pub fn value(&self, key: &str) -> Option<String> {
        env_value(&self.env, key)
    }

    /// Host source path of the bind mount at `dest`, if any.
    pub fn mount(&self, dest: &str) -> Option<PathBuf> {
        self.mounts
            .iter()
            .find(|(d, _)| d == dest)
            .map(|(_, s)| s.as_str())
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
    }
}

/// `docker inspect` format for a container's or image's environment, as
/// `KEY=value` lines. Still read directly off an image (`docker image
/// inspect`), whose environment is a different object from the container's.
pub const ENV_FORMAT: &str = "{{range .Config.Env}}{{println .}}{{end}}";

/// The container's state, for the commands that need no more of it than
/// that; `Absent` when it does not exist. One `docker inspect`.
pub fn inspect_state() -> io::Result<State> {
    Ok(inspect()?.map_or(State::Absent, |c| c.state()))
}

/// A non-empty variable from `KEY=value` lines.
pub fn env_value(env: &str, key: &str) -> Option<String> {
    let prefix = format!("{key}=");
    env.lines().find_map(|l| l.strip_prefix(prefix.as_str())).filter(|v| !v.is_empty()).map(str::to_string)
}

/// The test seam: a stand-in for the `docker` CLI, installed on the current
/// test thread, that answers the reads dockerix makes from a fixture and
/// records every call, so `host.rs`'s command flows can be driven with no
/// Docker daemon. `docker_output`, `docker_quiet` and `docker_run` consult
/// it; `host.rs`'s call sites are unchanged.
#[cfg(test)]
pub(crate) mod fake {
    use std::cell::RefCell;

    use super::{CONTAINER, ENV_FORMAT};

    thread_local! {
        static FAKE: RefCell<Option<Fake>> = const { RefCell::new(None) };
    }

    /// The fixture for one test thread: a container (or its absence) and the
    /// log of `docker` calls made against it.
    struct Fake {
        /// The container, or None for "no such container" (every `inspect`
        /// then fails, as Docker's does).
        container: Option<Container>,
        calls: RefCell<Vec<Vec<String>>>,
    }

    /// What a present container answers `inspect`, `port` and `exec … agent`
    /// from.
    #[derive(Clone, Default)]
    pub struct Container {
        /// `.State.Status`: `running`, `exited`, `created`, …
        pub status: String,
        /// `.State.Health.Status`: `healthy`, `starting`, `unhealthy`, or
        /// `none` for an image with no health check.
        pub health: String,
        pub started: String,
        /// `.Config.Image`, as named at start.
        pub image: String,
        /// `.Image`, the image ID the tag pointed at then.
        pub image_id: String,
        /// The container's environment, as `inspect` lists it.
        pub env: Vec<(String, String)>,
        /// The image's own environment (its `ENV` defaults).
        pub image_env: Vec<(String, String)>,
        /// Bind mounts, as (destination, source).
        pub mounts: Vec<(String, String)>,
        /// The host side of the published 6080 port, if any.
        pub port6080: Option<String>,
        /// Replies for `dockerix agent <key> …` run through `docker exec`,
        /// keyed by the agent subcommand (`nfs`, `ps`, `exec`).
        pub agent: Vec<(String, String)>,
    }

    /// Install a fake `docker` on the current thread, answering for
    /// `container` (None for "no such container"). The returned guard removes
    /// it when it drops, so no fixture leaks into the next test on this thread.
    pub fn install(container: Option<Container>) -> Guard {
        FAKE.with(|f| *f.borrow_mut() = Some(Fake { container, calls: RefCell::new(Vec::new()) }));
        Guard
    }

    pub struct Guard;

    impl Drop for Guard {
        fn drop(&mut self) {
            FAKE.with(|f| *f.borrow_mut() = None);
        }
    }

    /// The `docker` calls made since the fake was installed, oldest first.
    pub fn calls() -> Vec<Vec<String>> {
        FAKE.with(|f| f.borrow().as_ref().map(|fake| fake.calls.borrow().clone()).unwrap_or_default())
    }

    /// Answer a `docker` invocation from the installed fake, recording it;
    /// None when no fake is installed (the real `docker` then runs).
    pub(super) fn intercept(args: &[&str]) -> Option<(i32, String)> {
        FAKE.with(|f| {
            let g = f.borrow();
            let fake = g.as_ref()?;
            fake.calls.borrow_mut().push(args.iter().map(|s| s.to_string()).collect());
            Some(fake.reply(args))
        })
    }

    impl Fake {
        /// `(exit code, stdout)` for one `docker` invocation. The lifecycle
        /// commands (`run`, `stop`, `rm`) are reported as success; the reads
        /// come from the container fixture, and a read of an absent container
        /// fails as Docker's `inspect` does.
        fn reply(&self, args: &[&str]) -> (i32, String) {
            if args == ["stop", CONTAINER] || args == ["rm", CONTAINER] || args.first() == Some(&"run") {
                return (0, String::new());
            }
            let Some(c) = &self.container else {
                return (1, String::new());
            };
            match args {
                ["port", CONTAINER, "6080"] => match &c.port6080 {
                    Some(p) => (0, p.clone()),
                    None => (1, String::new()),
                },
                // The one container reader: the combined snapshot template.
                ["inspect", "--format", _, name] if *name == CONTAINER => (0, c.snapshot()),
                // The image's own environment, a different object.
                ["image", "inspect", "--format", fmt, _] if *fmt == ENV_FORMAT => (0, lines(&c.image_env)),
                ["image", "inspect", "--format", "{{.Id}}", _] => (0, c.image_id.clone()),
                _ => c.exec_reply(args).unwrap_or((0, String::new())),
            }
        }
    }

    impl Container {
        /// Render the fixture as one `SNAPSHOT_FORMAT` reading: the state line,
        /// then the environment and mounts as tagged lines.
        fn snapshot(&self) -> String {
            let mut out = format!(
                "{}\t{}\t{}\t{}\t{}\n",
                self.status, self.health, self.started, self.image, self.image_id
            );
            for (k, v) in &self.env {
                out.push_str(&format!("E\t{k}={v}\n"));
            }
            for (dest, source) in &self.mounts {
                out.push_str(&format!("M\t{dest}\t{source}\n"));
            }
            out
        }

        /// A reply for `docker exec … dockerix agent <key> …`, by the agent
        /// subcommand.
        fn exec_reply(&self, args: &[&str]) -> Option<(i32, String)> {
            let agent = args.iter().position(|a| *a == "agent")?;
            let key = args.get(agent + 1)?;
            match self.agent.iter().find(|(k, _)| k == key) {
                Some((_, out)) => Some((0, out.clone())),
                None => Some((1, String::new())),
            }
        }
    }

    /// `KEY=value` lines, as `inspect`'s environment format prints them.
    fn lines(env: &[(String, String)]) -> String {
        env.iter().map(|(k, v)| format!("{k}={v}\n")).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_value_finds_non_empty_values() {
        let env = "PATH=/bin\nEMPTY=\nXKEY=no\nKEY=a=b\n";
        assert_eq!(env_value(env, "PATH").as_deref(), Some("/bin"));
        assert_eq!(env_value(env, "KEY").as_deref(), Some("a=b"));
        assert_eq!(env_value(env, "EMPTY"), None);
        assert_eq!(env_value(env, "KE"), None);
        assert_eq!(env_value("", "PATH"), None);
    }

    #[test]
    fn parse_container_reads_the_state_env_and_mounts() {
        let out = "running\thealthy\t2026-10-12T06:00:00Z\tdockerix:tag\tsha256:img\n\
                   E\tPATH=/bin\n\
                   E\tDOCKERIX_NFS=1\n\
                   M\t/workarea\t/home/me/wa\n\
                   M\t/state\t/s\n";
        let c = parse_container(out);
        assert_eq!(
            c.state(),
            State::Running {
                health: "healthy".into(),
                started: "2026-10-12T06:00:00Z".into(),
                image: "dockerix:tag".into(),
            }
        );
        assert_eq!(c.image_id(), "sha256:img");
        assert_eq!(c.value("DOCKERIX_NFS").as_deref(), Some("1"));
        assert_eq!(c.mount("/workarea"), Some(PathBuf::from("/home/me/wa")));
        assert_eq!(c.mount("/assets"), None);

        // A stopped container: no health, no mounts.
        let c = parse_container("exited\tnone\t\tdockerix:tag\tsha256:img");
        assert_eq!(c.state(), State::Stopped("exited".into()));
        assert_eq!(c.mount("/state"), None);
    }
}
