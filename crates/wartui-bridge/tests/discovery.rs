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
fn discovery_filters_macos_tty_aliases_when_probing_serial_candidates() {
    // Opening the /dev/tty.* side blocks on carrier detect, which looks exactly like
    // a hung bridge.
    assert!(!is_usable_path("/dev/tty.usbmodem14201"));
    assert!(is_usable_path("/dev/cu.usbmodem14201"));
    assert!(is_usable_path("/dev/ttyACM0"), "Linux ttyACM devices are fine");
    assert!(is_usable_path("COM3"));
}

#[test]
fn discovery_returns_empty_list_without_error_when_no_devices_are_attached() {
    // An empty list is the normal state, not an error; the transport retries.
    let found = discover_ports().expect("listing ports should succeed");
    for port in found {
        assert!(is_usable_path(&port.path));
        assert_eq!(port.vid, Some(ESPRESSIF_VID));
    }
}

#[test]
fn ports_scanner_extracts_mac_address_when_given_esp32_serial_number() {
    // The whole reason `wartui ports` can say which board is which: an ESP32's
    // serial number is the address its radio transmits from.
    assert_eq!(esp("/dev/ttyACM0", BRIDGE_MAC).mac(), ports::parse_mac(BRIDGE_MAC));
    assert_eq!(esp("/dev/ttyACM0", BRIDGE_MAC).mac().expect("an address")[0], 0x10);
}

#[test]
fn ports_scanner_yields_none_for_mac_when_given_manufacturing_serial() {
    // Everything else on the bus. `ports` says so rather than inventing one.
    let puck = candidate("/dev/ttyACM1", Some(0x1546), Some(0x01A7), Some("0001"));
    assert_eq!(puck.mac(), None);
    assert_eq!(candidate("/dev/ttyACM1", Some(0x1546), Some(0x01A7), None).mac(), None);
}

#[test]
fn bridge_spec_distinguishes_mac_address_from_file_path_when_parsed() {
    let by_address: BridgeSpec = BRIDGE_MAC.parse().expect("parsing cannot fail");
    let by_path: BridgeSpec = "/dev/ttyACM0".parse().expect("parsing cannot fail");
    assert_eq!(by_address, BridgeSpec::Mac(ports::parse_mac(BRIDGE_MAC).expect("an address")));
    assert_eq!(by_path, BridgeSpec::Path("/dev/ttyACM0".to_owned()));
    // And a spec prints back in the form it was given, so advice can quote it.
    assert_eq!(by_address.to_string(), BRIDGE_MAC);
    assert_eq!(by_path.to_string(), "/dev/ttyACM0");
}

#[test]
fn bridge_spec_parses_path_containing_colons_as_path_when_parsed() {
    // Nothing rules out a device node with colons in it, and reading one as an
    // address would open a board the operator never named.
    let spec: BridgeSpec = "/dev/serial/by-id/usb-Espressif_10:BD:A3:EC:44:C0-if00"
        .parse()
        .expect("parsing cannot fail");
    assert!(matches!(spec, BridgeSpec::Path(_)));
}

