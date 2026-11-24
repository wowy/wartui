//! `wartui sniff` — decode and print, and nothing else.
//!
//! No database, no fleet table, no transmit. The point is to be able to say
//! "the bridge hears the fleet and the decoder agrees" with nothing else in the
//! way, and to have something to reach for later when a node goes quiet and the
//! question is whether its records are arriving at all.

use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::Result;
use clap::Args as ClapArgs;
use wartui_bridge::LinkEvent;
use wartui_proto::beacon::rcoi_text;
use wartui_proto::bus::{
    Compat, DecodeError, Heartbeat, NODE_VERSION, NodeBoard, RecordKind, SightingMsg, Slot, UpMsg,
    UpType,
};
use wartui_proto::hci::BleTrailer;
use wartui_proto::link::BridgeToHost;
use wartui_proto::mac;

use crate::text::{hex, message_text};

#[derive(ClapArgs)]
pub struct Args {
    /// The bridge, as a device path, the board's address, or the end of it (`00:08`). Detected
    /// if omitted.
    #[arg(long, value_name = "PATH|MAC")]
    bridge: Option<String>,

    #[command(flatten)]
    link_flag: crate::config::LinkFlag,

    /// Read the remembered bridge from this file instead of the default `wartui.toml`. A file
    /// that does not load is no bridge remembered. Never written; see
    /// `crates/wartui/README.md` § "Config file".
    #[arg(long, value_name = "PATH")]
    config: Option<PathBuf>,

    /// Use the built-in simulator with this many fake nodes instead of hardware.
    #[arg(
        long,
        value_name = "NODES",
        num_args = 0..=1,
        default_missing_value = "3",
        value_parser = crate::sim_nodes(),
        conflicts_with = "link"
    )]
    sim: Option<u8>,

    /// Also print the raw bytes of every record.
    #[arg(long)]
    raw: bool,

    /// Print the bridge's own log lines, and its GPS receiver's bytes as text.
    #[arg(long)]
    verbose: bool,
}

pub async fn run(args: Args) -> Result<()> {
    let (mut link, over) = super::open_remembered(
        args.bridge.as_deref(),
        args.sim,
        args.link_flag.link,
        args.config.as_deref(),
    )?;
    println!("# waiting for the bridge; ctrl-c to stop");

    let mut counts = Counts::default();

    // Said once, and only into the silence it describes: `sniff` keeps waiting
    // afterwards, and waiting silently is how a wedged dongle passes for a quiet fleet.
    let mut spoken = false;
    // Built before the loop, not inside the arm below: see `crate::Terminate`.
    let mut terminate = crate::Terminate::new();
    let notice = tokio::time::sleep(super::connect_wait(over));
    tokio::pin!(notice);

    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => break,
            // Same reasoning as the view's: leaving without releasing the port
            // is what costs a replug. See `crate::Terminate`.
            () = terminate.recv() => break,
            () = &mut notice, if !spoken => {
                spoken = true;
                if !counts.heard_a_bridge {
                    eprintln!("\n{}\n", super::no_bridge_notice(args.bridge.as_deref(), over).await);
                }
            }
            event = link.recv() => match event {
                Some(event) => handle(event, &args, &mut counts),
                None => {
                    println!("# link closed");
                    break;
                }
            },
        }
    }

    println!(
        "\n# {} records: {} sighting in {} batches, {} heartbeat, {} update status, {} panic, \
         {} undecodable",
        counts.total,
        counts.sighting,
        counts.batches,
        counts.heartbeat,
        counts.update_status,
        counts.panic,
        counts.undecodable
    );
    if counts.update_status > 0 {
        println!(
            "# {} update statuses arrived: a node is answering an update session this \
             host is not running.",
            counts.update_status
        );
    }
    if counts.incompatible > 0 {
        println!(
            "# {} records were in a layout this build does not read. Those nodes are \
             invisible to `run` until they are updated with firmware/node.",
            counts.incompatible
        );
    }
    if counts.needs_update > 0 {
        println!(
            "# {} heartbeats came from nodes on firmware older than {NODE_VERSION}. `run` \
             shows those nodes as `needs update` and assigns them nothing.",
            counts.needs_update
        );
    }
    if counts.host_behind > 0 {
        println!(
            "# {} heartbeats came from nodes on firmware newer than {NODE_VERSION}. `run` \
             shows those nodes as `host behind` and assigns them nothing; update wartui.",
            counts.host_behind
        );
    }
    Ok(())
}

