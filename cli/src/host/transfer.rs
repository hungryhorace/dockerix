//! `dockerix get` and `put`: one path between the guest and the host, through
//! `dockerix agent get|put` in the container (transfer.rs there).

use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use super::rix::{map_cwd, progress_style, workarea_rel, workarea_root};
use super::*;
use crate::docker::{docker, docker_missing};

/// `docker exec [-i] [-e DOCKERIX_SUBDIR=…] dockerix-vm dockerix agent
/// <get|put> [-r] <guest> [<rel>]`, ready to run. `subdir` is the guest cwd
/// for a relative guest path (none without a work area: then relative to
/// the guest's `/`); it is only required when the guest path is relative,
/// so `dockerix get /workarea/game/game.tar ~/Desktop` works from anywhere.
pub(super) fn transfer_command(
    verb: &str,
    guest: &str,
    rel: Option<&str>,
    recursive: bool,
    plain: bool,
    root: Option<&Path>,
) -> io::Result<Command> {
    let mut cmd = docker();
    cmd.arg("exec");
    if verb == "put" {
        cmd.arg("-i");
    }
    if !guest.starts_with('/')
        && let Some(subdir) = map_cwd(root)?
    {
        cmd.arg("-e").arg(format!("{}={subdir}", var::SUBDIR));
    }
    cmd.arg("-e").arg(format!("{}={}", var::PROGRESS, progress_style(plain)));
    cmd.args([CONTAINER, "dockerix", "agent", verb]);
    if recursive {
        cmd.arg("-r");
    }
    cmd.arg(guest);
    if let Some(rel) = rel {
        cmd.arg(if rel.is_empty() { "." } else { rel });
    }
    Ok(cmd)
}

pub(super) fn transfer_status(status: std::process::ExitStatus) -> i32 {
    child_code(status, 1)
}

/// `dockerix get [-r] <guest-path> [<host-path>]`: copy one file (or, with
/// -r, a directory) from the guest to the host. The guest path is relative to
/// the guest's copy of the cwd unless absolute; the host path defaults to
/// the cwd and has `cp` semantics. Inside the work area the container
/// writes it through the bind mount and marks it in the sync manifest as
/// mirrored (so the next push neither re-ships it nor, should you delete
/// it, removes the guest's); elsewhere the archive streams out through
/// `docker exec` and is unpacked here.
pub fn get(guest: String, host: Option<PathBuf>, recursive: bool, plain: bool) -> io::Result<i32> {
    let container = require_running()?;
    let root = workarea_root(&container);
    let name = guest.trim_end_matches('/').rsplit('/').next().unwrap_or("").to_string();
    if name.is_empty() || name == "." || name == ".." {
        return Err(io::Error::other(format!("{guest}: not a file or directory name")));
    }
    let dest = match get_dest(host) {
        // `-`: the tar comes back and goes to our stdout, as a file's
        // contents (so it reads like `cat`) or, with -r, the archive whole
        // (like `tar cf -`). Nothing is placed, and the manifest is left
        // alone.
        GetDest::Stdout => {
            let out = fetch_to_pipe(&guest, recursive, plain, root.as_deref())?;
            if !out.status.success() {
                return Ok(transfer_status(out.status));
            }
            let bytes = if recursive { out.stdout } else { crate::sync::one_file(&out.stdout)? };
            crate::stdout::write_bytes(&bytes);
            return Ok(0);
        }
        GetDest::Path(dest) => dest,
    };
    let rel = workarea_rel(root.as_deref(), &dest)
        .map_err(|e| io::Error::other(format!("{}: {e}", dest.display())))?;
    let mut cmd = transfer_command("get", &guest, rel.as_deref(), recursive, plain, root.as_deref())?;
    if rel.is_some() {
        return Ok(transfer_status(cmd.status().map_err(docker_missing)?));
    }
    // Outside the work area: the tar comes back on stdout; stderr (errors,
    // progress) stays on the terminal.
    let out = cmd.stdout(Stdio::piped()).stderr(Stdio::inherit()).output().map_err(docker_missing)?;
    if !out.status.success() {
        return Ok(transfer_status(out.status));
    }
    crate::sync::place(&out.stdout, &name, &dest)
        .map_err(|e| io::Error::other(format!("get {guest}: {e}")))?;
    Ok(0)
}

/// Where `dockerix get` writes: a lone `-` means our stdout, every other
/// host argument (`./-` included) is a path, and none is the current
/// directory.
enum GetDest {
    Stdout,
    Path(PathBuf),
}

fn get_dest(host: Option<PathBuf>) -> GetDest {
    match host {
        Some(p) if p.as_os_str() == "-" => GetDest::Stdout,
        Some(p) => GetDest::Path(p),
        None => GetDest::Path(PathBuf::from(".")),
    }
}

