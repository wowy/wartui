//! A bridge's operations outside a capture, over the simulated bridge and a scripted link.

use std::time::Duration;

use tokio::time::timeout;
use wartui_app::ops::{self, Cleared, LinkError, NotReady, ReadyWaited, Status};
use wartui_app::settings::Link;
use wartui_bridge::sim::{SimConfig, SimTransport};
use wartui_bridge::{BridgeCore, DisconnectCause, LinkEvent, LinkHandle, LinkPlumbing, link_pair};
use wartui_proto::bus::{Slot, Version};
use wartui_proto::fan::FanDuty;
use wartui_proto::fleet::ScanMode;
use wartui_proto::link::{
    BRIDGE_VERSION, BridgeToHost, PowerOutcome, QueueStats, RadioLinkStats, SdFormatOutcome,
    Standing, Trial,
};
use wartui_proto::plan::{ChannelPool, PlanSettings};

/// The longest any operation below may take before the test fails rather than hangs.
const WAIT: Duration = Duration::from_secs(300);

/// A simulated bridge of `nodes` nodes, announced.
async fn sim(nodes: u8) -> LinkHandle {
    let config = SimConfig { node_count: nodes, speed: 20.0, ..SimConfig::default() };
    let mut link = SimTransport::new(config).start().expect("starting the simulator");
    let ReadyWaited { ready, .. } =
        ops::ready(&mut link, Link::Usb, ops::REPLY_TIMEOUT, ops::REPLY_TIMEOUT, &mut |_| {}).await;
    ready.expect("the simulator connects").expect("in time");
    link
}

/// A fault sink that ignores every fault.
fn ignore(_: &str) {}

/// A `Status` message holding `pool`.
fn status_message(pool: ChannelPool) -> BridgeToHost {
    BridgeToHost::Status {
        rx_count: 2,
        dropped_tx: 0,
        uptime_ms: 3,
        rx_oversize: 0,
        queues: QueueStats::new(),
        radio_link: RadioLinkStats::default(),
        host_uart_errors: 0,
        fan: None,
        pool,
        planner: PlanSettings::DEFAULT,
        scan: ScanMode::Stopped,
        ble: None,
        present: 0,
        members: Default::default(),
        sd: None,
        bluetooth: Default::default(),
        health: Default::default(),
    }
}

#[tokio::test(start_paused = true)]
async fn ops_status_returns_bridge_status_when_asked() {
    let mut link = sim(3).await;
    let status = timeout(WAIT, ops::status(&mut link, ops::REPLY_TIMEOUT, &mut ignore))
        .await
        .expect("in time")
        .expect("a status");
    // The simulator boots stopped, as a bridge does, holding every channel.
    assert_eq!(status.scan, ScanMode::Stopped);
    assert_eq!(status.pool, ChannelPool::All);
    assert!(status.sd.is_some(), "the simulated bridge has a card slot");
}

#[tokio::test(start_paused = true)]
async fn ops_hand_fault_to_caller_when_bridge_reports_error() {
    let (mut link, plumbing) = link_pair();
    let fault = "updating the radio to the bundled image (40%)";
    let message = fault.try_into().expect("short");
    plumbing.events.send(LinkEvent::Message(BridgeToHost::Error { message })).await.unwrap();
    plumbing.events.send(LinkEvent::Message(status_message(ChannelPool::Us))).await.unwrap();

    let mut faults = Vec::new();
    let status = ops::status(&mut link, ops::REPLY_TIMEOUT, &mut |m| faults.push(m.to_owned()))
        .await
        .expect("the status after the fault");

    assert_eq!(faults, [fault], "handed over, not an end to the wait");
    assert_eq!(status, Status::from_message(status_message(ChannelPool::Us)).expect("a status"));
}

#[tokio::test(start_paused = true)]
async fn ops_report_link_closed_when_transport_ends() {
    let (mut link, plumbing) = link_pair();
    // The commands still queue; only the events end.
    drop(plumbing.events);

    let ended = ops::status(&mut link, ops::REPLY_TIMEOUT, &mut ignore).await;

    assert!(matches!(ended, Err(LinkError::Closed)), "{ended:?}");
    assert_eq!(ended.unwrap_err().to_string(), "the link closed");
}

#[tokio::test(start_paused = true)]
async fn ops_report_link_down_with_transports_reason_when_it_disconnects() {
    let (mut link, plumbing) = link_pair();
    let reason = "the bridge went away".to_owned();
    let down = LinkEvent::Disconnected { reason: reason.clone(), cause: DisconnectCause::Other };
    plumbing.events.send(down).await.unwrap();

    let ended = ops::settle(&mut link, Duration::from_secs(1), &mut ignore).await;

    assert_eq!(ended.expect_err("down").to_string(), reason);
}

