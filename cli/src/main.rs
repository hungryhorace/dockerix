//! DockerIX CLI.
//!
//! Two faces share one binary:
//!   * `dockerix agent …` runs inside the container — it is the entrypoint,
//!     the healthcheck, the graceful-halt hook, and the exec channel.
//!   * `rix …` (a symlink to this binary) is the in-guest exec shortcut, so
//!     `docker exec -it dockerix-vm rix cc -o game game.c` just works.
//!
//!   * `dockerix start|stop|restart|status|console|login|exec|run|reset|build` are
//!     the host-side conveniences: short sequences of `docker` commands (see
//!     host.rs).

// First, so its `outln!`/`out!` are in scope in every module below.
#[macro_use]
mod stdout;

mod agent;
mod config;
#[cfg(test)]
mod context;
mod discpatch;
mod docker;
mod exec;
#[cfg(test)]
mod fake_guest;
mod host;
mod nfs_server;
mod progress;
mod relay;
mod serve;
mod session;
mod sync;
mod telnet;
#[cfg(test)]
mod testsupport;
mod timeserver;
mod transfer;
mod tty;
mod uu;

use std::path::{Path, PathBuf};
use std::process::exit;

use clap::{Args, Parser, Subcommand};

use config::Config;
use exec::ExecOpts;

#[derive(Parser)]
#[command(
    name = "dockerix",
    version,
    about = "DockerIX RISC iX VM control",
    after_help = "Run a guest command in the running emulator with `exec` (in the guest's \
                  copy of the current directory, which must be inside the work area):\n    \
                  dockerix exec cc -o game game.c\n\
                  Use `dockerix exec -t …` for programs that need a terminal, \
                  `dockerix exec --no-sync …` to skip the work-area push, and \
                  `dockerix run …` for a one-shot start, run and stop."
)]
struct Cli {
    /// No progress bars or other terminal drawing: plain one-line notices
    /// instead (also DOCKERIX_PLAIN=1). Goes before the command:
    /// `dockerix --plain exec make`.
    #[arg(long, global = true)]
    plain: bool,
    /// No transfer progress at all, bars or notices (also
    /// DOCKERIX_PROGRESS=0).
    #[arg(long, global = true)]
    quiet: bool,
    /// Guest disc and CMOS directory (default: $DOCKERIX_STATE, else
    /// ~/.dockerix/state). A second one keeps a scratch guest apart from
    /// your real one.
    #[arg(long, global = true, value_name = "DIR")]
    state: Option<PathBuf>,
    /// The ROMs and pristine disc image (default: $DOCKERIX_ASSETS, else
    /// ~/.dockerix/assets).
    #[arg(long, global = true, value_name = "DIR")]
    assets: Option<PathBuf>,
    /// Run this image as is, with no building (default: $DOCKERIX_IMAGE,
    /// else the one built from this dockerix's sources).
    #[arg(long, global = true, value_name = "TAG")]
    image: Option<String>,
    #[command(subcommand)]
    cmd: Top,
}

/// The agent's start-time settings, for `start`, `run` and `restart`. Each
/// given wins over its environment variable; for `restart`, one not given
/// keeps what the container was started with.
#[derive(Args)]
struct AgentArgs {
    /// Share the work area as a live NFS mount instead of pushing it before
    /// each command (DOCKERIX_NFS=1). Needs a work area.
    #[arg(long, overrides_with = "no_nfs")]
    nfs: bool,
    /// Push the work area before each command (the default; DOCKERIX_NFS=0).
    #[arg(long)]
    no_nfs: bool,
    /// Forward guest X clients to XQuartz; run `dockerix x11` on the host
    /// too (DOCKERIX_X11=1).
    #[arg(long, overrides_with = "no_x11")]
    x11: bool,
    /// No X11 forwarding (the default; DOCKERIX_X11=0).
    #[arg(long)]
    no_x11: bool,
    /// Set the guest clock from the host's at boot and keep it in step (the
    /// default; DOCKERIX_SET_CLOCK=1).
    #[arg(long, overrides_with = "no_clock_sync")]
    clock_sync: bool,
    /// Leave the guest clock alone: RISC iX then believes it is 1992
    /// (DOCKERIX_SET_CLOCK=0).
    #[arg(long)]
    no_clock_sync: bool,
    /// Seconds between guest clock re-syncs (default 300;
    /// DOCKERIX_CLOCK_INTERVAL).
    #[arg(long, value_name = "SECS", value_parser = clap::value_parser!(u64).range(1..))]
    clock_interval: Option<u64>,
    /// Guest sessions held logged in, so this many commands can run at once
    /// (default 2; DOCKERIX_SESSIONS).
    #[arg(long, value_name = "N", value_parser = clap::value_parser!(u64).range(1..))]
    sessions: Option<u64>,
}

