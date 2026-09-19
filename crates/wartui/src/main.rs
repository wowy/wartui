//! wartui's command line.
//!
//! `run` is the tool; everything else checks one link in the chain when `run` is not
//! showing what it should, and `crates/wartui/README.md` § "When nothing arrives"
//! has which to reach for. `--log-file` is there because the TUI owns the terminal,
//! so the transport has nowhere else to say what it tried and what the OS said.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use wartui_bridge::remember::BridgeMemory;
use wartui_bridge::serial::{self, BridgeSpec, SerialTransport, discover_ports};
use wartui_bridge::sim::{SimConfig, SimTransport};
use wartui_bridge::{BridgeInfo, LinkEvent, LinkHandle};
use wartui_proto::link::{LoopPhase, Mac, ResetCause};

mod bench;
mod capture;
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
    /// List the Espressif boards attached, and the address of each.
    Ports,
    /// Drive the simulator into a store with nothing drawn, and report what it
    /// cost the disk.
    #[command(hide = true)]
    Bench(bench::Args),
}

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() -> Result<()> {
    let cli = parse(std::env::args_os()).unwrap_or_else(|e| e.exit());
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
        Some(Command::Bench(args)) => bench::run(args).await,
    }
}

/// Parse the command line, refusing `run`'s arguments ahead of another subcommand.
///
/// They are flattened into [`Cli`] so that a bare `wartui` runs, which means clap
/// accepts them before any subcommand too, and then nothing reads them:
/// `wartui --bridge X status` opens whatever board detection found and reports that
/// no bridge was there. An argument that silently does nothing is worse than an error,
/// so one typed there is refused. `--log-file` is global and means the same in either
/// place.
fn parse<I, T>(args: I) -> Result<Cli, clap::Error>
where
    I: IntoIterator<Item = T>,
    T: Into<std::ffi::OsString> + Clone,
{
    use clap::{CommandFactory, FromArgMatches, error::ErrorKind, parser::ValueSource};

    let mut command = Cli::command();
    let matches = command.try_get_matches_from_mut(args)?;
    if let Some((name, _)) = matches.subcommand() {
        let misplaced = command.get_arguments().find(|arg| {
            !arg.is_global_set()
                && matches.value_source(arg.get_id().as_str()) == Some(ValueSource::CommandLine)
        });
        if let Some(arg) = misplaced {
            let flag = arg.get_long().unwrap_or(arg.get_id().as_str());
            let takes_it = command
                .find_subcommand(name)
                .is_some_and(|sub| sub.get_arguments().any(|a| a.get_long() == Some(flag)));
            let advice = if takes_it {
                format!("put it after the subcommand: wartui {name} --{flag} ...")
            } else {
                format!("'{name}' does not take it")
            };
            return Err(command.error(
                ErrorKind::ArgumentConflict,
                format!("'--{flag}' before '{name}' would be ignored; {advice}"),
            ));
        }
    }
    Cli::from_arg_matches(&matches)
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

/// List the Espressif boards attached, each named by the address it transmits from.
///
/// The address is the point of the command. A node plugged in by USB is the same
/// vendor and product as the bridge and sits on an adjacent device node, so a path
/// says nothing about which board it reaches — while an ESP32's USB serial number
/// *is* its MAC, so the OS has already answered, with nothing opened and no `esp`
/// tool. The bridge is then the row whose address the fleet table shows as the
/// bridge's, and a node the row `wartui sniff` attributes heartbeats to.
fn ports() -> Result<()> {
    let found = discover_ports().context("listing serial ports")?;
    if found.is_empty() {
        println!("No Espressif board is attached.");
        println!("If the bridge is plugged in, name it with --bridge.");
        return Ok(());
    }
    for candidate in found {
        println!("{}", candidate.path);
        // Padded to the width of the longer of the two, which is the words rather
        // than an address: an unaddressed board in the list must not shift every
        // column on its own row.
        let address = candidate
            .mac()
            .map_or_else(|| "address not reported".to_owned(), |address| mac(&address));
        let product = candidate.product.as_deref().unwrap_or("unknown device");
        match (candidate.vid, candidate.pid) {
            (Some(vid), Some(pid)) => println!("  {address:<20}  {product}  ({vid:04x}:{pid:04x})"),
            _ => println!("  {address:<20}  {product}"),
        }
    }
    Ok(())
}

/// Open whichever transport the arguments called for.
fn open(bridge: Option<&str>, sim: Option<u8>, sim_c6: u8) -> Result<LinkHandle> {
    if let Some(node_count) = sim {
        let config =
            SimConfig { node_count, c6_nodes: sim_c6.min(node_count), ..SimConfig::default() };
        return SimTransport::new(config).start().context("starting the simulator");
    }
    match bridge.map(spec) {
        Some(spec) => SerialTransport::with_spec(spec),
        None => SerialTransport::new(),
    }
    // Only detection writes to the file. A board named on the command line is a
    // decision that is already written down, in the place it can be read — and
    // recording it here would mean a single `wartui status --bridge X`, which
    // changes nothing and is asked in order to find something out, quietly
    // deciding what every later run opens.
    .remember_in(if bridge.is_none() { BridgeMemory::discover() } else { BridgeMemory::none() })
    .start()
    .context("opening the link")
}

/// How a `--bridge` value was meant. Parsing it cannot fail: what is not an address
/// is a path, and whether that path exists is the transport's question.
fn spec(bridge: &str) -> BridgeSpec {
    bridge.parse().unwrap_or_else(|never| match never {})
}

/// The device path a `--bridge` value names, when one is attached.
///
/// An address is the durable way to name a board and the useless way to name it to
/// another program, so advice that quotes `espflash` resolves it first rather than
/// printing a line that cannot be run.
fn named_device(bridge: &str) -> Option<String> {
    serial::resolve(&spec(bridge)).ok()
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
    wartui_bridge::ports::mac_text(mac)
}

/// One line saying how the bridge came to be running this life.
///
/// Every command that has a [`BridgeInfo`] prints this, because a bridge that
/// restarted lost its channel and its peer table. [`ResetCause::PowerOn`] says so
/// plainly rather than being hidden, so an absent line never needs interpreting.
#[must_use]
pub fn last_reset_line(info: &BridgeInfo) -> String {
    let cause = match info.reset_cause {
        ResetCause::PowerOn => "powered on",
        ResetCause::Software => "reset by its own firmware, a panic or a reset command",
        ResetCause::Watchdog => "reset by its watchdog, so its main loop had stopped",
        ResetCause::Lockup => "reset by the chip after the CPU locked up",
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
/// `q` and ctrl-c already reach the orderly exit; `SIGTERM` and `SIGHUP` did not,
/// and the difference is not cosmetic. Leaving by a route that skips the transport's
/// `Shutdown` guard leaves whatever was queued for the bridge in the tty's output
/// queue, and closing a tty waits for that queue to drain against a device that may
/// not be reading. The wait is inside the driver, so the process survives `SIGKILL`
/// still holding the port and the only way out is unplugging the board. A window
/// closing, a `kill` or a logout are all ordinary ways to end a capture, and none
/// should cost a replug. This is the rule `serial.rs` and `sniff.rs` point at.
///
/// Built **once**, before the loop that selects on it, which is why it is a value
/// rather than an `async fn` called in the arm: asking tokio for a signal stream
/// installs a process-wide handler that replaces the default disposition, and a
/// stream subscribes only from the moment it is created. One delivered between a
/// stream being dropped at the end of a select and the next being built is seen by
/// nobody, with the default action already gone — so the capture carries on deaf and
/// the operator is left with `kill -9`.
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
/// The case that otherwise produces no output at all, spelled out because the three
/// causes need three different actions and the symptom is identical. A failure to
/// *open* the port is not this: that arrives as [`LinkEvent::Disconnected`] carrying
/// the OS's own message. The wedged case is the hard one — the port opens, the writes
/// succeed, and nothing comes back — and a reset is the only way to tell.
pub fn no_bridge_notice(bridge: Option<&str>) -> String {
    let seconds = CONNECT_NOTICE_AFTER.as_secs();
    let device = bridge.and_then(named_device);
    let (where_, reset, wartui_reset) = match (bridge, device.as_deref()) {
        (Some(name), Some(path)) => (
            format!("on {path}"),
            format!("espflash reset --port {path}"),
            format!("wartui reset --bridge {name}"),
        ),
        // A board that is not attached needs none of what follows: there is no port
        // to have been opened, so the three causes below are all about something
        // else, and the one fact worth saying is already known.
        (Some(name), None) => {
            return [
                format!("no attached board is {name}, so nothing was opened."),
                "`wartui ports` lists what is attached, each board with its address.".to_owned(),
            ]
            .join("\n");
        }
        (None, _) => (
            "on the board that was detected".to_owned(),
            "espflash reset".to_owned(),
            "wartui reset".to_owned(),
        ),
    };
    // Assembled a line at a time: a continued literal puts the source's own
    // indentation inside the string.
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
        // The fallback rather than the first suggestion: a wedged bridge reads its
        // receive endpoint perfectly well and reboots on being asked, so `espflash`
        // driving DTR/RTS is what is left when even the asking goes unanswered.
        format!("If that goes unanswered too, reset it over USB instead: {reset}"),
    ]
    .join("\n")
}

#[cfg(test)]
mod tests {
    use super::{BridgeSpec, Command, no_bridge_notice, parse, spec};

    #[test]
    fn a_bridge_before_a_subcommand_is_refused_rather_than_ignored() {
        let error = parse(["wartui", "--bridge", "/dev/ttyACM2", "status"]).err().unwrap();
        let message = error.to_string();
        assert!(message.contains("wartui status --bridge"), "{message}");
    }

    #[test]
    fn a_run_argument_the_subcommand_does_not_take_says_so() {
        let error = parse(["wartui", "--sim", "3", "export", "--out", "-"]).err().unwrap();
        let message = error.to_string();
        assert!(message.contains("'export' does not take it"), "{message}");
    }

    #[test]
    fn a_bridge_after_the_subcommand_reaches_it() {
        let cli = parse(["wartui", "status", "--bridge", "/dev/ttyACM2"]).unwrap();
        let Some(Command::Status(args)) = cli.command else { panic!("not status") };
        assert_eq!(args.bridge.as_deref(), Some("/dev/ttyACM2"));
    }

    #[test]
    fn a_bridge_can_be_named_by_its_address_as_readily_as_by_a_path() {
        let cli = parse(["wartui", "status", "--bridge", "10:BD:A3:EC:44:C0"]).unwrap();
        let Some(Command::Status(args)) = cli.command else { panic!("not status") };
        assert!(matches!(spec(args.bridge.as_deref().expect("a bridge")), BridgeSpec::Mac(_)));
    }

    #[test]
    fn run_arguments_without_a_subcommand_still_run() {
        let cli = parse(["wartui", "--bridge", "/dev/ttyACM2"]).unwrap();
        assert!(cli.command.is_none());
        assert_eq!(cli.run.bridge.as_deref(), Some("/dev/ttyACM2"));
    }

    #[test]
    fn the_log_file_is_global_and_allowed_on_either_side() {
        for args in [
            ["wartui", "--log-file", "w.log", "status", "--bridge", "/dev/x"],
            ["wartui", "status", "--log-file", "w.log", "--bridge", "/dev/x"],
        ] {
            let cli = parse(args).unwrap();
            assert_eq!(cli.log_file.as_deref(), Some(std::path::Path::new("w.log")));
        }
    }

    #[test]
    fn the_notice_names_the_board_it_was_waiting_on() {
        let notice = no_bridge_notice(Some("/dev/cu.usbmodem2101"));
        assert!(notice.contains("on /dev/cu.usbmodem2101 after 5s"), "{notice}");
        // The remedy has to be runnable as printed, which means carrying the
        // same port through rather than leaving the reader to fill it in.
        assert!(notice.contains("espflash reset --port /dev/cu.usbmodem2101"), "{notice}");
    }

    #[test]
    fn without_a_named_board_the_notice_still_reads_as_a_sentence() {
        let notice = no_bridge_notice(None);
        assert!(notice.contains("on the board that was detected"), "{notice}");
        assert!(notice.contains("espflash reset"), "{notice}");
        // No dangling `--port` with nothing after it.
        assert!(!notice.contains("--port"), "{notice}");
    }

    #[test]
    fn the_notice_carries_no_stray_indentation() {
        // A wrapped literal keeps the source's leading whitespace, which reads as
        // ragged gaps mid-sentence and is invisible here.
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