#[derive(Default)]
struct Counts {
    /// Whether anything behind the port has behaved like a bridge: an announcement, a
    /// decoded message of any kind, or the transport saying why the link is down. An
    /// undecodable frame is not one of them — see [`handle`].
    heard_a_bridge: bool,
    /// What carries the nodes, from the bridge's announcement, which names each slot. No carrier
    /// until one arrives.
    board: Option<NodeBoard>,
    total: u64,
    /// Sightings seen, across every batch — not the number of records.
    sighting: u64,
    /// Sighting batches, each carrying one or more sightings.
    batches: u64,
    heartbeat: u64,
    /// Update statuses nodes sent, answering some host's session.
    update_status: u64,
    /// Panic records: a node saying why its last boot ended.
    panic: u64,
    /// Records in a layout this build does not read, as the engine counts them, which is what a
    /// fleet half-way through a reflash looks like.
    incompatible: u64,
    /// Heartbeats from nodes on an older major or minor, read from their prefix.
    needs_update: u64,
    /// Heartbeats from nodes on a newer major or minor, read the same way.
    host_behind: u64,
    undecodable: u64,
    /// Each slot's standing from its latest heartbeat, so its sighting batches are read only
    /// when the engine would read them.
    compat: HashMap<Slot, Compat>,
}

fn handle(event: LinkEvent, args: &Args, counts: &mut Counts) {
    let LinkEvent::Message(message) = &event else {
        // `Garbled` deliberately does not count: undecodable bytes are the loudest
        // symptom of the very thing the notice explains, so treating one as proof a
        // bridge answered would silence it in the case it was written for.
        if !matches!(event, LinkEvent::Garbled(_)) {
            counts.heard_a_bridge = true;
        }
        if let Some(line) = super::describe(&event) {
            println!("# {line}");
        }
        // Printed here rather than under `Ready`, which the transport turns into this
        // event. A bridge that reboots mid-capture arrives here a second time, and
        // `sniff` being the raw view, the fields go out as the bridge sent them.
        if let LinkEvent::Connected(info) = &event {
            println!(
                "#   reset {:?}, last phase {:?}, up {}ms",
                info.reset_cause, info.last_phase, info.uptime_ms
            );
            if info.node_board.is_carrier() {
                println!("#   nodes on {}", info.node_board);
            } else {
                println!("#   {}", info.node_board);
            }
            counts.board = Some(info.node_board);
        }
        return;
    };

    // Any decoded message, not just `Connected`: a busy fleet can cost a bridge every
    // `Ready` to its oldest-first transmit rings while its observations arrive
    // perfectly well, and accusing it underneath a screenful of decoded records is the
    // one output worse than saying nothing.
    counts.heard_a_bridge = true;

    match message {
        BridgeToHost::FromNode { slot, rx_us, kind, body } => {
            counts.total += 1;
            let head = head(*rx_us, *slot, counts);
            record(*slot, &head, *kind, body, counts);
            if args.raw {
                println!("      {}", hex(body));
            }
        }

        // The receiver's stream as the bridge read it, split wherever a batch ended. The
        // text is how the wiring is checked by eye, so it is shown escaped, not parsed.
        BridgeToHost::Gps { bytes } => {
            println!("{}", message_line(message, NodeBoard::NoCarrier));
            if args.verbose {
                println!("#   {}", String::from_utf8_lossy(bytes).escape_debug());
            }
            if args.raw {
                println!("      {}", hex(bytes));
            }
        }

        BridgeToHost::Log { .. } if !args.verbose => {}

        _ => {
            let board = counts.board.unwrap_or(NodeBoard::NoCarrier);
            println!("{}", message_line(message, board));
            // The P4's plan: each slot's share under its mode.
            if let (BridgeToHost::Fleet(report), true) = (message, args.verbose) {
                for share in &report.shares {
                    let job = if share.ble {
                        "bluetooth".to_owned()
                    } else {
                        format!("{} channels", share.channels.len())
                    };
                    println!("#   {} epoch {}: {job}", board.label(share.slot), share.epoch);
                }
            }
        }
    }
}