impl AgentArgs {
    fn settings(&self) -> host::AgentSettings {
        // `--x` or `--no-x` (clap keeps only the last given), else neither.
        let either = |on: bool, off: bool| (on || off).then_some(on);
        host::AgentSettings {
            nfs: either(self.nfs, self.no_nfs),
            x11: either(self.x11, self.no_x11),
            clock_sync: either(self.clock_sync, self.no_clock_sync),
            clock_interval: self.clock_interval,
            sessions: self.sessions.map(|n| n as usize),
        }
    }
}

#[derive(Subcommand)]
enum Top {
    /// Start the emulator container in the background and wait for RISC iX.
    Start {
        /// Directory to export as the work area (default: $DOCKERIX_WORKAREA,
        /// else none). Fixed for the container's life.
        #[arg(long)]
        workarea: Option<PathBuf>,
        /// Guest directory for logins and commands started while the
        /// container runs (default: /workarea for login, the guest's copy of
        /// the host cwd for commands; / for both without a work area).
        /// Relative to /workarea unless absolute.
        #[arg(short = 'w', long)]
        workdir: Option<String>,
        /// Leave out the noVNC console (`dockerix run` always does). Fixed
        /// for the container's life.
        #[arg(long)]
        no_vnc: bool,
        /// Emulated machine speed and memory (default: fast). Fixed for the
        /// container's life.
        #[arg(long, value_enum)]
        profile: Option<config::Profile>,
        #[command(flatten)]
        agent: AgentArgs,
    },
    /// Halt the guest cleanly and remove the container.
    Stop,
    /// Stop the container and start it again from this dockerix's image,
    /// with the same options as before (work area, -w, --no-vnc, --profile,
    /// --nfs and the like). Options given now win: `restart --x11` turns X11
    /// on and keeps the rest, and --state/--assets/--image apply too.
    Restart(AgentArgs),
    /// Is it running, is it booted, and how it was started.
    Status {
        /// Also ask the guest itself (whether the NFS share is mounted):
        /// a command on a held session, so a moment longer.
        #[arg(short = 'v', long)]
        verbose: bool,
    },
    /// What dockerix is running in the guest: commands, `exec -t` and
    /// logins, with how long each has run and whether a command is still
    /// waiting for a session or pushing the work area. Not the guest's own
    /// processes (`dockerix exec ps` lists those).
    Ps,
    /// Open the noVNC console in the browser.
    Console,
    /// Interactive root shell in the guest.
    Login(LoginArgs),
    /// Run a command in the running emulator's guest.
    Exec(ExecCmdArgs),
    /// One-shot (for CI): start the emulator (without the noVNC console), run
    /// a command, stop it again.
    Run(RunArgs),
    /// Push work-area changes to the guest now, without running anything
    /// (for when you are working in a login shell).
    Sync {
        /// First delete everything in the guest work area (guest build
        /// products too) and start a new manifest: a full push from scratch.
        #[arg(long)]
        clear: bool,
        /// Compare file contents, not just sizes and times, to find what
        /// changed (DOCKERIX_SYNC_HASH=1): slower, but sure after a branch
        /// switch.
        #[arg(long)]
        hash: bool,
    },
    /// Copy a file (or, with -r, a directory) from the guest to the host.
    Get(GetArgs),
    /// Copy a file (or, with -r, a directory) from the host into the guest.
    Put(PutArgs),
    /// Discard the guest disc and CMOS so the next start re-seeds them.
    Reset {
        /// Don't ask for confirmation.
        #[arg(long, short = 'y')]
        yes: bool,
    },
    /// Build the container image from the sources built into this binary,
    /// unless it is already there (start does this when it needs to).
    /// Prints the image's tag.
    Build {
        /// Rebuild from scratch, even if the image is already there.
        #[arg(long)]
        no_cache: bool,
    },
    /// In-container agent commands.
    #[command(hide = true)]
    Agent {
        #[command(subcommand)]
        cmd: AgentCmd,
    },
    /// Byte-transparent TCP relay for X11 forwarding (`dockerix x11` wraps it).
    ///
    /// The same command runs on the host and in the container — on the host
    /// forward to XQuartz's socket (`--to unix:/tmp/.X11-unix/X0`), in the
    /// container to the host (`--to tcp:host.docker.internal:6000`). Blocks
    /// until interrupted.
    #[command(hide = true)]
    Relay(RelayArgs),
    /// Forward guest X clients to XQuartz on the host (needs DOCKERIX_X11=1).
    ///
    /// Runs a foreground relay from a local TCP port to XQuartz's unix socket,
    /// so guest X clients display on your desktop. The container must have been
    /// started with DOCKERIX_X11=1. Ctrl-C stops.
    X11(X11Args),
}

