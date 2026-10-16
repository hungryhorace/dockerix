//! The image `dockerix start` runs: built from the build context embedded
//! in this binary, tagged with its hash, and rebuilt when that changes.

use std::io::{self, Write};
use std::process::Stdio;

use super::*;
use crate::docker::{docker, docker_missing};

/// The image's build context, packed by build.rs (src/context.rs), and
/// its sha256 (both empty in the agent built inside the image).
pub(super) const CONTEXT: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/context.tar"));

pub(super) const CONTEXT_SHA256: &str = include_str!(concat!(env!("OUT_DIR"), "/context.sha256"));

/// The repository of the images built from it, each tagged with the first
/// 12 digits of the hash.
pub(super) const IMAGE_REPO: &str = "dockerix";

/// `dockerix:<hash12>`, the image built from the embedded context.
pub(super) fn context_image() -> io::Result<String> {
    match CONTEXT_SHA256.get(..12) {
        Some(hash) => Ok(format!("{IMAGE_REPO}:{hash}")),
        None => Err(io::Error::other(
            "this dockerix was built without the image's sources (outside a dockerix \
             checkout); name an image to run with --image",
        )),
    }
}

/// `--image`, else `DOCKERIX_IMAGE`: an image to run as is, with no hashing
/// or building.
pub(super) fn given_image() -> Option<String> {
    globals().image.clone().or_else(config::image)
}

pub(super) fn image_exists(image: &str) -> io::Result<bool> {
    Ok(docker_output(&["image", "inspect", "--format", "{{.Id}}", image])?.is_some())
}

/// The image to run, and whether it is local only: `DOCKERIX_IMAGE` as
/// given, or `dockerix:<hash12>`, built from the embedded context the first
/// time it is needed.
pub(super) fn ensure_image() -> io::Result<(String, bool)> {
    if let Some(image) = given_image() {
        return Ok((image, false));
    }
    let image = context_image()?;
    if !image_exists(&image)? {
        eprintln!("dockerix: building image {image} (first build takes several minutes)");
        build_image(&image, false)?;
    }
    Ok((image, true))
}

/// `docker build --tag <image> -` with the embedded context on stdin and
/// BuildKit's progress on the terminal; then drop the older tags.
pub(super) fn build_image(image: &str, no_cache: bool) -> io::Result<()> {
    let mut cmd = docker();
    cmd.args(["build", "--tag", image]);
    if no_cache {
        cmd.arg("--no-cache");
    }
    let mut child =
        cmd.arg("-").stdin(Stdio::piped()).stdout(io::stderr()).spawn().map_err(docker_missing)?;
    // Write before waiting, but report docker's failure ahead of a broken
    // pipe: if docker exits early, its own message says why.
    let written = child.stdin.take().map_or(Ok(()), |mut stdin| stdin.write_all(CONTEXT));
    let status = child.wait()?;
    if !status.success() {
        return Err(io::Error::other(format!("docker build failed (exit {})", status.code().unwrap_or(1))));
    }
    written?;
    prune_images(image);
    Ok(())
}

/// Remove every other `dockerix:*` tag, best effort: one that a container
/// still uses refuses to go, and stays. `DOCKERIX_IMAGE` is kept too.
pub(super) fn prune_images(keep: &str) {
    let fmt = "{{.Repository}}:{{.Tag}}";
    let Ok(Some(list)) = docker_output(&["image", "ls", IMAGE_REPO, "--format", fmt]) else {
        return;
    };
    let given = given_image();
    for tag in list.lines() {
        if tag != keep && !tag.ends_with(":<none>") && given.as_deref() != Some(tag) {
            let _ = docker().args(["image", "rm", tag]).stdout(Stdio::null()).stderr(Stdio::null()).status();
        }
    }
}

/// The image this dockerix would start: `DOCKERIX_IMAGE`, else the one
/// built from its embedded context (None in a binary without one).
pub(super) fn expected_image() -> Option<String> {
    given_image().or_else(|| context_image().ok())
}

/// The warning for a container started from an image other than `expected`:
/// the host side and the agent inside may then disagree on the exec channel.
pub(super) fn stale_warning(image: &str, expected: Option<&str>) -> Option<String> {
    let expected = expected.filter(|e| *e != image)?;
    Some(format!(
        "{CONTAINER} runs {image}, not {expected} (this dockerix's image); \
         `dockerix restart` to switch, which ends the session"
    ))
}

/// Print the stale-image warning to stderr, if there is one.
pub(super) fn warn_if_stale(image: &str) {
    if let Some(w) = stale_warning(image, expected_image().as_deref()) {
        eprintln!("dockerix: warning: {w}");
    }
}

/// `dockerix build [--no-cache]`: build the image from the embedded context
/// unless it is already there (a CI warm-up, or after `cargo install`);
/// with `--no-cache`, from scratch regardless. Prints the tag on stdout.
pub fn build(no_cache: bool) -> io::Result<i32> {
    let image = context_image()?;
    if image_exists(&image)? && !no_cache {
        eprintln!("dockerix: {image} is already built");
    } else {
        eprintln!("dockerix: building image {image}");
        build_image(&image, no_cache)?;
    }
    if let Some(given) = given_image() {
        eprintln!(
            "dockerix: note: an image is named (--image or DOCKERIX_IMAGE), so start runs {given}, not this one"
        );
    }
    outln!("{image}");
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stale_warning_only_on_a_mismatch() {
        assert_eq!(stale_warning("dockerix:aaaaaaaaaaaa", Some("dockerix:aaaaaaaaaaaa")), None);
        // No context and no DOCKERIX_IMAGE: nothing to compare with.
        assert_eq!(stale_warning("dockerix:aaaaaaaaaaaa", None), None);
        let w = stale_warning("dockerix:latest", Some("dockerix:bbbbbbbbbbbb")).unwrap();
        assert!(w.contains("runs dockerix:latest, not dockerix:bbbbbbbbbbbb"), "{w}");
        assert!(w.contains("dockerix restart"), "{w}");
    }
}
