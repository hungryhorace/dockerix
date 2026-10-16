//! Runtime configuration, resolved from the environment with sensible defaults
//! that work both inside the container and when testing from the host (the
//! guest telnet port is published on 127.0.0.1 either way).
//!
//! Every `DOCKERIX_*` variable is named once, in `var`, and read through
//! `string`, `optional`, `path`, `flag` or `number`. The "Configuration
//! reference" tables in docs/how-it-works.md document the same set; a test
//! holds the two to it.

use std::path::PathBuf;
use std::str::FromStr;

/// The `DOCKERIX_*` environment variables: every one the code reads, or
/// sets for the container or `rix`.
pub mod var {
    macro_rules! vars {
        ($($(#[$doc:meta])* $name:ident = $value:literal;)*) => {
            $($(#[$doc])* pub const $name: &str = $value;)*
            /// Every variable above.
            #[cfg(test)]
            pub const ALL: &[&str] = &[$($name),*];
        };
    }

    vars! {
        // Read by `dockerix` on the host.
        /// Directory to share as the work area (`start`/`run --workarea`).
        WORKAREA = "DOCKERIX_WORKAREA";
        /// Guest disc and CMOS directory, mounted at `/state`.
        STATE = "DOCKERIX_STATE";
        /// ROMs and pristine disc image, mounted read-only at `/assets`.
        ASSETS = "DOCKERIX_ASSETS";
        /// An image to run as is, with no building.
        IMAGE = "DOCKERIX_IMAGE";
        /// One-line progress notices instead of bars (`--plain`).
        PLAIN = "DOCKERIX_PLAIN";

        // Read by the container's agent at start.
        /// Live NFS mount of the work area instead of the push.
        NFS = "DOCKERIX_NFS";
        /// X11 forwarding: the container relay and the guest's `DISPLAY`.
        X11 = "DOCKERIX_X11";
        /// Set the guest clock after boot.
        SET_CLOCK = "DOCKERIX_SET_CLOCK";
        /// Seconds between guest clock re-syncs.
        CLOCK_INTERVAL = "DOCKERIX_CLOCK_INTERVAL";
        /// The noVNC console (`start --no-vnc` turns it off).
        VNC = "DOCKERIX_VNC";
        /// The emulated machine (`start`/`run --profile`).
        PROFILE = "DOCKERIX_PROFILE";
        /// Guest directory for logins and commands (`start -w`, `exec -w`).
        WORKDIR = "DOCKERIX_WORKDIR";
        /// Guest NFS mount options.
        NFS_OPTS = "DOCKERIX_NFS_OPTS";
        /// NFS server address the guest mounts from.
        NFS_SERVER = "DOCKERIX_NFS_SERVER";
        /// Address the guest's `rdate` asks for the time.
        TIME_SERVER = "DOCKERIX_TIME_SERVER";
        /// The guest's `DISPLAY` when X11 is on.
        X11_DISPLAY = "DOCKERIX_X11_DISPLAY";
        /// Guest sessions the agent holds logged in.
        SESSIONS = "DOCKERIX_SESSIONS";
        /// Seconds before an idle held session is logged out.
        SESSION_IDLE = "DOCKERIX_SESSION_IDLE";
        /// The Xvfb screen the emulator draws on.
        SCREEN = "DOCKERIX_SCREEN";
        /// Arculator machine config name.
        CONFIG = "DOCKERIX_CONFIG";
        /// Guest telnet address, as seen from the container.
        TELNET = "DOCKERIX_TELNET";
        /// Guest login name.
        USER = "DOCKERIX_USER";
        /// Guest login password.
        PASS = "DOCKERIX_PASS";
        /// One-shot `docker run <image> <cmd>`: seconds to wait for boot.
        BOOT_TIMEOUT = "DOCKERIX_BOOT_TIMEOUT";
        /// One-shot `docker run <image> <cmd>`: seconds before the command
        /// is abandoned.
        EXEC_TIMEOUT = "DOCKERIX_EXEC_TIMEOUT";

        // Read by `rix` per command (passed through by `dockerix exec`).
        /// Push the work area before the command (`exec --no-sync` sets `0`).
        SYNC = "DOCKERIX_SYNC";
        /// Push size limit in bytes; a bigger push is skipped.
        SYNC_MAX = "DOCKERIX_SYNC_MAX";
        /// Compare file contents, not just size and mtime.
        SYNC_HASH = "DOCKERIX_SYNC_HASH";
        /// Session-affinity key for held sessions.
        SESSION = "DOCKERIX_SESSION";
        /// Log in per command instead of using a held session.
        DIRECT = "DOCKERIX_DIRECT";
        /// Transfer progress: `bar`, `plain`, or `0` for none.
        PROGRESS = "DOCKERIX_PROGRESS";
        /// Terminal type advertised to the guest.
        TERM = "DOCKERIX_TERM";
        /// Dump the raw telnet bytes on stderr.
        TELNET_TRACE = "DOCKERIX_TELNET_TRACE";
        /// Push over the tty, not NFS staging (a debugging aid).
        PUSH_TTY = "DOCKERIX_PUSH_TTY";

        // Set by `dockerix` for `rix`, and paths overridden by the tests.
        /// The host cwd relative to the work area, for the guest cwd.
        SUBDIR = "DOCKERIX_SUBDIR";
        /// Run id, so Ctrl-C can interrupt the guest command.
        RUN_ID = "DOCKERIX_RUN_ID";
        /// Empty the guest work area and start a new manifest (`sync --clear`).
        SYNC_CLEAR = "DOCKERIX_SYNC_CLEAR";
        /// The agent's exec socket.
        SOCK = "DOCKERIX_SOCK";
        /// The work-area mount point in the container.
        CONTAINER_WORKAREA = "DOCKERIX_CONTAINER_WORKAREA";
        /// The work area in the guest.
        GUEST_WORKAREA = "DOCKERIX_GUEST_WORKAREA";
        /// The push manifest.
        SYNC_MANIFEST = "DOCKERIX_SYNC_MANIFEST";
        /// Where pushes stage their archives, exported read-only to the guest.
        STAGE = "DOCKERIX_STAGE";
    }
}

/// The variable's value, or `default` when it is unset (set but empty is a
/// value).
pub fn string(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

/// The variable's value, if set and not empty.
pub fn optional(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.is_empty())
}

/// The variable as a path, if set and not empty (any bytes, not only UTF-8).
pub fn path(key: &str) -> Option<PathBuf> {
    std::env::var_os(key).filter(|v| !v.is_empty()).map(PathBuf::from)
}

/// A switch: `1` turns it on, `0` off, and anything else, or nothing,
/// leaves `default`.
pub fn flag(key: &str, default: bool) -> bool {
    match std::env::var(key).as_deref() {
        Ok("1") => true,
        Ok("0") => false,
        _ => default,
    }
}

/// A number: `default` when unset or not a number.
pub fn number<T: FromStr>(key: &str, default: T) -> T {
    std::env::var(key).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

/// A century: the longest timeout, and what 0 ("no limit") stands for. Any
/// longer and adding it to an `Instant` could overflow.
pub const NO_LIMIT_SECS: u64 = 100 * 365 * 24 * 3600;

/// The seconds before a guest command is abandoned: `DOCKERIX_EXEC_TIMEOUT`
/// (`dockerix exec --timeout`), else an hour; 0 for no limit.
pub fn exec_timeout() -> u64 {
    clamp_timeout(number(var::EXEC_TIMEOUT, 3600u64))
}

/// A timeout as given (0 for no limit) made safe to add to an `Instant`:
/// 0 becomes `NO_LIMIT_SECS`, and nothing is longer.
pub fn clamp_timeout(secs: u64) -> u64 {
    match secs {
        0 => NO_LIMIT_SECS,
        secs => secs.min(NO_LIMIT_SECS),
    }
}

/// The settings a container-side process (the agent, `rix`, the transfers)
/// resolves once from the environment at entry, and then reads from, rather
/// than calling `string`/`flag`/`number` scattered through the code (see the
/// quality-followups plan, M9). Being folded in a module at a time; the
/// agent's startup and clock settings are here so far.
#[derive(Clone, Debug)]
pub struct Config {
    pub telnet_addr: String,
    pub user: String,
    pub pass: String,
    /// Arculator machine config name (`DOCKERIX_CONFIG`).
    pub config_name: String,
    /// The Xvfb display (`DISPLAY`).
    pub display: String,
    /// The Xvfb screen geometry (`DOCKERIX_SCREEN`).
    pub screen: String,
    /// `DOCKERIX_NFS` as given (before the "is a work area mounted?" check);
    /// the effective state is `config::nfs_enabled()`.
    pub nfs_requested: bool,
    /// One-shot boot wait, seconds (`DOCKERIX_BOOT_TIMEOUT`).
    pub boot_timeout: u64,
    /// Correct the guest clock after boot (`DOCKERIX_SET_CLOCK`).
    pub set_clock: bool,
    /// RFC 868 time server the guest's `rdate` asks (`DOCKERIX_TIME_SERVER`).
    pub time_server: String,
    /// Seconds between guest clock re-syncs (`DOCKERIX_CLOCK_INTERVAL`).
    pub clock_interval: u64,
    /// The work-area bind-mount point in the container
    /// (`DOCKERIX_CONTAINER_WORKAREA`).
    pub container_workarea: String,
    /// Where the work area lives in the guest, on its disc
    /// (`DOCKERIX_GUEST_WORKAREA`).
    pub guest_workarea: String,
    /// Whether a host directory is mounted at the container work-area path
    /// (read once from /proc/self/mountinfo; see `workarea_mounted`).
    pub workarea_mounted: bool,
    /// Where transfers stage their archives, exported to the guest
    /// (`DOCKERIX_STAGE`).
    pub stage: String,
    /// The NFS server the guest mounts the staging export and work area from
    /// (`DOCKERIX_NFS_SERVER`).
    pub nfs_server: String,
    /// The work area's NFS mount options (`DOCKERIX_NFS_OPTS`).
    pub nfs_opts: String,
    /// Push over the tty rather than the NFS staging export
    /// (`DOCKERIX_PUSH_TTY`).
    pub push_tty: bool,
    /// Where the work-area sync manifest lives (`DOCKERIX_SYNC_MANIFEST`).
    pub sync_manifest: PathBuf,
    /// A fixed guest working directory, if any (`DOCKERIX_WORKDIR`).
    pub workdir: Option<String>,
    /// The host cwd relative to the work-area root (`DOCKERIX_SUBDIR`).
    pub subdir: Option<String>,
    /// The per-command push switch as given (`DOCKERIX_SYNC`, on unless `0`);
    /// the effective decision is `Config::sync_enabled` (off in NFS mode and
    /// when no work area is mounted).
    pub sync: bool,
    /// Empty the guest work area and start a new manifest first
    /// (`DOCKERIX_SYNC_CLEAR`; `dockerix sync --clear`).
    pub sync_clear: bool,
    /// Compare file contents, not just size and mtime (`DOCKERIX_SYNC_HASH`).
    pub sync_hash: bool,
    /// The push size ceiling in bytes (`DOCKERIX_SYNC_MAX`).
    pub sync_max: usize,
    /// Log in per command rather than use a held session (`DOCKERIX_DIRECT`).
    pub direct: bool,
}

impl Default for Config {
    /// The values `from_env` gives with nothing set: the defaults, so a test
    /// builds a `Config` with `..Config::default()` and overrides what it
    /// exercises, no environment needed.
    fn default() -> Config {
        Config {
            telnet_addr: "127.0.0.1:42323".to_string(),
            user: "root".to_string(),
            pass: "Tal540bo".to_string(),
            config_name: "riscix".to_string(),
            display: ":99".to_string(),
            screen: "1024x768x24".to_string(),
            nfs_requested: false,
            boot_timeout: 1800,
            set_clock: true,
            time_server: "10.0.2.2".to_string(),
            clock_interval: 300,
            container_workarea: "/workarea".to_string(),
            guest_workarea: "/workarea".to_string(),
            // A test's Config stands for a mounted work area unless it says
            // otherwise (the push/transfer tests all push); `from_env` reads
            // the real state.
            workarea_mounted: true,
            stage: "/xfer".to_string(),
            nfs_server: "10.0.2.2".to_string(),
            nfs_opts: "intr,actimeo=1,rsize=8192,wsize=8192".to_string(),
            push_tty: false,
            sync_manifest: PathBuf::from("/state/sync/manifest"),
            workdir: None,
            subdir: None,
            sync: true,
            sync_clear: false,
            sync_hash: false,
            sync_max: DEFAULT_SYNC_MAX,
            direct: false,
        }
    }
}

impl Config {
    pub fn from_env() -> Config {
        Config {
            telnet_addr: string(var::TELNET, "127.0.0.1:42323"),
            user: string(var::USER, "root"),
            pass: string(var::PASS, "Tal540bo"),
            config_name: string(var::CONFIG, "riscix"),
            display: string("DISPLAY", ":99"),
            screen: string(var::SCREEN, "1024x768x24"),
            nfs_requested: flag(var::NFS, false),
            boot_timeout: number(var::BOOT_TIMEOUT, 1800),
            set_clock: flag(var::SET_CLOCK, true),
            time_server: string(var::TIME_SERVER, "10.0.2.2"),
            clock_interval: number(var::CLOCK_INTERVAL, 300),
            container_workarea: container_workarea(),
            guest_workarea: guest_workarea(),
            workarea_mounted: workarea_mounted(),
            stage: stage_dir(),
            nfs_server: nfs_server(),
            nfs_opts: nfs_opts(),
            push_tty: push_tty(),
            sync_manifest: sync_manifest_path(),
            workdir: workdir(),
            subdir: subdir(),
            sync: flag(var::SYNC, true),
            sync_clear: sync_clear(),
            sync_hash: sync_hash(),
            sync_max: sync_max(),
            direct: direct(),
        }
    }

    /// Whether the work area is a live NFS mount rather than the per-command
    /// push: requested (`DOCKERIX_NFS=1`) and a work area is mounted. See the
    /// free `nfs_enabled` for the reasoning.
    pub fn nfs_enabled(&self) -> bool {
        self.nfs_requested && self.workarea_mounted
    }

    /// Whether the per-command work-area push runs: on (`DOCKERIX_SYNC`),
    /// a work area is mounted, and not in NFS mode (the mount is live). NFS
    /// supersedes the push (the work area is then a live mount), and an
    /// unmounted work area has nothing to push.
    pub fn sync_enabled(&self) -> bool {
        self.sync && self.workarea_mounted && !self.nfs_enabled()
    }

    /// The guest directory commands and logins start in when none is fixed:
    /// the work area, or `/` when no work area is mounted.
    fn default_dir(&self) -> String {
        if self.workarea_mounted { self.guest_workarea.clone() } else { "/".to_string() }
    }

    /// The directory a command runs in inside the guest: the fixed working
    /// directory (`DOCKERIX_WORKDIR`) if there is one, else the work area
    /// plus the host cwd's subdirectory (`DOCKERIX_SUBDIR`), else `/` when no
    /// work area is mounted.
    pub fn guest_cwd(&self) -> String {
        if let Some(dir) = &self.workdir {
            return dir.clone();
        }
        if !self.workarea_mounted {
            return self.default_dir();
        }
        match &self.subdir {
            Some(sub) => format!("{}/{}", self.guest_workarea, sub.trim_matches('/')),
            None => self.guest_workarea.clone(),
        }
    }

    /// The guest directory a login shell starts in: the fixed working
    /// directory, else the work area (or `/`).
    pub fn login_dir(&self) -> String {
        self.workdir.clone().unwrap_or_else(|| self.default_dir())
    }
}

/// Whether the work area is shared by a live NFS mount (`DOCKERIX_NFS=1`)
/// rather than the per-command delta push. Read inside the container by the
/// entrypoint (which starts the NFSv2 daemons), the exec path (which then
/// skips the push), the pool (which mounts the share on each fresh guest
/// login) and `get`/`put` (which then leave the manifest alone). Off by
/// default: see docs/how-it-works.md, "The live NFS mount". Also off when
/// no work area is mounted, as there is nothing to share.
pub fn nfs_enabled() -> bool {
    flag(var::NFS, false) && workarea_mounted()
}

/// Whether a host directory is mounted at the container's work-area path.
/// `dockerix start` without `--workarea` mounts none, leaving only the
/// image's empty /workarea: there is then nothing to push or share, and a
/// push of that empty directory would delete everything earlier pushes put
/// on the guest. Read from /proc/self/mountinfo (the mount point is the
/// fifth field). Always true when DOCKERIX_CONTAINER_WORKAREA overrides the
/// path (tests).
pub fn workarea_mounted() -> bool {
    if std::env::var_os(var::CONTAINER_WORKAREA).is_some() {
        return true;
    }
    let target = container_workarea();
    std::fs::read_to_string("/proc/self/mountinfo")
        .map(|m| m.lines().any(|l| l.split(' ').nth(4) == Some(target.as_str())))
        .unwrap_or(false)
}

/// The work-area bind-mount point in the container (`DOCKERIX_CONTAINER_WORKAREA`,
/// else /workarea; tests override it).
pub fn container_workarea() -> String {
    string(var::CONTAINER_WORKAREA, "/workarea")
}

/// Where the work area lives in the guest, on its disc (`DOCKERIX_GUEST_WORKAREA`,
/// else /workarea).
pub fn guest_workarea() -> String {
    string(var::GUEST_WORKAREA, "/workarea")
}

/// The container directory transfers stage their archives in
/// (`DOCKERIX_STAGE`, else /xfer), exported read-only to the guest. Present
/// only once the agent has started the NFS server.
pub fn stage_dir() -> String {
    string(var::STAGE, "/xfer")
}

/// The NFS server address the guest mounts the staging export and the work
/// area from (`DOCKERIX_NFS_SERVER`, the SLiRP gateway by default).
pub fn nfs_server() -> String {
    string(var::NFS_SERVER, "10.0.2.2")
}

/// The work area's NFS mount options (`DOCKERIX_NFS_OPTS`). `actimeo=1`: RISC
/// iX holds a file's attributes for up to 60 s, so a `make` missed a host edit
/// for nearly a minute; with it, a second at most. 8K sizes: a large
/// guest→host write fragments into SLiRP packets the bundled SLiRP reassembles
/// (patches/arculator/0003); without that patch, keep wsize=1024 via this
/// variable.
pub fn nfs_opts() -> String {
    string(var::NFS_OPTS, "intr,actimeo=1,rsize=8192,wsize=8192")
}

/// Push over the tty rather than the NFS staging export (`DOCKERIX_PUSH_TTY`,
/// a debugging aid); staging is used by default when the export is there.
pub fn push_tty() -> bool {
    flag(var::PUSH_TTY, false)
}

/// Empty the guest work area and start a new manifest before the push
/// (`DOCKERIX_SYNC_CLEAR`; `dockerix sync --clear`).
pub fn sync_clear() -> bool {
    flag(var::SYNC_CLEAR, false)
}

/// Compare file contents, not just size and mtime, in the push
/// (`DOCKERIX_SYNC_HASH`). Read where the environment is the user's (`rix`,
/// `get`/`put`); the serve-mode push gets it in the request.
pub fn sync_hash() -> bool {
    flag(var::SYNC_HASH, false)
}

/// Default ceiling on a push's tar; the tty channel is slow, so a bigger
/// tree is skipped rather than take painfully long.
const DEFAULT_SYNC_MAX: usize = 16 * 1024 * 1024;

/// The push size ceiling in bytes (`DOCKERIX_SYNC_MAX`).
pub fn sync_max() -> usize {
    number(var::SYNC_MAX, DEFAULT_SYNC_MAX)
}

/// Log in per command rather than use a held session (`DOCKERIX_DIRECT`).
pub fn direct() -> bool {
    flag(var::DIRECT, false)
}

/// The fixed guest working directory, if any (`DOCKERIX_WORKDIR`; `start -w`,
/// `exec -w`).
pub fn workdir() -> Option<String> {
    optional(var::WORKDIR)
}

/// The host cwd relative to the work-area root, for the guest cwd
/// (`DOCKERIX_SUBDIR`).
pub fn subdir() -> Option<String> {
    optional(var::SUBDIR)
}

/// The session-affinity key for held sessions, if the client wants one
/// (`DOCKERIX_SESSION`).
pub fn session_key() -> Option<String> {
    optional(var::SESSION)
}

/// The run id, so Ctrl-C can ask for the command to be interrupted
/// (`DOCKERIX_RUN_ID`).
pub fn run_id() -> Option<String> {
    optional(var::RUN_ID)
}

/// The transfer progress style as the host wrapper set it (`DOCKERIX_PROGRESS`:
/// `bar`, `plain`, `0`), if any.
pub fn progress_style() -> Option<String> {
    optional(var::PROGRESS)
}

/// The terminal type to advertise to the guest (`DOCKERIX_TERM`), if set.
pub fn term() -> Option<String> {
    optional(var::TERM)
}

/// Dump the raw telnet bytes on stderr (`DOCKERIX_TELNET_TRACE`, a debug aid).
pub fn telnet_trace() -> bool {
    flag(var::TELNET_TRACE, false)
}

/// The agent's exec socket (`DOCKERIX_SOCK`).
pub fn socket_path() -> String {
    string(var::SOCK, "/tmp/dockerix-exec.sock")
}

/// How many guest sessions the agent holds logged in (`DOCKERIX_SESSIONS`).
pub fn sessions() -> usize {
    number(var::SESSIONS, 2)
}

/// Seconds before an idle held session is logged out (`DOCKERIX_SESSION_IDLE`).
pub fn session_idle_secs() -> u64 {
    number(var::SESSION_IDLE, 600)
}

// host-side settings: the environment behind the global options
// (`host::Globals`), read when the option is not given on the command line.

/// The guest-state directory (`DOCKERIX_STATE`; `--state`).
pub fn state() -> Option<PathBuf> {
    path(var::STATE)
}

/// The ROMs-and-disc directory (`DOCKERIX_ASSETS`; `--assets`).
pub fn assets() -> Option<PathBuf> {
    path(var::ASSETS)
}

/// The directory shared as the work area (`DOCKERIX_WORKAREA`;
/// `start`/`run --workarea`).
pub fn workarea() -> Option<PathBuf> {
    path(var::WORKAREA)
}

/// An image to run as is, with no building (`DOCKERIX_IMAGE`; `--image`).
pub fn image() -> Option<String> {
    optional(var::IMAGE)
}

/// One-line progress notices instead of bars (`DOCKERIX_PLAIN`; `--plain`).
pub fn plain() -> bool {
    flag(var::PLAIN, false)
}

/// Where the work-area sync manifest lives: what the delta push last sent
/// to the guest (see sync.rs). Under `/state`, beside the guest disc it
/// describes, so `dockerix reset` clears both together. Overridable with
/// `DOCKERIX_SYNC_MANIFEST` (tests).
pub fn sync_manifest_path() -> PathBuf {
    PathBuf::from(string(var::SYNC_MANIFEST, "/state/sync/manifest"))
}

/// Whether X11 forwarding is on (`DOCKERIX_X11=1`). When set, the container
/// runs a relay on :6000 that forwards guest X traffic to the host's XQuartz
/// (see relay.rs and `dockerix x11`), and the guest login exports `DISPLAY` so
/// X clients reach it. Off by default. Read inside the container by the
/// entrypoint (starts the relay) and the login path (sets `DISPLAY`).
pub fn x11_enabled() -> bool {
    flag(var::X11, false)
}

/// Whether the entrypoint serves the emulator's screen over VNC and noVNC
/// (x11vnc, websockify on port 6080). On unless `DOCKERIX_VNC=0`, which
/// `dockerix start --no-vnc` and `dockerix run` set.
pub fn vnc_enabled() -> bool {
    flag(var::VNC, true)
}

/// The `DISPLAY` the guest uses when X11 forwarding is on. Defaults to the
/// SLiRP gateway (the container) at display 0; the container relay forwards
/// that on to the host. Overridable with `DOCKERIX_X11_DISPLAY`.
pub fn x11_display() -> String {
    string(var::X11_DISPLAY, "10.0.2.2:0")
}

/// The emulated machine's speed and memory (`dockerix start --profile`,
/// `DOCKERIX_PROFILE` in the container). Each is the baked A540 config with
/// a different CPU, MEMC and RAM; only those three keys change. The guest
/// disc and CMOS do not depend on them, so a profile can change between
/// starts without a reset. Arculator paces itself to real time, so a slower
/// profile really is slower, but only the CPU and memory are period-accurate:
/// the disc is still the SCSI podule, not an R140's ST-506.
#[derive(Clone, Copy, PartialEq, Eq, Debug, clap::ValueEnum)]
pub enum Profile {
    /// ARM3 at 40 MHz, MEMC1a at 12 MHz, 16 MB: the fastest Arculator
    /// offers (the default).
    Fast,
    /// ARM2 at 8 MHz (it runs at the MEMC1a's clock), 4 MB: an R140's CPU
    /// and memory, about five times slower than `fast` at compiling.
    R140,
}

impl Profile {
    /// The `--profile` value and `DOCKERIX_PROFILE` spelling.
    pub fn name(self) -> &'static str {
        match self {
            Profile::Fast => "fast",
            Profile::R140 => "r140",
        }
    }

    /// For `dockerix status`.
    pub fn describe(self) -> &'static str {
        match self {
            Profile::Fast => "ARM3 40 MHz, 16 MB",
            Profile::R140 => "ARM2 8 MHz, 4 MB",
        }
    }

    pub fn from_name(name: &str) -> Option<Profile> {
        [Profile::Fast, Profile::R140].into_iter().find(|p| p.name() == name)
    }

    /// The machine-config keys this profile sets. Enum numbers are those of
    /// Arculator's src/wx-config.cc (src/config.c reads them unchecked, so
    /// an ARM2 in an A540 is fine): cpu_type 0 = ARM2, 10 = ARM3/40;
    /// memc_type 1 = MEMC1a 8 MHz, 2 = MEMC1a 12 MHz. mem_size is in KB.
    fn settings(self) -> [(&'static str, u32); 3] {
        match self {
            Profile::Fast => [("cpu_type", 10), ("memc_type", 2), ("mem_size", 16384)],
            Profile::R140 => [("cpu_type", 0), ("memc_type", 1), ("mem_size", 4096)],
        }
    }
}

/// The profile the entrypoint applies: `DOCKERIX_PROFILE`, default `fast`.
pub fn profile() -> std::io::Result<Profile> {
    match optional(var::PROFILE) {
        None => Ok(Profile::Fast),
        Some(v) => Profile::from_name(&v).ok_or_else(|| {
            std::io::Error::other(format!("{}={v}: not a profile (fast or r140)", var::PROFILE))
        }),
    }
}

/// `cfg` (an Arculator machine config) with the profile's keys set. They
/// belong to the machine section, before the first `[section]`; a key that
/// is missing is added at the top. Applied at every start, since Arculator
/// writes the config back on exit and a restarted container keeps the file.
pub fn apply_profile(cfg: &str, profile: Profile) -> String {
    let settings = profile.settings();
    let mut seen = [false; 3];
    let mut in_machine = true;
    let mut lines: Vec<String> = Vec::new();
    for line in cfg.lines() {
        if line.starts_with('[') {
            in_machine = false;
        }
        let key = line.split('=').next().unwrap_or("").trim();
        match settings.iter().position(|(k, _)| in_machine && line.contains('=') && *k == key) {
            Some(i) => {
                seen[i] = true;
                lines.push(format!("{key} = {}", settings[i].1));
            }
            None => lines.push(line.to_string()),
        }
    }
    let missing = settings.iter().zip(seen).filter(|(_, s)| !s).map(|((k, v), _)| format!("{k} = {v}"));
    let mut out: Vec<String> = missing.collect();
    out.extend(lines);
    out.join("\n") + "\n"
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testsupport::Env;

    #[test]
    fn exec_timeout_reads_zero_as_no_limit() {
        let mut env = Env::lock();
        env.unset(var::EXEC_TIMEOUT);
        assert_eq!(exec_timeout(), 3600);
        env.set(var::EXEC_TIMEOUT, "90");
        assert_eq!(exec_timeout(), 90);
        env.set(var::EXEC_TIMEOUT, "0");
        assert_eq!(exec_timeout(), NO_LIMIT_SECS);
        env.set(var::EXEC_TIMEOUT, &u64::MAX.to_string());
        assert_eq!(exec_timeout(), NO_LIMIT_SECS);
        // Still adds to an Instant.
        let _ = std::time::Instant::now() + std::time::Duration::from_secs(exec_timeout());
    }

    #[test]
    fn apply_profile_sets_machine_keys_only() {
        let cfg = "machine = a540\ncpu_type = 10\nmemc_type = 2\nmem_size = 16384\nfpa = 0\n\n\
                   [aka31.0]\nmem_size = 99\n";
        assert_eq!(
            apply_profile(cfg, Profile::R140),
            "machine = a540\ncpu_type = 0\nmemc_type = 1\nmem_size = 4096\nfpa = 0\n\n\
             [aka31.0]\nmem_size = 99\n"
        );
        // And back: the fast settings are what the baked config has.
        assert_eq!(apply_profile(&apply_profile(cfg, Profile::R140), Profile::Fast), cfg);
    }

    #[test]
    fn apply_profile_adds_missing_keys_at_top() {
        assert_eq!(
            apply_profile("machine = a540\ncpu_type = 4\n[x]\n", Profile::R140),
            "memc_type = 1\nmem_size = 4096\nmachine = a540\ncpu_type = 0\n[x]\n"
        );
    }

    #[test]
    fn baked_config_is_the_fast_profile() {
        let baked = include_str!("../../configs/riscix.cfg");
        assert_eq!(apply_profile(baked, Profile::Fast), baked);
    }

    #[test]
    fn profile_names_round_trip() {
        for p in [Profile::Fast, Profile::R140] {
            assert_eq!(Profile::from_name(p.name()), Some(p));
        }
        assert_eq!(Profile::from_name("a540"), None);
    }

    #[test]
    fn readers_parse_as_documented() {
        let mut env = crate::testsupport::Env::lock();
        let key = "DOCKERIX_TEST_ONLY";
        env.unset(key);
        assert_eq!(string(key, "d"), "d");
        assert_eq!(optional(key), None);
        assert_eq!(path(key), None);
        assert!(flag(key, true) && !flag(key, false));
        assert_eq!(number(key, 7u32), 7);
        env.set(key, "");
        assert_eq!(string(key, "d"), "");
        assert_eq!(optional(key), None);
        assert_eq!(path(key), None);
        assert!(flag(key, true) && !flag(key, false));
        env.set(key, "1");
        assert!(flag(key, false));
        assert_eq!(number(key, 7u32), 1);
        env.set(key, "0");
        assert!(!flag(key, true));
        env.set(key, "yes");
        assert!(flag(key, true) && !flag(key, false));
        assert_eq!(number(key, 7u32), 7);
        assert_eq!(optional(key).as_deref(), Some("yes"));
        assert_eq!(path(key), Some(PathBuf::from("yes")));
    }

    #[test]
    fn every_variable_is_documented_and_no_other() {
        let doc = include_str!("../../docs/how-it-works.md");
        let start = doc.find("\n## Configuration reference").expect("how-it-works.md has the section");
        let section = &doc[start + 1..];
        let section = &section[..section[1..].find("\n## ").map_or(section.len(), |i| i + 1)];
        // The first column of every table row names one or more variables.
        let mut documented = std::collections::BTreeSet::new();
        for row in section.lines().filter(|l| l.starts_with("| `DOCKERIX_")) {
            let first = row.split('|').nth(1).unwrap_or("");
            for part in first.split('`').filter(|p| p.starts_with("DOCKERIX_")) {
                documented.insert(part.to_string());
            }
        }
        let code: std::collections::BTreeSet<String> = var::ALL.iter().map(|v| v.to_string()).collect();
        assert_eq!(code.len(), var::ALL.len(), "a variable is named twice");
        let undocumented: Vec<_> = code.difference(&documented).collect();
        let unknown: Vec<_> = documented.difference(&code).collect();
        assert!(undocumented.is_empty(), "in config::var but not how-it-works.md: {undocumented:?}");
        assert!(unknown.is_empty(), "in how-it-works.md but not config::var: {unknown:?}");
    }
}
