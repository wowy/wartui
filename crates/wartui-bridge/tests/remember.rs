//! Holding the bridge's address for the transport and the view.

use wartui_bridge::remember::BridgeMemory;
use wartui_proto::mac;

const BRIDGE_MAC: &str = "02:00:5E:10:44:C0";

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
    memory.remember_on(address(), None);
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
    memory.remember_on(address(), None);
    assert_eq!(memory.recall(), None);
}

#[test]
fn bridge_memory_ignores_remember_when_disabled() {
    let memory = BridgeMemory::none();
    memory.set_enabled(false);
    memory.remember_on(address(), None);
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
    transport.remember_on(address(), None);
    assert_eq!(memory.recall(), Some(address()));
    memory.set_enabled(false);
    assert!(!transport.is_enabled());
    assert_eq!(transport.recall(), None);
    transport.remember_on(address(), None);
    memory.set_enabled(true);
    assert!(transport.is_enabled());
    assert_eq!(memory.recall(), None);
}

#[test]
fn bridge_memory_recalls_again_when_reenabled_after_remember() {
    let memory = BridgeMemory::none();
    memory.set_enabled(false);
    memory.set_enabled(true);
    memory.remember_on(address(), None);
    assert_eq!(memory.recall(), Some(address()));
}

#[test]
fn bridge_memory_recalls_serial_when_remembered_on_a_port() {
    let memory = BridgeMemory::none();
    memory.remember_on(address(), Some("5CF7153520".to_owned()));
    assert_eq!(memory.recall(), Some(address()));
    assert_eq!(memory.recall_serial().as_deref(), Some("5CF7153520"));
    let seeded = BridgeMemory::holding(Some(address())).on_port(Some("5CF7153520".to_owned()));
    assert_eq!(seeded.recall_serial().as_deref(), Some("5CF7153520"));
}

#[test]
fn bridge_memory_drops_serial_when_another_address_is_remembered_without_port() {
    // A port held for another address would put the wrong board first.
    let memory = BridgeMemory::none();
    memory.remember_on(address(), Some("5CF7153520".to_owned()));
    let other = mac::parse("02:F2:62:10:9D:24").expect("an address");
    memory.remember_on(other, None);
    assert_eq!(memory.recall(), Some(other));
    assert_eq!(memory.recall_serial(), None);
}

#[test]
fn bridge_memory_keeps_serial_when_same_address_is_remembered_without_port() {
    // A caller that does not know the port has not learned that it moved.
    let memory = BridgeMemory::none();
    memory.remember_on(address(), Some("5CF7153520".to_owned()));
    memory.remember_on(address(), None);
    assert_eq!(memory.recall(), Some(address()));
    assert_eq!(memory.recall_serial().as_deref(), Some("5CF7153520"));
}

#[test]
fn bridge_memory_matches_when_given_exactly_what_it_recalls() {
    let memory = BridgeMemory::none();
    assert!(memory.matches(None, None));
    memory.remember_on(address(), Some("5CF7153520".to_owned()));
    assert!(memory.matches(Some(address()), Some("5CF7153520")));
    assert!(!memory.matches(Some(address()), None));
    assert!(!memory.matches(None, Some("5CF7153520")));
    memory.set_enabled(false);
    assert!(memory.matches(None, None), "off holds nothing");
}

#[test]
fn bridge_memory_forgets_serial_when_disabled() {
    let memory = BridgeMemory::none();
    memory.remember_on(address(), Some("5CF7153520".to_owned()));
    memory.set_enabled(false);
    assert_eq!(memory.recall_serial(), None);
    memory.set_enabled(true);
    assert_eq!(memory.recall_serial(), None, "off cleared it");
}
