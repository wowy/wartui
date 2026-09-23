//! Finding an NMEA receiver, against recorded streams rather than a serial port.

use wartui_bridge::ports::{self, ESPRESSIF_VID, PortCandidate, candidate};
use wartui_core::discover::{
    BAUD_LADDER, SENTENCES_ON_A_NAMED_PORT, SENTENCES_TO_BELIEVE, candidates, looks_like_nmea,
    sentences_in, settle,
};

/// What a u-blox 7 says every second, as this bench's puck says it.
const NMEA: &[u8] = b"$GPRMC,183154.00,A,4501.66762,N,09348.12480,W,0.046,,190926,,,D*60\r\n\
                      $GPVTG,,T,,M,0.046,N,0.086,K,D*2A\r\n\
                      $GPGGA,183154.00,4501.66762,N,09348.12480,W,2,09,0.90,283.5,M,-30.4,M,,0000*6B\r\n";

/// The same receiver with no sky: valid sentences, no position.
const NO_LOCK: &[u8] = b"$GPGGA,123520.00,4807.038,N,01131.000,E,0,00,,,M,,M,,*76\r\n\
                         $GPGSV,4,1,13,01,32,265,31,02,22,243,37*7E\r\n";

fn puck(device: &str) -> PortCandidate {
    candidate(device, Some(0x1546), Some(0x01A7), None)
}

fn uart(device: &str) -> PortCandidate {
    candidate(device, Some(0x10C4), Some(0xEA60), None)
}

fn board(device: &str) -> PortCandidate {
    candidate(device, Some(ESPRESSIF_VID), Some(ports::BRIDGE_PID), Some("10:BD:A3:EC:44:C0"))
}

#[test]
fn nmea_detector_recognises_receiver_when_stream_contains_valid_sentences() {
    assert!(looks_like_nmea(NMEA));
}

#[test]
fn nmea_detector_accepts_receiver_when_sentences_valid_without_lock() {
    // Indoors, or thirty seconds into a cold start. It is the receiver either way,
    // and refusing it would mean never finding one in a garage.
    assert!(looks_like_nmea(NO_LOCK));
}

#[test]
fn nmea_detector_rejects_chatter_when_stream_contains_log_lines() {
    let chatter = b"I (443) wifi: mode : sta\r\nI (451) phy_init: phy ver 970\r\n\
                    E (462) radio: no peer\r\n"
        .as_slice();
    assert!(!looks_like_nmea(chatter));
}

#[test]
fn nmea_detector_rejects_stream_when_baud_rate_is_mismatched() {
    // What a 38400 receiver looks like read at 9600: the bytes are mangled, so the
    // checksums do not hold even where a `$` survives.
    let mangled: Vec<u8> = NMEA.iter().map(|byte| byte.rotate_left(3)).collect();
    assert!(!looks_like_nmea(&mangled));
}

#[test]
fn nmea_detector_requires_multiple_sentences_when_evaluating_unnamed_port() {
    // The checksum is an eight-bit XOR, so a device emitting text agrees with one
    // about once in 256 lines. Two in a window is not luck.
    let one = b"$GPGGA,123519.00,4807.038,N,01131.000,E,1,08,0.9,545.4,M,46.9,M,,*69\r\n";
    assert!(!looks_like_nmea(one));
    let two = [one.as_slice(), one.as_slice()].concat();
    assert!(looks_like_nmea(&two));
}

#[test]
fn nmea_detector_rejects_fragment_when_sentence_is_split_across_window() {
    // A probe joins the stream wherever it happens to be, so the first line is
    // normally a fragment. It has to fail rather than be patched up.
    let tail = &NMEA[30..];
    assert!(!looks_like_nmea(&tail[..tail.len().min(60)]));
}

#[test]
fn receiver_candidates_filters_espressif_devices_when_probing_gps() {
    // The one thing that must never be opened by something looking for a GPS: the
    // other half of wartui is transmitting into whatever it opens.
    let attached = vec![board("/dev/ttyACM0"), puck("/dev/ttyACM1")];
    let found = candidates(attached, &[]);
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].path, "/dev/ttyACM1");
}

