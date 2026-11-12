//! Spike only: a PC standing in for a phone host on the wartui Bluetooth link, through BlueZ on
//! the first adapter, or for a USB host on UART0. The checks are the library's; this is the radio,
//! the serial port and the printing.
//!
//! UART0 runs the same checks at UART0's rate, so a loss seen over Bluetooth can be set against
//! the same load with the C6 out of the path. Its port is opened as `wartui-bridge` opens one, and
//! never moves DTR or RTS: one without the other resets the P4. The P4 writes nothing until a host
//! frame arrives and closes after 15 s without one, so a UART0 run keeps a [`Keepalive`] going.
//! A tty has no attachment to cycle, so `--reconnect` is Bluetooth's alone.
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

use std::io::{ErrorKind, Read, Write};
use std::path::PathBuf;
use std::pin::{Pin, pin};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use ble_standin::events::clock;
use ble_standin::rtt::RTT_TIMEOUT;
use ble_standin::{
    Attachment, AttachmentSummary, CounterDelta, Event, FleetSummary, Keepalive, Reconcile, Rtt,
    RttSummary, Seen, StatusAsk, bridge, hex, host_frame,
};
use btleplug::api::{
    Central, Characteristic, Manager as _, Peripheral as _, ScanFilter, ValueNotification,
    WriteType,
};
use btleplug::platform::{Adapter, Manager, Peripheral};
use chrono::Local;
use clap::{ArgGroup, Args, Parser, Subcommand, ValueEnum};
use futures::channel::mpsc;
use futures::{Stream, StreamExt, future};
use serde::Serialize;
use tokio::time::{interval, interval_at, sleep, sleep_until};
use uuid::Uuid;
use wartui_proto::link::{HOST_BAUD, HostToBridge};
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
/// How long a UART0 read or write waits: long enough not to spin, short enough to stop promptly.
const UART_TIMEOUT: Duration = Duration::from_millis(50);

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
    /// Attach over Bluetooth or UART0 and check what arrives.
    Run(RunArgs),
    /// Print a host frame as hex, to write to Down by hand.
    #[command(subcommand)]
    Frame(Frame),
}

#[derive(Args)]
#[command(group(ArgGroup::new("target").required(true).args(["name", "port"])))]
struct RunArgs {
    /// The advertised name, e.g. "wartui 96:C4".
    #[arg(long)]
    name: Option<String>,
    /// The P4 board's tty, for UART0 instead of Bluetooth.
    #[arg(long, value_name = "PATH", conflicts_with = "reconnect")]
    port: Option<String>,
    /// Start the P4's load generator.
    #[arg(long)]
    profile: Option<Profile>,
    /// Records in a burst.
    #[arg(long, default_value_t = 1_000)]
    count: u32,
    /// How long the whole run lasts.
    #[arg(long, default_value_t = 30)]
    secs: u64,
    /// Disconnect and reconnect this many times, splitting `--secs` evenly. Bluetooth only.
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

/// The Up stream, as the chunks it arrived in: notifications, or reads from the tty.
type Up = Pin<Box<dyn Stream<Item = Vec<u8>>>>;

/// Where `run` reaches the P4.
enum Transport {
    /// The C6's GATT service, through BlueZ.
    Bluetooth {
        /// Held for the run, as the peripheral came from it.
        _adapter: Adapter,
        peripheral: Peripheral,
    },
    /// UART0, through the board's CH343.
    Uart(String),
}

impl Transport {
    async fn open(args: &RunArgs) -> Result<Self> {
        if let Some(path) = &args.port {
            return Ok(Self::Uart(path.clone()));
        }
        let name = args.name.as_deref().context("--name or --port is required")?;
        let adapter = adapter().await?;
        let peripheral = find(&adapter, name).await?;
        Ok(Self::Bluetooth { _adapter: adapter, peripheral })
    }

    async fn attach(&self) -> Result<(Link<'_>, Up)> {
        match self {
            Self::Bluetooth { peripheral, .. } => attach(peripheral).await,
            Self::Uart(path) => open_uart(path),
        }
    }

    /// The peripheral's address, over Bluetooth.
    fn address(&self) -> Option<String> {
        match self {
            Self::Bluetooth { peripheral, .. } => Some(peripheral.address().to_string()),
            Self::Uart(_) => None,
        }
    }

