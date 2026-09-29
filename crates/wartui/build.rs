//! Bakes the release tag into a release build.
//!
//! `flash-fleet` fetches the node image from the release it was built for, and the tag is the
//! only name for that: the crate version does not move before 1.0. A build without the variable
//! is a checkout, and builds its node image from source instead.

fn main() {
    println!("cargo:rerun-if-env-changed=WARTUI_RELEASE_TAG");
    if let Ok(tag) = std::env::var("WARTUI_RELEASE_TAG") {
        println!("cargo:rustc-env=WARTUI_RELEASE_TAG={tag}");
    }
}
