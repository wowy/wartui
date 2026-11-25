//! The sweep, the probe, the `Identify` cadence, the keepalive and the command tally, run against
//! [`FakePorts`] on real threads and real time.
//!
//! The reader and writer are real threads, so the clock is never paused. The cadence is short,
//! every wait is generous, and each test is built so that a board missing its window leads to a
//! retry rather than another outcome: the assertions are on order and "at least" counts, so a
//! loaded machine can slow a test without failing it.

use std::io::ErrorKind;
use std::time::Duration;

use wartui_proto::link::HostToBridge;
use wartui_proto::mac::{self, Mac};

use super::fake::{Behavior, FakePorts};
use super::{Cadence, SerialTransport, probe};
use crate::ports::{
    CH343_PID, CH343_VID, ESPRESSIF_VID, PortCandidate, SERIAL_JTAG_PID, candidate,
};
use crate::remember::BridgeMemory;
use crate::spec::BridgeSpec;
use crate::{BridgeInfo, LinkEvent, LinkHandle, TransportError};

/// One tick of the test cadence: a probe with company lasts three, a settle twelve.
const TICK: Duration = Duration::from_millis(30);

/// The longest any test waits on one event.
const WITHIN: Duration = Duration::from_secs(10);

/// CH343 serial numbers, one per board.
const SERIAL_A: &str = "AAAAAAAAAA";
const SERIAL_B: &str = "BBBBBBBBBB";

const ACM0: &str = "/dev/ttyACM0";
const ACM1: &str = "/dev/ttyACM1";
const ACM2: &str = "/dev/ttyACM2";

/// A locally administered bridge address ending in `last`.
const fn bridge_mac(last: u8) -> Mac {
    [0x02, 0x00, 0x5E, 0x10, 0x44, last]
}

fn ch343(path: &str, serial: &str) -> PortCandidate {
    candidate(path, Some(CH343_VID), Some(CH343_PID), Some(serial))
}

fn bridge(last: u8) -> Behavior {
    Behavior::Bridge { mac: bridge_mac(last), ignore: 0 }
}

fn cadence() -> Cadence {
    Cadence { identify_every: TICK, keepalive: Duration::from_secs(10), probe_wait: TICK * 6 }
}

fn start(transport: SerialTransport) -> LinkHandle {
    transport.cadence(cadence()).start().expect("the supervisor starts")
}

/// The next `Connected`, skipping everything else.
async fn connected(handle: &mut LinkHandle) -> BridgeInfo {
    let wait = async {
        loop {
            match handle.recv().await {
                Some(LinkEvent::Connected(info)) => return info,
                Some(_) => {}
                None => panic!("the link ended"),
            }
        }
    };
    tokio::time::timeout(WITHIN, wait).await.expect("connected in time")
}

/// The next `Disconnected` reason, skipping everything else.
async fn disconnected(handle: &mut LinkHandle) -> String {
    let wait = async {
        loop {
            match handle.recv().await {
                Some(LinkEvent::Disconnected { reason, .. }) => return reason,
                Some(_) => {}
                None => panic!("the link ended"),
            }
        }
    };
    tokio::time::timeout(WITHIN, wait).await.expect("disconnected in time")
}

/// The first `Disconnected` reason `wanted` accepts, skipping every other event and reason.
async fn disconnected_until(handle: &mut LinkHandle, wanted: impl Fn(&str) -> bool) -> String {
    let wait = async {
        loop {
            let reason = disconnected(handle).await;
            if wanted(&reason) {
                return reason;
            }
        }
    };
    tokio::time::timeout(WITHIN * 3, wait).await.expect("the reason in time")
}

/// Wait until `done` holds, polling.
async fn until(mut done: impl FnMut() -> bool) {
    let wait = async {
        while !done() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    };
    tokio::time::timeout(WITHIN, wait).await.expect("in time");
}

fn identifies(fake: &FakePorts, path: &str) -> usize {
    fake.decoded(path).iter().filter(|cmd| **cmd == HostToBridge::Identify).count()
}

#[tokio::test]
async fn serial_sweep_opens_no_node_when_looking_for_bridge() {
    // Finding a bridge writes into what is opened, so neither the node nor the GPS puck is.
    let fake = FakePorts::default();
    fake.attach(
        candidate(ACM0, Some(ESPRESSIF_VID), Some(SERIAL_JTAG_PID), Some("02:00:5E:10:9D:24")),
        Behavior::Silent,
    )
    .attach(candidate(ACM1, Some(0x1546), Some(0x01A7), None), Behavior::Silent)
    .attach(ch343(ACM2, SERIAL_A), bridge(1));
    let mut handle = start(SerialTransport::new(fake.ports()));

    assert_eq!(connected(&mut handle).await.mac, bridge_mac(1));
    let opened = fake.opened();
    assert!(!opened.is_empty() && opened.iter().all(|path| path == ACM2), "{opened:?}");
}