    const fn is_uart(&self) -> bool {
        matches!(self, Self::Uart(_))
    }
}

/// One attachment's Down, and what closing it takes.
enum Link<'a> {
    /// A subscribed Up and the Down to write to.
    Bluetooth {
        peripheral: &'a Peripheral,
        up: Characteristic,
        down: Characteristic,
    },
    Uart(UartLink),
}

/// The open tty's write half, and the thread reading the other.
struct UartLink {
    port: Box<dyn serialport::SerialPort>,
    /// Whether the next write leads with a `0x00`, as the first does.
    lead: bool,
    last_write: Instant,
    stop: Arc<AtomicBool>,
    reader: JoinHandle<()>,
}

async fn attach(peripheral: &Peripheral) -> Result<(Link<'_>, Up)> {
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
    let (up, down) = (pick(UP_UUID, "Up")?, pick(DOWN_UUID, "Down")?);
    let notes = peripheral.notifications().await?;
    peripheral.subscribe(&up).await.context("subscribing to Up")?;
    println!(
        "# attached to {} ({}), mtu {}",
        peripheral.address(),
        peripheral.id(),
        peripheral.mtu()
    );
    let up_uuid = Uuid::from_u128(UP_UUID);
    let notes = notes.filter_map(move |note: ValueNotification| {
        future::ready((note.uuid == up_uuid).then_some(note.value))
    });
    Ok((Link::Bluetooth { peripheral, up, down }, Box::pin(notes)))
}

/// Open `path` at [`HOST_BAUD`], 8N1 by `serialport`'s defaults, and read it on a thread.
fn open_uart<'a>(path: &str) -> Result<(Link<'a>, Up)> {
    let port = serialport::new(path, HOST_BAUD)
        .timeout(UART_TIMEOUT)
        // As `wartui-bridge` opens a port: the setting that keeps the driver from moving RTS.
        // Nothing here touches DTR or RTS.
        .flow_control(serialport::FlowControl::None)
        .open()
        .with_context(|| format!("opening {path}"))?;
    // Whatever the P4 wrote before this run is not this run's.
    port.clear(serialport::ClearBuffer::Input).with_context(|| format!("clearing {path}"))?;
    let mut rx = port.try_clone().with_context(|| format!("splitting {path}"))?;
    let stop = Arc::new(AtomicBool::new(false));
    let (tx, chunks) = mpsc::unbounded();
    let reader = std::thread::Builder::new()
        .name("ble-standin-uart-rx".to_owned())
        .spawn({
            let stop = Arc::clone(&stop);
            move || read_uart(rx.as_mut(), &tx, &stop)
        })
        .context("starting the UART0 reader")?;
    println!("# attached to {path} at {HOST_BAUD} baud");
    let link = UartLink { port, lead: true, last_write: Instant::now(), stop, reader };
    Ok((Link::Uart(link), Box::pin(chunks)))
}

/// Pass what the tty reads to `tx` until `stop` is set or a read fails. Returning drops `tx`,
/// which ends the stream: `run` reads that as the link dropping.
fn read_uart(rx: &mut dyn Read, tx: &mpsc::UnboundedSender<Vec<u8>>, stop: &AtomicBool) {
    let mut buf = [0u8; 4096];
    while !stop.load(Ordering::Relaxed) {
        match rx.read(&mut buf) {
            Ok(0) => {}
            Ok(n) if tx.unbounded_send(buf[..n].to_vec()).is_err() => return,
            Ok(_) => {}
            Err(e) if matches!(e.kind(), ErrorKind::TimedOut | ErrorKind::Interrupted) => {}
            Err(e) => {
                println!("# UART0 read failed: {e}");
                return;
            }
        }
    }
}