/// The line for a message the bridge sent: its one-liner as `wartui sd-log` prints it too,
/// behind `sniff`'s own label.
fn message_line(message: &BridgeToHost, board: NodeBoard) -> String {
    let label = match message {
        // Unreachable as the transport stands: every `Ready` it decodes becomes a
        // `LinkEvent::Connected`, handled in `handle`, and none is forwarded as a message. Kept
        // because the match is exhaustive over the wire format rather than over what happens to
        // arrive.
        BridgeToHost::Ready { .. } => "bridge ready: ",
        BridgeToHost::Status { .. } => "status: ",
        BridgeToHost::Log { .. } => "bridge log: ",
        BridgeToHost::Error { .. } => "bridge error: ",
        BridgeToHost::Gps { .. } => "gps  ",
        // Answers `flash-bridge`'s update ops, which `sniff` never sends.
        BridgeToHost::Update(_) => "update status ",
        // Answers `ota`'s cache upload, which `sniff` never sends.
        BridgeToHost::CacheImage(_) => "cache status ",
        BridgeToHost::Fleet(_) => "fleet ",
        // Answers a format of the bridge's microSD card, which `sniff` never asks for.
        BridgeToHost::SdFormatted { .. } => "sd ",
        // Each says what it is: a node's record, answers to `ToNode` and `power-cycle`, which
        // `sniff` never sends.
        BridgeToHost::FromNode { .. }
        | BridgeToHost::Delivered { .. }
        | BridgeToHost::Undelivered { .. }
        | BridgeToHost::PowerCycled { .. } => "",
    };
    format!("# {label}{}", message_text(message, board).text)
}

/// The start of each line about a record: the bridge's stamp, and the slot as the operator
/// reads it on the board the bridge announced.
fn head(rx_us: u32, slot: Slot, counts: &Counts) -> String {
    let board = counts.board.unwrap_or(NodeBoard::NoCarrier);
    format!("{rx_us:>10}us  {}", board.label(slot))
}

/// One record from the node in `slot`, decoded and counted as the engine reads it.
fn record(slot: Slot, head: &str, kind: u8, body: &[u8], counts: &mut Counts) {
    // A heartbeat from another major or minor opens with the prefix, which names the node's
    // firmware. The rest is not this build's layout, even when its length happens to match.
    let undrivable = (kind == UpType::Heartbeat.as_u8())
        .then(|| Heartbeat::identity(body).ok())
        .flatten()
        .filter(|(caps, _)| !caps.version.compat_with_host().drivable());
    if let Some((caps, mac)) = undrivable {
        counts.compat.insert(slot, caps.version.compat_with_host());
        let mac = mac::full(&mac);
        if caps.version.compat_with_host().node_behind() {
            counts.needs_update += 1;
            println!("{head}  HEARTBEAT {mac} {caps}, needs update to {NODE_VERSION}");
        } else {
            counts.host_behind += 1;
            println!("{head}  HEARTBEAT {mac} {caps}, newer than this host's {NODE_VERSION}");
        }
        return;
    }
    match UpMsg::decode(kind, body) {
        Ok(UpMsg::Heartbeat(heartbeat)) => {
            counts.heartbeat += 1;
            counts.compat.insert(slot, heartbeat.capabilities.version.compat_with_host());
            println!(
                "{head}  HEARTBEAT {} #{} e{} {}  {}",
                mac::full(&heartbeat.mac),
                heartbeat.counter,
                heartbeat.epoch,
                heartbeat.reset_cause,
                heartbeat.capabilities
            );
        }
        // Not this build's layout once the slot's heartbeat has named another major or minor,
        // even when it parses, as the engine reads it.
        Ok(UpMsg::Sightings(sightings))
            if counts.compat.get(&slot).is_some_and(|compat| !compat.drivable()) =>
        {
            counts.incompatible += 1;
            println!("{head}  B#{} from a node on another minor, not read", sightings.seq);
        }
        Ok(UpMsg::Sightings(sightings)) => {
            counts.batches += 1;
            for (i, (sighting, _raw)) in sightings.iter().enumerate() {
                counts.sighting += 1;
                println!(
                    "{head}  B#{} {}/{}  {}",
                    sightings.seq,
                    i + 1,
                    sightings.count,
                    render(&sighting)
                );
            }
        }
        Ok(UpMsg::UpdateStatus(status)) => {
            counts.update_status += 1;
            println!(
                "{head}  UPDATE STATUS s{} {:?} next @{}",
                status.session, status.state, status.next_offset
            );
        }
        Ok(UpMsg::Panic(_))
            if counts.compat.get(&slot).is_some_and(|compat| !compat.drivable()) =>
        {
            counts.incompatible += 1;
            println!("{head}  PANIC from a node on another minor, not read");
        }
        Ok(UpMsg::Panic(panic)) => {
            counts.panic += 1;
            println!("{head}  PANIC {} last boot: {}", mac::full(&panic.mac), panic.text);
        }
        // Named rather than left as "undecodable": a fleet half-way through a reflash is
        // exactly what this looks like.
        Err(DecodeError::BadLength { need, got }) => {
            counts.incompatible += 1;
            println!("{head}  record is {got} bytes, not {need}");
        }
        Err(err) => {
            counts.undecodable += 1;
            println!("{head}  type {kind}, undecodable: {err:?}");
        }
    }
}