#[test]
fn receiver_candidates_excludes_reserved_bridge_port_when_enumerating() {
    // `--bridge /dev/ttyUSB0` opens that path whatever it is, so it is named here
    // as well — the vendor filter cannot see it.
    let attached = vec![uart("/dev/ttyUSB0"), puck("/dev/ttyACM1")];
    let found = candidates(attached, &["/dev/ttyUSB0".to_owned()]);
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].path, "/dev/ttyACM1");
}

#[test]
fn receiver_candidates_prioritises_named_gps_when_sorting_devices() {
    let mut named = uart("/dev/ttyUSB1");
    named.product = Some("u-blox 7 - GPS/GNSS Receiver".to_owned());
    let attached = vec![uart("/dev/ttyUSB0"), named];
    let found = candidates(attached, &[]);
    assert_eq!(found[0].path, "/dev/ttyUSB1");
}

#[test]
fn receiver_candidates_includes_bare_uart_when_enumerating_ports() {
    // The common pucks sit behind one, so a hint can order the search and can never
    // decide it. Reading the port is what decides it.
    let found = candidates(vec![uart("/dev/ttyUSB0")], &[]);
    assert_eq!(found.len(), 1);
}

#[test]
fn receiver_ladder_selects_first_matching_baud_when_stepping_through_ladder() {
    let ports = [puck("/dev/ttyACM1")];
    let settled = settle(&ports, &BAUD_LADDER, SENTENCES_TO_BELIEVE, |_, baud| {
        if baud == 38_400 { NMEA.to_vec() } else { b"\xff\xfe junk\r\n".to_vec() }
    });
    assert_eq!(settled, Some(("/dev/ttyACM1", 38_400)));
}

#[test]
fn receiver_ladder_probes_single_baud_when_rate_is_pinned() {
    let ports = [puck("/dev/ttyACM1")];
    let mut tried = Vec::new();
    let settled = settle(&ports, &[9_600], SENTENCES_TO_BELIEVE, |_, baud| {
        tried.push(baud);
        NMEA.to_vec()
    });
    assert_eq!(settled, Some(("/dev/ttyACM1", 9_600)));
    assert_eq!(tried, vec![9_600]);
}

#[test]
fn receiver_ladder_skips_silent_port_when_probing_candidates() {
    let ports = [uart("/dev/ttyUSB0"), puck("/dev/ttyACM1")];
    let settled = settle(&ports, &BAUD_LADDER, SENTENCES_TO_BELIEVE, |path, _| {
        if path == "/dev/ttyACM1" { NMEA.to_vec() } else { Vec::new() }
    });
    assert_eq!(settled, Some(("/dev/ttyACM1", 9_600)));
}

#[test]
fn receiver_ladder_returns_none_when_no_ports_match_nmea() {
    let ports = [uart("/dev/ttyUSB0")];
    assert_eq!(
        settle(&ports, &BAUD_LADDER, SENTENCES_TO_BELIEVE, |_, _| b"nothing to see\r\n".to_vec()),
        None
    );
}

#[test]
fn receiver_ladder_exhausts_all_baud_combinations_when_probing_candidates() {
    let ports = [uart("/dev/ttyUSB0"), uart("/dev/ttyUSB1")];
    let mut tried = 0;
    let settled = settle(&ports, &BAUD_LADDER, SENTENCES_TO_BELIEVE, |_, _| {
        tried += 1;
        Vec::new()
    });
    assert_eq!(settled, None);
    assert_eq!(tried, 2 * BAUD_LADDER.len());
}

#[test]
fn receiver_ladder_settles_on_single_sentence_when_port_is_explicitly_named() {
    // The operator has already said what the device is; only the rate is in
    // question. A receiver emitting a single sentence a second is a real
    // configuration, and asking it for two inside one window would refuse it.
    let one = b"$GPRMC,183154.00,A,4501.66762,N,09348.12480,W,0.046,,190926,,,D*60\r\n";
    assert_eq!(sentences_in(one), 1);
    assert!(!looks_like_nmea(one), "not enough for a port nobody vouched for");

    let ports = [puck("/dev/ttyACM1")];
    let named = settle(&ports, &[9_600], SENTENCES_ON_A_NAMED_PORT, |_, _| one.to_vec());
    assert_eq!(named, Some(("/dev/ttyACM1", 9_600)));
    let unknown = settle(&ports, &[9_600], SENTENCES_TO_BELIEVE, |_, _| one.to_vec());
    assert_eq!(unknown, None);
}
