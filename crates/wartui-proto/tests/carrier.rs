//! The carrier's I/O expander as the P4 reads it at boot, and the watch for a lost node supply.

use wartui_proto::bus::{NodeBoard, Slot};
use wartui_proto::carrier::{
    ALL_INPUTS, DROP_WINDOW_MS, Decision, DropWatch, decide, port, ports, warm_board,
};

const CARRIERS: [NodeBoard; 3] = [NodeBoard::Carrier6, NodeBoard::Carrier10, NodeBoard::Carrier14];

/// The inputs with every port an input: low where a node socket is wired, high where spare.
fn inputs(board: NodeBoard) -> u16 {
    !ports(board)
}

#[test]
fn carrier_maps_ports_to_slots_when_read_as_one_value() {
    assert_eq!(port(Slot::new(0, 0).unwrap()), 1 << 15, "A1 is P17");
    assert_eq!(port(Slot::new(0, 6).unwrap()), 1 << 9, "A7 is P11");
    assert_eq!(port(Slot::new(1, 0).unwrap()), 1, "B1 is P00");
    assert_eq!(port(Slot::new(1, 6).unwrap()), 1 << 6, "B7 is P06");
    assert_eq!(ports(NodeBoard::Carrier6), 0b1110_0000_0000_0111);
    assert_eq!(ports(NodeBoard::Carrier10), 0b1111_1000_0001_1111);
    assert_eq!(ports(NodeBoard::Carrier14), 0b1111_1110_0111_1111);
    assert_eq!(ports(NodeBoard::NoCarrier), 0);
}

#[test]
fn carrier_powers_every_node_when_boot_is_cold() {
    for board in CARRIERS {
        // A power-up: every port an input, every output high.
        let decision = decide(ALL_INPUTS, 0xFFFF, inputs(board));
        assert_eq!(decision, Decision::Cold { board }, "{board}");
        assert_eq!(decision.power(), ports(board), "{board}");
    }
}

#[test]
fn carrier_powers_nothing_when_warm_boot_finds_all_nodes_on() {
    for board in CARRIERS {
        let config = !ports(board);
        assert_eq!(warm_board(config), Some(board), "{board}");
        // The inputs are not read on this path, so whatever they hold is ignored.
        let decision = decide(config, ports(board), 0);
        assert_eq!(decision, Decision::Warm { board, power: 0 }, "{board}");
    }
}

#[test]
fn carrier_powers_one_node_when_warm_boot_finds_its_output_low() {
    let board = NodeBoard::Carrier10;
    let b2 = port(Slot::new(1, 1).unwrap());
    let decision = decide(!ports(board), ports(board) & !b2, 0);
    assert_eq!(decision, Decision::Warm { board, power: b2 });
    assert_eq!(decision.power(), b2);
}

#[test]
fn carrier_takes_cold_path_when_config_matches_no_carrier() {
    let board = NodeBoard::Carrier14;
    // A port of the 14 left an input, and one config with a spare port made an output.
    for config in [!ports(board) | 1, !ports(NodeBoard::Carrier6) & !(1 << 8), 0, ALL_INPUTS] {
        assert_eq!(warm_board(config), None, "{config:#06x}");
        assert_eq!(decide(config, 0, inputs(board)), Decision::Cold { board }, "{config:#06x}");
    }
}

#[test]
fn carrier_ignores_unwired_ports_when_reading_inputs() {
    // P07 and P10 wire no node on any carrier, so their level names nothing.
    let board = NodeBoard::Carrier6;
    let input = inputs(board) & !(1 << 7) & !(1 << 8);
    assert_eq!(decide(ALL_INPUTS, 0xFFFF, input), Decision::Cold { board });
}

#[test]
fn carrier_reports_broken_board_when_inputs_match_no_carrier() {
    // A6 reads wired on a ten, and B1 reads spare on a six.
    let broken = [
        inputs(NodeBoard::Carrier10) & !port(Slot::new(0, 5).unwrap()),
        inputs(NodeBoard::Carrier6) | port(Slot::new(1, 0).unwrap()),
        0xFFFF,
    ];
    for input in broken {
        let decision = decide(ALL_INPUTS, 0xFFFF, input);
        assert_eq!(decision, Decision::Broken, "{input:#06x}");
        assert_eq!(decision.power(), 0);
    }
}

/// The present-mask bit for `bus` slot `index`.
fn bit(bus: u8, index: u8) -> u16 {
    1 << Slot::new(bus, index).unwrap().as_u8()
}

/// A watch that has seen `present` arrive at time 0.
fn watching(present: u16) -> DropWatch {
    let mut watch = DropWatch::new();
    assert_eq!(watch.note(present, 0, 0), None);
    watch
}

