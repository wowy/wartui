//! The node firmware's version, and the rule tying the wire to the manifest.

use wartui_proto::air::{NODE_VERSION, Version};

/// `version` under `[package]` in `firmware/node/Cargo.toml`.
fn manifest_version() -> String {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../firmware/node/Cargo.toml");
    let manifest = std::fs::read_to_string(path).expect("firmware/node/Cargo.toml is readable");
    let mut in_package = false;
    for line in manifest.lines().map(str::trim) {
        if line.starts_with('[') {
            in_package = line == "[package]";
            continue;
        }
        if in_package
            && let Some(rest) = line.strip_prefix("version")
            && let Some(value) = rest.trim_start().strip_prefix('=')
        {
            return value.trim().trim_matches('"').to_owned();
        }
    }
    panic!("no version under [package] in {path}");
}

#[test]
fn node_version_matches_firmware_manifest_when_compared() {
    // Every firmware bump edits both: the heartbeat announces `NODE_VERSION`, and the
    // app descriptor the manifest's version.
    assert_eq!(manifest_version(), NODE_VERSION.to_string());
}

#[test]
fn version_speaks_with_when_major_and_minor_match() {
    let here = Version::new(0, 1, 3);
    assert!(here.speaks_with(&Version::new(0, 1, 0)), "patch never changes the wire");
    assert!(here.speaks_with(&Version::new(0, 1, 9)));
}

#[test]
fn version_refuses_to_speak_with_when_minor_or_major_differs() {
    let here = Version::new(0, 1, 0);
    assert!(!here.speaks_with(&Version::new(0, 2, 0)));
    assert!(!here.speaks_with(&Version::new(0, 0, 9)));
    assert!(!here.speaks_with(&Version::new(1, 1, 0)));
}

#[test]
fn version_formats_as_dotted_triple_when_displayed() {
    assert_eq!(Version::new(0, 1, 0).to_string(), "0.1.0");
    assert_eq!(Version::new(12, 34, 255).to_string(), "12.34.255");
}
