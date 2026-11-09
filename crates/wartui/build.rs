//! Bakes the host's git commit and release tag into a build.
//!
//! The commit's rerun path is the reflog, which moves whenever HEAD does, including on a commit to
//! a packed ref.
//!
//! `flash-fleet` fetches the node image from the release it was built for, and the tag is the
//! only name for that: the crate version does not move before 1.0. A build without the variable
//! is a checkout, and builds its node image from source instead.

use std::process::Command;

fn git(args: &[&str]) -> Option<String> {
    let output = Command::new("git").args(args).output().ok()?;
    output.status.success().then(|| String::from_utf8_lossy(&output.stdout).trim_end().to_owned())
}

fn main() {
    if let Some(reflog) = git(&["rev-parse", "--git-path", "logs/HEAD"]) {
        println!("cargo:rerun-if-changed={reflog}");
    }
    if let Some(commit) = git(&["rev-parse", "--short", "HEAD"]) {
        println!("cargo:rustc-env=WARTUI_HOST_COMMIT={commit}");
    }
    println!("cargo:rerun-if-env-changed=WARTUI_RELEASE_TAG");
    if let Ok(tag) = std::env::var("WARTUI_RELEASE_TAG") {
        println!("cargo:rustc-env=WARTUI_RELEASE_TAG={tag}");
    }
}
