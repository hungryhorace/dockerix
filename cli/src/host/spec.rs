//! What `docker run` needs to start the container (`RunSpec`), its
//! arguments, and the same read back from a running container so `restart`
//! can recreate it.

use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};

use super::*;
use crate::docker::{ENV_FORMAT, docker_output, docker_quiet, docker_run, env_value};

/// Agent settings read at container start, passed into the container when
/// set on the host. Their defaults live in the Dockerfile's `ENV` and the
/// agent. See docs/how-it-works.md: the live NFS mount, the guest clock,
/// X11 forwarding, held sessions.
pub(super) const START_ENV: [&str; 5] =
    [var::NFS, var::SET_CLOCK, var::CLOCK_INTERVAL, var::X11, var::SESSIONS];

/// The same settings as given on the command line (`--nfs`, `--x11`,
/// `--clock-sync`, `--clock-interval`, `--sessions`), for `start`, `run` and
/// `restart`. Each one given wins over its variable; None leaves it to the
/// variable, or for `restart` to what the container was started with.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct AgentSettings {
    pub nfs: Option<bool>,
    pub x11: Option<bool>,
    pub clock_sync: Option<bool>,
    pub clock_interval: Option<u64>,
    pub sessions: Option<usize>,
}

impl AgentSettings {
    /// The settings given, as `START_ENV` variables.
    pub(super) fn env(&self) -> Vec<(String, String)> {
        let flag = |b: bool| if b { "1" } else { "0" }.to_string();
        [
            (var::NFS, self.nfs.map(flag)),
            (var::X11, self.x11.map(flag)),
            (var::SET_CLOCK, self.clock_sync.map(flag)),
            (var::CLOCK_INTERVAL, self.clock_interval.map(|s| s.to_string())),
            (var::SESSIONS, self.sessions.map(|n| n.to_string())),
        ]
        .into_iter()
        .filter_map(|(k, v)| v.map(|v| (k.to_string(), v)))
        .collect()
    }

    pub(super) fn any(&self) -> bool {
        *self != AgentSettings::default()
    }
}

/// `env` with `overrides` put in place of the same keys.
pub(super) fn merge_env(env: &mut Vec<(String, String)>, overrides: Vec<(String, String)>) {
    for (key, value) in overrides {
        env.retain(|(k, _)| *k != key);
        env.push((key, value));
    }
}

/// The `START_ENV` settings set on the host, with the ones given as options
/// in their place.
pub(super) fn start_env(settings: &AgentSettings) -> Vec<(String, String)> {
    let mut env = START_ENV
        .iter()
        .filter_map(|k| std::env::var(k).ok().filter(|v| !v.is_empty()).map(|v| (k.to_string(), v)))
        .collect();
    merge_env(&mut env, settings.env());
    env
}

/// How long `docker stop` waits for the entrypoint to halt the guest before
/// killing it; a halt takes about 15 s.
pub(super) const STOP_TIMEOUT_SECS: u32 = 60;

/// What `docker run` needs to start the container: absolute host paths.
pub(super) struct RunSpec {
    pub(super) image: String,
    /// Run only a local image (one built from the embedded context): never
    /// let docker look for it on Docker Hub.
    pub(super) local: bool,
    pub(super) state: PathBuf,
    pub(super) assets: PathBuf,
    pub(super) workarea: Option<PathBuf>,
    pub(super) workdir: Option<String>,
    /// Serve the console (noVNC on port 6080); off for `--no-vnc`.
    pub(super) vnc: bool,
    /// `--profile`, if given (the entrypoint defaults to `fast`).
    pub(super) profile: Option<Profile>,
    /// `START_ENV` settings that are set on the host.
    pub(super) env: Vec<(String, String)>,
}

