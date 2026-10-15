//! `wartui sniff` — decode and print, and nothing else.
//!
//! No database, no fleet table, no transmit. The point is to be able to say
//! "the bridge hears the fleet and the decoder agrees" with nothing else in the
//! way, and to have something to reach for later when a node goes quiet and the
//! question is whether the frames are arriving at all.

use anyhow::Result;
use clap::Args as ClapArgs;
use wartui_bridge::LinkEvent;
use wartui_proto::air::{
    Capabilities, DecodeError, Frame, NODE_VERSION, RecordKind, SightingMsg, foreign,
};
use wartui_proto::beacon::rcoi_text;
use wartui_proto::link::{BROADCAST, BridgeToHost};
use wartui_proto::mac;

#[derive(ClapArgs)]
pub struct Args {
    /// The bridge, as a device path, the board's address, or the end of it (`00:08`). Detected
    /// if omitted.
    #[arg(long, value_name = "PATH|MAC")]
    bridge: Option<String>,

    /// Use the built-in simulator with this many fake nodes instead of hardware.
    #[arg(long, value_name = "NODES", num_args = 0..=1, default_missing_value = "3")]
    sim: Option<u8>,

    /// Also print the raw bytes of every frame.
    #[arg(long)]
    raw: bool,

    /// Print the bridge's own log lines.
    #[arg(long)]
    verbose: bool,
}

