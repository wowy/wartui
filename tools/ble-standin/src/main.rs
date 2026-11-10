//! Spike only: a PC standing in for a phone host on the wartui Bluetooth link, through BlueZ on
//! the first adapter. The checks are the library's; this is the radio and the printing.
//!
//! Only the first attachment asks for load or a scan. The P4 keeps both across a reconnect, so a
//! later attachment shows whether the queue really starts empty. The last attachment stops what
//! the run started, so a host attached after it reads no leftover load.
//!
//! The first attachment asks for a `Status` before anything else, and the last asks for one when
//! its time is up and waits for it before stopping anything, so the bridge counters span the run.
//!
//! Events are stamped here, as their notification arrives, because the wall clock is the radio's
//! side of the run; the library hands them back untimed so its checks stay testable.

use std::path::PathBuf;
use std::pin::pin;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use ble_standin::events::clock;
use ble_standin::rtt::RTT_TIMEOUT;
use ble_standin::{
    Attachment, AttachmentSummary, CounterDelta, Event, FleetSummary, Reconcile, Rtt, RttSummary,
    Seen, StatusAsk, bridge, hex, host_frame,
};
use btleplug::api::{
    Central, Characteristic, Manager as _, Peripheral as _, ScanFilter, ValueNotification,
    WriteType,
};
use btleplug::platform::{Adapter, Manager, Peripheral};
use chrono::Local;
use clap::{Args, Parser, Subcommand, ValueEnum};
use futures::{Stream, StreamExt};
use serde::Serialize;
use tokio::time::{interval, interval_at, sleep, sleep_until};
use uuid::Uuid;
use wartui_proto::link::HostToBridge;
use wartui_proto::spike::{DOWN_UUID, NAME_PREFIX, STATS_EVERY_MS, SpikeProfile, UP_UUID};

/// How long `run` looks for the name before giving up.
const FIND_FOR: Duration = Duration::from_secs(20);
/// The pause between attachments.
const RECONNECT_AFTER: Duration = Duration::from_secs(3);
/// How long the last attachment waits for its closing `Status`: a timed ask's timeout, then the
/// closing ask's.
const CLOSE_WAIT: Duration = Duration::from_secs(2 * RTT_TIMEOUT.as_secs());
/// Gap ranges printed per attachment; the JSON has them all.
const GAPS_SHOWN: usize = 20;

#[derive(Parser)]
#[command(about = "Spike only: stand in for a phone host on the wartui Bluetooth link")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// List advertisers named `wartui …`.
    Scan {
        /// How long to listen.
        #[arg(long, default_value_t = 10)]
        secs: u64,
    },
    /// Connect, subscribe to Up and check what arrives.
    Run(RunArgs),
    /// Print a host frame as hex, to write to Down by hand.
    #[command(subcommand)]
    Frame(Frame),
}

#[derive(Args)]
struct RunArgs {
    /// The advertised name, e.g. "wartui 96:C4".
    #[arg(long)]
    name: String,
    /// Start the P4's load generator.
    #[arg(long)]
    profile: Option<Profile>,
    /// Records in a burst.
    #[arg(long, default_value_t = 1_000)]
    count: u32,
    /// How long the whole run lasts.
    #[arg(long, default_value_t = 30)]
    secs: u64,
    /// Disconnect and reconnect this many times, splitting `--secs` evenly.
    #[arg(long, default_value_t = 0)]
    reconnect: u32,
    /// Time this many `GetStatus` round trips, one at a time.
    #[arg(long, default_value_t = 0)]
    rtt: u32,
    /// Start the fleet scanning Wi-Fi, and tally node heartbeats per slot.
    #[arg(long)]
    start_scan: bool,
    /// Write the summary here as JSON.
    #[arg(long)]
    json: Option<PathBuf>,
}

#[derive(Subcommand)]
enum Frame {
    /// `GetStatus`.
    GetStatus,
    /// `SpikeLoad`.
    SpikeLoad {
        #[arg(long)]
        profile: Profile,
        #[arg(long, default_value_t = 0)]
        count: u32,
    },
}

