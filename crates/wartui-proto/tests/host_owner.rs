//! Who owns the P4's host queue (`wartui_proto::host_owner`): every input in every state.
//!
//! The four reachable states: no owner, USB alone, USB with a central waiting, and Bluetooth.
//! A central that owns the queue is always subscribed, and no owner means no central, since a
//! central with no owner takes the queue.

use wartui_proto::host_owner::{Effect, Input, Owner};
use wartui_proto::link::HostOwner;

fn none() -> Owner {
    Owner::new()
}

fn usb() -> Owner {
    let mut owner = Owner::new();
    let _ = owner.on(Input::UartFrame);
    owner
}

fn usb_with_central() -> Owner {
    let mut owner = usb();
    let _ = owner.on(Input::Subscribed);
    owner
}

fn bluetooth() -> Owner {
    let mut owner = Owner::new();
    let _ = owner.on(Input::Subscribed);
    owner
}

/// Feed `input` to `owner`, and return the effects and who owns the queue after.
fn step(mut owner: Owner, input: Input) -> (Vec<Effect>, HostOwner, bool) {
    let effects = owner.on(input).to_vec();
    (effects, owner.owner(), owner.central())
}

#[test]
fn host_owner_starts_with_no_owner_when_new() {
    let owner = Owner::new();
    assert_eq!(owner.owner(), HostOwner::None);
    assert!(!owner.central());
}

#[test]
fn host_owner_attaches_bluetooth_when_central_subscribes_with_no_owner() {
    let (effects, owner, central) = step(none(), Input::Subscribed);
    assert_eq!(effects, [Effect::AttachBluetooth]);
    assert_eq!(owner, HostOwner::Bluetooth);
    assert!(central);
}

#[test]
fn host_owner_keeps_usb_when_central_subscribes_while_usb_owns() {
    // The central waits for UART0 to fall silent.
    let (effects, owner, central) = step(usb(), Input::Subscribed);
    assert!(effects.is_empty());
    assert_eq!(owner, HostOwner::Usb);
    assert!(central);
}

#[test]
fn host_owner_changes_nothing_when_central_subscribes_again_while_waiting() {
    let (effects, owner, central) = step(usb_with_central(), Input::Subscribed);
    assert!(effects.is_empty());
    assert_eq!(owner, HostOwner::Usb);
    assert!(central);
}

#[test]
fn host_owner_changes_nothing_when_central_subscribes_again_while_bluetooth_owns() {
    // A `BleState` that changes only the MTU, say.
    let (effects, owner, central) = step(bluetooth(), Input::Subscribed);
    assert!(effects.is_empty());
    assert_eq!(owner, HostOwner::Bluetooth);
    assert!(central);
}

#[test]
fn host_owner_changes_nothing_when_unsubscribed_with_no_owner() {
    let (effects, owner, central) = step(none(), Input::Unsubscribed);
    assert!(effects.is_empty());
    assert_eq!(owner, HostOwner::None);
    assert!(!central);
}

#[test]
fn host_owner_keeps_usb_when_unsubscribed_while_usb_owns() {
    let (effects, owner, central) = step(usb(), Input::Unsubscribed);
    assert!(effects.is_empty());
    assert_eq!(owner, HostOwner::Usb);
    assert!(!central);
}

#[test]
fn host_owner_drops_waiting_central_when_unsubscribed_while_usb_owns() {
    let (effects, owner, central) = step(usb_with_central(), Input::Unsubscribed);
    assert!(effects.is_empty());
    assert_eq!(owner, HostOwner::Usb);
    assert!(!central);
}

#[test]
fn host_owner_releases_bluetooth_when_unsubscribed_while_bluetooth_owns() {
    let (effects, owner, central) = step(bluetooth(), Input::Unsubscribed);
    assert_eq!(effects, [Effect::ReleaseBluetooth]);
    assert_eq!(owner, HostOwner::None);
    assert!(!central);
}

#[test]
fn host_owner_attaches_usb_when_identify_arrives_with_no_owner() {
    let (effects, owner, _) = step(none(), Input::UartIdentify);
    assert_eq!(effects, [Effect::AttachUsb]);
    assert_eq!(owner, HostOwner::Usb);
}

