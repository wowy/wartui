//! The driver's finding, settling, connecting, serving and closing, run against [`FakeRadio`] on a
//! paused clock, so each test reads the exact time the driver took.

use std::time::Duration;

use tokio::time::Instant;
use wartui_proto::bus::Slot;
use wartui_proto::link::{BridgeToHost, HostToBridge, LogStr, ToNodeBody, USB_TOOK_BRIDGE};
use wartui_proto::mac::{self, Mac};

use super::fake::{
    Call, Connects, Did, Discovers, FakeBridge, FakeRadio, LINUX, MACOS, Subscribes, Writes, frame,
};
use super::{
    ALIVE_EVERY, Advertiser, BleTransport, CONNECT_WITHIN, IDENTIFY_EVERY, LIST_WITHIN, LISTEN_FOR,
    MAX_RETRY_DELAY, PAIRING_NOTICE_AFTER, RECALL_WITHIN, SCAN_FOR, SCAN_START_WITHIN,
    WRITE_WITHIN, WriteKind, connect_timeout_text, find_advertiser, pairing_notice, ready_notice,
    survey,
};
use crate::pipe::{RadioError, RadioErrorKind, Sighting};
use crate::remember::BridgeMemory;
use crate::spec::BridgeSpec;
use crate::{BridgeInfo, DisconnectCause, LinkEvent, LinkHandle, TransportError};

/// The longest any test waits on one event, on the paused clock.
const WITHIN: Duration = Duration::from_secs(600);

const PEER_A: &str = "peer-a";
const PEER_B: &str = "peer-b";

/// A locally administered bridge address ending in `44:last`.
const fn bridge_mac(last: u8) -> Mac {
    [0x02, 0x00, 0x5E, 0x10, 0x44, last]
}

const MAC_A: Mac = bridge_mac(0xC0);
const MAC_B: Mac = bridge_mac(0xC1);

fn spec(text: &str) -> BridgeSpec {
    text.parse().unwrap_or_else(|never| match never {})
}

/// Bridge A, `44:C0`.
fn bridge_a() -> FakeBridge {
    FakeBridge::new(PEER_A, MAC_A)
}

/// Bridge B, `44:C1`.
fn bridge_b() -> FakeBridge {
    FakeBridge::new(PEER_B, MAC_B)
}

/// A radio on Linux with `bridges` in range.
fn radio(bridges: impl IntoIterator<Item = FakeBridge>) -> FakeRadio {
    radio_on(LINUX, bridges)
}

fn radio_on(
    traits: crate::pipe::RadioTraits,
    bridges: impl IntoIterator<Item = FakeBridge>,
) -> FakeRadio {
    let fake = FakeRadio::new(traits);
    for bridge in bridges {
        fake.add(bridge);
    }
    fake
}

/// The transport looking for bridge A by its octets.
fn named(fake: &FakeRadio) -> BleTransport {
    BleTransport::with_spec(fake.radio(), spec("44:C0"))
}

fn start(transport: BleTransport) -> LinkHandle {
    transport.start().expect("the supervisor starts")
}

/// The next event.
async fn next(handle: &mut LinkHandle) -> LinkEvent {
    let event = tokio::time::timeout(WITHIN, handle.recv()).await.expect("an event in time");
    event.expect("the link is up")
}

/// The next `Connected`, skipping everything else.
async fn connected(handle: &mut LinkHandle) -> BridgeInfo {
    loop {
        if let LinkEvent::Connected(info) = next(handle).await {
            return info;
        }
    }
}

/// The next `Disconnected`, skipping everything else.
async fn disconnected(handle: &mut LinkHandle) -> (String, DisconnectCause) {
    loop {
        if let LinkEvent::Disconnected { reason, cause } = next(handle).await {
            return (reason, cause);
        }
    }
}

/// The next `Disconnected`'s reason.
async fn reason(handle: &mut LinkHandle) -> String {
    disconnected(handle).await.0
}