impl Link<'_> {
    /// Write `msg` to Down, split to the ATT payload a write without response carries, or to the
    /// tty whole.
    async fn send(&mut self, msg: &HostToBridge) -> Result<()> {
        let frame = host_frame(msg);
        match self {
            Self::Bluetooth { peripheral, down, .. } => {
                let size = usize::from(peripheral.mtu().saturating_sub(3)).max(20);
                for chunk in frame.chunks(size) {
                    peripheral.write(down, chunk, WriteType::WithoutResponse).await?;
                }
            }
            Self::Uart(uart) => {
                // The first frame leads with a `0x00`, as `wartui-bridge`'s does, so whatever the
                // P4's receiver held before this run ends there and the frame decodes. A short
                // blocking write: the CH343 always drains.
                let mut bytes = Vec::with_capacity(frame.len() + 1);
                if std::mem::take(&mut uart.lead) {
                    bytes.push(0);
                }
                bytes.extend_from_slice(&frame);
                uart.port.write_all(&bytes)?;
                uart.last_write = Instant::now();
            }
        }
        Ok(())
    }

    /// When the last host frame went out, over UART0, where the P4's host gate needs one.
    const fn last_write(&self) -> Option<Instant> {
        match self {
            Self::Bluetooth { .. } => None,
            Self::Uart(uart) => Some(uart.last_write),
        }
    }

    /// Unsubscribe, unless the link `dropped`, and disconnect; or drain and close the tty.
    async fn close(self, dropped: bool) {
        match self {
            Self::Bluetooth { peripheral, up, .. } => {
                if !dropped {
                    peripheral.unsubscribe(&up).await.ok();
                }
                peripheral.disconnect().await.ok();
            }
            Self::Uart(mut uart) => {
                // Drained rather than cleared, so the closing `SpikeLoad off` and `StopScan`
                // reach the P4.
                uart.port.flush().ok();
                uart.stop.store(true, Ordering::Relaxed);
                uart.reader.join().ok();
            }
        }
    }
}

