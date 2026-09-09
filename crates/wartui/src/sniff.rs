//! `wartui sniff` — decode and print, and nothing else.
//!
//! No database, no fleet table, no transmit. The point is to be able to say
//! "the bridge hears the fleet and the decoder agrees" with nothing else in the
//! way, and to have something to reach for later when a node goes quiet and the
//! question is whether the frames are arriving at all.

use anyhow::Result;
use clap::Args as ClapArgs;
use wartui_bridge::LinkEvent;
use wartui_proto::air::{Capabilities, Frame, MsgType, RecordKind, WardriveLine, is_legacy_admin};
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

    // Said once, and only into the silence it describes. `sniff` keeps waiting
    // afterwards rather than exiting, because it is the command left running
    // while a bridge is plugged in — but waiting without saying anything is how
    // a wedged dongle passes for a quiet fleet.
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
        "\n# {} frames: {} text, {} heartbeat, {} admin, {} other, {} undecodable",
        counts.total, counts.text, counts.heartbeat, counts.admin, counts.other, counts.undecodable
    );
    if counts.other > 0 {
        println!(
            "# {} core-protocol frames arrived, which only encrypted nodes send. \
             Turn encryption off in those nodes' web UI.",
            counts.other
        );
    }
    if counts.legacy_admin > 0 {
        println!(
            "# {} assignments arrived in the vendor core's 10-byte format. Something \
             other than this host is telling these nodes what to scan.",
            counts.legacy_admin
        );
    }
    if counts.unannounced_heartbeat > 0 {
        println!(
            "# {} of {} heartbeats carried no wartui token. `run` will not plan for \
             those nodes: they acknowledge an assignment and then discard it, so a \
             share cut for one is a share nobody scans. Flash them with firmware/node.",
            counts.unannounced_heartbeat, counts.heartbeat
        );
    }
    Ok(())
}

#[derive(Default)]
struct Counts {
    /// Whether anything behind the port has behaved like a bridge: an
    /// announcement, a decoded message of any kind, or the transport saying why
    /// the link is down. Any of those leaves the operator better informed than
    /// the notice would, whose whole subject is having heard nothing at all.
    /// An undecodable frame is not one of them — see [`handle`].
    heard_a_bridge: bool,
    total: u64,
    text: u64,
    heartbeat: u64,
    /// Heartbeats whose text field carried no wartui token. `run` will not
    /// plan for the nodes that send these, so a fleet that is being ignored
    /// looks exactly like this and nothing else would say why.
    unannounced_heartbeat: u64,
    admin: u64,
    /// The vendor core's ten-byte assignment. Counted apart from `admin`
    /// because seeing any at all means something else is driving this fleet.
    legacy_admin: u64,
    /// `CoreRequest` and `CoreReply`: not expected in a plaintext fleet, and
    /// worth a line of their own rather than being folded into a total.
    other: u64,
    undecodable: u64,
}

