//! Who owns the P4's host queue (`wartui_proto::host_owner`): every input in every state.
//!
//! The six reachable states: no owner, USB alone, USB with a central waiting, Bluetooth, a phone
//! alone, and a phone with a central waiting. A central that owns the queue is always subscribed,
//! and no owner means no central, since a central with no owner takes the queue.

use wartui_proto::host_owner::{Effect, Holder, Input, Owner, phones_admitted};
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

fn phone() -> Owner {
    let mut owner = Owner::new();
    let _ = owner.on(Input::PhoneSubscribed);
    owner
}

fn phone_with_central() -> Owner {
    let mut owner = phone();
    let _ = owner.on(Input::Subscribed);
    owner
}

/// Feed `input` to `owner`, and return the effects and who holds the queue after.
fn step_holder(mut owner: Owner, input: Input) -> (Vec<Effect>, Holder, bool) {
    let effects = owner.on(input).to_vec();
    (effects, owner.holder(), owner.central())
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

#[test]
fn host_owner_attaches_phone_when_phone_subscribes_with_no_owner() {
    let (effects, holder, central) = step_holder(none(), Input::PhoneSubscribed);
    assert_eq!(effects, [Effect::AttachPhone]);
    assert_eq!(holder, Holder::Phone);
    assert!(!central);
}

#[test]
fn host_owner_ignores_phone_when_usb_owns() {
    // `phones_admitted` is false while USB holds, so the C6 drops the phone.
    for owner in [usb(), usb_with_central()] {
        let (effects, holder, _) = step_holder(owner, Input::PhoneSubscribed);
        assert!(effects.is_empty());
        assert_eq!(holder, Holder::Usb);
    }
}

#[test]
fn host_owner_ignores_phone_when_bluetooth_owns() {
    let (effects, holder, central) = step_holder(bluetooth(), Input::PhoneSubscribed);
    assert!(effects.is_empty());
    assert_eq!(holder, Holder::Bluetooth);
    assert!(central);
}

#[test]
fn host_owner_releases_phone_when_phone_unsubscribes() {
    let (effects, holder, central) = step_holder(phone(), Input::PhoneUnsubscribed);
    assert_eq!(effects, [Effect::ReleasePhone]);
    assert_eq!(holder, Holder::None);
    assert!(!central);
}

#[test]
fn host_owner_hands_queue_to_waiting_central_when_phone_unsubscribes() {
    let (effects, holder, central) = step_holder(phone_with_central(), Input::PhoneUnsubscribed);
    assert_eq!(effects, [Effect::ReleasePhone, Effect::AttachBluetooth]);
    assert_eq!(holder, Holder::Bluetooth);
    assert!(central);
}

#[test]
fn host_owner_answers_ready_on_uart_when_identify_arrives_while_phone_owns() {
    // `wartui ports` on the PC the bridge is plugged into leaves the phone undisturbed.
    let (effects, holder, _) = step_holder(phone(), Input::UartIdentify);
    assert_eq!(effects, [Effect::ReadyOnUart]);
    assert_eq!(holder, Holder::Phone);
}

#[test]
fn host_owner_releases_phone_and_attaches_usb_when_uart_frame_arrives() {
    // No notice: the central's notice is a COBS frame, which a phone cannot read.
    let (effects, holder, _) = step_holder(phone(), Input::UartFrame);
    assert_eq!(effects, [Effect::ReleasePhone, Effect::AttachUsb]);
    assert_eq!(holder, Holder::Usb);
}

#[test]
fn host_owner_keeps_phone_when_uart_is_silent() {
    let (effects, holder, _) = step_holder(phone(), Input::UartSilent);
    assert!(effects.is_empty());
    assert_eq!(holder, Holder::Phone);
}

#[test]
fn host_owner_reports_no_host_when_phone_owns() {
    // No wartui host can see a phone hold the queue: any frame it sends takes it.
    assert_eq!(phone().owner(), HostOwner::None);
    assert_eq!(phone_with_central().owner(), HostOwner::None);
}

#[test]
fn host_owner_admits_no_phone_when_usb_owns() {
    assert!(!phones_admitted(true, Holder::Usb));
}

#[test]
fn host_owner_admits_phone_when_setting_is_on_and_usb_does_not_own() {
    for holder in [Holder::None, Holder::Bluetooth, Holder::Phone] {
        assert!(phones_admitted(true, holder));
    }
}

#[test]
fn host_owner_admits_no_phone_when_setting_is_off() {
    for holder in [Holder::None, Holder::Usb, Holder::Bluetooth, Holder::Phone] {
        assert!(!phones_admitted(false, holder));
    }
}

#[test]
fn host_owner_reads_through_c6_when_holder_is_bluetooth_or_phone() {
    assert!(Holder::Bluetooth.through_c6());
    assert!(Holder::Phone.through_c6());
    assert!(!Holder::None.through_c6());
    assert!(!Holder::Usb.through_c6());
}
