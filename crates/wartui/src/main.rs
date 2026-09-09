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
use std::time::Duration;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use wartui_bridge::serial::{SerialTransport, discover_ports};
use wartui_bridge::sim::{SimConfig, SimTransport};
use wartui_bridge::{BridgeInfo, LinkEvent, LinkHandle};
use wartui_proto::link::{LoopPhase, Mac, ResetCause};

mod export;
mod reset;
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
    /// Reboot the bridge, for when it has stopped answering.
    Reset(reset::Args),
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
        Some(Command::Reset(args)) => reset::run(args).await,
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
fn open(port: Option<&str>, sim: Option<u8>, sim_c6: u8) -> Result<LinkHandle> {
    if let Some(node_count) = sim {
        let config =
            SimConfig { node_count, c6_nodes: sim_c6.min(node_count), ..SimConfig::default() };
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

/// One line saying how the bridge came to be running this life.
///
/// Every command that has a [`BridgeInfo`] prints this, because a bridge that
/// restarted is a bridge that lost its channel and its peer table, and until
/// v3 of the link protocol the only trace of that was a counter going
/// backwards. [`ResetCause::PowerOn`] is the ordinary case and says so
/// plainly rather than being hidden, so that the absence of a line never has
/// to be interpreted.
#[must_use]
pub fn last_reset_line(info: &BridgeInfo) -> String {
    let cause = match info.reset_cause {
        ResetCause::PowerOn => "powered on",
        ResetCause::Software => "reset by its own firmware, a panic or a reset command",
        ResetCause::Watchdog => "reset by its watchdog, so its main loop had stopped",
        ResetCause::Brownout => "reset by a brownout, so check the cable and the hub",
        ResetCause::External => "reset over USB, by espflash or a replug",
        ResetCause::Unknown => "reset for a reason it could not name",
    };
    // The phase is only ever a hint, and a misleading one on its own: the loop
    // visits most of them every millisecond, so naming one is worth doing only
    // where it points at a specific blocking call.
    let phase = match info.last_phase {
        LoopPhase::TxStalled => Some("its transmit path had stopped draining"),
        LoopPhase::Transmit => Some("it was inside an ESP-NOW send"),
        LoopPhase::Command => Some("it was carrying out a host command"),
        LoopPhase::DrainRadio => Some("it was draining the radio"),
        LoopPhase::DrainLink => Some("it was reading host commands"),
        LoopPhase::Pump => Some("it was writing to the USB endpoint"),
        LoopPhase::Boot => Some("it had not reached its main loop"),
        LoopPhase::Idle | LoopPhase::Unknown => None,
    };
    match phase {
        Some(phase) => format!("last reset  {cause}; {phase}"),
        None => format!("last reset  {cause}"),
    }
}

/// Listens for the process being asked to stop by a signal rather than a key.
///
/// `q` and ctrl-c already reach the orderly exit; `SIGTERM` and `SIGHUP` did
/// not, and the difference is not cosmetic. Leaving by a route that skips the
/// transport's `Shutdown` guard leaves whatever was queued for the bridge
/// sitting in the tty's output queue, and closing a tty waits for that queue to
/// drain — against a device that may not be reading. That wait is inside the
/// driver, so the process survives `SIGKILL` still holding the port, and the
/// only way out is unplugging the board. A window that closes, a `kill`, or a
/// logout are all ordinary ways to end a capture and none of them should be
/// able to cost the operator a replug.
///
/// Built **once**, before the loop that selects on it, and this is the whole
/// reason it is a value rather than an `async fn` called in the arm. Asking
/// tokio for a signal stream installs a process-wide handler that replaces the
/// default disposition — after the first call, a `SIGTERM` no longer kills the
/// process by itself — and a stream subscribes from the moment it is created,
/// so one delivered between a stream being dropped at the end of a select and
/// the next one being built is seen by nobody. The default action is gone and
/// nothing replaced it: the capture carries on, deaf, and the operator is left
/// with `kill -9`, which is exactly the replug this exists to prevent.
///
/// Never fires if the handlers cannot be installed. A future that fired
/// spuriously here would quit a capture for no reason at all.
#[cfg(unix)]
pub struct Terminate {
    // `Option` because a failure to install is not a failure to run: a process
    // that cannot watch for `SIGTERM` still has a `q` key.
    term: Option<tokio::signal::unix::Signal>,
    hup: Option<tokio::signal::unix::Signal>,
}

#[cfg(unix)]
impl Terminate {
    pub fn new() -> Self {
        use tokio::signal::unix::{SignalKind, signal};
        Self { term: signal(SignalKind::terminate()).ok(), hup: signal(SignalKind::hangup()).ok() }
    }

    /// Resolves when one of them arrives. Cancel-safe, as `Signal::recv` is,
    /// so losing the race in a `select!` drops nothing.
    pub async fn recv(&mut self) {
        match (&mut self.term, &mut self.hup) {
            (Some(term), Some(hup)) => {
                tokio::select! {
                    _ = term.recv() => {}
                    _ = hup.recv() => {}
                }
            }
            (Some(only), None) | (None, Some(only)) => {
                only.recv().await;
            }
            (None, None) => std::future::pending().await,
        }
    }
}

/// No `SIGTERM` to catch, so this simply never fires.
#[cfg(not(unix))]
pub struct Terminate;

#[cfg(not(unix))]
impl Terminate {
    pub fn new() -> Self {
        Self
    }

    pub async fn recv(&mut self) {
        std::future::pending().await
    }
}

impl Default for Terminate {
    fn default() -> Self {
        Self::new()
    }
}

/// How long to wait for a bridge to identify itself before saying nothing has.
///
/// The bridge answers in about two milliseconds when it is well, so this is not
/// a latency budget — it is long enough that a dongle still bringing its radio
/// up is not accused of being wedged.
pub const CONNECT_NOTICE_AFTER: Duration = Duration::from_secs(5);

/// What to say when the port opened and nothing behind it answered.
///
/// This is the case that otherwise produces no output at all, and it is worth
/// spelling out because the three causes need three different actions and the
/// symptom is identical for all of them. A failure to *open* the port is not
/// this: that arrives as [`LinkEvent::Disconnected`] carrying the OS's own
/// message, which every command already prints.
///
/// The wedged case is the one that cost an afternoon to recognise: a bridge
/// that has been powered for a long time can stop answering while still
/// enumerating as a USB device, so the port opens, the writes succeed and
/// nothing comes back. A reset clears it, and there is no way to tell that
/// from the host except by trying.
pub fn no_bridge_notice(port: Option<&str>) -> String {
    let seconds = CONNECT_NOTICE_AFTER.as_secs();
    let (where_, reset, wartui_reset) = match port {
        Some(path) => (
            format!("on {path}"),
            format!("espflash reset --port {path}"),
            format!("wartui reset --port {path}"),
        ),
        None => (
            "on the port that was discovered".to_owned(),
            "espflash reset".to_owned(),
            "wartui reset".to_owned(),
        ),
    };
    // Assembled a line at a time rather than as one continued literal: the
    // wrapped form puts the source's own indentation inside the string, and
    // this text is read by someone already having a bad afternoon.
    [
        format!("nothing has identified itself as a bridge {where_} after {seconds}s."),
        "The port opened, so a device is there and it is not speaking the link protocol."
            .to_owned(),
        "Usually one of:".to_owned(),
        "  - it is running node firmware rather than bridge firmware".to_owned(),
        format!("  - the bridge is wedged, which a reboot clears: {wartui_reset}"),
        "  - another program is holding the port".to_owned(),
        "`wartui ports` lists what is attached; `--log-file` records what the transport tried."
            .to_owned(),
        // The fallback rather than the first suggestion, now that the first one
        // is known to work: a wedged bridge reads its receive endpoint
        // perfectly well, so it reboots on being asked. `espflash` drives
        // DTR/RTS and needs no firmware at all, which is what is left when even
        // the asking goes unanswered.
        format!("If that goes unanswered too, reset it over USB instead: {reset}"),
    ]
    .join("\n")
}

#[cfg(test)]
mod tests {
    use super::no_bridge_notice;

    #[test]
    fn the_notice_names_the_port_it_was_waiting_on() {
        let notice = no_bridge_notice(Some("/dev/cu.usbmodem2101"));
        assert!(notice.contains("on /dev/cu.usbmodem2101 after 5s"), "{notice}");
        // The remedy has to be runnable as printed, which means carrying the
        // same port through rather than leaving the reader to fill it in.
        assert!(notice.contains("espflash reset --port /dev/cu.usbmodem2101"), "{notice}");
    }

    #[test]
    fn without_a_port_the_notice_still_reads_as_a_sentence() {
        let notice = no_bridge_notice(None);
        assert!(notice.contains("on the port that was discovered"), "{notice}");
        assert!(notice.contains("espflash reset"), "{notice}");
        // No dangling `--port` with nothing after it.
        assert!(!notice.contains("--port"), "{notice}");
    }

    #[test]
    fn the_notice_carries_no_stray_indentation() {
        // A wrapped string literal keeps the source's leading whitespace, which
        // reads as ragged gaps mid-sentence and is invisible in the source.
        let notice = no_bridge_notice(Some("/dev/x"));
        for line in notice.lines() {
            // The bullets' own two-space indent is deliberate; a gap after the
            // text has started is the symptom being guarded against.
            let body = line.trim_start();
            assert!(!body.contains("  "), "gap inside {line:?}");
            assert_eq!(line, line.trim_end(), "trailing space in {line:?}");
        }
    }

    #[test]
    fn the_bullets_are_the_only_indented_lines() {
        let notice = no_bridge_notice(Some("/dev/x"));
        for line in notice.lines().filter(|l| l.starts_with(' ')) {
            assert!(line.starts_with("  - "), "unexpected indent: {line:?}");
        }
    }

    #[test]
    fn the_notice_names_all_three_causes() {
        let notice = no_bridge_notice(Some("/dev/x"));
        for cause in ["node firmware", "wedged", "another program"] {
            assert!(notice.contains(cause), "missing {cause}: {notice}");
        }
    }
}
