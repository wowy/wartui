//! `wartui sniff` — decode and print, and nothing else.
//!
//! No database, no fleet table, no transmit. The point is to be able to say
//! "the bridge hears the fleet and the decoder agrees" with nothing else in the
//! way, and to have something to reach for later when a node goes quiet and the
//! question is whether the frames are arriving at all.

use anyhow::Result;
use clap::Args as ClapArgs;
use wartui_bridge::LinkEvent;
use wartui_proto::air::{Frame, RecordKind, WardriveLine};
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
    let mut link = super::open(args.port.as_deref(), args.sim)?;
    println!("# waiting for the bridge; ctrl-c to stop");

    let mut counts = Counts::default();

    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => break,
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
        "\n# {} frames: {} text, {} heartbeat, {} admin, {} undecodable",
        counts.total, counts.text, counts.heartbeat, counts.admin, counts.undecodable
    );
    Ok(())
}

#[derive(Default)]
struct Counts {
    total: u64,
    text: u64,
    heartbeat: u64,
    admin: u64,
    undecodable: u64,
}

fn handle(event: LinkEvent, args: &Args, counts: &mut Counts) {
    let LinkEvent::Message(message) = &event else {
        if let Some(line) = super::describe(&event) {
            println!("# {line}");
        }
        return;
    };

    match message {
        BridgeToHost::Rx { src, dst, rssi, rx_us, payload, .. } => {
            counts.total += 1;
            let addressing = if *dst == BROADCAST { "bcast" } else { "unicast" };
            let head = format!("{:>10}us  {}  {rssi:>4}dBm  {addressing:>7}", rx_us, mac(src));

            match Frame::decode(payload) {
                Ok(Frame::Text(text)) => {
                    let line = WardriveLine::parse(text.text);
                    match line {
                        Ok(observation) => {
                            counts.text += 1;
                            println!("{head}  {}", render(&observation));
                        }
                        Err(_) => {
                            // A `MSG_HEARTBEAT` carries a counter and no text,
                            // so failing to parse the payload as an observation
                            // is the normal case rather than a problem.
                            counts.heartbeat += 1;
                            println!(
                                "{head}  {:?} #{}  {}",
                                text.msg_type,
                                text.counter,
                                String::from_utf8_lossy(text.text)
                            );
                        }
                    }
                }
                Ok(Frame::Admin(admin)) => {
                    counts.admin += 1;
                    println!(
                        "{head}  ADMIN v{} node {}/{} channel idx {}..{}  -> {}",
                        admin.assignment_version,
                        admin.node_index,
                        admin.node_count,
                        admin.start_channel_idx,
                        admin.end_channel_idx,
                        mac(dst),
                    );
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

        BridgeToHost::Ready { chip, mac: bridge_mac, fw_version, proto_version } => {
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
