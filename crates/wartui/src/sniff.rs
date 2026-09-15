//! `wartui sniff` — decode and print, and nothing else.
//!
//! No database, no fleet table, no transmit. The point is to be able to say
//! "the bridge hears the fleet and the decoder agrees" with nothing else in the
//! way, and to have something to reach for later when a node goes quiet and the
//! question is whether the frames are arriving at all.

use anyhow::Result;
use clap::Args as ClapArgs;
use wartui_bridge::LinkEvent;
use wartui_proto::air::{DecodeError, Frame, RecordKind, SightingMsg, foreign};
use wartui_proto::beacon::rcoi_text;
use wartui_proto::link::{BROADCAST, BridgeToHost};

use super::mac;

#[derive(ClapArgs)]
pub struct Args {
    /// Serial port of the bridge. Discovered automatically if omitted.
    #[arg(long, value_name = "PATH")]
    port: Option<String>,

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
    let mut link = super::open(args.port.as_deref(), args.sim, 0)?;
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
                    eprintln!("\n{}\n", super::no_bridge_notice(args.port.as_deref()));
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
        "\n# {} frames: {} sighting, {} heartbeat, {} admin, {} not ours, {} undecodable",
        counts.total,
        counts.sighting,
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
            "# {} frames were ours but from a build speaking a wire version this one \
             does not. Those nodes are invisible to `run` until they are reflashed \
             with firmware/node.",
            counts.incompatible
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
    sighting: u64,
    heartbeat: u64,
    admin: u64,
    /// Vendor heartbeats and observations. Another fleet is on this channel,
    /// transmitting where these nodes are listening.
    foreign_fleet: u64,
    /// Another core's assignment. Counted apart from `foreign_fleet` because
    /// seeing any at all means something else is driving a fleet nearby.
    foreign_admin: u64,
    /// Frames of ours from a build speaking a wire version this one does not, which is
    /// what a fleet half-way through a reflash looks like.
    incompatible: u64,
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
            let head = format!("{:>10}us  {}  {rssi:>4}dBm  {addressing:>7}", rx_us, mac(src));

            match Frame::decode(payload) {
                Ok(Frame::Heartbeat(heartbeat)) => {
                    counts.heartbeat += 1;
                    println!(
                        "{head}  HEARTBEAT #{}  {}",
                        heartbeat.counter, heartbeat.capabilities
                    );
                }
                Ok(Frame::Sighting(sighting)) => {
                    counts.sighting += 1;
                    println!("{head}  {}", render(&sighting));
                }
                Ok(Frame::Admin(admin)) => {
                    counts.admin += 1;
                    let indices: Vec<String> =
                        admin.channels.indices().map(|idx| idx.to_string()).collect();
                    println!(
                        "{head}  ADMIN e{} node {}/{}{} channel idx {}  -> {}",
                        admin.epoch,
                        admin.node_index,
                        admin.node_count,
                        if admin.scan_ble() { " +ble" } else { "" },
                        indices.join(","),
                        mac(dst),
                    );
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
                        println!("{head}  ADMIN from a vendor core  -> {}", mac(dst));
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
                Err(err) => {
                    counts.undecodable += 1;
                    println!("{head}  undecodable: {err:?}");
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
                mac(bridge_mac)
            );
        }

        BridgeToHost::Status { channel, peer_count, rx_count, dropped_tx, uptime_ms } => {
            println!(
                "# status: channel {channel}, {peer_count} peers, {rx_count} received, \
                 {dropped_tx} dropped, up {}s",
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
        mac(&sighting.bssid),
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
        Args { port: None, sim: None, raw: false, verbose: false }
    }

    fn heard(event: LinkEvent) -> bool {
        let mut counts = Counts::default();
        handle(event, &args(), &mut counts);
        counts.heard_a_bridge
    }

    #[test]
    fn an_undecodable_frame_is_not_a_bridge_answering() {
        // The board that produces these fastest is one running node firmware,
        // which is the first cause the notice names. Counting a garbled frame
        // as contact would suppress the notice permanently after one of them.
        assert!(!heard(LinkEvent::Garbled(LinkError::Corrupt)));
    }

    #[test]
    fn a_decoded_message_counts_even_without_an_announcement() {
        // `Ready` can be lost to the bridge's transmit rings while everything
        // else it sends arrives; frames on screen must not be contradicted.
        let status = BridgeToHost::Status {
            channel: 6,
            peer_count: 0,
            rx_count: 0,
            dropped_tx: 0,
            uptime_ms: 1_000,
        };
        assert!(heard(LinkEvent::Message(status)));
    }

    #[test]
    fn a_reason_the_link_is_down_counts_because_it_is_the_better_diagnosis() {
        assert!(heard(LinkEvent::Disconnected { reason: "could not open".to_owned() }));
    }
}