#[tokio::test(start_paused = true)]
async fn ops_report_no_answer_when_wait_runs_out() {
    let (mut link, _plumbing) = link_pair();

    let ended = ops::status(&mut link, Duration::from_secs(1), &mut ignore).await;

    assert!(matches!(ended, Err(LinkError::NoAnswer(_))), "{ended:?}");
}

#[tokio::test(start_paused = true)]
async fn ops_report_queue_failure_with_its_request_when_command_cannot_be_queued() {
    let (mut link, plumbing) = link_pair();
    drop(plumbing);

    let ended = ops::status(&mut link, ops::REPLY_TIMEOUT, &mut ignore).await;

    assert!(
        matches!(ended, Err(LinkError::Queue { request: ops::Request::Status, .. })),
        "{ended:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn ops_ready_reports_closed_when_transport_ends_before_announcing() {
    let (mut link, plumbing) = link_pair();
    drop(plumbing);

    let waited =
        ops::ready(&mut link, Link::Usb, ops::REPLY_TIMEOUT, ops::REPLY_TIMEOUT, &mut |_| {}).await;

    assert!(matches!(waited.ready, Err(NotReady::Closed)), "{waited:?}");
}

#[tokio::test(start_paused = true)]
async fn ops_status_and_plan_returns_plan_when_bridge_reports_one() {
    let mut link = sim(3).await;
    timeout(WAIT, ops::start_scan(&mut link, false, &mut ignore))
        .await
        .expect("in time")
        .expect("the start")
        .expect("reported started");

    let (status, plan) =
        timeout(WAIT, ops::status_and_plan(&mut link, ops::REPLY_TIMEOUT, &mut ignore))
            .await
            .expect("in time")
            .expect("a status");

    assert_eq!(status.scan, ScanMode::Wifi);
    assert_eq!(plan.expect("the plan behind the status").mode, ScanMode::Wifi);
}

#[tokio::test(start_paused = true)]
async fn ops_start_scan_reports_mode_pick_and_plan_when_started_with_bluetooth() {
    let mut link = sim(3).await;

    let seen = timeout(WAIT, ops::start_scan(&mut link, true, &mut ignore))
        .await
        .expect("in time")
        .expect("the start")
        .expect("reported started");

    assert_eq!(seen.mode, Some(ScanMode::WifiBluetooth));
    assert!(seen.pick.is_some(), "three nodes, so one scans Bluetooth: {seen:?}");
    assert_eq!(seen.planner, Some(PlanSettings::DEFAULT));
    assert!(seen.plan.is_some_and(|plan| plan.shares.len() == 3));
}

#[tokio::test(start_paused = true)]
async fn ops_set_pool_reads_pool_back_when_bridge_takes_it() {
    let mut link = sim(2).await;
    let set = timeout(WAIT, ops::set_pool(&mut link, ChannelPool::Eu, &mut ignore))
        .await
        .expect("in time")
        .expect("the setting");
    assert!(set.taken);
    assert_eq!(set.status.pool, ChannelPool::Eu);
}

#[tokio::test(start_paused = true)]
async fn ops_set_fan_reads_duty_back_when_bridge_takes_it() {
    let mut link = sim(2).await;
    let duty = FanDuty::new(40).expect("in range");
    let set = timeout(WAIT, ops::set_fan(&mut link, duty, &mut ignore))
        .await
        .expect("in time")
        .expect("the setting");
    assert!(set.taken, "{:?}", set.status.fan);
}

#[tokio::test(start_paused = true)]
async fn ops_set_sd_log_and_wardrive_esp_read_back_what_was_sent_when_each_is_switched() {
    let mut link = sim(1).await;
    for on in [false, true] {
        let log = ops::set_sd_log(&mut link, on, &mut ignore).await.expect("the log");
        assert!(log.taken, "sd log {on}");
        let phones = ops::set_wardrive_esp(&mut link, on, &mut ignore).await.expect("phones");
        assert!(phones.taken, "wardrive esp {on}");
        assert_eq!(phones.status.bluetooth.wardrive_esp, on);
    }
}

#[tokio::test(start_paused = true)]
async fn ops_format_sd_returns_done_when_card_formats() {
    let mut link = sim(1).await;
    let outcome = timeout(WAIT, ops::format_sd(&mut link, &mut ignore))
        .await
        .expect("in time")
        .expect("an answer");
    assert_eq!(outcome, SdFormatOutcome::Done);
}

#[tokio::test(start_paused = true)]
async fn ops_power_cycle_returns_done_when_bridge_switches_the_slot() {
    let mut link = sim(2).await;
    let slot = Slot::new(0, 0).expect("a slot");
    let outcome = timeout(WAIT, ops::power_cycle(&mut link, slot, &mut ignore))
        .await
        .expect("in time")
        .expect("an answer");
    assert_eq!(outcome, PowerOutcome::Done);
}

#[tokio::test(start_paused = true)]
async fn ops_clear_bonds_reads_status_back_when_over_usb() {
    let mut link = sim(1).await;
    let cleared = timeout(WAIT, ops::clear_bonds(&mut link, Link::Usb, &mut ignore))
        .await
        .expect("in time")
        .expect("cleared");
    let Cleared::Status(status) = cleared else { panic!("over USB, a status: {cleared:?}") };
    assert_eq!(status.bluetooth.bonds, 0);
}

#[tokio::test(start_paused = true)]
async fn ops_clear_bonds_ends_on_disconnect_when_over_bluetooth() {
    let (mut link, plumbing) = link_pair();
    let down = LinkEvent::Disconnected {
        reason: "the bridge dropped this host".to_owned(),
        cause: DisconnectCause::Other,
    };
    plumbing.events.send(down).await.unwrap();

    let cleared = ops::clear_bonds(&mut link, Link::Bluetooth, &mut ignore).await;

    assert!(matches!(cleared, Ok(Cleared::Disconnected)), "{cleared:?}");
}

#[tokio::test(start_paused = true)]
async fn ops_status_and_plan_keeps_status_when_it_arrives_late() {
    let (mut link, plumbing) = link_pair();
    // The answer comes half a second before the wait would run out, and no plan follows it.
    let answer = tokio::spawn(async move {
        tokio::time::sleep(ops::REPLY_TIMEOUT - Duration::from_millis(500)).await;
        plumbing.events.send(LinkEvent::Message(status_message(ChannelPool::Us))).await.unwrap();
        plumbing
    });

    let (status, plan) = ops::status_and_plan(&mut link, ops::REPLY_TIMEOUT, &mut ignore)
        .await
        .expect("the late status is kept");

    assert_eq!(status.pool, ChannelPool::Us);
    assert!(plan.is_none(), "no plan came behind it");
    drop(answer.await.expect("the scripted bridge"));
}

/// The core of a bridge one major past this host's.
fn other_major() -> BridgeCore {
    let version = Version::new(BRIDGE_VERSION.major + 1, 0, 0);
    BridgeCore {
        mac: [0x02, 0x00, 0x5E, 0x10, 0x44, 0xC0],
        image: None,
        version,
        trial: Trial::Settled,
        uptime_ms: 400,
        standing: Standing::of(version, BRIDGE_VERSION, false),
    }
}

/// What the transport does with a `Ready` whose rest goes unread: the send rule, then the event.
async fn answer_unread(plumbing: &LinkPlumbing) {
    plumbing.compatibility.heard(&other_major());
    let unread = LinkEvent::Incompatible { core: other_major(), fresh: true };
    plumbing.events.send(unread).await.expect("the link reads");
}

#[tokio::test(start_paused = true)]
async fn ops_report_incompatible_when_answer_to_command_queued_before_any_ready_is_unread() {
    // Nothing was refused up front: no `Ready` had arrived when the status was asked for.
    let (mut link, plumbing) = link_pair();
    let mut sink = ignore;
    let started = tokio::time::Instant::now();
    let asked = ops::status(&mut link, ops::REPLY_TIMEOUT, &mut sink);
    let answered = async {
        tokio::time::sleep(Duration::from_millis(10)).await;
        answer_unread(&plumbing).await;
    };
    let (ended, ()) = tokio::join!(asked, answered);
    assert!(
        matches!(ended, Err(LinkError::Incompatible(core)) if core == other_major()),
        "{ended:?}"
    );
    assert_eq!(started.elapsed(), Duration::from_millis(10), "at the answer, not the timeout");
}

#[tokio::test(start_paused = true)]
async fn ops_report_incompatible_when_bridge_turns_incompatible_during_a_wait() {
    // A bridge that reboots into another major while a setting settles, and while the bonds clear
    // over Bluetooth.
    let (mut link, plumbing) = link_pair();
    plumbing.events.send(LinkEvent::Message(status_message(ChannelPool::Us))).await.unwrap();
    answer_unread(&plumbing).await;
    let ended = ops::settle(&mut link, Duration::from_secs(1), &mut ignore).await;
    assert!(matches!(ended, Err(LinkError::Incompatible(_))), "{ended:?}");

    let (mut link, plumbing) = link_pair();
    let mut sink = ignore;
    let cleared = ops::clear_bonds(&mut link, Link::Bluetooth, &mut sink);
    let answered = async {
        tokio::time::sleep(Duration::from_millis(10)).await;
        answer_unread(&plumbing).await;
    };
    let (ended, ()) = tokio::join!(cleared, answered);
    assert!(matches!(ended, Err(LinkError::Incompatible(_))), "{ended:?}");
}