/// Run `agent get` with no destination, so it streams the tar to stdout
/// (the outside-the-work-area path); stderr (errors, progress) stays on the
/// terminal.
fn fetch_to_pipe(
    guest: &str,
    recursive: bool,
    plain: bool,
    root: Option<&Path>,
) -> io::Result<std::process::Output> {
    let mut cmd = transfer_command("get", guest, None, recursive, plain, root)?;
    cmd.stdout(Stdio::piped()).stderr(Stdio::inherit()).output().map_err(docker_missing)
}

/// `dockerix put [-r] <host-path> [<guest-path>]`: the reverse of `get`.
/// The guest path defaults to the same place in the guest's work area for a
/// host path inside it (where the automatic push would have put it anyway —
/// `put` is the escape hatch for `--no-sync` and paths outside the work
/// area), else to the guest's copy of the cwd.
pub fn put(host: PathBuf, guest: Option<String>, recursive: bool, plain: bool) -> io::Result<i32> {
    let container = require_running()?;
    let root = workarea_root(&container);
    if host.as_os_str() == "-" {
        return put_from_stdin(guest, recursive, plain, root.as_deref());
    }
    let with_host = |e: io::Error| io::Error::other(format!("{}: {e}", host.display()));
    let md = std::fs::symlink_metadata(&host).map_err(with_host)?;
    if md.is_dir() && !recursive {
        return Err(io::Error::other(format!("{}: is a directory (use -r)", host.display())));
    }
    let Some(name) = host.file_name().and_then(|n| n.to_str()).map(str::to_string) else {
        return Err(io::Error::other(format!("{}: not a file or directory name", host.display())));
    };
    let rel = workarea_rel(root.as_deref(), &host).map_err(with_host)?.filter(|r| !r.is_empty());
    let guest = guest.unwrap_or_else(|| match &rel {
        Some(r) => format!("{WORKAREA_MOUNT}/{r}"),
        None => name.clone(),
    });
    let mut cmd = transfer_command("put", &guest, rel.as_deref(), recursive, plain, root.as_deref())?;
    if rel.is_some() {
        return Ok(transfer_status(cmd.stdin(Stdio::null()).status().map_err(docker_missing)?));
    }
    // Outside the work area: pack here and stream the tar in on stdin.
    let parent = host.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
    let tar = crate::sync::pack(parent, &name)?;
    let mut child = cmd.stdin(Stdio::piped()).spawn().map_err(docker_missing)?;
    if let Err(e) = child.stdin.take().expect("piped stdin").write_all(&tar) {
        return Err(io::Error::other(format!("streaming {} to the container: {e}", host.display())));
    }
    Ok(transfer_status(child.wait()?))
}

/// `dockerix put - <guest-path>`: take one file's contents from stdin and
/// write them to the guest at `guest`. The guest path is required (there is
/// no host name to default to), and `-r` makes no sense for a byte stream.
/// The bytes are packed as a one-file tar named for the guest path's last
/// component and sent as for a host path outside the work area.
fn put_from_stdin(
    guest: Option<String>,
    recursive: bool,
    plain: bool,
    root: Option<&Path>,
) -> io::Result<i32> {
    if recursive {
        return Err(io::Error::other("put -: -r cannot read a directory from stdin"));
    }
    let Some(guest) = guest else {
        return Err(io::Error::other("put -: a guest path is required (there is no file name to reuse)"));
    };
    let name = guest.trim_end_matches('/').rsplit('/').next().unwrap_or("");
    if name.is_empty() || name == "." || name == ".." {
        return Err(io::Error::other(format!("{guest}: not a file name")));
    }
    let mut data = Vec::new();
    io::stdin().read_to_end(&mut data)?;
    let tar = crate::sync::archive_bytes(name, &data)?;
    let mut cmd = transfer_command("put", &guest, None, false, plain, root)?;
    let mut child = cmd.stdin(Stdio::piped()).spawn().map_err(docker_missing)?;
    if let Err(e) = child.stdin.take().expect("piped stdin").write_all(&tar) {
        return Err(io::Error::other(format!("streaming stdin to the container: {e}")));
    }
    Ok(transfer_status(child.wait()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn get_dest_reads_a_lone_dash_as_stdout() {
        // A lone `-` is stdout; `./-` is a file named `-`; nothing is the cwd.
        assert!(matches!(get_dest(Some(PathBuf::from("-"))), GetDest::Stdout));
        assert!(matches!(get_dest(Some(PathBuf::from("./-"))), GetDest::Path(p) if p == Path::new("./-")));
        assert!(matches!(get_dest(Some(PathBuf::from("out"))), GetDest::Path(p) if p == Path::new("out")));
        assert!(matches!(get_dest(None), GetDest::Path(p) if p == Path::new(".")));
    }
}