/// The `docker run` arguments for the persistent container.
pub(super) fn run_args(spec: &RunSpec) -> Vec<OsString> {
    let bind = |src: &Path, dest: &str, opts: &str| {
        let mut v = OsString::from(src);
        v.push(format!(":{dest}{opts}"));
        v
    };
    let mut args: Vec<OsString> = ["run", "-d", "--name", CONTAINER].iter().map(OsString::from).collect();
    if spec.local {
        args.extend(["--pull".into(), "never".into()]);
    }
    // Bound to loopback only: the guest's telnet allows root and is
    // cleartext, and noVNC has no password. The exec channel does not use
    // these (the agent telnets to localhost:42323 inside the container); they
    // are for reaching the VM from the host itself, not the LAN.
    if spec.vnc {
        args.extend(["-p".into(), "127.0.0.1:6080:6080".into()]);
    }
    args.extend(["-p".into(), "127.0.0.1:42323:42323".into()]);
    args.extend(["-v".into(), bind(&spec.state, STATE_MOUNT, "")]);
    args.extend(["-v".into(), bind(&spec.assets, ASSETS_MOUNT, ":ro")]);
    if let Some(dir) = &spec.workarea {
        args.extend(["-v".into(), bind(dir, WORKAREA_MOUNT, "")]);
    }
    for (key, value) in &spec.env {
        args.extend(["-e".into(), format!("{key}={value}").into()]);
    }
    if !spec.vnc {
        args.extend(["-e".into(), format!("{}=0", var::VNC).into()]);
    }
    if let Some(p) = spec.profile {
        args.extend(["-e".into(), format!("{}={}", var::PROFILE, p.name()).into()]);
    }
    // Fixed for the container's life: later logins and commands start there.
    if let Some(dir) = &spec.workdir {
        args.extend(["-e".into(), format!("{}={dir}", var::WORKDIR).into()]);
    }
    args.extend(["--stop-timeout".into(), STOP_TIMEOUT_SECS.to_string().into(), (&spec.image).into()]);
    args
}

/// Start the container. A stopped container of the same name (after a crash
/// or a bare `docker stop`) is removed first: nothing in it needs keeping,
/// since the disc and CMOS are in /state.
pub(super) fn run_container(spec: &RunSpec) -> io::Result<()> {
    if let State::Stopped(_) = inspect_state()? {
        docker_quiet(&["rm", CONTAINER])?;
    }
    eprintln!("dockerix: starting {CONTAINER}…");
    match docker_run(&run_args(spec))? {
        0 => Ok(()),
        code => Err(io::Error::other(format!("docker run failed (exit {code})"))),
    }
}

/// The `RunSpec` the existing container was started with, from its snapshot,
/// image aside (left empty for the caller).
pub(super) fn current_spec(c: &Container) -> io::Result<RunSpec> {
    let mount = |dest: &str| {
        c.mount(dest).ok_or_else(|| io::Error::other(format!("{CONTAINER} has no {dest} mount")))
    };
    // The image's own ENV (its defaults), to tell them from settings given
    // at start; by ID, since the tag may have moved. A different object from
    // the container, so still its own `docker image inspect`.
    let image_env =
        docker_output(&["image", "inspect", "--format", ENV_FORMAT, c.image_id()])?.unwrap_or_default();
    let started = started_with(c.env(), &image_env);
    let profile = match started.profile {
        Some(name) => Some(Profile::from_name(&name).ok_or_else(|| {
            io::Error::other(format!("{CONTAINER} runs profile {name:?}, which this dockerix does not know"))
        })?),
        None => None,
    };
    Ok(RunSpec {
        image: String::new(),
        local: false,
        state: mount(STATE_MOUNT)?,
        assets: mount(ASSETS_MOUNT)?,
        workarea: c.mount(WORKAREA_MOUNT),
        workdir: started.workdir,
        vnc: started.vnc,
        profile,
        env: started.env,
    })
}

/// The start options found in a container's environment.
#[derive(Debug, PartialEq)]
pub(super) struct StartedWith {
    workdir: Option<String>,
    vnc: bool,
    profile: Option<String>,
    /// `START_ENV` settings that differ from the image's defaults.
    env: Vec<(String, String)>,
}

