//! The desktop's serial ports: listing them through the shared predicates, and naming each by
//! the most stable path udev offers.

use wartui_bridge::ports::{self, ESPRESSIF_VID, PortCandidate, candidate, is_usable_path};
use wartui_bridge::serial::{discover_bridges, discover_ports};
use wartui_desktop::SerialPorts;
use wartui_desktop::serial::{StableNames, with_stable_paths};

/// A P4 bridge and a node, with nothing but the address to tell them apart.
const BRIDGE_MAC: &str = "02:00:5E:10:44:C0";
const NODE_MAC: &str = "02:00:5E:10:9D:24";

fn esp(device: &str, serial: &str) -> PortCandidate {
    candidate(device, Some(ESPRESSIF_VID), Some(0x1001), Some(serial))
}

fn link(device: &str, name: &str) -> (String, String) {
    (device.to_owned(), name.to_owned())
}

#[test]
fn discovery_returns_empty_list_without_error_when_no_devices_are_attached() {
    // An empty list is the normal state, not an error; the transport retries.
    let found = discover_ports(&SerialPorts).expect("listing ports should succeed");
    for port in found {
        assert!(is_usable_path(&port.path));
        assert!(ports::is_wartui_board(&port), "{port:?}");
    }
}

#[test]
fn discovery_lists_only_ch343s_when_looking_for_a_bridge() {
    for port in discover_bridges(&SerialPorts).expect("listing ports should succeed") {
        assert!(ports::is_ch343(&port), "{port:?}");
    }
}

#[test]
fn ports_scanner_prefers_by_id_symlink_when_stable_path_is_available() {
    // The path `wartui ports` prints is the one worth copying into `--bridge`: it
    // survives the re-enumeration that moves ttyACM0 to ttyACM1.
    let by_id = "/dev/serial/by-id/usb-Espressif_USB_JTAG_serial_debug_unit_02:00:5E:10:44:C0-if00";
    let names = StableNames::from_pairs(
        [link("/dev/ttyACM0", by_id)],
        [link("/dev/ttyACM0", "/dev/serial/by-path/pci-0000:00:14.0-usb-0:3.2:1.0")],
    );
    let named = with_stable_paths(vec![esp("/dev/ttyACM0", BRIDGE_MAC)], &names);
    assert_eq!(named[0].path, by_id);
    assert_eq!(named[0].device, "/dev/ttyACM0", "the device node is kept for comparing");
}

#[test]
fn ports_scanner_preserves_device_path_when_symlink_is_unavailable() {
    let named = with_stable_paths(vec![esp("/dev/ttyACM0", BRIDGE_MAC)], &StableNames::default());
    assert_eq!(named[0].path, "/dev/ttyACM0");
}

#[test]
fn ports_scanner_falls_back_to_by_path_when_by_id_is_ambiguous() {
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
fn ports_scanner_keeps_distinct_by_id_paths_when_boards_have_unique_macs() {
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