fn handle(event: LinkEvent, args: &Args, counts: &mut Counts) {
    let LinkEvent::Message(message) = &event else {
        // `Garbled` deliberately does not count. Undecodable bytes are the
        // loudest symptom of the thing the notice exists to explain — a board
        // running node firmware talks constantly and none of it is a frame,
        // and any `0x00` in that stream terminates a partial frame and lands
        // here. Treating one as proof a bridge answered would silence the
        // notice in exactly the case it was written for.
        if !matches!(event, LinkEvent::Garbled(_)) {
            counts.heard_a_bridge = true;
        }
        if let Some(line) = super::describe(&event) {
            println!("# {line}");
        }
        // Printed here rather than under `Ready`, because `Ready` is what the
        // transport turns into this event and no `Ready` ever reaches the arm
        // below. A bridge that reboots mid-capture announces itself again on
        // the same connection and arrives here a second time, which in a
        // sniff log is the interesting one: `sniff` is the raw view, so the
        // fields go out as the bridge sent them rather than through the prose
        // `last_reset_line` writes for the commands that show one line.
        if let LinkEvent::Connected(info) = &event {
            println!(
                "#   reset {:?}, last phase {:?}, {} bytes of heap free, up {}ms",
                info.reset_cause, info.last_phase, info.heap_free, info.uptime_ms
            );
        }
        return;
    };

    // Any decoded message, not just `Connected`. The transport re-sends
    // `Identify` until it is answered, but the bridge's transmit rings evict
    // oldest-first, so a busy fleet can cost it every `Ready` it sends while
    // its observations arrive perfectly well. Accusing a bridge of not
    // speaking the link protocol underneath a screenful of frames it decoded
    // is the one output worse than saying nothing.
    counts.heard_a_bridge = true;

    match message {
        BridgeToHost::Rx { src, dst, rssi, rx_us, payload, .. } => {
            counts.total += 1;
            let addressing = if *dst == BROADCAST { "bcast" } else { "unicast" };
            let head = format!("{:>10}us  {}  {rssi:>4}dBm  {addressing:>7}", rx_us, mac(src));

            match Frame::decode(payload) {
                // Every one of these shares the 212-byte text layout; what the
                // payload means is the type's business. Classifying by whether
                // the text parses as an observation would file a node emitting
                // malformed lines under "heartbeat", which is precisely the
                // case this summary would be read to diagnose.
                Ok(Frame::Text(text)) => {
                    match text.msg_type {
                        MsgType::Heartbeat => {
                            counts.heartbeat += 1;
                            if Capabilities::parse(text.text).is_none() {
                                counts.unannounced_heartbeat += 1;
                            }
                        }
                        MsgType::Text => counts.text += 1,
                        _ => counts.other += 1,
                    }
                    match WardriveLine::parse(text.text) {
                        Ok(observation) if text.msg_type == MsgType::Text => {
                            println!("{head}  {}", render(&observation));
                        }
                        // A heartbeat's text is its capability token, which is
                        // not a wardrive line, so failing to parse as one is
                        // the normal case here. Printed raw: `wartui/0.1;ble,5g`
                        // is meant to be read, and an empty one is the whole
                        // diagnosis for a node the planner will not touch.
                        _ => println!(
                            "{head}  {:?} #{}  {}",
                            text.msg_type,
                            text.counter,
                            String::from_utf8_lossy(text.text)
                        ),
                    }
                }
                Ok(Frame::Admin(admin)) => {
                    counts.admin += 1;
                    let indices: Vec<String> =
                        admin.channels.indices().map(|idx| idx.to_string()).collect();
                    println!(
                        "{head}  ADMIN v{} node {}/{}{} channel idx {}  -> {}",
                        admin.assignment_version,
                        admin.node_index,
                        admin.node_count,
                        if admin.scan_ble() { " +ble" } else { "" },
                        indices.join(","),
                        mac(dst),
                    );
                }
                // Named rather than left as "undecodable", because it is the
                // one undecodable frame with a meaning: a stock core in the
                // same room is assigning channels this host did not choose.
                Err(_) if is_legacy_admin(payload) => {
                    counts.legacy_admin += 1;
                    println!("{head}  ADMIN from a vendor core (10-byte)  -> {}", mac(dst));
                }
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
fn render(line: &WardriveLine<'_>) -> String {
    let kind = match line.kind {
        RecordKind::Wifi => "wifi",
        RecordKind::Ble => "ble ",
    };
    format!(
        "{kind}  {}  ch {:>3}  {:>4}dBm  {:<16}  {}",
        mac(&line.bssid),
        line.channel,
        line.rssi,
        String::from_utf8_lossy(line.security.as_bytes()),
        // SSIDs are whatever the access point beaconed, not text, and hidden
        // networks beacon an empty one.
        if line.ssid.is_empty() {
            "<hidden>".to_string()
        } else {
            format!("{:?}", String::from_utf8_lossy(line.ssid))
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