/// What the run came to, for `--json`.
#[derive(Serialize)]
struct RunSummary {
    /// The advertised name, over Bluetooth.
    name: Option<String>,
    /// The tty, over UART0.
    port: Option<String>,
    /// The peripheral's address, over Bluetooth.
    address: Option<String>,
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
    let transport = Transport::open(&args).await?;
    let each = Duration::from_secs(args.secs) / (args.reconnect + 1);
    let mut rtt = Rtt::new(args.rtt);
    let mut ask = StatusAsk::default();
    let mut keepalive = Keepalive::default();
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
        let (mut link, mut up) = transport.attach().await?;
        let start = Instant::now();
        let mut attachment = Attachment::new(start);
        if index == 0 {
            ask.want();
            ask.sent(Instant::now());
            link.send(&HostToBridge::GetStatus).await.context("asking for status")?;
            if let Some(profile) = args.profile {
                let msg = HostToBridge::SpikeLoad { profile: profile.into(), count: args.count };
                link.send(&msg).await.context("asking for load")?;
            }
            if args.start_scan {
                let msg = HostToBridge::StartScan { bluetooth: false };
                link.send(&msg).await.context("starting the scan")?;
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
                chunk = up.next() => match chunk {
                    Some(chunk) => {
                        for seen in attachment.feed(&chunk) {
                            match seen {
                                // One ask is out at a time, so this answers it.
                                Seen::Status => {
                                    if keepalive.answered() {
                                        // Not timed, and not the counters' snapshot.
                                    } else if !ask.answered() {
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
                    None => {
                        println!("# the link dropped");
                        dropped = true;
                        break;
                    }
                },
                _ = tick.tick() => print_tick(&mut attachment, transport.is_uart()),
                _ = poll.tick() => {
                    let now = Instant::now();
                    rtt.expire(now);
                    ask.expire(now);
                    keepalive.expire(now);
                    let busy = rtt.is_out() || ask.pending();
                    let send = if ask.due(rtt.is_out() || keepalive.is_out()) {
                        ask.sent(now);
                        true
                    } else if !ask.pending() && !keepalive.is_out() && !closing && rtt.due(now) {
                        rtt.sent(now);
                        true
                    } else if link.last_write().is_some_and(|at| keepalive.due(now, at, busy)) {
                        keepalive.sent(now);
                        true
                    } else {
                        false
                    };
                    if send && let Err(e) = link.send(&HostToBridge::GetStatus).await {
                        println!("# GetStatus write failed: {e:#}");
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
        keepalive = Keepalive::default();
        let summary = attachment.summary(now);
        print_attachment(index, args.reconnect, &summary);
        attachments.push(summary);
        if !dropped && (index == args.reconnect || stopped) {
            stop_what_was_started(&mut link, &args).await;
        }
        link.close(dropped).await;
    }

    let summary = RunSummary {
        name: args.name.clone(),
        port: args.port.clone(),
        address: transport.address(),
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

async fn stop_what_was_started(link: &mut Link<'_>, args: &RunArgs) {
    if args.profile.is_some() {
        let msg = HostToBridge::SpikeLoad { profile: SpikeProfile::Off, count: 0 };
        if let Err(e) = link.send(&msg).await {
            println!("# could not stop the load: {e:#}");
        }
    }
    if args.start_scan
        && let Err(e) = link.send(&HostToBridge::StopScan).await
    {
        println!("# could not stop the scan: {e:#}");
    }
}

fn print_tick(attachment: &mut Attachment, uart: bool) {
    let now = Instant::now();
    let window = attachment.window(now);
    if let (_, Some(stats)) = attachment.stats() {
        let text = stats.text().to_string();
        println!("{}", if uart { with_uart_rate(&text, window.bytes_per_s) } else { text });
    }
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

/// `SpikeStats` text with its rate, which counts only the time Bluetooth owned the queue, swapped
/// for what the stand-in read over UART0.
fn with_uart_rate(text: &str, bytes_per_s: f64) -> String {
    let (head, rest) = text.split_once('\n').map_or((text, None), |(h, r)| (h, Some(r)));
    let Some((fields, _)) = head.rsplit_once("; ") else { return text.to_owned() };
    let head = format!("{fields}; {bytes_per_s:.0} B/s read by the stand-in over UART0");
    rest.map_or_else(|| head.clone(), |rest| format!("{head}\n{rest}"))
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

#[cfg(test)]
mod tests {
    use clap::CommandFactory;
    use clap::error::ErrorKind;
    use wartui_proto::spike::SpikeStats;

    use super::*;

    fn parse(args: &[&str]) -> Result<Cli, clap::Error> {
        Cli::try_parse_from(std::iter::once("ble-standin").chain(args.iter().copied()))
    }

    fn run_args(args: &[&str]) -> RunArgs {
        match parse(args).unwrap().command {
            Command::Run(run) => run,
            _ => panic!("not a run"),
        }
    }

    #[test]
    fn cli_passes_debug_assert_when_built() {
        Cli::command().debug_assert();
    }

    #[test]
    fn cli_takes_run_when_name_given() {
        let run = run_args(&["run", "--name", "wartui 96:C4", "--reconnect", "2"]);
        assert_eq!((run.name.as_deref(), run.port, run.reconnect), (Some("wartui 96:C4"), None, 2));
    }

    #[test]
    fn cli_takes_run_when_port_given() {
        let run = run_args(&["run", "--port", "/dev/ttyACM0", "--profile", "burst"]);
        assert_eq!((run.name, run.port.as_deref()), (None, Some("/dev/ttyACM0")));
    }

    #[test]
    fn cli_refuses_run_when_port_and_name_given() {
        let err = parse(&["run", "--name", "wartui 96:C4", "--port", "/dev/ttyACM0"]);
        assert_eq!(err.err().map(|e| e.kind()), Some(ErrorKind::ArgumentConflict));
    }

    #[test]
    fn cli_refuses_run_when_neither_port_nor_name_given() {
        let err = parse(&["run", "--secs", "5"]);
        assert_eq!(err.err().map(|e| e.kind()), Some(ErrorKind::MissingRequiredArgument));
    }

    #[test]
    fn cli_refuses_run_when_port_and_reconnect_given() {
        let err = parse(&["run", "--port", "/dev/ttyACM0", "--reconnect", "2"]);
        assert_eq!(err.err().map(|e| e.kind()), Some(ErrorKind::ArgumentConflict));
    }

    #[test]
    fn tick_shows_standin_rate_when_over_uart() {
        let text = SpikeStats::default().text().to_string();
        let shown = with_uart_rate(&text, 123_456.4);
        let (head, rest) = shown.split_once('\n').unwrap();
        assert!(head.starts_with("# spike stats: up "), "{head}");
        assert!(head.ends_with("; 123456 B/s read by the stand-in over UART0"), "{head}");
        assert!(!head.contains("no Bluetooth time yet"), "{head}");
        // Every other line, the Bluetooth fields among them, is the P4's as sent.
        assert_eq!(Some(rest), text.split_once('\n').map(|(_, r)| r));
    }
}