/// [`SpikeProfile`], as the command line names it.
#[derive(Debug, Clone, Copy, ValueEnum)]
enum Profile {
    Off,
    Drive,
    Fleet20,
    Saturate,
    Burst,
}

impl From<Profile> for SpikeProfile {
    fn from(profile: Profile) -> Self {
        match profile {
            Profile::Off => Self::Off,
            Profile::Drive => Self::Drive,
            Profile::Fleet20 => Self::Fleet20,
            Profile::Saturate => Self::Saturate,
            Profile::Burst => Self::Burst,
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    match Cli::parse().command {
        Command::Scan { secs } => scan(Duration::from_secs(secs)).await,
        Command::Run(args) => run(args).await,
        Command::Frame(frame) => {
            let msg = match frame {
                Frame::GetStatus => HostToBridge::GetStatus,
                Frame::SpikeLoad { profile, count } => {
                    HostToBridge::SpikeLoad { profile: profile.into(), count }
                }
            };
            println!("{}", hex(&host_frame(&msg)));
            Ok(())
        }
    }
}

async fn adapter() -> Result<Adapter> {
    let manager = Manager::new().await.context("opening BlueZ")?;
    let adapter = manager.adapters().await?.into_iter().next().context("no Bluetooth adapter")?;
    println!("# adapter {}", adapter.adapter_info().await?);
    Ok(adapter)
}

/// The name a peripheral advertises, if any.
async fn name_of(peripheral: &Peripheral) -> Result<Option<(String, Option<i16>)>> {
    let Some(props) = peripheral.properties().await? else { return Ok(None) };
    Ok(props.local_name.or(props.advertisement_name).map(|name| (name, props.rssi)))
}

async fn scan(secs: Duration) -> Result<()> {
    let adapter = adapter().await?;
    adapter.start_scan(ScanFilter::default()).await?;
    sleep(secs).await;
    adapter.stop_scan().await?;
    let mut found = Vec::new();
    for peripheral in adapter.peripherals().await? {
        if let Some((name, rssi)) = name_of(&peripheral).await?
            && name.starts_with(NAME_PREFIX)
        {
            found.push((name, peripheral.address(), rssi));
        }
    }
    found.sort();
    if found.is_empty() {
        println!("# nothing named `{NAME_PREFIX}…` advertised in {} s", secs.as_secs());
    }
    for (name, address, rssi) in found {
        let rssi = rssi.map_or_else(|| "?".to_owned(), |r| r.to_string());
        println!("{name}  {address}  rssi {rssi} dBm");
    }
    Ok(())
}

async fn find(adapter: &Adapter, name: &str) -> Result<Peripheral> {
    adapter.start_scan(ScanFilter::default()).await?;
    let deadline = Instant::now() + FIND_FOR;
    let found = loop {
        let mut hit = None;
        for peripheral in adapter.peripherals().await? {
            if name_of(&peripheral).await?.is_some_and(|(n, _)| n == name) {
                hit = Some(peripheral);
                break;
            }
        }
        if let Some(peripheral) = hit {
            break peripheral;
        }
        if Instant::now() >= deadline {
            adapter.stop_scan().await?;
            bail!("nothing advertised `{name}` in {} s", FIND_FOR.as_secs());
        }
        sleep(Duration::from_millis(250)).await;
    };
    adapter.stop_scan().await?;
    Ok(found)
}

/// A subscribed Up and the Down to write to.
struct Link {
    up: Characteristic,
    down: Characteristic,
}

async fn attach(
    peripheral: &Peripheral,
) -> Result<(Link, impl Stream<Item = ValueNotification> + use<>)> {
    if !peripheral.is_connected().await? {
        peripheral.connect().await.context("connecting")?;
    }
    peripheral.discover_services().await.context("discovering services")?;
    let chars = peripheral.characteristics();
    let pick = |uuid: u128, what: &str| {
        chars
            .iter()
            .find(|c| c.uuid == Uuid::from_u128(uuid))
            .cloned()
            .with_context(|| format!("no {what} characteristic {}", Uuid::from_u128(uuid)))
    };
    let link = Link { up: pick(UP_UUID, "Up")?, down: pick(DOWN_UUID, "Down")? };
    let notes = peripheral.notifications().await?;
    peripheral.subscribe(&link.up).await.context("subscribing to Up")?;
    println!(
        "# attached to {} ({}), mtu {}",
        peripheral.address(),
        peripheral.id(),
        peripheral.mtu()
    );
    Ok((link, notes))
}

/// Write `msg` to Down, split to the ATT payload a write without response carries.
async fn send(peripheral: &Peripheral, link: &Link, msg: &HostToBridge) -> Result<()> {
    let frame = host_frame(msg);
    let size = usize::from(peripheral.mtu().saturating_sub(3)).max(20);
    for chunk in frame.chunks(size) {
        peripheral.write(&link.down, chunk, WriteType::WithoutResponse).await?;
    }
    Ok(())
}

/// What the run came to, for `--json`.
#[derive(Serialize)]
struct RunSummary {
    name: String,
    address: String,
    profile: Option<SpikeProfile>,
    count: u32,
    secs: u64,
    reconnect: u32,
    attachments: Vec<AttachmentSummary>,
    rtt: Option<RttSummary>,
    reconcile: Reconcile,
    /// The bridge's counters, first `Status` to last.
    bridge: Option<Vec<CounterDelta>>,
    /// The last `Fleet` report.
    fleet: Option<FleetSummary>,
    /// Every event, in the order seen.
    events: Vec<Event>,
}

async fn run(args: RunArgs) -> Result<()> {
    let adapter = adapter().await?;
    let peripheral = find(&adapter, &args.name).await?;
    let each = Duration::from_secs(args.secs) / (args.reconnect + 1);
    let mut rtt = Rtt::new(args.rtt);
    let mut ask = StatusAsk::default();
    let mut attachments = Vec::new();
    let mut events = Vec::new();
    let mut ctrl_c = pin!(tokio::signal::ctrl_c());
    let mut stopped = false;

    for index in 0..=args.reconnect {
        if stopped {
            break;
        }
        if index > 0 {
            sleep(RECONNECT_AFTER).await;
        }
        let (link, notes) = attach(&peripheral).await?;
        let mut notes = pin!(notes);
        let start = Instant::now();
        let mut attachment = Attachment::new(start);
        if index == 0 {
            ask.want();
            ask.sent(Instant::now());
            send(&peripheral, &link, &HostToBridge::GetStatus)
                .await
                .context("asking for status")?;
            if let Some(profile) = args.profile {
                let msg = HostToBridge::SpikeLoad { profile: profile.into(), count: args.count };
                send(&peripheral, &link, &msg).await.context("asking for load")?;
            }
            if args.start_scan {
                let msg = HostToBridge::StartScan { bluetooth: false };
                send(&peripheral, &link, &msg).await.context("starting the scan")?;
            }
        }
        let every = Duration::from_millis(STATS_EVERY_MS);
        let mut tick = interval_at((start + every).into(), every);
        let mut poll = interval(Duration::from_millis(20));
        let mut done = pin!(sleep_until((start + each).into()));
        let mut dropped = false;
        let mut closing = false;
        'attachment: loop {
            tokio::select! {
                note = notes.next() => match note {
                    Some(note) if note.uuid == Uuid::from_u128(UP_UUID) => {
                        for seen in attachment.feed(&note.value) {
                            match seen {
                                // One ask is out at a time, so this answers it.
                                Seen::Status => {
                                    if !ask.answered() {
                                        rtt.answered(Instant::now());
                                    } else if closing {
                                        break 'attachment;
                                    }
                                }
                                Seen::Event { rx_us, kind } => {
                                    let event = Event {
                                        wall: clock(&Local::now()),
                                        attachment: index + 1,
                                        secs: start.elapsed().as_secs_f64(),
                                        rx_us,
                                        kind,
                                    };
                                    println!("{}", event.line());
                                    events.push(event);
                                }
                            }
                        }
                    }
                    Some(_) => {}
                    None => {
                        println!("# the link dropped");
                        dropped = true;
                        break;
                    }
                },
                _ = tick.tick() => print_tick(&mut attachment),
                _ = poll.tick() => {
                    let now = Instant::now();
                    rtt.expire(now);
                    ask.expire(now);
                    if ask.due(rtt.is_out()) {
                        ask.sent(now);
                        if let Err(e) = send(&peripheral, &link, &HostToBridge::GetStatus).await {
                            println!("# GetStatus write failed: {e:#}");
                        }
                    } else if !ask.pending() && !closing && rtt.due(now) {
                        rtt.sent(now);
                        if let Err(e) = send(&peripheral, &link, &HostToBridge::GetStatus).await {
                            println!("# GetStatus write failed: {e:#}");
                        }
                    }
                }
                () = &mut done => {
                    if closing || index < args.reconnect {
                        break;
                    }
                    closing = true;
                    ask.want();
                    done.as_mut().reset((Instant::now() + CLOSE_WAIT).into());
                }
                _ = &mut ctrl_c, if !stopped => {
                    stopped = true;
                    if !closing {
                        closing = true;
                        ask.want();
                        done.as_mut().reset((Instant::now() + CLOSE_WAIT).into());
                    }
                }
            }
        }
        let now = Instant::now();
        rtt.drop_out(now);
        // An ask this attachment never heard answered is not the next one's.
        ask = StatusAsk::default();
        let summary = attachment.summary(now);
        print_attachment(index, args.reconnect, &summary);
        attachments.push(summary);
        if !dropped {
            if index == args.reconnect || stopped {
                stop_what_was_started(&peripheral, &link, &args).await;
            }
            peripheral.unsubscribe(&link.up).await.ok();
        }
        peripheral.disconnect().await.ok();
    }

    let summary = RunSummary {
        name: args.name.clone(),
        address: peripheral.address().to_string(),
        profile: args.profile.map(Into::into),
        count: args.count,
        secs: args.secs,
        reconnect: args.reconnect,
        rtt: rtt.summary(),
        reconcile: Reconcile::of(&attachments),
        bridge: bridge::over_run(&attachments),
        fleet: attachments.iter().rev().find_map(|a| a.fleet.clone()),
        attachments,
        events,
    };
    print_run(&summary);
    if let Some(path) = &args.json {
        let file =
            std::fs::File::create(path).with_context(|| format!("creating {}", path.display()))?;
        serde_json::to_writer_pretty(file, &summary)?;
        println!("# summary written to {}", path.display());
    }
    Ok(())
}

async fn stop_what_was_started(peripheral: &Peripheral, link: &Link, args: &RunArgs) {
    if args.profile.is_some() {
        let msg = HostToBridge::SpikeLoad { profile: SpikeProfile::Off, count: 0 };
        if let Err(e) = send(peripheral, link, &msg).await {
            println!("# could not stop the load: {e:#}");
        }
    }
    if args.start_scan
        && let Err(e) = send(peripheral, link, &HostToBridge::StopScan).await
    {
        println!("# could not stop the scan: {e:#}");
    }
}

fn print_tick(attachment: &mut Attachment) {
    let now = Instant::now();
    if let (_, Some(stats)) = attachment.stats() {
        println!("{}", stats.text());
    }
    let window = attachment.window(now);
    let overall = attachment.overall(now);
    let seq = attachment.seq();
    println!(
        "# stand-in: {:.0} B/s, {:.1} frames/s over {:.1} s; {:.0} B/s, {:.1} frames/s over the \
         attachment; {} records, {} missing in {} gaps, {} crc, {} garbled",
        window.bytes_per_s,
        window.frames_per_s,
        window.secs,
        overall.bytes_per_s,
        overall.frames_per_s,
        seq.records,
        seq.missing,
        seq.gaps.len(),
        seq.crc_failures,
        attachment.garbled(),
    );
}

fn yes(b: bool) -> &'static str {
    if b { "yes" } else { "NO" }
}

