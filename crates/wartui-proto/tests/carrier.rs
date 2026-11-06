//! The carrier's I/O expander as the P4 reads it at boot.

use wartui_proto::bus::{NodeBoard, Slot};
use wartui_proto::carrier::{ALL_INPUTS, Decision, decide, port, ports, warm_board};

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
    assert_eq!(ports(NodeBoard::Bench), 0);
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
