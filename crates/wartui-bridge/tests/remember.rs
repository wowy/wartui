//! Carrying the bridge's address from one run to the next.

use wartui_bridge::ports;
use wartui_bridge::remember::BridgeMemory;

const BRIDGE_MAC: &str = "10:BD:A3:EC:44:C0";

fn address() -> [u8; 6] {
    ports::parse_mac(BRIDGE_MAC).expect("an address")
}

#[test]
fn bridge_memory_persists_and_recalls_mac_when_stored_to_disk() {
    let dir = tempfile::tempdir().expect("temp dir");
    let memory = BridgeMemory::at(dir.path().join("bridge"));
    assert_eq!(memory.recall(), None, "nothing has been found yet");
    memory.remember(address());
    assert_eq!(BridgeMemory::at(dir.path().join("bridge")).recall(), Some(address()));
}

#[test]
fn bridge_memory_formats_mac_as_colon_separated_string_when_persisted() {
    // So that it can be grepped against `wartui sniff` output and against the
    // by-id symlink, which carry the same spelling.
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("bridge");
    BridgeMemory::at(&path).remember(address());
    assert_eq!(std::fs::read_to_string(&path).expect("the file").trim(), BRIDGE_MAC);
}

#[test]
fn bridge_memory_creates_parent_directories_when_persisting_mac() {
    let dir = tempfile::tempdir().expect("temp dir");
    let memory = BridgeMemory::at(dir.path().join("state/wartui/bridge"));
    memory.remember(address());
    assert_eq!(memory.recall(), Some(address()));
}

#[test]
fn bridge_memory_returns_none_when_persisted_file_contains_invalid_mac() {
    // Which is also what a torn write leaves, and why one write is enough.
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("bridge");
    for junk in ["", "10:BD:A3", "not an address", "10:BD:A3:EC:44:C0:FF"] {
        std::fs::write(&path, junk).expect("writing the file");
        assert_eq!(BridgeMemory::at(&path).recall(), None, "{junk:?}");
    }
}

#[test]
fn bridge_memory_acts_as_noop_when_configured_with_none_backend() {
    // What a host with no state directory gets. It must cost a sweep and nothing
    // else — never a failed capture.
    let memory = BridgeMemory::none();
    memory.remember(address());
    memory.forget();
    assert_eq!(memory.recall(), None);
}

#[test]
fn bridge_memory_fails_silently_when_storage_path_is_unwritable() {
    let dir = tempfile::tempdir().expect("temp dir");
    // A file where the directory would have to go: `create_dir_all` cannot win.
    let blocked = dir.path().join("blocked");
    std::fs::write(&blocked, "").expect("writing the blocker");
    let memory = BridgeMemory::at(blocked.join("bridge"));
    memory.remember(address());
    assert_eq!(memory.recall(), None);
}

#[test]
fn bridge_memory_clears_persisted_mac_when_forget_is_called() {
    let dir = tempfile::tempdir().expect("temp dir");
    let memory = BridgeMemory::at(dir.path().join("bridge"));
    memory.remember(address());
    memory.forget();
    assert_eq!(memory.recall(), None);
    memory.forget(); // Idempotent: a missing file is the state being asked for.
}