pub async fn run(args: Args) -> Result<()> {
    let mut link = super::open(
        args.bridge.as_deref(),
        args.sim,
        0,
        super::memory(args.bridge.as_deref(), super::Remember::Detected),
    )?;
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
        "\n# {} frames: {} sighting in {} batches, {} heartbeat, {} admin, {} not ours, \
         {} undecodable",
        counts.total,
        counts.sighting,
        counts.batches,
        counts.heartbeat,
        counts.admin,
        counts.foreign_fleet + counts.foreign_admin,
        counts.undecodable
    );
    if counts.foreign_fleet > 0 {
        println!(
            "# {} frames came from a fleet running the vendor firmware. It is not ours \
             and cannot be driven from here, but it is transmitting on the channel these \
             nodes listen on.",
            counts.foreign_fleet
        );
    }
    // Every assignment on the air during a sniff came from somewhere else: this
    // command holds the port and a radio does not hear its own frames. One in our own
    // format is the harder to notice any other way, because our nodes obey a second
    // wartui core's assignments and the fleet table cannot show the difference.
    if counts.admin > 0 {
        println!(
            "# {} assignments arrived in wartui's own wire format, and this host sent \
             none of them. A second wartui core is driving these nodes, and they are \
             taking assignments from both.",
            counts.admin
        );
    }
    if counts.foreign_admin > 0 {
        println!(
            "# {} assignments arrived from a vendor core. Something other than this host \
             is telling a fleet nearby what to scan.",
            counts.foreign_admin
        );
    }
    if counts.incompatible > 0 {
        println!(
            "# {} frames were ours but in a wire version or layout this build does not \
             read. Those nodes are invisible to `run` until they are reflashed with \
             firmware/node.",
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
    /// Records seen, across every batch — not the number of `Rx` frames.
    sighting: u64,
    /// Sighting-batch frames, each carrying one or more `sighting` records.
    batches: u64,
    heartbeat: u64,
    admin: u64,
    /// Vendor heartbeats and observations. Another fleet is on this channel,
    /// transmitting where these nodes are listening.
    foreign_fleet: u64,
    /// Another core's assignment. Counted apart from `foreign_fleet` because
    /// seeing any at all means something else is driving a fleet nearby.
    foreign_admin: u64,
    /// Frames of ours in a wire version or layout this build does not read, as the engine
    /// counts them, which is what a fleet half-way through a reflash looks like.
    incompatible: u64,
    /// Heartbeats from nodes on an older major or minor, read from their frozen prefix.
    needs_update: u64,
    /// Heartbeats from nodes on a newer major or minor, read the same way.
    host_behind: u64,
    undecodable: u64,
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
            // What the bridge said about its own screen, printed as it said it:
            // this is the field that decides whether the host pushes a panel at all.
            match info.panel {
                Some(panel) => println!("#   panel {}x{}", panel.cols, panel.rows),
                None => println!("#   no panel"),
            }
        }
        return;
    };

    // Any decoded message, not just `Connected`: a busy fleet can cost a bridge every
    // `Ready` to its oldest-first transmit rings while its observations arrive
    // perfectly well, and accusing it underneath a screenful of decoded frames is the
    // one output worse than saying nothing.
    counts.heard_a_bridge = true;

    match message {
        BridgeToHost::Rx { src, dst, rssi, rx_us, payload, .. } => {
            counts.total += 1;
            let addressing = if *dst == BROADCAST { "bcast" } else { "unicast" };
            let head =
                format!("{:>10}us  {}  {rssi:>4}dBm  {addressing:>7}", rx_us, mac::full(src));

            // A heartbeat from another major or minor opens with the frozen prefix, which
            // names the node's firmware. The rest is not this build's layout, even when its
            // length happens to match.
            let undrivable = Capabilities::decode_prefix(payload)
                .ok()
                .filter(|caps| !caps.version.compat_with_host().drivable());
            if let Some(caps) = undrivable {
                if caps.version.compat_with_host().node_behind() {
                    counts.needs_update += 1;
                    println!("{head}  HEARTBEAT {caps}, needs update to {NODE_VERSION}");
                } else {
                    counts.host_behind += 1;
                    println!("{head}  HEARTBEAT {caps}, newer than this host's {NODE_VERSION}");
                }
            } else {
                match Frame::decode(payload) {
                    Ok(Frame::Heartbeat(heartbeat)) => {
                        counts.heartbeat += 1;
                        println!(
                            "{head}  HEARTBEAT #{} b{} e{} u{} d{}<{} p{} {}  {}",
                            heartbeat.counter,
                            heartbeat.beat,
                            heartbeat.epoch,
                            heartbeat.unsent,
                            heartbeat.dwell,
                            heartbeat.prev_dwell,
                            heartbeat.tx_power,
                            heartbeat.reset_cause,
                            heartbeat.capabilities
                        );
                    }
                    Ok(Frame::Sightings(sightings)) => {
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
                    Ok(Frame::Admin(admin)) => {
                        counts.admin += 1;
                        let indices: Vec<String> =
                            admin.channels.indices().map(|idx| idx.to_string()).collect();
                        // `none` rather than nothing: the one frame with an empty set is
                        // the Bluetooth node's, and a blank there reads as a bug.
                        let indices =
                            if indices.is_empty() { "none".to_owned() } else { indices.join(",") };
                        println!(
                            "{head}  ADMIN e{}{} channel idx {}  -> {}",
                            admin.epoch,
                            if admin.scan_ble() { " +ble" } else { "" },
                            indices,
                            mac::full(dst),
                        );
                    }
                    // A clear this host did not send is the same fact as an assignment
                    // it did not send: something else is driving these nodes.
                    Ok(Frame::Clear(_)) => {
                        counts.admin += 1;
                        println!("{head}  CLEAR  -> {}", mac::full(dst));
                    }
                    // Named rather than left as "undecodable": a fleet half-way
                    // through a reflash is exactly what this looks like.
                    Err(DecodeError::BadVersion(version)) => {
                        counts.incompatible += 1;
                        println!("{head}  wire version {version}, which this build does not speak");
                    }
                    // Also named: another fleet on this channel is transmitting
                    // where these nodes are listening.
                    Err(DecodeError::BadMagic) => match foreign::classify(payload) {
                        Some(foreign::Foreign::Admin) => {
                            counts.foreign_admin += 1;
                            println!("{head}  ADMIN from a vendor core  -> {}", mac::full(dst));
                        }
                        Some(foreign::Foreign::Node) => {
                            counts.foreign_fleet += 1;
                            println!("{head}  a vendor node's frame");
                        }
                        None => {
                            counts.undecodable += 1;
                            println!("{head}  undecodable: not ESP-NOW wardriving traffic");
                        }
                    },
                    // Ours, from a node on this minor: a bug in one of the two builds.
                    Err(DecodeError::BadLength { need, got }) => {
                        counts.incompatible += 1;
                        println!("{head}  frame is {got} bytes, not {need}");
                    }
                    Err(err) => {
                        counts.undecodable += 1;
                        println!("{head}  undecodable: {err:?}");
                    }
                }
            }

            if args.raw {
                println!("      {}", hex(payload));
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

        BridgeToHost::Status { peer_count, rx_count, dropped_tx, uptime_ms, rx_oversize } => {
            println!(
                "# status: {peer_count} peers, {rx_count} received, \
                 {dropped_tx} dropped, {rx_oversize} oversize, up {}s",
                uptime_ms / 1000
            );
        }

        BridgeToHost::Log { level, message } => {
            if args.verbose {
                println!("# bridge {level:?}: {message}");
            }
        }

        BridgeToHost::Error { message } => println!("# bridge error: {message}"),

        // Only reachable once transmit exists; harmless to print until then.
        BridgeToHost::SendResult { id, status, tx_us } => {
            println!("# send {id} -> {status:?} at {tx_us}us");
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
        Args { bridge: None, sim: None, raw: false, verbose: false }
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
            peer_count: 0,
            rx_count: 0,
            dropped_tx: 0,
            uptime_ms: 1_000,
            rx_oversize: 0,
        };
        assert!(heard(LinkEvent::Message(status)));
    }

    #[test]
    fn sniff_cmd_marks_bridge_heard_when_receiving_disconnected_event_with_reason() {
        assert!(heard(LinkEvent::Disconnected { reason: "could not open".to_owned() }));
    }

    /// Count one received `payload` the way `handle` does.
    fn counted(payload: &[u8]) -> Counts {
        let mut counts = Counts::default();
        let rx = BridgeToHost::Rx {
            src: [0x02, 0, 0, 0, 0x57, 0x84],
            dst: BROADCAST,
            rssi: -41,
            rx_us: 0,
            payload: wartui_proto::link::EspNowPayload::from_slice(payload).expect("fits"),
        };
        handle(LinkEvent::Message(rx), &args(), &mut counts);
        counts
    }

    fn heartbeat_with(version: wartui_proto::air::Version) -> Vec<u8> {
        use wartui_proto::air::{Board, DieTemp, HeartbeatMsg, Vdd};
        HeartbeatMsg {
            counter: 1,
            epoch: 0,
            capabilities: Capabilities { version, ..Capabilities::here(Board::Esp32c5) },
            wifi_refused: 0,
            ble_refused: 0,
            beat: 1,
            unsent: 0,
            dwell: 0,
            prev_dwell: 0,
            tx_power: 8,
            reset_cause: wartui_proto::reset::ResetCause::PowerOn,
            vdd: Vdd::NONE,
            die_temp: DieTemp::NONE,
        }
        .encode()
        .to_vec()
    }

    #[test]
    fn sniff_cmd_counts_heartbeat_when_node_runs_this_minor() {
        let counts = counted(&heartbeat_with(NODE_VERSION));
        assert_eq!((counts.heartbeat, counts.needs_update, counts.host_behind), (1, 0, 0));
    }

    #[test]
    fn sniff_cmd_counts_needs_update_when_full_length_heartbeat_names_older_minor() {
        // Full length, so it would decode, but past the prefix its fields are not ours.
        let counts = counted(&heartbeat_with(wartui_bridge::sim::minor_behind(NODE_VERSION)));
        assert_eq!((counts.heartbeat, counts.needs_update, counts.incompatible), (0, 1, 0));
    }

    #[test]
    fn sniff_cmd_counts_host_behind_when_heartbeat_names_newer_minor() {
        let minor = NODE_VERSION.minor + 1;
        let ahead = wartui_proto::air::Version { minor, ..NODE_VERSION };
        let mut frame = heartbeat_with(ahead);
        frame.truncate(14);
        let counts = counted(&frame);
        assert_eq!((counts.heartbeat, counts.host_behind, counts.incompatible), (0, 1, 0));
    }

    #[test]
    fn sniff_cmd_counts_incompatible_when_this_minor_sends_wrong_length() {
        let mut frame = heartbeat_with(NODE_VERSION);
        frame.push(0);
        let counts = counted(&frame);
        assert_eq!((counts.incompatible, counts.undecodable, counts.heartbeat), (1, 0, 0));
    }
}