/// Wait until `done` holds, on the paused clock.
async fn until(mut done: impl FnMut() -> bool) {
    let wait = async {
        while !done() {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    };
    tokio::time::timeout(WITHIN, wait).await.expect("in time");
}

fn count(fake: &FakeRadio, wanted: impl Fn(&Did) -> bool) -> usize {
    fake.did().iter().filter(|did| wanted(did)).count()
}

fn identifies(fake: &FakeRadio) -> usize {
    fake.frames().iter().filter(|frame| **frame == HostToBridge::Identify).count()
}

/// When each `Identify` reached a bridge, from `start`.
fn identifies_at(fake: &FakeRadio, start: Instant) -> Vec<Duration> {
    fake.did_at()
        .into_iter()
        .filter(|(_, did)| *did == Did::Frame(HostToBridge::Identify))
        .map(|(at, _)| at - start)
        .collect()
}

/// The writes to Down, in order.
fn writes(fake: &FakeRadio) -> Vec<(Vec<u8>, WriteKind)> {
    fake.did()
        .into_iter()
        .filter_map(|did| match did {
            Did::Write(bytes, kind) => Some((bytes, kind)),
            _ => None,
        })
        .collect()
}

/// A `ToNode` whose body is `len` bytes.
fn to_node(len: usize) -> HostToBridge {
    let body = ToNodeBody::from_slice(&vec![0x5A; len]).unwrap();
    HostToBridge::ToNode { slot: Slot::new(0, 0).unwrap(), id: 1, kind: 2, body }
}

// Finding.

#[tokio::test(start_paused = true)]
async fn ble_scan_takes_named_bridge_at_once_when_heard_advertising() {
    let fake = radio([bridge_a()]);
    let start_at = Instant::now();
    let mut handle = start(named(&fake));

    assert_eq!(connected(&mut handle).await.mac, MAC_A);
    assert_eq!(start_at.elapsed(), Duration::ZERO, "before {LISTEN_FOR:?}");
    assert_eq!(fake.did()[..3], [Did::ScanStarted, Did::ScanStopped, Did::Connect(PEER_A.into())]);
}

#[tokio::test(start_paused = true)]
async fn ble_scan_listens_before_choosing_when_no_bridge_is_named() {
    // A second bridge advertising nearby has to be seen rather than passed over.
    let fake = radio([bridge_a()]);
    let start_at = Instant::now();
    let mut handle = start(BleTransport::new(fake.radio()));

    assert_eq!(connected(&mut handle).await.mac, MAC_A);
    assert_eq!(start_at.elapsed(), LISTEN_FOR);
    let found = [Did::ScanStarted, Did::Known, Did::ScanStopped, Did::Connect(PEER_A.into())];
    assert_eq!(fake.did()[..4], found);
}

#[tokio::test(start_paused = true)]
async fn ble_scan_reports_several_advertising_when_two_bridges_answer_unnamed() {
    let fake = radio([bridge_b(), bridge_a()]);
    let mut handle = start(BleTransport::new(fake.radio()));

    let several = TransportError::SeveralAdvertising { bridges: "44:C0, 44:C1".to_owned() };
    let (reason, cause) = disconnected(&mut handle).await;
    assert_eq!((reason, cause), (several.to_string(), DisconnectCause::SeveralAdvertising));
    assert_eq!(count(&fake, |did| matches!(did, Did::Connect(_))), 0);
}

#[tokio::test(start_paused = true)]
async fn ble_scan_prefers_remembered_bridge_when_two_advertise() {
    let fake = radio([bridge_a(), bridge_b()]);
    let memory = BridgeMemory::holding(Some(MAC_B));
    let mut handle = start(BleTransport::new(fake.radio()).remember_in(memory));

    assert_eq!(connected(&mut handle).await.mac, MAC_B);
}

#[tokio::test(start_paused = true)]
async fn ble_scan_takes_listed_bridge_when_it_is_the_only_one_and_not_heard() {
    // One BlueZ still holds a connection to does not advertise.
    let fake = radio([FakeBridge { advertises: None, listed: true, ..bridge_a() }]);
    let start_at = Instant::now();
    let mut handle = start(BleTransport::new(fake.radio()));

    assert_eq!(connected(&mut handle).await.mac, MAC_A);
    assert_eq!(start_at.elapsed(), LISTEN_FOR);
}

#[tokio::test(start_paused = true)]
async fn ble_scan_passes_over_listed_bridge_when_another_advertises() {
    // A bonded bridge that is off is not taken while one that answers advertises.
    let listed = FakeBridge { advertises: None, listed: true, ..bridge_a() };
    let fake = radio([listed, bridge_b()]);
    let mut handle = start(BleTransport::new(fake.radio()));

    assert_eq!(connected(&mut handle).await.mac, MAC_B);
}

#[tokio::test(start_paused = true)]
async fn ble_scan_reports_not_found_when_nothing_advertises_in_time() {
    let fake = radio([]);
    let start_at = Instant::now();
    let mut handle = start(named(&fake));

    let not_found = TransportError::NotFound { bridge: Some("44:C0".to_owned()), secs: 12 };
    let (reason, cause) = disconnected(&mut handle).await;
    assert_eq!((reason, cause), (not_found.to_string(), DisconnectCause::NotAdvertising));
    assert_eq!(start_at.elapsed(), SCAN_FOR);
    assert_eq!(fake.did(), [Did::ScanStarted, Did::Known, Did::ScanStopped]);
}

#[tokio::test(start_paused = true)]
async fn ble_scan_reports_adapter_failure_when_sightings_end() {
    let fake = radio([]);
    let mut handle = start(named(&fake));
    tokio::time::sleep(Duration::from_millis(500)).await;

    fake.end_sightings();
    assert_eq!(reason(&mut handle).await, "Bluetooth: the Bluetooth adapter stopped reporting");
    assert_eq!(fake.did(), [Did::ScanStarted, Did::ScanStopped]);
}

#[tokio::test(start_paused = true)]
async fn ble_link_reports_no_adapter_when_radio_has_none() {
    let fake = FakeRadio::without_adapter();
    let mut handle = start(named(&fake));

    assert_eq!(reason(&mut handle).await, TransportError::NoAdapter.to_string());
}

#[tokio::test(start_paused = true)]
async fn ble_link_connects_without_scan_when_os_recalls_remembered_address() {
    let fake = radio([FakeBridge { advertises: None, recalled: true, ..bridge_a() }]);
    let memory = BridgeMemory::holding(Some(MAC_A)).on_bluetooth(Some(PEER_A.to_owned()));
    let mut handle = start(BleTransport::new(fake.radio()).remember_in(memory));

    assert_eq!(connected(&mut handle).await.mac, MAC_A);
    assert_eq!(fake.did()[..2], [Did::Recall(PEER_A.into()), Did::Connect(PEER_A.into())]);
    assert_eq!(count(&fake, |did| *did == Did::ScanStarted), 0);
}

#[tokio::test(start_paused = true)]
async fn ble_link_scans_every_later_try_when_direct_connection_failed() {
    let refused = Connects::Fails("refused".to_owned());
    let fake = radio([FakeBridge { recalled: true, connects: refused, ..bridge_a() }]);
    let memory = BridgeMemory::holding(Some(MAC_A)).on_bluetooth(Some(PEER_A.to_owned()));
    let mut handle = start(named(&fake).remember_in(memory));

    for _ in 0..3 {
        assert_eq!(reason(&mut handle).await, "could not connect to 44:C0: refused");
    }
    assert_eq!(count(&fake, |did| matches!(did, Did::Recall(_))), 1);
    assert_eq!(count(&fake, |did| *did == Did::ScanStarted), 2);
}

#[tokio::test(start_paused = true)]
async fn ble_link_retries_when_scan_never_starts() {
    let fake = radio([bridge_a()]);
    fake.never_answers(Call::Scan);
    let start_at = Instant::now();
    let mut handle = start(named(&fake));

    let (reason, cause) = disconnected(&mut handle).await;
    assert_eq!(reason, "Bluetooth: the Bluetooth adapter did not start a scan");
    assert_eq!(cause, DisconnectCause::Other);
    assert_eq!(start_at.elapsed(), SCAN_START_WITHIN);
    // The scan the OS may yet start is stopped.
    assert_eq!(fake.did()[..2], [Did::ScanStarted, Did::ScanStopped]);
    until(|| count(&fake, |did| *did == Did::ScanStarted) == 2).await;
}

#[tokio::test(start_paused = true)]
async fn ble_link_retries_when_recall_never_answers() {
    let fake = radio([FakeBridge { recalled: true, ..bridge_a() }]);
    fake.never_answers(Call::Recall);
    let memory = BridgeMemory::holding(Some(MAC_A)).on_bluetooth(Some(PEER_A.to_owned()));
    let start_at = Instant::now();
    let mut handle = start(named(&fake).remember_in(memory));

    let (reason, cause) = disconnected(&mut handle).await;
    assert_eq!(reason, "Bluetooth: the Bluetooth adapter did not look up the remembered bridge");
    assert_eq!(cause, DisconnectCause::Other);
    assert_eq!(start_at.elapsed(), RECALL_WITHIN);
    until(|| count(&fake, |did| matches!(did, Did::Recall(_))) == 2).await;
}

#[tokio::test(start_paused = true)]
async fn ble_link_retries_when_listing_never_answers() {
    // Heard only late, so the listen reads the OS's listing first.
    let late = FakeBridge { advertises: Some(Duration::from_secs(5)), ..bridge_a() };
    let fake = radio([late]);
    fake.never_answers(Call::Known);
    let start_at = Instant::now();
    let mut handle = start(BleTransport::new(fake.radio()));

    let (reason, cause) = disconnected(&mut handle).await;
    assert_eq!(reason, "Bluetooth: the Bluetooth adapter did not list its devices");
    assert_eq!(cause, DisconnectCause::Other);
    assert_eq!(start_at.elapsed(), LISTEN_FOR + LIST_WITHIN);
    until(|| count(&fake, |did| *did == Did::Known) == 2).await;
}

#[tokio::test(start_paused = true)]
async fn ble_link_scans_when_remembered_bridge_is_not_the_one_named() {
    let fake = radio([bridge_a(), FakeBridge { recalled: true, ..bridge_b() }]);
    let memory = BridgeMemory::holding(Some(MAC_B)).on_bluetooth(Some(PEER_B.to_owned()));
    let mut handle = start(named(&fake).remember_in(memory));

    assert_eq!(connected(&mut handle).await.mac, MAC_A);
    assert_eq!(count(&fake, |did| matches!(did, Did::Recall(_))), 0);
}

#[tokio::test(start_paused = true)]
async fn ble_find_advertiser_names_bridge_when_it_advertises() {
    let fake = radio([FakeBridge { name: Some("wartui 44:C0".to_owned()), ..bridge_a() }]);

    let found = find_advertiser(&*fake.radio(), Some(&spec("44:C0")), None).await;
    let want = Advertiser {
        octets: [0x44, 0xC0],
        name: Some("wartui 44:C0".to_owned()),
        address: PEER_A.to_owned(),
    };
    assert_eq!(found.expect("found"), want);
    assert_eq!(fake.did(), [Did::ScanStarted, Did::ScanStopped]);
}

/// What a survey lists of `peer` at `mac`.
fn sighted(peer: &str, mac: Mac, live: bool) -> Sighting {
    Sighting { peer: peer.to_owned(), octets: [mac[4], mac[5]], name: None, live }
}

#[tokio::test(start_paused = true)]
async fn ble_survey_lists_every_bridge_heard_when_several_advertise() {
    // Added out of order, and listed by octets.
    let fake = radio([bridge_b(), bridge_a()]);

    let heard = survey(&*fake.radio(), LISTEN_FOR).await.expect("surveyed");
    assert_eq!(heard, [sighted(PEER_A, MAC_A, true), sighted(PEER_B, MAC_B, true)]);
}

#[tokio::test(start_paused = true)]
async fn ble_survey_lists_a_listed_bridge_when_it_does_not_advertise() {
    // One BlueZ still holds a connection to advertises nothing.
    let held = FakeBridge { advertises: None, listed: true, ..bridge_b() };
    let fake = radio([bridge_a(), held]);

    let heard = survey(&*fake.radio(), LISTEN_FOR).await.expect("surveyed");
    assert_eq!(heard, [sighted(PEER_A, MAC_A, true), sighted(PEER_B, MAC_B, false)]);
}

#[tokio::test(start_paused = true)]
async fn ble_survey_stops_the_scan_when_time_is_up() {
    let fake = radio([bridge_a()]);
    let start_at = Instant::now();

    survey(&*fake.radio(), Duration::from_secs(3)).await.expect("surveyed");
    assert_eq!(start_at.elapsed(), Duration::from_secs(3));
    assert_eq!(fake.did(), [Did::ScanStarted, Did::Known, Did::ScanStopped]);
}

#[tokio::test(start_paused = true)]
async fn ble_survey_fails_when_scan_cannot_start() {
    let fake = FakeRadio::without_adapter();
    let error = survey(&*fake.radio(), LISTEN_FOR).await.expect_err("no adapter");
    assert_eq!(error.kind, RadioErrorKind::NoAdapter);
    assert_eq!(fake.did(), [Did::ScanStopped]);

    // A start the OS never answers is timed, and the scan it may yet start is stopped.
    let fake = radio([bridge_a()]);
    fake.never_answers(Call::Scan);
    let start_at = Instant::now();
    let error = survey(&*fake.radio(), LISTEN_FOR).await.expect_err("never started");
    assert_eq!(error.kind, RadioErrorKind::Other);
    assert_eq!(start_at.elapsed(), SCAN_START_WITHIN);
    assert_eq!(fake.did(), [Did::ScanStarted, Did::ScanStopped]);
}

#[tokio::test(start_paused = true)]
async fn ble_scan_hears_bridge_when_it_advertises_after_listing_began() {
    // The listing at the end of the listen finds nothing, and the scan listens on.
    let late = FakeBridge { advertises: Some(Duration::from_secs(5)), ..bridge_a() };
    let fake = radio([late]);
    let start_at = Instant::now();
    let mut handle = start(BleTransport::new(fake.radio()));

    assert_eq!(connected(&mut handle).await.mac, MAC_A);
    assert_eq!(start_at.elapsed(), Duration::from_secs(5));
}

// Settling.

#[tokio::test(start_paused = true)]
async fn ble_link_reconnects_to_settled_peer_without_scan_when_link_drops() {
    let fake = radio([bridge_a()]);
    let mut handle = start(BleTransport::new(fake.radio()));
    connected(&mut handle).await;

    fake.drop_link(PEER_A);
    assert_eq!(reason(&mut handle).await, "the bridge disconnected");
    assert_eq!(connected(&mut handle).await.mac, MAC_A);
    assert_eq!(count(&fake, |did| *did == Did::ScanStarted), 1);
    assert_eq!(count(&fake, |did| *did == Did::Connect(PEER_A.into())), 2);
}

#[tokio::test(start_paused = true)]
async fn ble_link_refuses_bridge_when_ready_names_another_address() {
    // It shares only the octets its id carries.
    let other: Mac = [0x02, 0x00, 0x5E, 0x77, 0x44, 0xC0];
    let fake = radio([FakeBridge { ready_mac: Some(other), ..bridge_a() }]);
    let named = spec("02:00:5E:10:44:C0");
    let mut handle = start(BleTransport::with_spec(fake.radio(), named.clone()));

    let wrong = format!("the bridge that answered is {}, not {named}", mac::full(&other));
    assert_eq!(reason(&mut handle).await, wrong);
}

#[tokio::test(start_paused = true)]
async fn ble_link_remembers_address_when_ready_proves_bridge() {
    let fake = radio([bridge_a()]);
    let memory = BridgeMemory::none();
    let mut handle = start(named(&fake).remember_in(memory.clone()));

    connected(&mut handle).await;
    assert_eq!(memory.recall(), Some(MAC_A));
    assert_eq!(memory.recall_bluetooth().as_deref(), Some(PEER_A));
}

// Connecting and subscribing.

#[tokio::test(start_paused = true)]
async fn ble_link_abandons_connect_when_it_does_not_complete_in_time() {
    let fake = radio([FakeBridge { connects: Connects::Never, ..bridge_a() }]);
    let start_at = Instant::now();
    let mut handle = start(named(&fake));

    assert_eq!(reason(&mut handle).await, connect_timeout_text("44:C0", true, false));
    assert_eq!(start_at.elapsed(), CONNECT_WITHIN);
    let tail = [Did::Connect(PEER_A.into()), Did::Abandon(PEER_A.into())];
    assert_eq!(fake.did()[2..4], tail);
}

#[tokio::test(start_paused = true)]
async fn ble_link_names_stale_bond_when_heard_bridge_times_out_on_macos() {
    let fake = radio_on(MACOS, [FakeBridge { connects: Connects::Never, ..bridge_a() }]);
    let mut handle = start(named(&fake));

    assert_eq!(reason(&mut handle).await, connect_timeout_text("44:C0", true, true));
}

#[tokio::test(start_paused = true)]
async fn ble_link_reports_connect_failure_when_os_refuses() {
    let gone = Connects::Fails("Peripheral no longer available".to_owned());
    let fake = radio([FakeBridge { connects: gone, ..bridge_a() }]);
    let mut handle = start(named(&fake));

    let (reason, cause) = disconnected(&mut handle).await;
    assert_eq!(reason, "could not connect to 44:C0: Peripheral no longer available");
    assert_eq!(cause, DisconnectCause::Other);
}

#[tokio::test(start_paused = true)]
async fn ble_link_reports_radio_off_and_retries_when_connect_fails_powered_off() {
    let off = Connects::FailsAs(RadioErrorKind::PoweredOff, "Bluetooth is off".to_owned());
    let fake = radio([FakeBridge { connects: off, ..bridge_a() }]);
    let mut handle = start(named(&fake));

    for _ in 0..2 {
        let (reason, cause) = disconnected(&mut handle).await;
        assert_eq!(reason, "could not connect to 44:C0: Bluetooth is off");
        assert_eq!(cause, DisconnectCause::RadioOff);
    }
    assert_eq!(count(&fake, |did| matches!(did, Did::Connect(_))), 2, "tried again");
}

#[tokio::test(start_paused = true)]
async fn ble_link_reports_not_authorized_when_connect_fails_unauthorized() {
    let denied = Connects::FailsAs(RadioErrorKind::Unauthorized, "not allowed".to_owned());
    let fake = radio([FakeBridge { connects: denied, ..bridge_a() }]);
    let mut handle = start(named(&fake));

    let (reason, cause) = disconnected(&mut handle).await;
    assert_eq!(reason, "could not connect to 44:C0: not allowed");
    assert_eq!(cause, DisconnectCause::NotAuthorized);
}

#[tokio::test(start_paused = true)]
async fn ble_link_reports_stale_bond_when_connect_fails_peer_forgot_bond() {
    let text = "Peer removed pairing information";
    let forgot = Connects::FailsAs(RadioErrorKind::PeerForgotBond, text.to_owned());
    let fake = radio([FakeBridge { connects: forgot, ..bridge_a() }]);
    let mut handle = start(named(&fake));

    let (reason, cause) = disconnected(&mut handle).await;
    assert_eq!(reason, format!("could not connect to 44:C0: {text}"));
    assert_eq!(cause, DisconnectCause::StaleBond);
}

#[tokio::test(start_paused = true)]
async fn ble_link_reports_no_link_service_when_bridge_offers_none() {
    let fake = radio([FakeBridge { discovers: Discovers::NoService, ..bridge_a() }]);
    let mut handle = start(named(&fake));

    assert_eq!(reason(&mut handle).await, "44:C0 offers no wartui link service");
    assert_eq!(fake.did().last(), Some(&Did::Close { unsubscribe: false }));
}

#[tokio::test(start_paused = true)]
async fn ble_link_reports_not_bonded_when_subscription_is_refused_for_bonding() {
    let fake = radio([FakeBridge { subscribes: Subscribes::NotBonded, ..bridge_a() }]);
    let mut handle = start(named(&fake));

    let (reason, cause) = disconnected(&mut handle).await;
    assert_eq!(reason, TransportError::NotBonded { bridge: "44:C0".to_owned() }.to_string());
    assert_eq!(cause, DisconnectCause::NotBonded);
}

#[tokio::test(start_paused = true)]
async fn ble_link_reports_maybe_not_bonded_when_link_drops_ten_seconds_after_connect_before_ready()
{
    // The C6 drops an unbonded central about 10 s after it connects.
    let fake = radio([FakeBridge { answers: None, ..bridge_a() }]);
    let mut handle = start(named(&fake));
    tokio::time::sleep(Duration::from_secs(10)).await;

    fake.drop_link(PEER_A);
    let (reason, cause) = disconnected(&mut handle).await;
    assert_eq!(reason, TransportError::MaybeNotBonded { bridge: "44:C0".to_owned() }.to_string());
    assert_eq!(cause, DisconnectCause::MaybeNotBonded);
}

#[tokio::test(start_paused = true)]
async fn ble_link_keeps_radio_off_when_link_drops_powered_off_before_ready() {
    // Inside the unbonded drop's timing, but the OS named why.
    let fake = radio([FakeBridge { answers: None, ..bridge_a() }]);
    let mut handle = start(named(&fake));
    tokio::time::sleep(Duration::from_secs(10)).await;

    fake.drop_link_as(PEER_A, RadioError::new(RadioErrorKind::PoweredOff, "Bluetooth is off"));
    let (reason, cause) = disconnected(&mut handle).await;
    assert_eq!((reason.as_str(), cause), ("Bluetooth is off", DisconnectCause::RadioOff));
}

#[tokio::test(start_paused = true)]
async fn ble_link_does_not_guess_bonding_when_os_already_held_the_link() {
    // Nothing says when the C6's deadline started.
    let held = FakeBridge { answers: None, connects: Connects::AlreadyHeld, ..bridge_a() };
    let fake = radio([held]);
    let mut handle = start(named(&fake));
    tokio::time::sleep(Duration::from_secs(10)).await;

    fake.drop_link(PEER_A);
    let (reason, cause) = disconnected(&mut handle).await;
    assert_eq!(reason, "the bridge disconnected");
    assert_eq!(cause, DisconnectCause::Other);
}

#[tokio::test(start_paused = true)]
async fn ble_link_announces_pairing_wait_when_os_pairs_and_subscription_is_slow() {
    // The operator clicks Connect 10 s in.
    let slow = Subscribes::After(Duration::from_secs(10));
    let fake = radio_on(MACOS, [FakeBridge { subscribes: slow, ..bridge_a() }]);
    let start_at = Instant::now();
    let mut handle = start(named(&fake));

    let waiting = (pairing_notice("44:C0", true), DisconnectCause::AwaitingPairing);
    assert_eq!(disconnected(&mut handle).await, waiting);
    assert_eq!(start_at.elapsed(), PAIRING_NOTICE_AFTER);
    let through = (ready_notice("44:C0"), DisconnectCause::AwaitingReady);
    assert_eq!(disconnected(&mut handle).await, through);
    assert!(matches!(next(&mut handle).await, LinkEvent::Connected(_)));
    assert_eq!(start_at.elapsed(), Duration::from_secs(10));
}

#[tokio::test(start_paused = true)]
async fn ble_link_sends_no_pairing_notice_when_run_has_settled() {
    // A bridge that answered a `Ready` holds this host's bond.
    let fake = radio_on(MACOS, [bridge_a()]);
    let mut handle = start(named(&fake));
    connected(&mut handle).await;

    fake.set_subscribes(PEER_A, Subscribes::After(Duration::from_secs(5)));
    fake.drop_link(PEER_A);
    assert_eq!(reason(&mut handle).await, "the bridge disconnected");
    assert!(matches!(next(&mut handle).await, LinkEvent::Connected(_)), "no notice between");
}

// Serving.

#[tokio::test(start_paused = true)]
async fn ble_link_asks_identify_every_second_when_no_ready_answers() {
    let fake = radio([FakeBridge { answers: None, ..bridge_a() }]);
    let start_at = Instant::now();
    let _handle = start(named(&fake));
    tokio::time::sleep(Duration::from_millis(3500)).await;

    let at: Vec<Duration> = (0..4).map(|n| IDENTIFY_EVERY * n).collect();
    assert_eq!(identifies_at(&fake, start_at), at);
}

#[tokio::test(start_paused = true)]
async fn ble_link_leads_first_write_with_zero_when_connection_is_new() {
    // The `0x00` ends whatever partial frame the P4 held from an earlier attachment.
    let fake = radio([FakeBridge { answers: Some(1), ..bridge_a() }]);
    let mut handle = start(named(&fake));
    connected(&mut handle).await;
    let first = writes(&fake);
    assert_eq!(first.len(), 2, "{first:?}");
    assert_eq!(first[0].0[0], 0x00);
    assert_ne!(first[1].0[0], 0x00, "only the connection's first frame");

    fake.drop_link(PEER_A);
    connected(&mut handle).await;
    assert_eq!(writes(&fake)[2].0[0], 0x00, "and the next connection's");
}

#[tokio::test(start_paused = true)]
async fn ble_link_stops_asking_when_ready_answers() {
    let fake = radio([FakeBridge { answers: Some(2), ..bridge_a() }]);
    let start_at = Instant::now();
    let mut handle = start(named(&fake));

    connected(&mut handle).await;
    assert_eq!(start_at.elapsed(), IDENTIFY_EVERY * 2);
    tokio::time::sleep(Duration::from_secs(5)).await;
    assert_eq!(identifies(&fake), 3);
}

#[tokio::test(start_paused = true)]
async fn ble_link_reports_takeover_and_stays_subscribed_when_usb_host_takes_bridge() {
    let fake = radio([bridge_a()]);
    let mut handle = start(named(&fake));
    connected(&mut handle).await;
    tokio::time::sleep(Duration::from_millis(500)).await;

    fake.take_over(PEER_A);
    let took = (USB_TOOK_BRIDGE.to_owned(), DisconnectCause::UsbTookBridge);
    assert_eq!(disconnected(&mut handle).await, took);
    let taken_at = Instant::now();
    // The stream starts afresh at the handback, so its `Ready` is reported.
    assert!(matches!(next(&mut handle).await, LinkEvent::Connected(_)));
    assert_eq!(taken_at.elapsed(), IDENTIFY_EVERY, "asked a period later");
    assert_eq!(identifies(&fake), 2);
    assert_eq!(count(&fake, |did| matches!(did, Did::Connect(_))), 1);
    assert_eq!(count(&fake, |did| matches!(did, Did::Close { .. })), 0);
}

#[tokio::test(start_paused = true)]
async fn ble_link_drops_queued_commands_when_ready_arrives() {
    // A command queued before the `Ready` is stale by then.
    let fake = radio([FakeBridge { answers: Some(1), ..bridge_a() }]);
    let mut handle = start(named(&fake));
    tokio::time::sleep(Duration::from_millis(500)).await;
    handle.send_bulk(HostToBridge::GetStatus).expect("queued");

    connected(&mut handle).await;
    assert_eq!(handle.written_count(), 1, "counted done");
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert!(!fake.frames().contains(&HostToBridge::GetStatus), "{:?}", fake.frames());
}

#[tokio::test(start_paused = true)]
async fn ble_link_ends_group_with_request_when_commands_are_written() {
    let fake = radio([bridge_a()]);
    let mut handle = start(named(&fake));
    connected(&mut handle).await;
    let before = writes(&fake).len();

    let sent = [to_node(150), to_node(150), to_node(150)];
    for cmd in &sent {
        handle.send_bulk(cmd.clone()).expect("queued");
    }
    until(|| handle.written_count() == 3).await;
    let group: Vec<WriteKind> = writes(&fake)[before..].iter().map(|(_, kind)| *kind).collect();
    assert_eq!(group, [WriteKind::Command, WriteKind::Command, WriteKind::Request]);
    assert_eq!(fake.frames()[1..], sent);
}

#[tokio::test(start_paused = true)]
async fn ble_link_counts_commands_done_when_group_is_answered() {
    let fake = radio([bridge_a()]);
    let mut handle = start(named(&fake));
    connected(&mut handle).await;

    handle.send_bulk(HostToBridge::GetStatus).expect("queued");
    until(|| handle.written_count() == 1).await;
    assert_eq!(fake.frames(), [HostToBridge::Identify, HostToBridge::GetStatus]);
}

#[tokio::test(start_paused = true)]
async fn ble_link_ends_connection_when_write_is_not_answered_in_time() {
    let fake = radio([bridge_a()]);
    let mut handle = start(named(&fake));
    connected(&mut handle).await;

    fake.set_writes(PEER_A, Writes::NeverAnswered);
    handle.send_bulk(HostToBridge::GetStatus).expect("queued");
    let sent_at = Instant::now();
    assert_eq!(reason(&mut handle).await, "a write to the bridge was not answered in 5s");
    assert_eq!(sent_at.elapsed(), WRITE_WITHIN);
    assert_eq!(handle.written_count(), 1, "lost with the connection, so done");
}

#[tokio::test(start_paused = true)]
async fn ble_link_reports_not_bonded_when_write_request_is_refused_for_bonding() {
    let fake = radio([bridge_a()]);
    let mut handle = start(named(&fake));
    connected(&mut handle).await;

    fake.set_writes(PEER_A, Writes::Refused);
    handle.send_bulk(HostToBridge::GetStatus).expect("queued");
    let (reason, cause) = disconnected(&mut handle).await;
    assert_eq!(reason, TransportError::NotBonded { bridge: "44:C0".to_owned() }.to_string());
    assert_eq!(cause, DisconnectCause::NotBonded);
}

#[tokio::test(start_paused = true)]
async fn ble_link_reports_disconnect_when_bridge_drops() {
    let fake = radio([bridge_a()]);
    let mut handle = start(named(&fake));
    connected(&mut handle).await;

    fake.drop_link(PEER_A);
    let (reason, cause) = disconnected(&mut handle).await;
    assert_eq!((reason.as_str(), cause), ("the bridge disconnected", DisconnectCause::Other));
    assert_eq!(fake.did().last(), Some(&Did::Close { unsubscribe: false }));
}

#[tokio::test(start_paused = true)]
async fn ble_link_reports_disconnect_when_os_says_link_is_gone() {
    // No disconnect event: only asking the OS says so.
    let fake = radio([bridge_a()]);
    let start_at = Instant::now();
    let mut handle = start(named(&fake));
    connected(&mut handle).await;
    tokio::time::sleep(Duration::from_millis(500)).await;

    fake.lose_link(PEER_A);
    assert_eq!(reason(&mut handle).await, "the bridge disconnected");
    assert_eq!(start_at.elapsed(), ALIVE_EVERY);
}

#[tokio::test(start_paused = true)]
async fn ble_link_reassembles_frame_when_notifications_split_it() {
    let fake = radio([bridge_a()]);
    let mut handle = start(named(&fake));
    connected(&mut handle).await;

    let log = BridgeToHost::Log { message: LogStr::try_from("split across three").unwrap() };
    let bytes = frame(&log);
    let third = bytes.len() / 3;
    for piece in [&bytes[..third], &bytes[third..2 * third], &bytes[2 * third..]] {
        fake.notify(PEER_A, piece);
    }
    assert_eq!(next(&mut handle).await, LinkEvent::Message(log));
}

#[tokio::test(start_paused = true)]
async fn ble_link_doubles_retry_delay_when_failure_repeats() {
    let refused = Connects::Fails("refused".to_owned());
    let fake = radio([FakeBridge { connects: refused, ..bridge_a() }]);
    let mut handle = start(named(&fake));

    for _ in 0..7 {
        reason(&mut handle).await;
    }
    let at = fake.connects_at();
    let gaps: Vec<Duration> = at.windows(2).map(|pair| pair[1] - pair[0]).collect();
    let secs = Duration::from_secs;
    let want = [secs(1), secs(2), secs(4), secs(8), MAX_RETRY_DELAY, MAX_RETRY_DELAY];
    assert_eq!(gaps, want);
}

// Closing.

#[tokio::test(start_paused = true)]
async fn ble_link_unsubscribes_and_closes_when_handle_is_dropped() {
    let fake = radio([bridge_a()]);
    let mut handle = start(named(&fake));
    connected(&mut handle).await;

    handle.close().await;
    assert_eq!(fake.did().last(), Some(&Did::Close { unsubscribe: true }));
}

#[tokio::test(start_paused = true)]
async fn ble_link_stops_scan_when_handle_is_dropped_while_scanning() {
    let fake = radio([]);
    let handle = start(named(&fake));
    tokio::time::sleep(Duration::from_secs(1)).await;

    handle.close().await;
    assert_eq!(fake.did(), [Did::ScanStarted, Did::ScanStopped]);
}

#[tokio::test(start_paused = true)]
async fn ble_link_abandons_connect_when_handle_is_dropped_while_connecting() {
    let fake = radio([FakeBridge { connects: Connects::Never, ..bridge_a() }]);
    let handle = start(named(&fake));
    tokio::time::sleep(Duration::from_secs(1)).await;

    handle.close().await;
    assert_eq!(fake.did().last(), Some(&Did::Abandon(PEER_A.into())));
    assert_eq!(count(&fake, |did| matches!(did, Did::Close { .. })), 0);
}

#[tokio::test(start_paused = true)]
async fn ble_link_closes_without_unsubscribe_when_handle_is_dropped_before_subscription() {
    let fake = radio([FakeBridge { subscribes: Subscribes::Never, ..bridge_a() }]);
    let handle = start(named(&fake));
    tokio::time::sleep(Duration::from_secs(1)).await;

    handle.close().await;
    assert_eq!(fake.did().last(), Some(&Did::Close { unsubscribe: false }));
}