fn print_attachment(index: u32, reconnect: u32, summary: &AttachmentSummary) {
    let frames: Vec<String> = summary.by_variant.iter().map(|(k, v)| format!("{k} {v}")).collect();
    println!(
        "# attachment {} of {}: {:.1} s, {:.0} B/s, {:.1} frames/s; Ready first {}, seq from 0 {}",
        index + 1,
        reconnect + 1,
        summary.rates.secs,
        summary.rates.bytes_per_s,
        summary.rates.frames_per_s,
        yes(summary.ready_first),
        yes(summary.seq_from_zero),
    );
    println!("#   frames: {}; {} garbled", frames.join(", "), summary.garbled);
    let spike = &summary.spike;
    if spike.records > 0 {
        let range =
            spike.first.zip(spike.last).map(|(f, l)| format!("{f}..={l}")).unwrap_or_default();
        println!(
            "#   load: {} records, seq {range}, {} missing in {} gaps, {} backwards, {} crc",
            spike.records,
            spike.missing,
            spike.gaps.len(),
            spike.backwards,
            spike.crc_failures,
        );
        if !spike.gaps.is_empty() {
            let shown: Vec<String> = spike
                .gaps
                .iter()
                .take(GAPS_SHOWN)
                .map(|g| {
                    if g.from == g.to { g.from.to_string() } else { format!("{}-{}", g.from, g.to) }
                })
                .collect();
            let more = spike.gaps.len().saturating_sub(GAPS_SHOWN);
            let more = if more > 0 { format!(" and {more} more") } else { String::new() };
            println!("#   gaps: {}{more}", shown.join(", "));
        }
    }
    for (label, beats) in &summary.beats {
        println!(
            "#   {label}: {} heartbeats, ~{} missed (inferred from timing), {} restarts, \
             {} undecodable, longest spacing {:.1} s",
            beats.beats,
            beats.missed,
            beats.restarts,
            beats.undecodable,
            f64::from(beats.max_spacing_ms) / 1000.0,
        );
    }
    for (label, b) in &summary.batches {
        println!(
            "#   {label}: {} batches, {} lost in {} gaps, {} duplicates, {} reused seq, \
             {} uncounted gaps, {} undecodable, {} restarts",
            b.batches,
            b.lost,
            b.gaps,
            b.duplicates,
            b.reused_seq,
            b.uncounted,
            b.undecodable,
            b.restarts,
        );
    }
}

