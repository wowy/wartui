//! Holding the bridge's address for the transport and the view.

use wartui_bridge::remember::BridgeMemory;
use wartui_proto::mac;

const BRIDGE_MAC: &str = "10:BD:A3:EC:44:C0";

fn address() -> [u8; 6] {
    mac::parse(BRIDGE_MAC).expect("an address")
}

#[test]
fn bridge_memory_recalls_seeded_mac_when_built_holding_one() {
    assert_eq!(BridgeMemory::holding(Some(address())).recall(), Some(address()));
    assert_eq!(BridgeMemory::holding(None).recall(), None);
    assert_eq!(BridgeMemory::none().recall(), None);
}

#[test]
fn bridge_memory_recalls_mac_when_remembered() {
    let memory = BridgeMemory::none();
    memory.remember(address());
    assert_eq!(memory.recall(), Some(address()));
}

#[test]
fn bridge_memory_recalls_nothing_when_forget_is_called() {
    let memory = BridgeMemory::holding(Some(address()));
    memory.forget();
    assert_eq!(memory.recall(), None);
    memory.forget(); // Idempotent: nothing held is the state being asked for.
    assert_eq!(memory.recall(), None);
}

#[test]
fn bridge_memory_recalls_nothing_when_disabled() {
    let memory = BridgeMemory::none();
    memory.set_enabled(false);
    memory.remember(address());
    assert_eq!(memory.recall(), None);
}

#[test]
fn bridge_memory_ignores_remember_when_disabled() {
    let memory = BridgeMemory::none();
    memory.set_enabled(false);
    memory.remember(address());
    memory.set_enabled(true);
    assert_eq!(memory.recall(), None, "nothing was held while off");
}

#[test]
fn bridge_memory_clears_mac_when_disabled() {
    let memory = BridgeMemory::holding(Some(address()));
    memory.set_enabled(false);
    assert!(!memory.is_enabled());
    memory.set_enabled(true);
    assert_eq!(memory.recall(), None, "off means nothing held");
}

#[test]
fn bridge_memory_shares_slot_and_switch_when_cloned() {
    // The transport and the view hold clones of one handle: the view's switch has
    // to reach the transport's next reconnect, and the transport's connection has
    // to reach the view that saves it.
    let memory = BridgeMemory::none();
    let transport = memory.clone();
    transport.remember(address());
    assert_eq!(memory.recall(), Some(address()));
    memory.set_enabled(false);
    assert!(!transport.is_enabled());
    assert_eq!(transport.recall(), None);
    transport.remember(address());
    memory.set_enabled(true);
    assert!(transport.is_enabled());
    assert_eq!(memory.recall(), None);
}

#[test]
fn bridge_memory_recalls_again_when_reenabled_after_remember() {
    let memory = BridgeMemory::none();
    memory.set_enabled(false);
    memory.set_enabled(true);
    memory.remember(address());
    assert_eq!(memory.recall(), Some(address()));
}