/// One observation, in the order the firmware wrote it.
fn render(sighting: &SightingMsg<'_>) -> String {
    let kind = match sighting.kind {
        RecordKind::Wifi => "wifi",
        RecordKind::Ble => "ble ",
    };
    // What the trailer carries depends on the kind, the same split the engine
    // makes of it: roaming consortium identifiers for Wi-Fi, a manufacturer
    // identifier for BLE.
    let trailer = match sighting.kind {
        RecordKind::Wifi if !sighting.ext.is_empty() => {
            format!("  rcoi {}", rcoi_text(sighting.ext))
        }
        RecordKind::Ble => BleTrailer::decode(sighting.ext)
            .and_then(|trailer| trailer.mfgr)
            .map_or_else(String::new, |id| format!("  mfgr {id}")),
        RecordKind::Wifi => String::new(),
    };
    format!(
        "{kind}  {}  ch {:>3}  {:>4}dBm  {:<16}  {}{trailer}",
        mac::full(&sighting.bssid),
        sighting.channel,
        sighting.rssi,
        sighting.security.to_string(),
        // SSIDs are whatever the access point beaconed, not text, and hidden
        // networks beacon an empty one.
        if sighting.ssid.is_empty() {
            "<hidden>".to_string()
        } else {
            format!("{:?}", String::from_utf8_lossy(sighting.ssid))
        }
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use wartui_proto::link::LinkError;

    fn args() -> Args {
        Args {
            bridge: None,
            link_flag: crate::config::LinkFlag::default(),
            config: None,
            sim: None,
            raw: false,
            verbose: false,
        }
    }

    fn heard(event: LinkEvent) -> bool {
        let mut counts = Counts::default();
        handle(event, &args(), &mut counts);
        counts.heard_a_bridge
    }

    #[test]
    fn sniff_cmd_ignores_garbled_frame_when_tracking_bridge_liveness() {
        // The board that produces these fastest is one running node firmware,
        // which is the first cause the notice names. Counting a garbled frame
        // as contact would suppress the notice permanently after one of them.
        assert!(!heard(LinkEvent::Garbled(LinkError::Corrupt)));
    }

    #[test]
    fn sniff_cmd_increments_decoded_counter_when_sighting_arrives_without_announcement() {
        // `Ready` can be lost to the bridge's transmit rings while everything
        // else it sends arrives; frames on screen must not be contradicted.
        let status = BridgeToHost::Status {
            rx_count: 0,
            dropped_tx: 0,
            uptime_ms: 1_000,
            rx_oversize: 0,
            queues: wartui_proto::link::QueueStats::new(),
            radio_link: wartui_proto::link::RadioLinkStats::default(),
            host_uart_errors: 0,
            fan: None,
            pool: wartui_proto::plan::ChannelPool::All,
            scan: wartui_proto::fleet::ScanMode::Stopped,
            ble: None,
            present: 0,
            members: Default::default(),
            sd: None,
            bluetooth: Default::default(),
        };
        assert!(heard(LinkEvent::Message(status)));
    }

    #[test]
    fn sniff_cmd_prints_label_and_shared_text_when_bridge_sends_a_message() {
        use wartui_proto::link::{LogStr, PowerOutcome, SdFormatOutcome};
        let board = NodeBoard::Carrier14;
        let log = BridgeToHost::Log { message: LogStr::try_from("hello").expect("short") };
        let error = BridgeToHost::Error { message: LogStr::try_from("no").expect("short") };
        let cycled = BridgeToHost::PowerCycled {
            slot: Slot::new(1, 0).expect("B1"),
            outcome: PowerOutcome::Done,
        };
        let lines = [
            (log, "# bridge log: hello"),
            (error, "# bridge error: no"),
            (BridgeToHost::Delivered { id: 4 }, "# record 4 delivered"),
            (cycled, "# N8 power-cycled"),
            (BridgeToHost::SdFormatted { outcome: SdFormatOutcome::Done }, "# sd formatted: Done"),
        ];
        for (message, line) in lines {
            assert_eq!(message_line(&message, board), line);
        }
    }

    #[test]
    fn sniff_cmd_names_slot_by_silk_when_bridge_announces_a_carrier() {
        let b1 = Slot::new(1, 0).expect("B1");
        let mut counts = Counts::default();
        assert_eq!(head(7, b1, &counts), "         7us  B1");
        let info = wartui_bridge::BridgeInfo {
            mac: [0; 6],
            fw_version: String::new(),
            reset_cause: wartui_proto::reset::ResetCause::PowerOn,
            last_phase: wartui_proto::link::LoopPhase::Unknown,
            uptime_ms: 0,
            node_board: NodeBoard::Carrier14,
            radio: None,
            image: None,
        };
        handle(LinkEvent::Connected(info), &args(), &mut counts);
        assert_eq!(head(7, b1, &counts), "         7us  N8");
    }

    #[test]
    fn sniff_cmd_marks_bridge_heard_when_receiving_disconnected_event_with_reason() {
        let cause = wartui_bridge::DisconnectCause::Other;
        assert!(heard(LinkEvent::Disconnected { reason: "could not open".to_owned(), cause }));
    }

    /// Count one record the way `handle` does.
    fn counted(kind: UpType, body: &[u8]) -> Counts {
        let mut counts = Counts::default();
        feed(&mut counts, kind, body);
        counts
    }

    /// Hand `handle` one record from the same slot every time.
    fn feed(counts: &mut Counts, kind: UpType, body: &[u8]) {
        let rx = BridgeToHost::FromNode {
            slot: Slot::new(0, 0).expect("A1"),
            rx_us: 0,
            kind: kind.as_u8(),
            body: wartui_proto::link::FromNodeBody::from_slice(body).expect("fits"),
        };
        handle(LinkEvent::Message(rx), &args(), counts);
    }

    fn one_sighting() -> Vec<u8> {
        use wartui_proto::bus::{Security, SightingBatchWriter};
        let mut writer = SightingBatchWriter::new(1);
        let msg = SightingMsg {
            kind: RecordKind::Wifi,
            bssid: [0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF],
            channel: 6,
            rssi: -60,
            security: Security::Wpa2Psk,
            ssid: b"example",
            ext: &[],
        };
        assert!(writer.push(&msg), "fits");
        writer.as_bytes().to_vec()
    }

    #[test]
    fn sniff_cmd_counts_batch_incompatible_when_its_node_runs_another_minor() {
        let mut counts = Counts::default();
        let behind = heartbeat_with(wartui_bridge::sim::minor_behind(NODE_VERSION));
        feed(&mut counts, UpType::Heartbeat, &behind);
        feed(&mut counts, UpType::Sightings, &one_sighting());
        assert_eq!((counts.incompatible, counts.batches, counts.sighting), (1, 0, 0));

        // The same node, reflashed: its batches read again.
        feed(&mut counts, UpType::Heartbeat, &heartbeat_with(NODE_VERSION));
        feed(&mut counts, UpType::Sightings, &one_sighting());
        assert_eq!((counts.incompatible, counts.batches, counts.sighting), (1, 1, 1));
    }

    fn heartbeat_with(version: wartui_proto::bus::Version) -> Vec<u8> {
        use wartui_proto::bus::{Board, Capabilities, DieTemp, NodeBus, Vdd};
        Heartbeat {
            capabilities: Capabilities { version, ..Capabilities::here(Board::Esp32c5) },
            mac: [0x02, 0, 0, 0, 0x57, 0x84],
            counter: 1,
            epoch: 0,
            wifi_refused: 0,
            ble_refused: 0,
            reset_cause: wartui_proto::reset::ResetCause::PowerOn,
            vdd: Vdd::NONE,
            die_temp: DieTemp::NONE,
            heap_free: 0,
            heap_used: 0,
            heap_high_water: 0,
            bus: NodeBus::default(),
        }
        .encode()
        .to_vec()
    }

    #[test]
    fn sniff_cmd_counts_update_status_when_node_answers_a_session() {
        use wartui_proto::bus::{UpdateState, UpdateStatus};
        let status = UpdateStatus { session: 1, state: UpdateState::Receiving, next_offset: 4096 };
        let counts = counted(UpType::UpdateStatus, &status.encode());
        assert_eq!((counts.update_status, counts.undecodable), (1, 0));
    }

    #[test]
    fn sniff_cmd_counts_panic_when_node_reports_last_boot() {
        use wartui_proto::bus::{PANIC_MAX, PanicMsg};
        let mut body = [0; PANIC_MAX];
        let msg = PanicMsg { mac: [0x02, 0, 0, 0, 0x57, 0x84], text: "src/main.rs:1:1: oops" };
        let len = msg.encode_into(&mut body).expect("fits");
        let counts = counted(UpType::Panic, &body[..len]);
        assert_eq!((counts.panic, counts.undecodable), (1, 0));
    }

    #[test]
    fn sniff_cmd_counts_heartbeat_when_node_runs_this_minor() {
        let counts = counted(UpType::Heartbeat, &heartbeat_with(NODE_VERSION));
        assert_eq!((counts.heartbeat, counts.needs_update, counts.host_behind), (1, 0, 0));
    }

    #[test]
    fn sniff_cmd_counts_needs_update_when_full_length_heartbeat_names_older_minor() {
        // Full length, so it would decode, but past the prefix its fields are not ours.
        let counts = counted(
            UpType::Heartbeat,
            &heartbeat_with(wartui_bridge::sim::minor_behind(NODE_VERSION)),
        );
        assert_eq!((counts.heartbeat, counts.needs_update, counts.incompatible), (0, 1, 0));
    }

    #[test]
    fn sniff_cmd_counts_host_behind_when_heartbeat_names_newer_minor() {
        let minor = NODE_VERSION.minor + 1;
        let ahead = wartui_proto::bus::Version { minor, ..NODE_VERSION };
        let mut body = heartbeat_with(ahead);
        body.truncate(14);
        let counts = counted(UpType::Heartbeat, &body);
        assert_eq!((counts.heartbeat, counts.host_behind, counts.incompatible), (0, 1, 0));
    }

    #[test]
    fn sniff_cmd_counts_incompatible_when_this_minor_sends_wrong_length() {
        let mut body = heartbeat_with(NODE_VERSION);
        body.push(0);
        let counts = counted(UpType::Heartbeat, &body);
        assert_eq!((counts.incompatible, counts.undecodable, counts.heartbeat), (1, 0, 0));
    }

    #[test]
    fn sniff_cmd_counts_undecodable_when_record_type_is_unknown() {
        let mut counts = Counts::default();
        let rx = BridgeToHost::FromNode {
            slot: Slot::new(0, 0).expect("A1"),
            rx_us: 0,
            kind: 0x42,
            body: wartui_proto::link::FromNodeBody::from_slice(b"anything").expect("fits"),
        };
        handle(LinkEvent::Message(rx), &args(), &mut counts);
        assert_eq!(
            (counts.total, counts.undecodable, counts.incompatible, counts.heartbeat),
            (1, 1, 0, 0)
        );
        assert!(counts.compat.is_empty());
    }
}
