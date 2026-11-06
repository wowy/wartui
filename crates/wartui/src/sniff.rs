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
    Compat, DecodeError, Heartbeat, NODE_VERSION, RecordKind, SightingMsg, Slot, UpMsg, UpType,
};
use wartui_proto::link::BridgeToHost;
use wartui_proto::mac;

#[derive(ClapArgs)]
pub struct Args {
    /// The bridge, as a device path, the board's address, or the end of it (`00:08`). Detected
    /// if omitted.
    #[arg(long, value_name = "PATH|MAC")]
    bridge: Option<String>,

    /// Read the remembered bridge from this file instead of the default `wartui.toml`. A file
    /// that does not load is no bridge remembered. Never written; see
    /// `crates/wartui/README.md` § "Config file".
    #[arg(long, value_name = "PATH")]
    config: Option<PathBuf>,

    /// Use the built-in simulator with this many fake nodes instead of hardware.
    #[arg(long, value_name = "NODES", num_args = 0..=1, default_missing_value = "3")]
    sim: Option<u8>,

    /// Also print the raw bytes of every record.
    #[arg(long)]
    raw: bool,

    /// Print the bridge's own log lines, and its GPS receiver's bytes as text.
    #[arg(long)]
    verbose: bool,
}

pub async fn run(args: Args) -> Result<()> {
    let mut link =
        super::open(args.bridge.as_deref(), args.sim, super::memory(args.config.as_deref()))?;
    println!("# waiting for the bridge; ctrl-c to stop");

    let mut counts = Counts::default();

    // Said once, and only into the silence it describes: `sniff` keeps waiting
    // afterwards, and waiting silently is how a wedged dongle passes for a quiet fleet.
    let mut spoken = false;
    // Built before the loop, not inside the arm below: see `crate::Terminate`.
    let mut terminate = crate::Terminate::new();
    let notice = tokio::time::sleep(super::CONNECT_NOTICE_AFTER);
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
                    eprintln!("\n{}\n", super::no_bridge_notice(args.bridge.as_deref()));
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
        "\n# {} records: {} sighting in {} batches, {} heartbeat, {} update status, \
         {} undecodable",
        counts.total,
        counts.sighting,
        counts.batches,
        counts.heartbeat,
        counts.update_status,
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
             invisible to `run` until they are reflashed with firmware/node.",
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
    total: u64,
    /// Sightings seen, across every batch — not the number of records.
    sighting: u64,
    /// Sighting batches, each carrying one or more sightings.
    batches: u64,
    heartbeat: u64,
    /// Update statuses nodes sent, answering some host's session.
    update_status: u64,
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
                "#   reset {:?}, last phase {:?}, {} bytes of heap free, up {}ms",
                info.reset_cause, info.last_phase, info.heap_free, info.uptime_ms
            );
            // A port, not a receiver: one that does not answer is a bridge `Error`.
            println!("#   {}", if info.gps { "gps port" } else { "no gps port" });
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
            let head = format!("{rx_us:>10}us  {slot}");
            record(*slot, &head, *kind, body, counts);
            if args.raw {
                println!("      {}", hex(body));
            }
        }

        BridgeToHost::Ready { chip, mac: bridge_mac, fw_version, proto_version, .. } => {
            // Unreachable as the transport stands: every `Ready` it decodes
            // becomes a `LinkEvent::Connected`, handled above, and none is
            // forwarded as a message. Kept because the match is exhaustive
            // over the wire format rather than over what happens to arrive.
            println!(
                "# bridge ready: {chip:?} {} firmware {fw_version} link v{proto_version}",
                mac::full(bridge_mac)
            );
        }

        BridgeToHost::Status { rx_count, dropped_tx, uptime_ms, rx_oversize, .. } => {
            println!(
                "# status: {rx_count} received, {dropped_tx} dropped, {rx_oversize} oversize, \
                 up {}s",
                uptime_ms / 1000
            );
        }

        BridgeToHost::Log { level, message } => {
            if args.verbose {
                println!("# bridge {level:?}: {message}");
            }
        }

        BridgeToHost::Error { message } => println!("# bridge error: {message}"),

        // The receiver's stream as the bridge read it, split wherever a batch ended. The
        // text is how the wiring is checked by eye, so it is shown escaped, not parsed.
        BridgeToHost::Gps { bytes } => {
            println!("# gps  {} bytes", bytes.len());
            if args.verbose {
                println!("#   {}", String::from_utf8_lossy(bytes).escape_debug());
            }
            if args.raw {
                println!("      {}", hex(bytes));
            }
        }

        // Answers `flash-bridge`'s update ops, which `sniff` never sends.
        BridgeToHost::Update(status) => println!("# update status {}", hex(status)),

        // Answers to `ToNode`, which `sniff` never sends.
        BridgeToHost::Delivered { id } => println!("# record {id} delivered"),
        BridgeToHost::Undelivered { id, reason } => {
            println!("# record {id} undelivered: {reason:?}");
        }
    }
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
        RecordKind::Ble if sighting.ext.len() == 2 => {
            format!("  mfgr {}", u16::from_le_bytes([sighting.ext[0], sighting.ext[1]]))
        }
        _ => String::new(),
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

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join("")
}

#[cfg(test)]
mod tests {
    use super::*;
    use wartui_proto::link::LinkError;

    fn args() -> Args {
        Args { bridge: None, config: None, sim: None, raw: false, verbose: false }
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
            radio_link: None,
            host_uart_errors: 0,
        };
        assert!(heard(LinkEvent::Message(status)));
    }

    #[test]
    fn sniff_cmd_marks_bridge_heard_when_receiving_disconnected_event_with_reason() {
        assert!(heard(LinkEvent::Disconnected { reason: "could not open".to_owned() }));
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
        use wartui_proto::bus::{Board, Capabilities, DieTemp, Vdd};
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