#[test]
fn ports_scanner_prefers_by_id_symlink_when_stable_path_is_available() {
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

#[test]
fn ports_scanner_strictly_separates_bridge_and_gps_devices_when_filtering() {
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
fn serial_discovery_resolves_board_path_when_specified_by_mac_address() {
    let boards = [esp("/dev/ttyACM0", BRIDGE_MAC), esp("/dev/ttyACM1", NODE_MAC)];
    let spec: BridgeSpec = NODE_MAC.parse().expect("parsing cannot fail");
    assert_eq!(serial::resolve_in(&boards, &spec).expect("that board"), "/dev/ttyACM1");
}

#[test]
fn serial_discovery_rejects_unattached_mac_address_when_resolving_bridge() {
    // The failure that matters: answering with the other board would attribute a
    // whole capture to the wrong fleet, and say nothing about having done so.
    let boards = [esp("/dev/ttyACM0", BRIDGE_MAC)];
    let spec: BridgeSpec = NODE_MAC.parse().expect("parsing cannot fail");
    let refused = serial::resolve_in(&boards, &spec).expect_err("no such board");
    assert!(refused.to_string().contains(NODE_MAC), "{refused}");
}

#[test]
fn serial_discovery_accepts_raw_path_verbatim_when_specified_by_path() {
    // Whether it exists is the question the open asks, and the OS answers it
    // better than a list does — so a path is never checked against one.
    let spec: BridgeSpec = "/dev/ttyACM9".parse().expect("parsing cannot fail");
    assert_eq!(serial::resolve_in(&[], &spec).expect("the path as given"), "/dev/ttyACM9");
}

#[test]
fn serial_discovery_prioritises_remembered_mac_when_ordering_probe_candidates() {
    // The ordinary run: one port opened, and it is the right one.
    let boards = [esp("/dev/ttyACM0", NODE_MAC), esp("/dev/ttyACM1", BRIDGE_MAC)];
    let order = serial::select(&boards, None, ports::parse_mac(BRIDGE_MAC));
    assert_eq!(order[0].path, "/dev/ttyACM1");
    assert_eq!(order[1].path, "/dev/ttyACM0", "and the rest are still swept behind it");
}

#[test]
fn serial_discovery_falls_back_to_full_sweep_when_remembered_mac_is_missing() {
    // Plugging in a different bridge has to work on the first run that sees it,
    // so the address orders the sweep and never filters it.
    let boards = [esp("/dev/ttyACM0", NODE_MAC)];
    let order = serial::select(&boards, None, ports::parse_mac("AA:BB:CC:DD:EE:FF"));
    assert_eq!(order.len(), 1);
    assert_eq!(order[0].path, "/dev/ttyACM0");
}

#[test]
fn serial_discovery_maintains_deterministic_sweep_order_across_runs() {
    // Two runs on one machine must agree about what they tried, or a log from one
    // says nothing about the other.
    let boards = [esp("/dev/ttyACM2", NODE_MAC), esp("/dev/ttyACM0", BRIDGE_MAC)];
    let first = serial::select(&boards, None, None);
    let again = serial::select(&boards, None, None);
    assert_eq!(first, again);
    assert_eq!(first[0].path, "/dev/ttyACM0", "and by path, having nothing better");
}

#[test]
fn serial_discovery_restricts_candidate_to_named_path_when_explicitly_given() {
    // Naming a board is naming it: reaching for another would open something the
    // operator did not ask for, and transmit into it.
    let boards = [esp("/dev/ttyACM0", BRIDGE_MAC), esp("/dev/ttyACM1", NODE_MAC)];
    let spec: BridgeSpec = "/dev/ttyACM9".parse().expect("parsing cannot fail");
    let order = serial::select(&boards, Some(&spec), ports::parse_mac(BRIDGE_MAC));
    assert_eq!(order.len(), 1);
    assert_eq!(order[0].path, "/dev/ttyACM9", "even one that is not attached");
}

#[test]
fn serial_discovery_returns_empty_candidates_when_named_mac_is_not_attached() {
    let boards = [esp("/dev/ttyACM0", BRIDGE_MAC)];
    let spec: BridgeSpec = NODE_MAC.parse().expect("parsing cannot fail");
    assert!(serial::select(&boards, Some(&spec), None).is_empty());
}

#[test]
fn serial_discovery_selects_sole_attached_board_when_resetting_without_hint() {
    let boards = [esp("/dev/ttyACM0", BRIDGE_MAC)];
    assert_eq!(serial::unambiguous_bridge(&boards, None).expect("the only board"), "/dev/ttyACM0");
}

#[test]
fn serial_discovery_selects_remembered_bridge_when_resetting_among_multiple_boards() {
    let boards = [esp("/dev/ttyACM0", NODE_MAC), esp("/dev/ttyACM1", BRIDGE_MAC)];
    let known = serial::unambiguous_bridge(&boards, ports::parse_mac(BRIDGE_MAC));
    assert_eq!(known.expect("the remembered board"), "/dev/ttyACM1");
}

#[test]
fn serial_discovery_refuses_reset_when_multiple_unremembered_boards_are_present() {
    // A `Reset` sent to a node reboots the node and costs it the addresses it was
    // holding back, so this command never sweeps.
    let boards = [esp("/dev/ttyACM0", BRIDGE_MAC), esp("/dev/ttyACM1", NODE_MAC)];
    let refused = serial::unambiguous_bridge(&boards, None).expect_err("no way to tell");
    let message = refused.to_string();
    assert!(message.contains(BRIDGE_MAC) && message.contains(NODE_MAC), "{message}");
    assert!(message.contains("--bridge"), "{message}");
}

#[test]
fn serial_discovery_returns_error_when_resetting_with_no_boards_attached() {
    let refused = serial::unambiguous_bridge(&[], None).expect_err("nothing attached");
    assert!(refused.to_string().contains("no bridge found"), "{refused}");
}
