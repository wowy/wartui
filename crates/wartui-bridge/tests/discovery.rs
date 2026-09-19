//! Naming the boards, and telling one from another.

use wartui_bridge::ports::{
    self, ESPRESSIF_VID, PortCandidate, StableNames, candidate, is_usable_path, with_stable_paths,
};
use wartui_bridge::serial::{self, BridgeSpec, discover_ports};

/// The bridge on this bench, and a node beside it: same vendor, same product,
/// adjacent device nodes, and nothing but the address to tell them apart.
const BRIDGE_MAC: &str = "10:BD:A3:EC:44:C0";
const NODE_MAC: &str = "02:00:5E:10:9D:24";

fn esp(device: &str, serial: &str) -> PortCandidate {
    candidate(device, Some(ESPRESSIF_VID), Some(0x1001), Some(serial))
}

fn link(device: &str, name: &str) -> (String, String) {
    (device.to_owned(), name.to_owned())
}

#[test]
fn macos_tty_aliases_are_never_offered() {
    // Opening the /dev/tty.* side blocks on carrier detect, which looks exactly like
    // a hung bridge.
    assert!(!is_usable_path("/dev/tty.usbmodem14201"));
    assert!(is_usable_path("/dev/cu.usbmodem14201"));
    assert!(is_usable_path("/dev/ttyACM0"), "Linux ttyACM devices are fine");
    assert!(is_usable_path("COM3"));
}

#[test]
fn discovery_does_not_fail_when_nothing_is_plugged_in() {
    // An empty list is the normal state, not an error; the transport retries.
    let found = discover_ports().expect("listing ports should succeed");
    for port in found {
        assert!(is_usable_path(&port.path));
        assert_eq!(port.vid, Some(ESPRESSIF_VID));
    }
}

#[test]
fn a_board_is_named_by_the_mac_its_usb_serial_number_carries() {
    // The whole reason `wartui ports` can say which board is which: an ESP32's
    // serial number is the address its radio transmits from.
    assert_eq!(esp("/dev/ttyACM0", BRIDGE_MAC).mac(), ports::parse_mac(BRIDGE_MAC));
    assert_eq!(esp("/dev/ttyACM0", BRIDGE_MAC).mac().expect("an address")[0], 0x10);
}

#[test]
fn a_device_carrying_a_manufacturing_serial_has_no_address() {
    // Everything else on the bus. `ports` says so rather than inventing one.
    let puck = candidate("/dev/ttyACM1", Some(0x1546), Some(0x01A7), Some("0001"));
    assert_eq!(puck.mac(), None);
    assert_eq!(candidate("/dev/ttyACM1", Some(0x1546), Some(0x01A7), None).mac(), None);
}

#[test]
fn a_bridge_given_as_a_mac_is_told_from_one_given_as_a_path() {
    let by_address: BridgeSpec = BRIDGE_MAC.parse().expect("parsing cannot fail");
    let by_path: BridgeSpec = "/dev/ttyACM0".parse().expect("parsing cannot fail");
    assert_eq!(by_address, BridgeSpec::Mac(ports::parse_mac(BRIDGE_MAC).expect("an address")));
    assert_eq!(by_path, BridgeSpec::Path("/dev/ttyACM0".to_owned()));
    // And a spec prints back in the form it was given, so advice can quote it.
    assert_eq!(by_address.to_string(), BRIDGE_MAC);
    assert_eq!(by_path.to_string(), "/dev/ttyACM0");
}

#[test]
fn a_path_that_merely_contains_hex_is_still_a_path() {
    // Nothing rules out a device node with colons in it, and reading one as an
    // address would open a board the operator never named.
    let spec: BridgeSpec = "/dev/serial/by-id/usb-Espressif_10:BD:A3:EC:44:C0-if00"
        .parse()
        .expect("parsing cannot fail");
    assert!(matches!(spec, BridgeSpec::Path(_)));
}

#[test]
fn an_espressif_board_is_named_by_its_by_id_symlink_when_one_exists() {
    // The path `wartui ports` prints is the one worth copying into `--bridge`: it
    // survives the re-enumeration that moves ttyACM0 to ttyACM1.
    let by_id = "/dev/serial/by-id/usb-Espressif_USB_JTAG_serial_debug_unit_10:BD:A3:EC:44:C0-if00";
    let names = StableNames::from_pairs(
        [link("/dev/ttyACM0", by_id)],
        [link("/dev/ttyACM0", "/dev/serial/by-path/pci-0000:00:14.0-usb-0:3.2:1.0")],
    );
    let named = with_stable_paths(vec![esp("/dev/ttyACM0", BRIDGE_MAC)], &names);
    assert_eq!(named[0].path, by_id);
    assert_eq!(named[0].device, "/dev/ttyACM0", "the device node is kept for comparing");
}