#[test]
fn drop_watch_reports_supply_loss_when_every_node_drops_within_window() {
    let (a1, b1, a2) = (bit(0, 0), bit(1, 0), bit(0, 1));
    let mut watch = watching(a1 | b1 | a2);
    assert_eq!(watch.note(b1 | a2, 0, 10_000), None);
    assert_eq!(watch.note(a2, 0, 10_700), None);
    assert_eq!(watch.note(0, 0, 10_000 + DROP_WINDOW_MS), Some(a1 | b1 | a2));
}

#[test]
fn drop_watch_reports_nothing_when_a_node_stays_present() {
    let (a1, b1) = (bit(0, 0), bit(1, 0));
    let mut watch = watching(a1 | b1);
    assert_eq!(watch.note(b1, 0, 10_000), None);
    assert_eq!(watch.note(b1, 0, 10_500), None);
}

#[test]
fn drop_watch_reports_nothing_when_no_node_was_ever_present() {
    let mut watch = DropWatch::new();
    assert_eq!(watch.note(0, 0, 0), None);
    assert_eq!(watch.note(0, 0, 10_000), None);
}

#[test]
fn drop_watch_reports_nothing_when_lone_node_drops() {
    // One node has no inrush to stagger, and its drop may be a crash or an operator pulling it.
    let a1 = bit(0, 0);
    let mut watch = watching(a1);
    assert_eq!(watch.note(0, 0, 10_000), None);
}

#[test]
fn drop_watch_reports_nothing_when_dropping_nodes_are_power_cycled() {
    let (a1, b1) = (bit(0, 0), bit(1, 0));
    let mut watch = watching(a1 | b1);
    assert_eq!(watch.note(b1, a1 | b1, 10_000), None);
    assert_eq!(watch.note(0, a1 | b1, 10_500), None);
}

#[test]
fn drop_watch_leaves_out_node_when_it_drops_just_after_its_power_cycle() {
    // A node is absent only some time after it loses power, which may be after its cycle ends.
    let (a1, b1, a2) = (bit(0, 0), bit(1, 0), bit(0, 1));
    let mut watch = watching(a1 | b1 | a2);
    watch.cycled(a1, 10_000);
    assert_eq!(watch.note(b1 | a2, 0, 10_000 + DROP_WINDOW_MS), None);
    assert_eq!(watch.note(0, 0, 10_500 + DROP_WINDOW_MS), Some(b1 | a2));
}

#[test]
fn drop_watch_counts_node_when_its_power_cycle_ended_long_before() {
    let (a1, b1) = (bit(0, 0), bit(1, 0));
    let mut watch = watching(a1 | b1);
    watch.cycled(a1, 10_000);
    assert_eq!(watch.note(b1, 0, 10_001 + DROP_WINDOW_MS), None);
    assert_eq!(watch.note(0, 0, 10_500 + DROP_WINDOW_MS), Some(a1 | b1));
}

#[test]
fn drop_watch_reports_supply_loss_when_others_drop_beside_a_cycled_node() {
    let (a1, b1, a2) = (bit(0, 0), bit(1, 0), bit(0, 1));
    let mut watch = watching(a1 | b1 | a2);
    assert_eq!(watch.note(b1 | a2, a1, 10_000), None);
    assert_eq!(watch.note(0, a1, 10_500), Some(b1 | a2));
}

#[test]
fn drop_watch_reports_only_late_drops_when_earlier_ones_fall_outside_window() {
    // Once A1 had gone, B1 and A2 alone were present, and both then dropped.
    let (a1, b1, a2) = (bit(0, 0), bit(1, 0), bit(0, 1));
    let mut watch = watching(a1 | b1 | a2);
    assert_eq!(watch.note(b1 | a2, 0, 10_000), None);
    assert_eq!(watch.note(a2, 0, 10_001 + DROP_WINDOW_MS), None);
    assert_eq!(watch.note(0, 0, 10_500 + DROP_WINDOW_MS), Some(b1 | a2));
}

#[test]
fn drop_watch_reports_nothing_when_one_node_drops_long_after_the_rest() {
    let (a1, b1) = (bit(0, 0), bit(1, 0));
    let mut watch = watching(a1 | b1);
    assert_eq!(watch.note(b1, 0, 10_000), None);
    assert_eq!(watch.note(0, 0, 10_001 + DROP_WINDOW_MS), None);
}

#[test]
fn drop_watch_reports_nothing_when_last_node_cycled_after_an_old_drop() {
    let (a1, b1, a2) = (bit(0, 0), bit(1, 0), bit(0, 1));
    let mut watch = watching(a1 | b1 | a2);
    assert_eq!(watch.note(b1, 0, 10_000), None);
    assert_eq!(watch.note(0, b1, 30_000), None);
}

#[test]
fn drop_watch_reports_supply_loss_once_when_set_stays_empty() {
    let (a1, b1) = (bit(0, 0), bit(1, 0));
    let mut watch = watching(a1 | b1);
    assert_eq!(watch.note(0, 0, 10_000), Some(a1 | b1));
    assert_eq!(watch.note(0, 0, 10_100), None);
}
