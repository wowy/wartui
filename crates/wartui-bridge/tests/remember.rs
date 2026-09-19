//! Carrying the bridge's address from one run to the next.

use wartui_bridge::ports;
use wartui_bridge::remember::BridgeMemory;

const BRIDGE_MAC: &str = "10:BD:A3:EC:44:C0";

fn address() -> [u8; 6] {
    ports::parse_mac(BRIDGE_MAC).expect("an address")
}

#[test]
fn a_bridge_remembered_on_one_run_is_recalled_by_the_next() {
    let dir = tempfile::tempdir().expect("temp dir");
    let memory = BridgeMemory::at(dir.path().join("bridge"));
    assert_eq!(memory.recall(), None, "nothing has been found yet");
    memory.remember(address());
    assert_eq!(BridgeMemory::at(dir.path().join("bridge")).recall(), Some(address()));
}

#[test]
fn the_file_holds_the_address_in_the_form_everything_else_prints_it() {
    // So that it can be grepped against `wartui sniff` output and against the
    // by-id symlink, which carry the same spelling.
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("bridge");
    BridgeMemory::at(&path).remember(address());
    assert_eq!(std::fs::read_to_string(&path).expect("the file").trim(), BRIDGE_MAC);
}

#[test]
fn a_directory_that_does_not_exist_yet_is_made_rather_than_refused() {
    let dir = tempfile::tempdir().expect("temp dir");
    let memory = BridgeMemory::at(dir.path().join("state/wartui/bridge"));
    memory.remember(address());
    assert_eq!(memory.recall(), Some(address()));
}

#[test]
fn a_file_that_is_not_an_address_reads_as_nothing_remembered() {
    // Which is also what a torn write leaves, and why one write is enough.
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("bridge");
    for junk in ["", "10:BD:A3", "not an address", "10:BD:A3:EC:44:C0:FF"] {
        std::fs::write(&path, junk).expect("writing the file");
        assert_eq!(BridgeMemory::at(&path).recall(), None, "{junk:?}");
    }
}

#[test]
fn a_memory_with_nowhere_to_keep_anything_is_silent_rather_than_broken() {
    // What a host with no state directory gets. It must cost a sweep and nothing
    // else — never a failed capture.
    let memory = BridgeMemory::none();
    memory.remember(address());
    memory.forget();
    assert_eq!(memory.recall(), None);
}

#[test]
fn a_state_directory_that_cannot_be_written_costs_a_slower_start_and_nothing_else() {
    let dir = tempfile::tempdir().expect("temp dir");
    // A file where the directory would have to go: `create_dir_all` cannot win.
    let blocked = dir.path().join("blocked");
    std::fs::write(&blocked, "").expect("writing the blocker");
    let memory = BridgeMemory::at(blocked.join("bridge"));
    memory.remember(address());
    assert_eq!(memory.recall(), None);
}

#[test]
fn a_board_that_stops_being_the_bridge_is_forgotten() {
    let dir = tempfile::tempdir().expect("temp dir");
    let memory = BridgeMemory::at(dir.path().join("bridge"));
    memory.remember(address());
    memory.forget();
    assert_eq!(memory.recall(), None);
    memory.forget(); // Idempotent: a missing file is the state being asked for.
}
