//! Serial port selection.

use wartui_bridge::serial::{discover_ports, is_usable_path};

#[test]
fn macos_tty_aliases_are_never_offered() {
    // Every USB serial device shows up twice on macOS. Opening the /dev/tty.*
    // side blocks waiting for carrier detect, which looks exactly like a hung
    // bridge, so only the callout device is usable.
    assert!(!is_usable_path("/dev/tty.usbmodem14201"));
    assert!(is_usable_path("/dev/cu.usbmodem14201"));
    assert!(is_usable_path("/dev/ttyACM0"), "Linux ttyACM devices are fine");
    assert!(is_usable_path("COM3"));
}

#[test]
fn discovery_does_not_fail_when_nothing_is_plugged_in() {
    // An empty list is the normal state, not an error; the transport retries.
    let ports = discover_ports().expect("listing ports should succeed");
    for port in ports {
        assert!(is_usable_path(&port.path));
        assert_eq!(port.vid, Some(wartui_bridge::serial::ESPRESSIF_VID));
    }
}
