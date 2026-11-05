//! Naming the boards, and telling one from another.

use wartui_bridge::TransportError;
use wartui_bridge::ports::{
    self, CH343_PID, CH343_VID, ESPRESSIF_VID, PortCandidate, StableNames, candidate,
    is_usable_path, with_stable_paths,
};
use wartui_bridge::serial::{self, BridgeSpec, Probed, discover_bridges, discover_ports};
use wartui_proto::mac;

/// A P4 bridge on this bench, a second P4 bridge beside it, and a node, on adjacent device nodes
/// with nothing but the address to tell them apart.
const BRIDGE_MAC: &str = "10:BD:A3:EC:44:C0";
const OTHER_BRIDGE_MAC: &str = "A0:F2:62:10:9D:24";
const NODE_MAC: &str = "02:00:5E:10:9D:24";

/// The CH343 serial numbers of the two bridges' boards.
const BRIDGE_SERIAL: &str = "5CF7153520";
const OTHER_BRIDGE_SERIAL: &str = "5CF7153999";

fn esp(device: &str, serial: &str) -> PortCandidate {
    candidate(device, Some(ESPRESSIF_VID), Some(0x1001), Some(serial))
}

fn ch343(device: &str, serial: &str) -> PortCandidate {
    candidate(device, Some(CH343_VID), Some(CH343_PID), Some(serial))
}

/// A CH343 that a probe found answering as the bridge `mac`.
fn answered(device: &str, mac: &str) -> Probed {
    Probed { candidate: ch343(device, device), opened: true, mac: mac::parse(mac) }
}

/// A CH343 that a probe opened and heard nothing from.
fn silent(device: &str) -> Probed {
    Probed { candidate: ch343(device, device), opened: true, mac: None }
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
        assert!(ports::is_wartui_board(&port), "{port:?}");
    }
}

#[test]
fn discovery_lists_only_ch343s_when_looking_for_a_bridge() {
    for port in discover_bridges().expect("listing ports should succeed") {
        assert!(ports::is_ch343(&port), "{port:?}");
    }
}

