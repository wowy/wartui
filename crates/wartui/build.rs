//! Bakes the host source identity and release tag into a build.
//!
//! `flash-fleet` fetches the node image from the release it was built for, and the tag is the
//! only name for that: the crate version does not move before 1.0. A build without the variable
//! is a checkout, and builds its node image from source instead.

mod build_provenance;

fn main() {
    let manifest = std::path::PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap());
    let workspace = manifest.parent().unwrap().parent().unwrap();
    let sources = build_provenance::host_sources(workspace).expect("enumerating host sources");
    for path in sources.directories.iter().chain(&sources.files) {
        println!("cargo:rerun-if-changed={}", workspace.join(path).display());
    }
    let identity = build_provenance::build_id(workspace, &sources).expect("hashing host sources");
    println!("cargo:rustc-env=WARTUI_BUILD_ID={identity}");
    println!("cargo:rerun-if-env-changed=WARTUI_RELEASE_TAG");
    if let Ok(tag) = std::env::var("WARTUI_RELEASE_TAG") {
        println!("cargo:rustc-env=WARTUI_RELEASE_TAG={tag}");
    }
}
