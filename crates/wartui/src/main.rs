//! wartui's command line.
//!
//! `run` is the tool; everything else is a way of checking one link in the
//! chain when `run` is not showing what it should. `sniff` proves the whole
//! path — a node's radio, the bridge's radio, the USB link, the framing and the
//! `ENOW` decoder — with nothing in between to be wrong. `status` and `ports`
//! answer the two questions that come before it: is a dongle attached, and is
//! it listening.
//!
//! `--log-file` is the fourth answer. The TUI owns the terminal, so a link that
//! is failing has nowhere to say so except the one header line it shares with
//! everything else; with a log file the transport's own account of what it
//! tried and what the OS said goes somewhere it can be read afterwards.

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use wartui_bridge::serial::{SerialTransport, discover_ports};
use wartui_bridge::sim::{SimConfig, SimTransport};
use wartui_bridge::{LinkEvent, LinkHandle};
use wartui_proto::link::Mac;

mod export;
mod run;
mod sniff;
mod status;
mod tui;

#[derive(Parser)]
#[command(name = "wartui", version, about = "Fleet controller for ESP32-C5 wardriving nodes")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,

    /// Append this run's diagnostics to a file: which port was opened, why a
    /// link went down, commands that would not fit. `RUST_LOG` sets the level,
    /// `info` by default; `debug` adds every retry and every frame that would
    /// not decode. Without this, nothing is logged anywhere — the view cannot
    /// share a terminal with a log.
    #[arg(long, value_name = "PATH", global = true)]
    log_file: Option<PathBuf>,

    /// With no subcommand, these are `run`'s arguments.
    #[command(flatten)]
    run: run::Args,
}

#[derive(Subcommand)]
enum Command {
    /// Capture a fleet into the store and watch it live. The default.
    Run(run::Args),
    /// Write a WiGLE CSV from a capture.
    Export(export::Args),
    /// Print every frame the bridge hears.
    Sniff(sniff::Args),
    /// Ask the bridge for its channel, counters and uptime.
    Status(status::Args),
    /// List serial ports that look like an Espressif device.
    Ports,
}

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    // Held for the whole process: dropping the guard stops the writer thread,
    // and the last thing logged before an exit is usually the interesting one.
    let _log = logging(cli.log_file.as_deref())?;
    match cli.command {
        None => run::run(cli.run).await,
        Some(Command::Run(args)) => run::run(args).await,
        Some(Command::Export(args)) => export::run(args),
        Some(Command::Sniff(args)) => sniff::run(args).await,
        Some(Command::Status(args)) => status::run(args).await,
        Some(Command::Ports) => ports(),
    }
}

/// Send `tracing` output to `path`, if one was given.
///
/// Non-blocking, because the engine is on the other end of some of these
/// events and a log write must never be what delays an assignment. Returns the
/// worker guard, which flushes what is queued when it is dropped.
fn logging(
    path: Option<&std::path::Path>,
) -> Result<Option<tracing_appender::non_blocking::WorkerGuard>> {
    let Some(path) = path else { return Ok(None) };
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("opening the log file {}", path.display()))?;
    let (writer, guard) = tracing_appender::non_blocking(file);
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        // A log file is read with `tail`, and escape codes in one are noise.
        .with_ansi(false)
        .with_writer(writer)
        .init();
    tracing::info!(version = env!("CARGO_PKG_VERSION"), "wartui starting");
    Ok(Some(guard))
}

fn ports() -> Result<()> {
    let found = discover_ports().context("listing serial ports")?;
    if found.is_empty() {
        println!("No Espressif device found.");
        println!("If the bridge is plugged in, pass its path with --port.");
        return Ok(());
    }
    for candidate in found {
        let product = candidate.product.as_deref().unwrap_or("unknown device");
        match (candidate.vid, candidate.pid) {
            (Some(vid), Some(pid)) => {
                println!("{}  {product}  ({vid:04x}:{pid:04x})", candidate.path);
            }
            _ => println!("{}  {product}", candidate.path),
        }
    }
    Ok(())
}

/// Open whichever transport the arguments called for.
fn open(port: Option<&str>, sim: Option<u8>) -> Result<LinkHandle> {
    if let Some(node_count) = sim {
        let config = SimConfig { node_count, ..SimConfig::default() };
        return SimTransport::new(config).start().context("starting the simulator");
    }
    match port {
        Some(path) => SerialTransport::with_port(path),
        None => SerialTransport::new(),
    }
    .start()
    .context("opening the link")
}

/// Render a link-level event that is not a frame.
///
/// These are printed for every mode: a disconnect during a capture is exactly
/// the kind of thing that should not be silent.
fn describe(event: &LinkEvent) -> Option<String> {
    match event {
        LinkEvent::Connected(info) => Some(format!(
            "bridge {} on {:?}, firmware {}",
            mac(&info.mac),
            info.chip,
            info.fw_version
        )),
        LinkEvent::Disconnected { reason } => Some(format!("link down: {reason}")),
        LinkEvent::Garbled(err) => Some(format!("undecodable frame: {err}")),
        LinkEvent::Message(_) => None,
    }
}

/// A MAC in the form the firmware's own logs and the sniffer captures use, so
/// an address can be grepped for across all three.
pub fn mac(mac: &Mac) -> String {
    mac.iter().map(|b| format!("{b:02X}")).collect::<Vec<_>>().join(":")
}