#[derive(Args)]
struct LoginArgs {
    /// Guest directory to start in (default: the one given to `start -w`,
    /// else /workarea). Relative to /workarea unless absolute.
    #[arg(short = 'w', long)]
    workdir: Option<String>,
}

#[derive(Args)]
struct ExecCmdArgs {
    /// Allocate a tty (for interactive/curses programs). Disables the work-area push.
    #[arg(short = 't', long)]
    tty: bool,
    /// Skip the work-area push for this command (same as DOCKERIX_SYNC=0).
    #[arg(long)]
    no_sync: bool,
    /// Guest directory to run in (default: the one given to `start -w`, else
    /// the guest's copy of the host cwd). Relative to /workarea unless absolute.
    #[arg(short = 'w', long)]
    workdir: Option<String>,
    #[command(flatten)]
    run: CommandArgs,
    /// Command and arguments; omit for an interactive shell.
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    command: Vec<String>,
}

/// How a guest command runs, for `exec` and `run`.
#[derive(Args)]
struct CommandArgs {
    /// Compare file contents in the push, not just sizes and times
    /// (DOCKERIX_SYNC_HASH=1).
    #[arg(long)]
    hash: bool,
    /// Seconds before the command is abandoned, 0 for no limit (default
    /// 3600; DOCKERIX_EXEC_TIMEOUT).
    #[arg(long, value_name = "SECS")]
    timeout: Option<u64>,
}

impl CommandArgs {
    fn options(&self, workdir: Option<String>) -> host::ExecOptions {
        host::ExecOptions { workdir, hash: self.hash, timeout: self.timeout }
    }
}

#[derive(Args)]
struct RunArgs {
    /// Directory to export as the work area (default: $DOCKERIX_WORKAREA,
    /// else none).
    #[arg(long, value_name = "DIR")]
    workarea: Option<PathBuf>,
    /// Guest directory to run in (default: the guest's copy of the host cwd).
    /// Relative to /workarea unless absolute.
    #[arg(short = 'w', long)]
    workdir: Option<String>,
    /// Emulated machine speed and memory (default: fast).
    #[arg(long, value_enum)]
    profile: Option<config::Profile>,
    #[command(flatten)]
    agent: AgentArgs,
    #[command(flatten)]
    run: CommandArgs,
    /// Command and arguments.
    #[arg(required = true, trailing_var_arg = true, allow_hyphen_values = true)]
    command: Vec<String>,
}

#[derive(Args)]
struct GetArgs {
    /// Copy a directory and its contents.
    #[arg(short = 'r', long)]
    recursive: bool,
    /// Path in the guest: relative to the guest's copy of the current
    /// directory, or absolute.
    guest: String,
    /// Where to put it on the host (default: the current directory). An
    /// existing directory receives the file by name, like cp. `-` writes to
    /// stdout: a file's contents, or the tar itself with -r. A file named
    /// `-` is `./-`.
    host: Option<PathBuf>,
}

#[derive(Args)]
struct PutArgs {
    /// Copy a directory and its contents.
    #[arg(short = 'r', long)]
    recursive: bool,
    /// The host file or directory to send. `-` reads the file's contents
    /// from stdin, and then the guest path is required. A file named `-` is
    /// `./-`.
    host: PathBuf,
    /// Where to put it in the guest (default: the same place in the guest's
    /// work area, or the guest's copy of the current directory for a file
    /// from outside it). Relative paths are from the guest's copy of the
    /// current directory. Like cp: an existing directory receives it by name.
    guest: Option<String>,
}