#[test]
fn ports_scanner_extracts_mac_address_when_given_esp32_serial_number() {
    // The whole reason `wartui ports` can say which board is which: an ESP32's
    // serial number is the address its radio transmits from.
    assert_eq!(esp("/dev/ttyACM0", BRIDGE_MAC).mac(), mac::parse(BRIDGE_MAC));
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
    assert_eq!(by_address, BridgeSpec::Mac(mac::parse(BRIDGE_MAC).expect("an address")));
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
fn bridge_spec_parses_last_octets_as_tail_when_given_one_to_five_pairs() {
    // How boards are read aloud: the fleet table and `wartui ports` end with these.
    let short: BridgeSpec = "00:08".parse().expect("parsing cannot fail");
    let longer: BridgeSpec = "87:00:08".parse().expect("parsing cannot fail");
    let lower: BridgeSpec = "44:c0".parse().expect("parsing cannot fail");
    assert_eq!(short, BridgeSpec::Tail(vec![0x00, 0x08]));
    assert_eq!(longer, BridgeSpec::Tail(vec![0x87, 0x00, 0x08]));
    assert_eq!(lower, BridgeSpec::Tail(vec![0x44, 0xC0]));
    assert_eq!(lower.to_string(), "44:C0", "printed back as `wartui ports` spells it");
    // Six pairs is still a whole address.
    let whole: BridgeSpec = "a0:f2:62:87:00:08".parse().expect("parsing cannot fail");
    assert!(matches!(whole, BridgeSpec::Mac(_)));
}

#[test]
fn bridge_spec_parses_text_as_path_when_pairs_are_not_two_hex_digits() {
    // A single digit is not read as a guess at a pair, and nothing that merely
    // contains colons is an address.
    for text in ["0:08", "/dev/ttyACM0", "00:8g", "+0:08", "00:08:", "COM3", "a:b"] {
        let spec: BridgeSpec = text.parse().expect("parsing cannot fail");
        assert_eq!(spec, BridgeSpec::Path(text.to_owned()), "{text}");
    }
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
    // into what it opens, and a node's port must be in neither list.
    let bridge = ch343("/dev/ttyACM0", BRIDGE_SERIAL);
    let node = esp("/dev/ttyACM2", NODE_MAC);
    let puck = candidate("/dev/ttyACM1", Some(0x1546), Some(0x01A7), None);
    assert!(ports::could_be_a_bridge(&bridge));
    assert!(!ports::could_be_a_receiver(&bridge));
    assert!(!ports::could_be_a_bridge(&node));
    assert!(!ports::could_be_a_receiver(&node));
    assert!(ports::could_be_a_receiver(&puck));
    assert!(!ports::could_be_a_bridge(&puck));
}

#[test]
fn serial_discovery_resolves_board_path_when_specified_by_mac_address() {
    let probed = [answered("/dev/ttyACM0", BRIDGE_MAC), answered("/dev/ttyACM1", OTHER_BRIDGE_MAC)];
    let spec: BridgeSpec = OTHER_BRIDGE_MAC.parse().expect("parsing cannot fail");
    assert_eq!(serial::resolve_in(&probed, &spec).expect("that board"), "/dev/ttyACM1");
}

#[test]
fn serial_discovery_rejects_unattached_mac_address_when_resolving_bridge() {
    // The failure that matters: answering with the other board would attribute a
    // whole capture to the wrong fleet, and say nothing about having done so.
    let probed = [answered("/dev/ttyACM0", BRIDGE_MAC), silent("/dev/ttyACM1")];
    let spec: BridgeSpec = OTHER_BRIDGE_MAC.parse().expect("parsing cannot fail");
    let refused = serial::resolve_in(&probed, &spec).expect_err("no such board");
    assert!(matches!(refused, TransportError::NoSuchBridge { .. }), "{refused}");
    assert!(refused.to_string().contains(OTHER_BRIDGE_MAC), "{refused}");
}

#[test]
fn serial_discovery_accepts_raw_path_verbatim_when_specified_by_path() {
    // Whether it exists is the question the open asks, and the OS answers it
    // better than a list does — so a path is never checked against one.
    let spec: BridgeSpec = "/dev/ttyACM9".parse().expect("parsing cannot fail");
    assert_eq!(serial::resolve_in(&[], &spec).expect("the path as given"), "/dev/ttyACM9");
}

#[test]
fn serial_discovery_prioritizes_remembered_serial_when_ordering_probe_candidates() {
    // The ordinary run: one port opened, and it is the right one.
    let boards = [ch343("/dev/ttyACM0", OTHER_BRIDGE_SERIAL), ch343("/dev/ttyACM1", BRIDGE_SERIAL)];
    let order = serial::select(&boards, None, Some(BRIDGE_SERIAL));
    assert_eq!(order[0].path, "/dev/ttyACM1");
    assert_eq!(order[1].path, "/dev/ttyACM0", "and the rest are still swept behind it");
}

#[test]
fn serial_discovery_falls_back_to_full_sweep_when_remembered_serial_is_missing() {
    // Plugging in a different bridge has to work on the first run that sees it,
    // so the serial number orders the sweep and never filters it.
    let boards = [ch343("/dev/ttyACM0", OTHER_BRIDGE_SERIAL)];
    let order = serial::select(&boards, None, Some("0000000000"));
    assert_eq!(order.len(), 1);
    assert_eq!(order[0].path, "/dev/ttyACM0");
}

#[test]
fn serial_discovery_maintains_deterministic_sweep_order_across_runs() {
    // Two runs on one machine must agree about what they tried, or a log from one
    // says nothing about the other.
    let boards = [ch343("/dev/ttyACM2", OTHER_BRIDGE_SERIAL), ch343("/dev/ttyACM0", BRIDGE_SERIAL)];
    let first = serial::select(&boards, None, None);
    let again = serial::select(&boards, None, None);
    assert_eq!(first, again);
    assert_eq!(first[0].path, "/dev/ttyACM0", "and by path, having nothing better");
}

#[test]
fn serial_discovery_restricts_candidate_to_named_path_when_explicitly_given() {
    // Naming a board is naming it: reaching for another would open something the
    // operator did not ask for, and transmit into it.
    let boards = [ch343("/dev/ttyACM0", BRIDGE_SERIAL), ch343("/dev/ttyACM1", OTHER_BRIDGE_SERIAL)];
    let spec: BridgeSpec = "/dev/ttyACM9".parse().expect("parsing cannot fail");
    let order = serial::select(&boards, Some(&spec), Some(BRIDGE_SERIAL));
    assert_eq!(order.len(), 1);
    assert_eq!(order[0].path, "/dev/ttyACM9", "even one that is not attached");
}

#[test]
fn serial_discovery_asks_every_ch343_when_bridge_is_named_by_address() {
    // A CH343 names no board, so finding one by address means asking each.
    let boards = [ch343("/dev/ttyACM1", OTHER_BRIDGE_SERIAL), ch343("/dev/ttyACM0", BRIDGE_SERIAL)];
    let spec: BridgeSpec = BRIDGE_MAC.parse().expect("parsing cannot fail");
    let order = serial::select(&boards, Some(&spec), Some(BRIDGE_SERIAL));
    assert_eq!(order.len(), 2);
    assert_eq!(order[0].path, "/dev/ttyACM0", "the remembered port first");
}

#[test]
fn serial_discovery_resolves_board_path_when_tail_matches_one_board() {
    let probed = [answered("/dev/ttyACM0", BRIDGE_MAC), answered("/dev/ttyACM1", OTHER_BRIDGE_MAC)];
    let spec: BridgeSpec = "9d:24".parse().expect("parsing cannot fail");
    assert_eq!(serial::resolve_in(&probed, &spec).expect("that board"), "/dev/ttyACM1");
    let named = serial::named_board(&probed, &spec).expect("that board");
    assert_eq!(named, &probed[1]);
}

#[test]
fn serial_discovery_rejects_tail_when_no_attached_board_ends_with_it() {
    let probed = [answered("/dev/ttyACM0", BRIDGE_MAC), answered("/dev/ttyACM1", OTHER_BRIDGE_MAC)];
    let spec: BridgeSpec = "00:08".parse().expect("parsing cannot fail");
    let refused = serial::resolve_in(&probed, &spec).expect_err("no such board");
    assert!(matches!(refused, TransportError::NoSuchBridge { .. }), "{refused}");
    assert!(refused.to_string().contains("00:08"), "{refused}");
}

#[test]
fn serial_discovery_refuses_tail_when_several_attached_boards_end_with_it() {
    // Two P4 bridges can share their last octets. Opening either on a guess is
    // transmitting into another fleet's bridge, so nothing is chosen, and the refusal
    // says why rather than claiming the board is not attached.
    let twin = "38:44:BE:12:44:C0";
    let probed = [answered("/dev/ttyACM0", BRIDGE_MAC), answered("/dev/ttyACM1", twin)];
    let spec: BridgeSpec = "44:C0".parse().expect("parsing cannot fail");
    let refused = serial::resolve_in(&probed, &spec).expect_err("ambiguous");
    assert!(matches!(refused, TransportError::AmbiguousName { .. }), "{refused}");
    let message = refused.to_string();
    assert!(message.contains(BRIDGE_MAC) && message.contains(twin), "{message}");
    // One more octet settles it.
    let spec: BridgeSpec = "EC:44:C0".parse().expect("parsing cannot fail");
    assert_eq!(serial::resolve_in(&probed, &spec).expect("that board"), "/dev/ttyACM0");
}

#[test]
fn serial_discovery_refuses_address_when_only_a_node_carries_it() {
    // `wartui reset --bridge` transmits at once to the board this names, so answering
    // with the node would reboot it. A node is never probed, so it names nothing.
    let listed = vec![ch343("/dev/ttyACM0", BRIDGE_SERIAL), esp("/dev/ttyACM1", NODE_MAC)];
    assert_eq!(serial::bridges_among(listed).len(), 1);
    let probed = [answered("/dev/ttyACM0", BRIDGE_MAC)];
    let spec: BridgeSpec = "9D:24".parse().expect("parsing cannot fail");
    let refused = serial::resolve_in(&probed, &spec).expect_err("no bridge");
    assert!(matches!(refused, TransportError::NoSuchBridge { .. }), "{refused}");
}

#[test]
fn serial_discovery_selects_sole_attached_board_when_resetting_without_hint() {
    let boards = [ch343("/dev/ttyACM0", BRIDGE_SERIAL)];
    let only = serial::unambiguous_bridge(&boards, None).expect("the only board");
    assert_eq!(only.path, "/dev/ttyACM0");
}

#[test]
fn serial_discovery_selects_remembered_bridge_when_resetting_among_multiple_boards() {
    let boards = [ch343("/dev/ttyACM0", OTHER_BRIDGE_SERIAL), ch343("/dev/ttyACM1", BRIDGE_SERIAL)];
    let known = serial::unambiguous_bridge(&boards, Some(BRIDGE_SERIAL));
    assert_eq!(known.expect("the remembered board").path, "/dev/ttyACM1");
}

#[test]
fn serial_discovery_refuses_reset_when_multiple_unremembered_bridges_are_present() {
    // With two CH343s and neither remembered, a `Reset` could reboot another fleet's
    // bridge, so this command never guesses.
    let boards = [ch343("/dev/ttyACM0", BRIDGE_SERIAL), ch343("/dev/ttyACM1", OTHER_BRIDGE_SERIAL)];
    let refused = serial::unambiguous_bridge(&boards, None).expect_err("no way to tell");
    assert!(matches!(refused, TransportError::AmbiguousBridge { .. }), "{refused}");
    let message = refused.to_string();
    assert!(message.contains("several CH343s"), "{message}");
    assert!(message.contains("/dev/ttyACM0") && message.contains("/dev/ttyACM1"), "{message}");
    assert!(message.contains("--bridge"), "{message}");
}

#[test]
fn serial_discovery_returns_error_when_resetting_with_no_boards_attached() {
    let refused = serial::unambiguous_bridge(&[], None).expect_err("nothing attached");
    let message = refused.to_string();
    assert!(message.contains("no bridge found"), "{message}");
    // A bridge on another port has no address the OS reports, so a path is all that names it.
    assert!(message.contains("--bridge PATH"), "{message}");
    assert!(message.contains(wartui_bridge::NO_FIRMWARE_P4), "{message}");
}