#[tokio::test]
async fn serial_sweep_connects_to_next_board_when_first_is_silent() {
    let fake = FakePorts::default();
    fake.attach(ch343(ACM0, SERIAL_A), Behavior::Silent).attach(ch343(ACM1, SERIAL_B), bridge(2));
    let mut handle = start(SerialTransport::new(fake.ports()));

    assert_eq!(connected(&mut handle).await.mac, bridge_mac(2));
    assert_eq!(fake.opened()[..2], [ACM0, ACM1]);
}

#[tokio::test]
async fn serial_sweep_opens_remembered_port_first_when_two_boards_are_attached() {
    let fake = FakePorts::default();
    // The other board is silent, so a window the remembered one misses costs a retry, never a
    // connection elsewhere.
    fake.attach(ch343(ACM0, SERIAL_A), Behavior::Silent).attach(ch343(ACM1, SERIAL_B), bridge(2));
    let memory = BridgeMemory::holding(Some(bridge_mac(2))).on_port(Some(SERIAL_B.to_owned()));
    let mut handle = start(SerialTransport::new(fake.ports()).remember_in(memory));

    assert_eq!(connected(&mut handle).await.mac, bridge_mac(2));
    assert_eq!(fake.opened()[0], ACM1);
}

#[tokio::test]
async fn serial_sweep_passes_over_board_when_it_talks_and_none_of_it_is_the_protocol() {
    let fake = FakePorts::default();
    fake.attach(ch343(ACM0, SERIAL_A), Behavior::Garbage)
        .attach(ch343(ACM1, SERIAL_B), Behavior::Silent);
    let mut handle = start(SerialTransport::new(fake.ports()).reconnect_delay(TICK));

    // A pass that read fewer than the garble limit before the board's window closed says it
    // was silent instead, and the next pass reads more.
    let talking = "/dev/ttyACM0 is talking, and none of it is the link protocol";
    disconnected_until(&mut handle, |reason| reason.contains(talking)).await;
    assert_eq!(fake.opened()[..2], [ACM0, ACM1]);
}

#[tokio::test]
async fn serial_sweep_connects_to_named_board_when_named_by_address() {
    let fake = FakePorts::default();
    fake.attach(ch343(ACM0, SERIAL_A), bridge(1)).attach(ch343(ACM1, SERIAL_B), bridge(2));
    let spec = BridgeSpec::Mac(bridge_mac(2));
    let mut handle = start(SerialTransport::with_spec(fake.ports(), spec));

    assert_eq!(connected(&mut handle).await.mac, bridge_mac(2));
}

#[tokio::test]
async fn serial_sweep_reports_no_such_bridge_when_named_address_answers_nowhere() {
    let fake = FakePorts::default();
    fake.attach(ch343(ACM0, SERIAL_A), bridge(1)).attach(ch343(ACM1, SERIAL_B), bridge(2));
    let spec = BridgeSpec::Mac(bridge_mac(9));
    let wanted = TransportError::NoSuchBridge { spec: spec.to_string() }.to_string();
    let mut handle = start(SerialTransport::with_spec(fake.ports(), spec));

    assert_eq!(disconnected(&mut handle).await, wanted);
}

#[tokio::test]
async fn serial_sweep_waits_for_settled_board_when_it_is_unplugged() {
    // Reaching for the other bridge would have this run quietly driving another fleet.
    let fake = FakePorts::default();
    fake.attach(ch343(ACM0, SERIAL_A), bridge(1)).attach(ch343(ACM1, SERIAL_B), bridge(2));
    let mut handle = start(SerialTransport::new(fake.ports()).reconnect_delay(TICK));
    // Either may answer first, if the first misses its window.
    let settled = connected(&mut handle).await.mac;
    let path = if settled == bridge_mac(1) { ACM0 } else { ACM1 };
    let opened = fake.opened();

    fake.unplug(path);
    let gone = format!("the bridge {} is no longer attached", mac::full(&settled));
    disconnected_until(&mut handle, |reason| reason == gone).await;
    assert_eq!(fake.opened(), opened, "nothing more was opened");
}

#[tokio::test]
async fn serial_sweep_forgets_remembered_board_when_it_opens_and_stays_silent() {
    let fake = FakePorts::default();
    fake.attach(ch343(ACM0, SERIAL_A), Behavior::Silent);
    let memory = BridgeMemory::holding(Some(bridge_mac(1))).on_port(Some(SERIAL_A.to_owned()));
    let mut handle = start(SerialTransport::new(fake.ports()).remember_in(memory.clone()));

    disconnected(&mut handle).await;
    assert_eq!((memory.recall(), memory.recall_serial()), (None, None));
}

#[tokio::test]
async fn serial_sweep_keeps_remembered_board_when_its_port_will_not_open() {
    // It said nothing because nothing was asked of it.
    let fake = FakePorts::default();
    fake.attach(ch343(ACM0, SERIAL_A), Behavior::Refused(ErrorKind::ResourceBusy));
    let memory = BridgeMemory::holding(Some(bridge_mac(1))).on_port(Some(SERIAL_A.to_owned()));
    let mut handle = start(SerialTransport::new(fake.ports()).remember_in(memory.clone()));

    disconnected(&mut handle).await;
    assert_eq!(memory.recall(), Some(bridge_mac(1)));
    assert_eq!(memory.recall_serial().as_deref(), Some(SERIAL_A));
}