#[derive(Args)]
struct RelayArgs {
    /// TCP port to listen on (bound on all interfaces, 0.0.0.0).
    listen: u16,
    /// Where to forward each connection: `tcp:HOST:PORT` or `unix:/path`.
    #[arg(long)]
    to: String,
}

#[derive(Args)]
struct X11Args {
    /// Local TCP port to listen on (must match the container relay; 6000 = :0).
    #[arg(long, default_value_t = 6000)]
    port: u16,
    /// XQuartz unix socket to forward to.
    #[arg(long, default_value = "/tmp/.X11-unix/X0")]
    socket: String,
}

#[derive(Subcommand)]
enum AgentCmd {
    /// Container entrypoint: run the emulator and manage its lifecycle. A
    /// trailing command triggers one-shot mode (boot, run, halt).
    Entrypoint {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        command: Vec<String>,
    },
    /// Run a command in the guest, or open a shell if none is given.
    Exec(ExecArgs),
    /// Block until the guest login banner is up (healthcheck / boot gate).
    Wait {
        #[arg(long, default_value_t = 5)]
        timeout: u64,
    },
    /// Shut the guest down cleanly over the exec channel.
    Halt,
    /// How the NFS daemons are, a line `NAME STATE` each, for `dockerix
    /// status`.
    Nfs,
    /// What dockerix is running in the guest (`dockerix ps`).
    Ps {
        /// Just how many, for `dockerix status`.
        #[arg(long)]
        count: bool,
    },
    /// Container half of `dockerix get`: fetch a guest path into the work
    /// area (relative destination) or, with none, as a tar on stdout.
    Get {
        #[arg(short = 'r', long)]
        recursive: bool,
        guest: String,
        dest: Option<String>,
    },
    /// Container half of `dockerix put`: send a work-area path (relative
    /// source) or, with none, a tar from stdin, to a guest path.
    Put {
        #[arg(short = 'r', long)]
        recursive: bool,
        guest: String,
        src: Option<String>,
    },
}

#[derive(Args)]
struct ExecArgs {
    /// Allocate a tty (for interactive/curses programs).
    #[arg(short = 't', long)]
    tty: bool,
    /// Seconds before a non-interactive command is abandoned, 0 for no limit.
    #[arg(long, default_value_t = 3600)]
    timeout: u64,
    /// Command and arguments; omit for an interactive shell.
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    command: Vec<String>,
}