#[test]
fn a_port_with_no_symlink_keeps_the_path_the_os_gave_it() {
    let named = with_stable_paths(vec![esp("/dev/ttyACM0", BRIDGE_MAC)], &StableNames::default());
    assert_eq!(named[0].path, "/dev/ttyACM0");
}

#[test]
fn a_by_id_name_shared_by_two_devices_falls_back_to_the_socket() {
    // Two receivers of one model reporting no serial number answer to one by-id
    // name, and udev can only give it to whichever enumerated last. Opening it
    // would mean opening whichever was plugged in most recently rather than the
    // one that was named, so those are named by socket instead.
    let shared = "/dev/serial/by-id/usb-u-blox_AG_u-blox_7_-_GPS_GNSS_Receiver-if00";
    let names = StableNames::from_pairs(
        [link("/dev/ttyACM1", shared)],
        [
            link("/dev/ttyACM1", "/dev/serial/by-path/pci-0000:00:14.0-usb-0:3.4:1.0"),
            link("/dev/ttyACM2", "/dev/serial/by-path/pci-0000:00:14.0-usb-0:3.5:1.0"),
        ],
    );
    let twins = vec![
        candidate("/dev/ttyACM1", Some(0x1546), Some(0x01A7), None),
        candidate("/dev/ttyACM2", Some(0x1546), Some(0x01A7), None),
    ];
    let named = with_stable_paths(twins, &names);
    assert_eq!(named[0].path, "/dev/serial/by-path/pci-0000:00:14.0-usb-0:3.4:1.0");
    assert_eq!(named[1].path, "/dev/serial/by-path/pci-0000:00:14.0-usb-0:3.5:1.0");
}

#[test]
fn two_boards_reporting_their_own_addresses_each_keep_their_by_id_name() {
    // The contrast with the twins above: an ESP32 names itself, so no two share a
    // by-id name however many are attached.
    let names = StableNames::from_pairs(
        [
            link("/dev/ttyACM0", "/dev/serial/by-id/usb-Espressif_bridge-if00"),
            link("/dev/ttyACM1", "/dev/serial/by-id/usb-Espressif_node-if00"),
        ],
        [],
    );
    let boards = vec![esp("/dev/ttyACM0", BRIDGE_MAC), esp("/dev/ttyACM1", NODE_MAC)];
    let named = with_stable_paths(boards, &names);
    assert_eq!(named[0].path, "/dev/serial/by-id/usb-Espressif_bridge-if00");
    assert_eq!(named[1].path, "/dev/serial/by-id/usb-Espressif_node-if00");
}

#[test]
fn a_board_and_a_receiver_are_never_offered_to_each_other() {
    // The partition the two detectors rest on: whoever looks for a bridge writes
    // into what it opens, and a node's port must never be in the other's list.
    let bridge = esp("/dev/ttyACM0", BRIDGE_MAC);
    let puck = candidate("/dev/ttyACM1", Some(0x1546), Some(0x01A7), None);
    assert!(ports::could_be_a_bridge(&bridge));
    assert!(!ports::could_be_a_receiver(&bridge));
    assert!(ports::could_be_a_receiver(&puck));
    assert!(!ports::could_be_a_bridge(&puck));
}

#[test]
fn an_address_selects_the_board_carrying_it_and_answers_with_its_path() {
    let boards = [esp("/dev/ttyACM0", BRIDGE_MAC), esp("/dev/ttyACM1", NODE_MAC)];
    let spec: BridgeSpec = NODE_MAC.parse().expect("parsing cannot fail");
    assert_eq!(serial::resolve_in(&boards, &spec).expect("that board"), "/dev/ttyACM1");
}

#[test]
fn an_address_that_is_not_attached_is_refused_rather_than_answered_with_another() {
    // The failure that matters: answering with the other board would attribute a
    // whole capture to the wrong fleet, and say nothing about having done so.
    let boards = [esp("/dev/ttyACM0", BRIDGE_MAC)];
    let spec: BridgeSpec = NODE_MAC.parse().expect("parsing cannot fail");
    let refused = serial::resolve_in(&boards, &spec).expect_err("no such board");
    assert!(refused.to_string().contains(NODE_MAC), "{refused}");
}

#[test]
fn a_named_path_is_answered_without_consulting_what_is_attached() {
    // Whether it exists is the question the open asks, and the OS answers it
    // better than a list does — so a path is never checked against one.
    let spec: BridgeSpec = "/dev/ttyACM9".parse().expect("parsing cannot fail");
    assert_eq!(serial::resolve_in(&[], &spec).expect("the path as given"), "/dev/ttyACM9");
}