fn print_run(summary: &RunSummary) {
    if let Some(rtt) = summary.rtt {
        println!(
            "# rtt: {} answered, {} lost; p50 {:.1} ms, p99 {:.1} ms, max {:.1} ms",
            rtt.answered, rtt.lost, rtt.p50_ms, rtt.p99_ms, rtt.max_ms,
        );
    }
    let r = summary.reconcile;
    let count = |n: Option<u32>| n.map_or_else(|| "?".to_owned(), |n| format!("+{n}")).to_string();
    let unexplained = r.unexplained.map_or_else(|| "?".to_owned(), |n| n.to_string());
    println!(
        "# reconcile: {} seqs missing in {} gaps; evicted {}, refused {} over the run; \
         {unexplained} unexplained",
        r.missing,
        r.gap_ranges,
        count(r.evicted),
        count(r.refused),
    );
    let (batches, lost) = summary
        .attachments
        .iter()
        .flat_map(|a| a.batches.values())
        .fold((0, 0), |(n, l), b| (n + b.batches, l + b.lost));
    if batches > 0 {
        println!("# batches: {batches} received, {lost} lost over the run");
    }
    match &summary.bridge {
        Some(counters) => {
            println!("# bridge counters, first Status -> last:");
            for counter in counters {
                println!("#   {}", counter.text());
            }
        }
        None => println!("# bridge counters: no Status arrived"),
    }
    if let Some(fleet) = &summary.fleet {
        println!("# last fleet report: {}", fleet.text());
    }
}