fn main() {
    let cfg = Config::from_env();

    // `rix …` is a symlink alias for `agent exec …`.
    if invoked_as_rix() {
        let args: Vec<String> = std::env::args().skip(1).collect();
        // `rix --abort <id>`: interrupt a run in progress (see serve.rs).
        if args.len() == 2 && args[0] == "--abort" {
            match serve::client_abort(&serve::socket_path(), &args[1]) {
                Ok(()) => exit(0),
                Err(e) => {
                    eprintln!("rix: abort: {e}");
                    exit(1);
                }
            }
        }
        let (tty_flag, command) = split_rix_args(args);
        let code = do_exec(&cfg, tty_flag, command, config::exec_timeout());
        exit(code);
    }

    let cli = Cli::parse();
    let plain = cli.plain || config::plain();
    host::set_globals(host::Globals {
        state: cli.state,
        assets: cli.assets,
        image: cli.image,
        quiet: cli.quiet,
        plain: cli.plain,
    });
    match cli.cmd {
        Top::Start { workarea, workdir, no_vnc, profile, agent } => {
            host_exit(host::start(workarea, workdir, !no_vnc, profile, &agent.settings()))
        }
        Top::Stop => host_exit(host::stop()),
        Top::Restart(agent) => host_exit(host::restart(&agent.settings())),
        Top::Status { verbose } => host_exit(host::status(verbose)),
        Top::Ps => host_exit(host::ps()),
        Top::Console => host_exit(host::console()),
        Top::Login(a) => host_exit(host::login(a.workdir, plain)),
        Top::Exec(a) => host_exit(host::exec(a.tty, a.no_sync, a.run.options(a.workdir), a.command, plain)),
        Top::Run(a) => host_exit(host::run(
            a.workarea,
            a.profile,
            &a.agent.settings(),
            a.run.options(a.workdir),
            a.command,
            plain,
        )),
        Top::Sync { clear, hash } => host_exit(host::sync(clear, hash, plain)),
        Top::Get(a) => host_exit(host::get(a.guest, a.host, a.recursive, plain)),
        Top::Put(a) => host_exit(host::put(a.host, a.guest, a.recursive, plain)),
        Top::Reset { yes } => host_exit(host::reset(yes)),
        Top::Build { no_cache } => host_exit(host::build(no_cache)),
        Top::Relay(a) => {
            let target = match relay::parse_target(&a.to) {
                Ok(t) => t,
                Err(e) => {
                    eprintln!("relay: {e}");
                    exit(2);
                }
            };
            if let Err(e) = relay::serve(&format!("0.0.0.0:{}", a.listen), target) {
                eprintln!("relay: {e}");
                exit(1);
            }
            exit(0);
        }
        Top::X11(a) => host_exit(host::x11(a.port, &a.socket)),
        Top::Agent { cmd } => match cmd {
            AgentCmd::Entrypoint { command } => match agent::entrypoint(&cfg, command) {
                Ok(code) => exit(code),
                Err(e) => {
                    eprintln!("entrypoint: {e}");
                    exit(1);
                }
            },
            AgentCmd::Exec(a) => {
                let tty = if a.tty { Some(true) } else { None };
                exit(do_exec(&cfg, tty, a.command, config::clamp_timeout(a.timeout)));
            }
            AgentCmd::Wait { timeout } => exit(agent::wait(&cfg, timeout)),
            AgentCmd::Get { recursive, guest, dest } => {
                exit(report(transfer::get(&cfg, &guest, dest.as_deref(), recursive)))
            }
            AgentCmd::Put { recursive, guest, src } => {
                exit(report(transfer::put(&cfg, &guest, src.as_deref(), recursive)))
            }
            AgentCmd::Ps { count } => match serve::client_list(&serve::socket_path(), count) {
                Ok(table) => {
                    out!("{table}");
                    exit(0);
                }
                Err(e) => {
                    eprintln!("dockerix: the agent's exec socket: {e} (is RISC iX still booting?)");
                    exit(1);
                }
            },
            AgentCmd::Nfs => {
                nfs_server::check();
                exit(0)
            }
            AgentCmd::Halt => match agent::halt(&cfg) {
                Ok(()) => exit(0),
                Err(e) => {
                    eprintln!("halt: {e}");
                    exit(1);
                }
            },
        },
    }
}

/// Exit with a host-side command's status, or report its error as
/// `dockerix: …` and exit with `host::FAILURE`.
fn host_exit(result: std::io::Result<i32>) -> ! {
    match result {
        Ok(code) => exit(code),
        Err(e) => {
            eprintln!("dockerix: {e}");
            exit(host::FAILURE)
        }
    }
}

/// Exit status for an agent-side transfer: 0, or 1 with the error on stderr.
fn report(r: std::io::Result<()>) -> i32 {
    match r {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("dockerix: {e}");
            1
        }
    }
}

fn do_exec(cfg: &Config, tty: Option<bool>, command: Vec<String>, timeout: u64) -> i32 {
    let opts = ExecOpts { tty, command, timeout_secs: timeout };
    match exec::run(cfg, opts) {
        Ok(code) => code,
        Err(e) => {
            agent::flush_stdout();
            eprintln!("rix: {e}");
            125
        }
    }
}

fn invoked_as_rix() -> bool {
    std::env::args()
        .next()
        .map(|a0| Path::new(&a0).file_name().and_then(|s| s.to_str()) == Some("rix"))
        .unwrap_or(false)
}

