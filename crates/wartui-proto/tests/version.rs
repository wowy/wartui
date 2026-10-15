//! The node firmware's version, and the rule tying the wire to the manifest.

use wartui_proto::air::{Compat, NODE_VERSION, Version};

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
fn version_compat_returns_same_when_versions_are_equal() {
    let host = Version::new(0, 2, 3);
    assert_eq!(host.compat(&host), Compat::Same);
}

#[test]
fn version_compat_returns_patch_difference_when_major_and_minor_match() {
    // A patch never changes the wire, so either direction is still drivable.
    let host = Version::new(0, 2, 3);
    assert_eq!(host.compat(&Version::new(0, 2, 0)), Compat::PatchBehind);
    assert_eq!(host.compat(&Version::new(0, 2, 9)), Compat::PatchAhead);
    assert!(Compat::PatchBehind.drivable() && Compat::PatchAhead.drivable());
    assert!(Compat::Same.drivable());
}

#[test]
fn version_compat_returns_node_behind_when_minor_or_major_is_older() {
    let host = Version::new(1, 2, 0);
    assert_eq!(host.compat(&Version::new(1, 1, 9)), Compat::NodeBehind);
    assert_eq!(host.compat(&Version::new(0, 9, 9)), Compat::NodeBehind, "major decides first");
    assert!(!Compat::NodeBehind.drivable());
}

#[test]
fn version_compat_returns_node_ahead_when_minor_or_major_is_newer() {
    let host = Version::new(1, 2, 5);
    assert_eq!(host.compat(&Version::new(1, 3, 0)), Compat::NodeAhead);
    assert_eq!(host.compat(&Version::new(2, 0, 0)), Compat::NodeAhead, "major decides first");
    assert!(!Compat::NodeAhead.drivable());
}

#[test]
fn version_compat_marks_node_behind_when_its_version_is_older() {
    assert!(Compat::PatchBehind.node_behind() && Compat::NodeBehind.node_behind());
    assert!(!Compat::Same.node_behind());
    assert!(!Compat::PatchAhead.node_behind() && !Compat::NodeAhead.node_behind());
}

#[test]
fn version_compat_with_host_compares_against_node_version_when_called() {
    assert_eq!(NODE_VERSION.compat_with_host(), Compat::Same);
}

#[test]
fn version_formats_as_dotted_triple_when_displayed() {
    assert_eq!(Version::new(0, 1, 0).to_string(), "0.1.0");
    assert_eq!(Version::new(12, 34, 255).to_string(), "12.34.255");
}