/// Read the start options from a container's environment and its image's
/// (`KEY=value` lines each).
pub(super) fn started_with(env: &str, image_env: &str) -> StartedWith {
    let get = |key: &str| env_value(env, key);
    let defaults: Vec<&str> = image_env.lines().collect();
    StartedWith {
        workdir: get(var::WORKDIR),
        vnc: get(var::VNC).as_deref() != Some("0"),
        profile: get(var::PROFILE),
        env: START_ENV
            .iter()
            .filter_map(|k| {
                let v = get(k)?;
                (!defaults.contains(&format!("{k}={v}").as_str())).then(|| (k.to_string(), v))
            })
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(args: Vec<OsString>) -> Vec<String> {
        args.into_iter().map(|a| a.into_string().unwrap()).collect()
    }

    #[test]
    fn run_args_minimal() {
        let spec = RunSpec {
            image: "dockerix:0123456789ab".into(),
            local: true,
            state: "/s".into(),
            assets: "/a".into(),
            workarea: None,
            workdir: None,
            vnc: true,
            profile: None,
            env: vec![],
        };
        assert_eq!(
            strings(run_args(&spec)).join(" "),
            "run -d --name dockerix-vm --pull never \
             -p 127.0.0.1:6080:6080 -p 127.0.0.1:42323:42323 \
             -v /s:/state -v /a:/assets:ro --stop-timeout 60 dockerix:0123456789ab"
        );
    }

    #[test]
    fn options_given_replace_their_variables() {
        let settings = AgentSettings {
            nfs: Some(true),
            x11: Some(false),
            clock_sync: None,
            clock_interval: Some(60),
            sessions: Some(3),
        };
        let given = settings.env();
        let pair = |k: &str, v: &str| (k.to_string(), v.to_string());
        assert_eq!(
            given,
            [
                pair(var::NFS, "1"),
                pair(var::X11, "0"),
                pair(var::CLOCK_INTERVAL, "60"),
                pair(var::SESSIONS, "3")
            ]
        );
        assert!(settings.any());
        assert!(!AgentSettings::default().any());
        // As from the host's environment, or a container started before.
        let mut env = vec![pair(var::NFS, "0"), pair(var::SET_CLOCK, "0")];
        merge_env(&mut env, given);
        assert_eq!(env.iter().filter(|(k, _)| k == var::NFS).collect::<Vec<_>>(), [&pair(var::NFS, "1")]);
        assert!(env.contains(&pair(var::SET_CLOCK, "0")), "{env:?}");
        assert_eq!(env.len(), 5);
    }

    #[test]
    fn run_args_without_vnc() {
        let spec = RunSpec {
            image: "dockerix:0123456789ab".into(),
            local: true,
            state: "/s".into(),
            assets: "/a".into(),
            workarea: None,
            workdir: None,
            vnc: false,
            profile: None,
            env: vec![(var::NFS.into(), "1".into())],
        };
        // No console port, and the entrypoint told not to serve one.
        assert_eq!(
            strings(run_args(&spec)).join(" "),
            "run -d --name dockerix-vm --pull never -p 127.0.0.1:42323:42323 \
             -v /s:/state -v /a:/assets:ro -e DOCKERIX_NFS=1 -e DOCKERIX_VNC=0 \
             --stop-timeout 60 dockerix:0123456789ab"
        );
    }

    #[test]
    fn run_args_with_profile() {
        let spec = RunSpec {
            image: "dockerix:0123456789ab".into(),
            local: true,
            state: "/s".into(),
            assets: "/a".into(),
            workarea: None,
            workdir: None,
            vnc: true,
            profile: Some(Profile::R140),
            env: vec![],
        };
        assert_eq!(
            strings(run_args(&spec)).join(" "),
            "run -d --name dockerix-vm --pull never -p 127.0.0.1:6080:6080 \
             -p 127.0.0.1:42323:42323 -v /s:/state -v /a:/assets:ro \
             -e DOCKERIX_PROFILE=r140 --stop-timeout 60 dockerix:0123456789ab"
        );
    }

    #[test]
    fn run_args_with_workarea_env_and_workdir() {
        let spec = RunSpec {
            image: "example/dockerix:dev".into(),
            local: false,
            state: "/s".into(),
            assets: "/a b".into(),
            workarea: Some("/w".into()),
            workdir: Some("/workarea/game".into()),
            vnc: true,
            profile: None,
            env: vec![(var::NFS.into(), "1".into())],
        };
        let args = strings(run_args(&spec));
        // A given image may be pulled.
        assert!(!args.contains(&"--pull".to_string()));
        // One argument per path, spaces and all.
        assert!(args.contains(&"/a b:/assets:ro".to_string()));
        let tail = &args[args.iter().position(|a| a == "-v").unwrap()..];
        assert_eq!(
            tail.join(" "),
            "-v /s:/state -v /a b:/assets:ro -v /w:/workarea -e DOCKERIX_NFS=1 \
             -e DOCKERIX_WORKDIR=/workarea/game --stop-timeout 60 example/dockerix:dev"
        );
    }

    #[test]
    fn started_with_reads_the_start_options() {
        let image_env = "PATH=/usr/bin\nDOCKERIX_NFS=0\nDOCKERIX_SET_CLOCK=1\nDOCKERIX_X11=0\n";
        // Image defaults only: nothing given at start.
        assert_eq!(
            started_with(image_env, image_env),
            StartedWith { workdir: None, vnc: true, profile: None, env: vec![] }
        );
        let env = "PATH=/usr/bin\nDOCKERIX_NFS=1\nDOCKERIX_SET_CLOCK=1\nDOCKERIX_X11=0\n\
                   DOCKERIX_CLOCK_INTERVAL=60\nDOCKERIX_VNC=0\nDOCKERIX_PROFILE=r140\n\
                   DOCKERIX_WORKDIR=/workarea/game\n";
        assert_eq!(
            started_with(env, image_env),
            StartedWith {
                workdir: Some("/workarea/game".into()),
                vnc: false,
                profile: Some("r140".into()),
                env: vec![(var::NFS.into(), "1".into()), (var::CLOCK_INTERVAL.into(), "60".into()),],
            }
        );
    }
}