#[test]
fn host_owner_keeps_usb_when_identify_arrives_while_usb_owns() {
    let (effects, owner, _) = step(usb(), Input::UartIdentify);
    assert!(effects.is_empty());
    assert_eq!(owner, HostOwner::Usb);
}

#[test]
fn host_owner_keeps_usb_when_identify_arrives_while_central_waits() {
    let (effects, owner, central) = step(usb_with_central(), Input::UartIdentify);
    assert!(effects.is_empty());
    assert_eq!(owner, HostOwner::Usb);
    assert!(central);
}

#[test]
fn host_owner_answers_ready_on_uart_when_identify_arrives_while_bluetooth_owns() {
    // A port probe on the PC the bridge is plugged into must not take the central's queue.
    let (effects, owner, central) = step(bluetooth(), Input::UartIdentify);
    assert_eq!(effects, [Effect::ReadyOnUart]);
    assert_eq!(owner, HostOwner::Bluetooth);
    assert!(central);
}

#[test]
fn host_owner_attaches_usb_when_uart_frame_arrives_with_no_owner() {
    let (effects, owner, _) = step(none(), Input::UartFrame);
    assert_eq!(effects, [Effect::AttachUsb]);
    assert_eq!(owner, HostOwner::Usb);
}

#[test]
fn host_owner_keeps_usb_when_uart_frame_arrives_while_usb_owns() {
    let (effects, owner, _) = step(usb(), Input::UartFrame);
    assert!(effects.is_empty());
    assert_eq!(owner, HostOwner::Usb);
}

#[test]
fn host_owner_keeps_usb_when_uart_frame_arrives_while_central_waits() {
    let (effects, owner, central) = step(usb_with_central(), Input::UartFrame);
    assert!(effects.is_empty());
    assert_eq!(owner, HostOwner::Usb);
    assert!(central);
}

#[test]
fn host_owner_notices_and_releases_central_when_uart_frame_arrives_while_bluetooth_owns() {
    // `flash-bridge` over USB always gets in, and the central hears why it lost the queue.
    let (effects, owner, central) = step(bluetooth(), Input::UartFrame);
    assert_eq!(effects, [Effect::NoticeCentral, Effect::ReleaseBluetooth, Effect::AttachUsb]);
    assert_eq!(owner, HostOwner::Usb);
    assert!(central);
}

#[test]
fn host_owner_changes_nothing_when_uart_silent_with_no_owner() {
    let (effects, owner, _) = step(none(), Input::UartSilent);
    assert!(effects.is_empty());
    assert_eq!(owner, HostOwner::None);
}

#[test]
fn host_owner_releases_usb_when_uart_silent_with_no_central() {
    let (effects, owner, _) = step(usb(), Input::UartSilent);
    assert_eq!(effects, [Effect::ReleaseUsb]);
    assert_eq!(owner, HostOwner::None);
}

#[test]
fn host_owner_hands_queue_back_to_bluetooth_when_uart_silent_while_central_waits() {
    let (effects, owner, central) = step(usb_with_central(), Input::UartSilent);
    assert_eq!(effects, [Effect::ReleaseUsb, Effect::AttachBluetooth]);
    assert_eq!(owner, HostOwner::Bluetooth);
    assert!(central);
}

#[test]
fn host_owner_changes_nothing_when_uart_silent_while_bluetooth_owns() {
    // An `Identify` while Bluetooth owns opened no gate, so no silence closes one.
    let (effects, owner, _) = step(bluetooth(), Input::UartSilent);
    assert!(effects.is_empty());
    assert_eq!(owner, HostOwner::Bluetooth);
}

#[test]
fn host_owner_hands_queue_back_to_bluetooth_when_usb_takes_it_then_falls_silent() {
    // Run 10's sequence: a USB frame takes the queue from the central, then UART0 falls silent.
    let mut owner = bluetooth();
    let _ = owner.on(Input::UartFrame);
    let effects = owner.on(Input::UartSilent).to_vec();
    assert_eq!(effects, [Effect::ReleaseUsb, Effect::AttachBluetooth]);
    assert_eq!(owner.owner(), HostOwner::Bluetooth);
}
