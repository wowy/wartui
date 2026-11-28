//! The firmwares' versions, and the rules tying the wire to each manifest.

use wartui_proto::bus::{Compat, NODE_VERSION, Version};
use wartui_proto::link::{BRIDGE_VERSION, End, Standing};

/// `version` under `[package]` in `firmware/<firmware>/Cargo.toml`.
fn manifest_version(firmware: &str) -> String {
    let path = format!("{}/../../firmware/{firmware}/Cargo.toml", env!("CARGO_MANIFEST_DIR"));
    let manifest = std::fs::read_to_string(&path).expect("the firmware's Cargo.toml is readable");
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
    assert_eq!(manifest_version("node"), NODE_VERSION.to_string());
}

#[test]
fn bridge_version_matches_firmware_manifest_when_compared() {
    // Every bridge firmware bump edits both: `Ready` carries `BRIDGE_VERSION`, and the app
    // descriptor the manifest's version.
    assert_eq!(manifest_version("p4-bridge"), BRIDGE_VERSION.to_string());
}

#[test]
fn standing_returns_same_when_only_patch_differs() {
    // A patch changes no message, either way round.
    let host = Version::new(1, 2, 3);
    assert_eq!(Standing::of(Version::new(1, 2, 0), host, true), Standing::Same);
    assert_eq!(Standing::of(Version::new(1, 2, 9), host, true), Standing::Same);
    assert_eq!(Standing::of(host, host, true), Standing::Same);
}

#[test]
fn standing_names_older_end_when_minor_differs() {
    let host = Version::new(1, 2, 3);
    assert_eq!(
        Standing::of(Version::new(1, 1, 9), host, true),
        Standing::Fewer { older: End::Bridge }
    );
    assert_eq!(
        Standing::of(Version::new(1, 3, 0), host, true),
        Standing::Fewer { older: End::Host }
    );
}

#[test]
fn standing_names_older_end_when_major_differs() {
    // The major decides first, and a rest read or not changes nothing across majors.
    let host = Version::new(1, 2, 3);
    for rest_read in [false, true] {
        assert_eq!(
            Standing::of(Version::new(0, 9, 9), host, rest_read),
            Standing::Apart { older: End::Bridge }
        );
        assert_eq!(
            Standing::of(Version::new(2, 0, 0), host, rest_read),
            Standing::Apart { older: End::Host }
        );
    }
}

#[test]
fn standing_returns_other_build_when_rest_does_not_read_on_same_major() {
    let host = Version::new(1, 2, 3);
    assert_eq!(Standing::of(host, host, false), Standing::OtherBuild);
    assert_eq!(Standing::of(Version::new(1, 5, 0), host, false), Standing::OtherBuild);
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