/// `rix [-t] [command…]`. A leading `-t`/`--tty` forces a tty; everything else
/// is the command.
fn split_rix_args(mut args: Vec<String>) -> (Option<bool>, Vec<String>) {
    let mut tty = None;
    while let Some(first) = args.first() {
        match first.as_str() {
            "-t" | "--tty" => {
                tty = Some(true);
                args.remove(0);
            }
            "--" => {
                args.remove(0);
                break;
            }
            _ => break,
        }
    }
    (tty, args)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rix(args: &[&str]) -> (Option<bool>, Vec<String>) {
        split_rix_args(args.iter().map(|a| a.to_string()).collect())
    }

    #[test]
    fn split_rix_args_takes_leading_tty_flags_only() {
        assert_eq!(rix(&[]), (None, vec![]));
        assert_eq!(rix(&["ls", "-t"]), (None, vec!["ls".into(), "-t".into()]));
        assert_eq!(rix(&["-t", "vi", "x.c"]), (Some(true), vec!["vi".into(), "x.c".into()]));
        assert_eq!(rix(&["--tty", "-t", "top"]), (Some(true), vec!["top".into()]));
        assert_eq!(rix(&["-t"]), (Some(true), vec![]));
        // `--` ends the flags, so a command may itself start with -t.
        assert_eq!(rix(&["--", "-t"]), (None, vec!["-t".into()]));
        assert_eq!(rix(&["-t", "--", "--", "x"]), (Some(true), vec!["--".into(), "x".into()]));
    }

    fn parse(args: &str) -> Result<Cli, clap::Error> {
        Cli::try_parse_from(std::iter::once("dockerix").chain(args.split_whitespace()))
    }

    #[test]
    fn start_options_make_agent_settings() {
        let Ok(Cli { cmd: Top::Start { agent, .. }, .. }) =
            parse("start --nfs --no-x11 --no-clock-sync --clock-interval 60 --sessions 3")
        else {
            panic!("start did not parse");
        };
        let s = agent.settings();
        assert_eq!(
            (s.nfs, s.x11, s.clock_sync, s.clock_interval, s.sessions),
            (Some(true), Some(false), Some(false), Some(60), Some(3))
        );
        // Not given: left to the variables (or, for restart, the container).
        let Ok(Cli { cmd: Top::Restart(agent), .. }) = parse("restart") else { panic!() };
        assert_eq!(agent.settings(), host::AgentSettings::default());
        // Of a pair, the last one given counts.
        let Ok(Cli { cmd: Top::Restart(agent), .. }) = parse("restart --nfs --no-nfs") else { panic!() };
        assert_eq!(agent.settings().nfs, Some(false));
        let Ok(Cli { cmd: Top::Restart(agent), .. }) = parse("restart --no-x11 --x11") else { panic!() };
        assert_eq!(agent.settings().x11, Some(true));
        // No session and no zero interval: the agent cannot use them.
        assert!(parse("start --sessions 0").is_err());
        assert!(parse("start --clock-interval 0").is_err());
    }

    #[test]
    fn command_and_global_options_parse_anywhere_before_the_command() {
        let Ok(cli) = parse("exec --state /tmp/s --hash --timeout 0 make --timeout 5") else { panic!() };
        assert_eq!(cli.state.as_deref(), Some(std::path::Path::new("/tmp/s")));
        let Top::Exec(a) = cli.cmd else { panic!("not exec") };
        let o = a.run.options(None);
        assert_eq!((o.hash, o.timeout), (true, Some(0)));
        // What follows the command is the command's.
        assert_eq!(a.command, ["make", "--timeout", "5"]);
        let Ok(cli) = parse("--quiet --image dockerix:test run --x11 --timeout 7200 true") else { panic!() };
        assert!(cli.quiet);
        assert_eq!(cli.image.as_deref(), Some("dockerix:test"));
        let Top::Run(a) = cli.cmd else { panic!("not run") };
        assert_eq!((a.agent.settings().x11, a.run.timeout), (Some(true), Some(7200)));
        let Ok(Cli { cmd: Top::Sync { hash, .. }, .. }) = parse("sync --hash") else { panic!() };
        assert!(hash);
    }

    #[test]
    fn get_and_put_take_a_dash_as_a_path() {
        // A lone `-` is an ordinary positional, not an option: `get`'s host
        // and `put`'s host (host::get/put read it as stdout/stdin).
        let Ok(Cli { cmd: Top::Get(a), .. }) = parse("get /etc/passwd -") else {
            panic!("get did not parse")
        };
        assert_eq!((a.guest.as_str(), a.host.as_deref()), ("/etc/passwd", Some(Path::new("-"))));
        let Ok(Cli { cmd: Top::Put(a), .. }) = parse("put - /tmp/x") else { panic!("put did not parse") };
        assert_eq!((a.host.as_path(), a.guest.as_deref()), (Path::new("-"), Some("/tmp/x")));
    }
}