#[tokio::test]
async fn serial_link_reports_no_bridge_found_when_nothing_is_attached() {
    let fake = FakePorts::default();
    let mut handle = start(SerialTransport::new(fake.ports()));

    assert_eq!(disconnected(&mut handle).await, TransportError::NoBridgeFound.to_string());
    assert!(fake.opened().is_empty());
}

#[tokio::test]
async fn serial_link_reports_listing_failure_when_ports_cannot_be_listed() {
    let fake = FakePorts::default();
    fake.attach(ch343(ACM0, SERIAL_A), bridge(1)).fail_list();
    let mut handle = start(SerialTransport::new(fake.ports()));

    assert_eq!(
        disconnected(&mut handle).await,
        "could not list serial ports: the fake will not list"
    );
    assert!(fake.opened().is_empty());
}

#[tokio::test]
async fn serial_link_words_refusal_as_forbidden_when_open_is_denied() {
    // "No bridge found" is the wrong thing to say about a board that was found and refused.
    let fake = FakePorts::default();
    fake.attach(ch343(ACM0, SERIAL_A), Behavior::Refused(ErrorKind::PermissionDenied));
    let mut handle = start(SerialTransport::new(fake.ports()));

    let wanted = TransportError::Forbidden { port: ACM0.to_owned() }.to_string();
    assert_eq!(disconnected(&mut handle).await, wanted);
}

#[tokio::test]
async fn serial_link_asks_ch343_again_when_bridge_misses_first_identify() {
    // A P4 booting behind its CH343 loses what reaches its bootloader.
    let fake = FakePorts::default();
    fake.attach(ch343(ACM0, SERIAL_A), Behavior::Bridge { mac: bridge_mac(1), ignore: 2 });
    let mut handle = start(SerialTransport::new(fake.ports()));

    assert_eq!(connected(&mut handle).await.mac, bridge_mac(1));
    assert!(identifies(&fake, ACM0) >= 3, "{:?}", fake.decoded(ACM0));
}

#[tokio::test]
async fn serial_link_asks_once_when_named_path_is_not_a_ch343() {
    // It may be a node, which absorbs one packet and wedges `close` on the next.
    let fake = FakePorts::default();
    fake.attach(
        candidate(ACM0, Some(ESPRESSIF_VID), Some(SERIAL_JTAG_PID), None),
        Behavior::Silent,
    );
    let spec = BridgeSpec::Path(ACM0.to_owned());
    let transport =
        SerialTransport::with_spec(fake.ports(), spec).reconnect_delay(Duration::from_secs(60));
    let mut handle = start(transport);

    let reason = disconnected(&mut handle).await;
    assert!(reason.starts_with("nothing on /dev/ttyACM0 answered the link protocol"), "{reason}");
    // The next attempt waits a minute, longer than the test, so this is one connection's.
    assert_eq!(fake.opened(), [ACM0]);
    assert_eq!(fake.decoded(ACM0), [HostToBridge::Identify]);
}

#[tokio::test]
async fn serial_link_sends_get_status_when_proven_connection_is_quiet() {
    let fake = FakePorts::default();
    fake.attach(ch343(ACM0, SERIAL_A), bridge(1));
    let quick = Cadence { keepalive: TICK * 3, ..cadence() };
    let mut handle = SerialTransport::new(fake.ports()).cadence(quick).start().unwrap();
    connected(&mut handle).await;

    until(|| fake.decoded(ACM0).contains(&HostToBridge::GetStatus)).await;
    assert_eq!(handle.queued(), 0, "nothing was sent on the handle");
}

#[tokio::test]
async fn serial_link_counts_command_done_when_port_takes_it() {
    let fake = FakePorts::default();
    fake.attach(ch343(ACM0, SERIAL_A), bridge(1));
    let mut handle = start(SerialTransport::new(fake.ports()));
    connected(&mut handle).await;

    handle.send_bulk(HostToBridge::GetStatus).expect("queued");
    until(|| handle.written_count() >= 1).await;
    assert_eq!(handle.written_count(), 1);
    assert!(fake.decoded(ACM0).contains(&HostToBridge::GetStatus), "{:?}", fake.decoded(ACM0));
}

#[tokio::test]
async fn serial_probe_names_bridge_when_ready_answers() {
    let fake = FakePorts::default();
    fake.attach(ch343(ACM0, SERIAL_A), bridge(1));

    let probed = probe(&fake.ports(), ch343(ACM0, SERIAL_A)).await;
    assert!(probed.opened);
    assert_eq!(probed.mac, Some(bridge_mac(1)));
}

#[tokio::test]
async fn serial_probe_reports_port_unopened_when_open_is_refused() {
    let fake = FakePorts::default();
    fake.attach(ch343(ACM0, SERIAL_A), Behavior::Refused(ErrorKind::PermissionDenied));

    let probed = probe(&fake.ports(), ch343(ACM0, SERIAL_A)).await;
    assert!(!probed.opened);
    assert_eq!(probed.mac, None);
}
