//! wartui's command line.
//!
//! `run` is the tool; `flash-fleet` and `flash-bridge` put the firmware on the boards it
//! drives, and `ota` updates the nodes' firmware through the bridge. Everything else checks one
//! link in the chain when `run` is not showing what it should, and `crates/wartui/README.md`
//! § "When nothing arrives" has which to reach for.
//! `--log-file` is there because the TUI owns the terminal, so the transport has nowhere else
//! to say what it tried and what the OS said.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use wartui_bridge::ports::{PortCandidate, is_ch343};
use wartui_bridge::remember::BridgeMemory;
use wartui_bridge::serial::{self, BridgeSpec, SerialTransport, discover_ports};
use wartui_bridge::sim::{SimConfig, SimTransport};
use wartui_bridge::{BridgeInfo, LinkEvent, LinkHandle, NO_FIRMWARE_P4};
use wartui_proto::link::{BridgeToHost, LoopPhase};
use wartui_proto::mac;
use wartui_proto::reset::ResetCause;

mod analyze;
mod bench;
mod capture;
mod config;
mod export;
mod fan;
mod flash;
mod ota;
mod pool;
mod power_cycle;
mod reset;
mod run;
mod scan;
mod sniff;
mod status;
#[cfg(test)]
mod testing;
mod text;
mod tui;
mod upload;

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
    /// Summarize a capture: what it holds and what it lost on the way in.
    Analyze(analyze::Args),
    /// Upload a capture to the WDGWars leaderboard.
    Upload(upload::Args),
    /// Print every frame the bridge hears.
    Sniff(sniff::Args),
    /// Ask the bridge for its counters and uptime.
    Status(status::Args),
    /// Reboot the bridge, for when it has stopped answering.
    Reset(reset::Args),
    /// Switch one node off and on through the carrier board.
    PowerCycle(power_cycle::Args),
    /// Read or set the carrier's lid fan duty, which the bridge keeps across its reboots.
    Fan(fan::Args),
    /// Read or set the channel pool, which the bridge keeps across its reboots.
    Pool(pool::Args),
    /// Start or stop the bridge's scan, or show it. The bridge scans on with no host attached.
    Scan(scan::Args),
    /// List the Espressif boards and CH343s attached, and the address of each, asking each CH343.
    Ports,
    /// Flash node firmware onto every attached ESP32-C5, skipping the P4 bridge.
    FlashFleet(flash::fleet::Args),
    /// Update the P4 bridge's firmware over its link.
    FlashBridge(flash::bridge::Args),
    /// Update nodes' firmware through the bridge.
    Ota(ota::Args),
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
        // Each command loads `wartui.toml` itself, if at all. `status`, `sniff`, `reset`,
        // `power-cycle`, `fan`, `pool`, `scan` and `ota` read a broken one as no bridge
        // remembered, and `ports` and `flash-fleet` never read it.
        None => run::run(cli.run).await,
        Some(Command::Run(args)) => run::run(args).await,
        Some(Command::Export(args)) => export::run(args),
        Some(Command::Analyze(args)) => analyze::run(args),
        Some(Command::Upload(args)) => upload::run(args),
        Some(Command::Sniff(args)) => sniff::run(args).await,
        Some(Command::Status(args)) => status::run(args).await,
        Some(Command::Reset(args)) => reset::run(args).await,
        Some(Command::PowerCycle(args)) => power_cycle::run(args).await,
        Some(Command::Fan(args)) => fan::run(args).await,
        Some(Command::Pool(args)) => pool::run(args).await,
        Some(Command::Scan(args)) => scan::run(args).await,
        Some(Command::Ports) => ports().await,
        Some(Command::FlashFleet(args)) => flash::fleet::run(args),
        Some(Command::FlashBridge(args)) => flash::bridge::run(args).await,
        Some(Command::Ota(args)) => ota::run(args).await,
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

/// List the Espressif boards and CH343s attached, each named by the address it transmits
/// from.
///
/// The address is the point of the command. Every node reports one vendor and product
/// and sits on an adjacent device node, so a path says nothing about which node it
/// reaches — while a node's USB serial number *is* its MAC, so the OS has already
/// answered, with nothing opened and no `esp` tool. A CH343's serial number is its own,
/// so each is asked with `Identify` (`serial::probe`), and the row names the bridge
/// that answered. A node is the row `wartui sniff` attributes heartbeats to.
async fn ports() -> Result<()> {
    let found = discover_ports().context("listing serial ports")?;
    if found.is_empty() {
        println!("{}", no_boards_notice());
        return Ok(());
    }
    for candidate in found {
        println!("{}", candidate.path);
        let (address, product) = if is_ch343(&candidate) {
            let probed = serial::probe(candidate.clone()).await;
            match (probed.mac, probed.opened) {
                (Some(mac), _) => (mac::full(&mac).to_string(), "P4 bridge on a CH343"),
                (None, true) => ("not answering".to_owned(), "CH343"),
                (None, false) => ("busy or refused".to_owned(), "CH343"),
            }
        } else {
            let address = candidate.mac().map_or_else(
                || "address not reported".to_owned(),
                |mac| mac::full(&mac).to_string(),
            );
            (address, candidate.product.as_deref().unwrap_or("unknown device"))
        };
        // Padded to the width of the longest words rather than an address: an unaddressed
        // board in the list must not shift every column on its own row.
        match (candidate.vid, candidate.pid) {
            (Some(vid), Some(pid)) => println!("  {address:<20}  {product}  ({vid:04x}:{pid:04x})"),
            _ => println!("  {address:<20}  {product}"),
        }
    }
    Ok(())
}

/// What `wartui ports` says with no board to list. A board the list misses has no address to
/// name it by, so only a path reaches it.
fn no_boards_notice() -> String {
    [
        "No Espressif board or CH343 is attached.".to_owned(),
        NO_FIRMWARE_P4.to_owned(),
        "A bridge not listed here can be named only by its path: --bridge PATH.".to_owned(),
    ]
    .join("\n")
}

/// The bridge saved in `wartui.toml`, from `explicit` (`--config`) or the default file, for a
/// command that does not save it to open first.
///
/// What the transport remembers into it is dropped at exit. Only `run` and `flash-bridge` save
/// the bridge, so a one-off `wartui status --bridge X` never decides what every later run opens.
fn memory(explicit: Option<&Path>) -> BridgeMemory {
    let bridge = config::remembered_bridge(explicit);
    BridgeMemory::holding(bridge.mac).on_port(bridge.serial)
}

/// `--sim`'s node count: at most one node per node-bus slot, as on a real bridge.
fn sim_nodes() -> clap::builder::RangedI64ValueParser<u8> {
    clap::value_parser!(u8).range(0..=wartui_proto::plan::MAX_NODES as i64)
}

/// Open whichever transport the arguments called for.
fn open(bridge: Option<&str>, sim: Option<u8>, memory: BridgeMemory) -> Result<LinkHandle> {
    if let Some(node_count) = sim {
        let config = SimConfig { node_count, ..SimConfig::default() };
        return SimTransport::new(config).start().context("starting the simulator");
    }
    match bridge.map(spec) {
        Some(spec) => SerialTransport::with_spec(spec),
        None => SerialTransport::new(),
    }
    .remember_in(memory)
    .start()
    .context("opening the link")
}

/// Open the one bridge `named` resolves to, as `wartui reset` resolves it
/// ([`reset::board_to_reset`]), with the remembered bridge read from `config`, and wait for its
/// `Ready`. `fan`, `pool`, `scan` and `power-cycle` connect through this.
async fn connect_one(
    named: Option<&str>,
    config: Option<&Path>,
) -> Result<(LinkHandle, BridgeInfo)> {
    let remembered = crate::config::remembered_bridge(config);
    let port =
        reset::board_to_reset(named, &remembered, serial::discover_bridges, |found| async move {
            serial::probe_all(&found).await
        })
        .await?;
    let memory = BridgeMemory::holding(remembered.mac).on_port(remembered.serial);
    let mut link = open(Some(&port), None, memory)?;
    match tokio::time::timeout(CONNECT_NOTICE_AFTER, status::wait_for_ready(&mut link)).await {
        Ok(ready) => Ok((link, ready?)),
        Err(_) => anyhow::bail!("{}", no_bridge_notice(named)),
    }
}

/// Wait `wait`, printing each fault the bridge reports meanwhile, such as a setting not saved.
async fn settle(link: &mut LinkHandle, wait: Duration) -> Result<()> {
    let until = tokio::time::Instant::now() + wait;
    loop {
        match tokio::time::timeout_at(until, link.recv()).await {
            Err(_) => return Ok(()),
            Ok(Some(LinkEvent::Message(BridgeToHost::Error { message }))) => {
                println!("fault      {message}");
            }
            Ok(Some(LinkEvent::Disconnected { reason })) => anyhow::bail!("{reason}"),
            Ok(Some(_)) => {}
            Ok(None) => anyhow::bail!("the link closed"),
        }
    }
}

/// How a `--bridge` value was meant. Parsing it cannot fail: what is not an address
/// is a path, and whether that path exists is the transport's question.
fn spec(bridge: &str) -> BridgeSpec {
    bridge.parse().unwrap_or_else(|never| match never {})
}

/// Render a link-level event that is not a frame.
///
/// These are printed for every mode: a disconnect during a capture is exactly
/// the kind of thing that should not be silent.
fn describe(event: &LinkEvent) -> Option<String> {
    match event {
        LinkEvent::Connected(info) => {
            Some(format!("bridge {}, firmware {}", mac::full(&info.mac), info.fw_version))
        }
        LinkEvent::Disconnected { reason } => Some(format!("link down: {reason}")),
        LinkEvent::Garbled(err) => Some(format!("undecodable frame: {err}")),
        LinkEvent::Message(_) => None,
    }
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
        ResetCause::Watchdog => {
            "reset by its watchdog: a hung image, or one on trial that no host confirmed"
        }
        ResetCause::Lockup => "reset by the chip after the CPU locked up",
        ResetCause::Brownout => "reset by a brownout, so check the cable and the hub",
        ResetCause::External => "reset over USB, by espflash or a replug",
        ResetCause::Unknown => "reset for a reason it could not name",
    };
    // The phase is only ever a hint: the radio link marks it on every packet, so it says
    // what ran last, not what stopped the bridge (`LoopPhase`). `Boot` is the exception:
    // nothing else has marked yet, so it is a fact. `Unknown` has nothing to name.
    let phase = match info.last_phase {
        LoopPhase::Command => Some("it last ran a host command"),
        LoopPhase::DrainLink => Some("it last read host commands"),
        LoopPhase::Pump => Some("it last wrote to the host"),
        LoopPhase::Boot => Some("it had not finished starting"),
        LoopPhase::RadioLink => Some("it last ran its radio link"),
        LoopPhase::Unknown => None,
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

/// What to say when no bridge has answered: [`silence_notice`] over the CH343s attached now.
/// A listing that fails reads as none attached, and the transport names that failure.
pub fn no_bridge_notice(bridge: Option<&str>) -> String {
    silence_notice(bridge, &serial::discover_bridges().unwrap_or_default())
}

/// What to say about a silence, given the CH343s attached.
///
/// The case that otherwise produces no output at all, spelled out because each cause needs
/// its own action and the symptom is identical. A failure to *open* the port is not this:
/// that arrives as [`LinkEvent::Disconnected`] carrying the OS's own message. The wedged
/// case is the hard one — the port opens, the writes succeed, and nothing comes back — and
/// a reset is the only way to tell.
///
/// Only a path can reach a node: an address and a sweep ask CH343s alone. So node
/// firmware is a cause for a path and for nothing else. An address is matched by asking each
/// CH343, so a silence there may be no CH343 answering with it, and the transport's reason
/// says which.
fn silence_notice(bridge: Option<&str>, bridges: &[PortCandidate]) -> String {
    let seconds = CONNECT_NOTICE_AFTER.as_secs();
    let named = bridge.map(spec);
    let by_path = matches!(named, Some(BridgeSpec::Path(_)));
    if bridges.is_empty() && !by_path {
        // Nothing to open, so nothing to reset: `wartui reset` would refuse too.
        return [
            "no CH343 is attached, so nothing was opened.".to_owned(),
            "`wartui ports` lists what is attached, each board with its address.".to_owned(),
            NO_FIRMWARE_P4.to_owned(),
        ]
        .join("\n");
    }
    let (where_, wartui_reset, opened) = match (bridge, &named) {
        (Some(name), Some(BridgeSpec::Path(path))) => (
            format!("on {path}"),
            format!("wartui reset --bridge {name}"),
            "The port opened, so a device is there and it is not speaking the link protocol.",
        ),
        (Some(name), _) => (
            format!("as {name}"),
            format!("wartui reset --bridge {name}"),
            "Each CH343 attached was asked, and none has answered with that address.",
        ),
        (None, _) => (
            "on the board that was detected".to_owned(),
            "wartui reset".to_owned(),
            "The port opened, so a device is there and it is not speaking the link protocol.",
        ),
    };
    // Assembled a line at a time: a continued literal puts the source's own
    // indentation inside the string.
    let mut lines = vec![
        format!("nothing has identified itself as a bridge {where_} after {seconds}s."),
        opened.to_owned(),
        "Usually one of:".to_owned(),
    ];
    if by_path {
        lines.push("  - it is running node firmware rather than bridge firmware".to_owned());
    }
    lines.extend([
        format!("  - the bridge is wedged, which a reboot clears: {wartui_reset}"),
        // Reachable with the port open: the exclusive open is refused only by a holder that
        // took `TIOCEXCL` or `flock`. One that took neither, such as `cat`, keeps reading from it.
        "  - another program is holding the port".to_owned(),
        "`wartui ports` lists what is attached; `--log-file` records what the transport tried."
            .to_owned(),
        // The fallback rather than the first suggestion: a bridge that has stopped
        // answering may still read, and reboots on being asked.
        "If that goes unanswered too, power-cycle it.".to_owned(),
    ]);
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use wartui_bridge::ports::{CH343_PID, CH343_VID, PortCandidate, candidate};

    use super::{
        BridgeInfo, BridgeSpec, Command, last_reset_line, no_boards_notice, parse, silence_notice,
        spec,
    };
    use wartui_proto::link::LoopPhase;
    use wartui_proto::reset::ResetCause;

    /// A P4 bridge's CH343 as the OS lists it.
    fn p4_bridge(device: &str) -> PortCandidate {
        candidate(device, Some(CH343_VID), Some(CH343_PID), Some("5CF7153520"))
    }

    #[test]
    fn cli_parser_rejects_global_bridge_flag_when_placed_before_subcommand() {
        let error = parse(["wartui", "--bridge", "/dev/ttyACM2", "status"]).err().unwrap();
        let message = error.to_string();
        assert!(message.contains("wartui status --bridge"), "{message}");
    }

    #[test]
    fn cli_parser_rejects_run_specific_flags_when_passed_to_subcommand() {
        let error = parse(["wartui", "--sim", "3", "export", "--out", "-"]).err().unwrap();
        let message = error.to_string();
        assert!(message.contains("'export' does not take it"), "{message}");
    }

    #[test]
    fn cli_parser_passes_bridge_argument_when_placed_after_subcommand() {
        let cli = parse(["wartui", "status", "--bridge", "/dev/ttyACM2"]).unwrap();
        let Some(Command::Status(args)) = cli.command else { panic!("not status") };
        assert_eq!(args.bridge.as_deref(), Some("/dev/ttyACM2"));
    }

    #[test]
    fn cli_parser_accepts_mac_address_as_bridge_spec_when_parsing_args() {
        let cli = parse(["wartui", "status", "--bridge", "02:00:5E:10:44:C0"]).unwrap();
        let Some(Command::Status(args)) = cli.command else { panic!("not status") };
        assert!(matches!(spec(args.bridge.as_deref().expect("a bridge")), BridgeSpec::Mac(_)));
    }

    #[test]
    fn cli_parser_defaults_to_run_command_when_subcommand_is_omitted() {
        let cli = parse(["wartui", "--bridge", "/dev/ttyACM2"]).unwrap();
        assert!(cli.command.is_none());
        assert_eq!(cli.run.bridge.as_deref(), Some("/dev/ttyACM2"));
    }

    #[test]
    fn cli_parser_rejects_removed_flags_when_given_before_or_after_subcommand() {
        // The bridge holds the pool, set with `wartui pool` or the settings modal; the store
        // commits at its built-in interval.
        for flag in [["--pool", "eu"], ["--commit-interval", "500"]] {
            for args in [
                ["wartui", flag[0], flag[1]].as_slice(),
                ["wartui", "run", flag[0], flag[1]].as_slice(),
            ] {
                assert!(parse(args).is_err(), "{args:?} should be refused");
            }
        }
    }

    #[test]
    fn cli_parser_rejects_sim_node_count_when_over_max_nodes() {
        let max = wartui_proto::plan::MAX_NODES.to_string();
        let over = (wartui_proto::plan::MAX_NODES + 1).to_string();
        for command in [None, Some("run"), Some("sniff"), Some("status"), Some("ota")] {
            let args: Vec<&str> =
                ["wartui"].into_iter().chain(command).chain(["--sim", &over]).collect();
            assert!(parse(args.as_slice()).is_err(), "{args:?} should be refused");
        }
        assert!(parse(["wartui", "--sim", max.as_str()]).is_ok());
    }

    #[test]
    fn cli_parser_accepts_log_file_flag_when_placed_before_or_after_subcommand() {
        for args in [
            ["wartui", "--log-file", "w.log", "status", "--bridge", "/dev/x"],
            ["wartui", "status", "--log-file", "w.log", "--bridge", "/dev/x"],
        ] {
            let cli = parse(args).unwrap();
            assert_eq!(cli.log_file.as_deref(), Some(std::path::Path::new("w.log")));
        }
    }

    #[test]
    fn cli_parser_accepts_config_flag_when_passed_to_run() {
        let cli = parse(["wartui", "--config", "w.toml"]).unwrap();
        assert!(cli.command.is_none());
        assert_eq!(cli.run.config.as_deref(), Some(std::path::Path::new("w.toml")));

        let cli = parse(["wartui", "run", "--config", "w.toml"]).unwrap();
        let Some(Command::Run(args)) = cli.command else { panic!("not run") };
        assert_eq!(args.config.as_deref(), Some(std::path::Path::new("w.toml")));
    }

    #[test]
    fn cli_parser_accepts_config_flag_when_passed_after_bridge_reading_subcommand() {
        for command in ["status", "sniff", "reset", "ota"] {
            assert!(parse(["wartui", command, "--config", "w.toml"]).is_ok(), "{command}");
        }
        assert!(parse(["wartui", "flash-bridge", "--config", "w.toml"]).is_ok());
    }

    #[test]
    fn cli_parser_accepts_power_cycle_when_given_slot_bridge_and_config() {
        let args = ["wartui", "power-cycle", "--slot", "N3", "--bridge", "44:C0", "--config", "w"];
        assert!(matches!(parse(args).unwrap().command, Some(Command::PowerCycle(_))));
        // Which node is the whole question, so there is no default.
        assert!(parse(["wartui", "power-cycle"]).is_err());
    }

    #[test]
    fn cli_parser_refuses_bluetooth_when_given_as_a_command() {
        // The bridge picks the node that scans Bluetooth; `scan start --bluetooth` asks for it.
        assert!(parse(["wartui", "bluetooth", "N3"]).is_err());
    }

    #[test]
    fn cli_parser_accepts_scan_when_given_an_action_or_none() {
        for args in [
            ["wartui", "scan"].as_slice(),
            &["wartui", "scan", "--bridge", "44:C0", "--config", "w.toml"],
            &["wartui", "scan", "start"],
            &["wartui", "scan", "start", "--bluetooth", "--bridge", "44:C0"],
            &["wartui", "scan", "stop", "--config", "w.toml"],
            &["wartui", "scan", "stop", "--sim"],
        ] {
            let cli = parse(args).unwrap_or_else(|e| panic!("{args:?}: {e}"));
            assert!(matches!(cli.command, Some(Command::Scan(_))), "{args:?}");
        }
    }

    #[test]
    fn cli_parser_refuses_scan_when_given_unknown_action_or_bluetooth_on_stop() {
        // Wi-Fi is the scan; Bluetooth is added to it, never started alone.
        for args in [
            ["wartui", "scan", "bluetooth"].as_slice(),
            &["wartui", "scan", "wifi"],
            &["wartui", "scan", "stop", "--bluetooth"],
            &["wartui", "scan", "--bluetooth"],
            &["wartui", "scan", "--bridge", "44:C0", "start"],
        ] {
            assert!(parse(args).is_err(), "{args:?} should be refused");
        }
    }

    #[test]
    fn cli_parser_accepts_fan_when_given_duty_bridge_and_config_or_nothing() {
        let args = ["wartui", "fan", "40", "--bridge", "44:C0", "--config", "w"];
        assert!(matches!(parse(args).unwrap().command, Some(Command::Fan(_))));
        assert!(matches!(parse(["wartui", "fan"]).unwrap().command, Some(Command::Fan(_))));
    }

    #[test]
    fn cli_parser_rejects_fan_duty_when_over_100() {
        assert!(parse(["wartui", "fan", "101"]).is_err());
        assert!(parse(["wartui", "fan", "100"]).is_ok());
    }

    #[test]
    fn cli_parser_rejects_bridge_and_config_flags_when_passed_to_flash_fleet() {
        // It knows a P4 bridge by its USB IDs, so it has no use for either.
        for flag in [["--bridge", "/dev/x"], ["--config", "w.toml"]] {
            let args = ["wartui", "flash-fleet", flag[0], flag[1]];
            assert!(parse(args).is_err(), "{args:?} should be refused");
        }
    }

    #[test]
    fn cli_parser_rejects_config_flag_when_placed_before_other_subcommand() {
        // `run`'s flags are refused ahead of another subcommand, same as `--bridge` above.
        let error = parse(["wartui", "--config", "w.toml", "status"]).err().unwrap();
        let advice = "put it after the subcommand: wartui status --config";
        assert!(error.to_string().contains(advice), "{error}");

        let error = parse(["wartui", "--config", "w.toml", "ports"]).err().unwrap();
        assert!(error.to_string().contains("'ports' does not take it"), "{error}");
        // After it: `ports` has no `--config` of its own, so clap refuses an unknown argument.
        assert!(parse(["wartui", "ports", "--config", "w.toml"]).is_err());
    }

    #[test]
    fn cli_parser_accepts_flash_fleet_when_given_image_and_dry_run() {
        let cli = parse(["wartui", "flash-fleet", "--image", "fw.bin", "--dry-run"]).unwrap();
        let Some(Command::FlashFleet(args)) = cli.command else { panic!("not flash-fleet") };
        assert_eq!(args.image.as_deref(), Some(std::path::Path::new("fw.bin")));
        assert!(args.dry_run);
        assert_eq!(args.jobs, 4);
    }

    #[test]
    fn cli_parser_refuses_flash_fleet_when_given_features() {
        // The node firmware builds for one chip and has no features to choose.
        assert!(parse(["wartui", "flash-fleet", "--features", "esp32c5"]).is_err());
    }

    #[test]
    fn cli_parser_accepts_flash_bridge_when_given_image_and_dry_run() {
        let cli = parse(["wartui", "flash-bridge", "--image", "fw.bin", "--dry-run"]).unwrap();
        let Some(Command::FlashBridge(args)) = cli.command else { panic!("not flash-bridge") };
        assert_eq!(args.image.as_deref(), Some(std::path::Path::new("fw.bin")));
        assert!(args.dry_run);
        assert_eq!(args.bridge, None);
    }

    #[test]
    fn cli_parser_accepts_flash_bridge_when_given_bridge() {
        let cli = parse(["wartui", "flash-bridge", "--bridge", "71:F4"]).unwrap();
        let Some(Command::FlashBridge(args)) = cli.command else { panic!("not flash-bridge") };
        assert_eq!(args.bridge.as_deref(), Some("71:F4"));
    }

    #[test]
    fn cli_parser_refuses_flash_bridge_when_given_features() {
        // The P4 bridge firmware has no features to select.
        assert!(parse(["wartui", "flash-bridge", "--features", "gps"]).is_err());
    }

    #[test]
    fn cli_diagnostics_points_at_first_flash_and_path_when_no_board_is_attached() {
        let notice = no_boards_notice();
        assert!(notice.contains("CH343"), "{notice}");
        assert!(notice.contains("firmware/p4-bridge/README.md"), "{notice}");
        assert!(notice.contains("--bridge PATH"), "{notice}");
    }

    #[test]
    fn cli_diagnostics_formats_named_bridge_notice_when_bridge_is_specified() {
        let notice = silence_notice(Some("/dev/cu.usbmodem2101"), &[]);
        assert!(notice.contains("on /dev/cu.usbmodem2101 after 5s"), "{notice}");
        // The remedy has to be runnable as printed, which means carrying the
        // same board through rather than leaving the reader to fill it in.
        assert!(notice.contains("wartui reset --bridge /dev/cu.usbmodem2101"), "{notice}");
    }

    #[test]
    fn cli_diagnostics_names_wedge_and_holder_when_detected_p4_bridge_is_silent() {
        let notice = silence_notice(None, &[p4_bridge("/dev/ttyACM0")]);
        assert!(notice.contains("on the board that was detected"), "{notice}");
        assert!(notice.contains("The port opened"), "{notice}");
        assert!(notice.contains("wedged, which a reboot clears: wartui reset"), "{notice}");
        assert!(notice.contains("another program"), "{notice}");
        assert!(notice.contains("power-cycle"), "{notice}");
        // A sweep opens only CH343s.
        assert!(!notice.contains("node firmware"), "{notice}");
        // No dangling `--bridge` with nothing after it.
        assert!(!notice.contains("--bridge"), "{notice}");
    }

    #[test]
    fn cli_diagnostics_says_nothing_opened_when_no_ch343_is_attached() {
        let notice = silence_notice(None, &[]);
        assert!(notice.contains("no CH343 is attached, so nothing was opened"), "{notice}");
        assert!(notice.contains("wartui ports"), "{notice}");
        assert!(notice.contains("CH343"), "{notice}");
        assert!(notice.contains("firmware/p4-bridge/README.md"), "{notice}");
        // There is no port for any of these to be about.
        for wrong in ["wartui reset", "The port opened", "power-cycle"] {
            assert!(!notice.contains(wrong), "{wrong}: {notice}");
        }
    }

    #[test]
    fn cli_diagnostics_omits_node_firmware_when_address_names_silent_p4_bridge() {
        let notice = silence_notice(Some("44:C0"), &[p4_bridge("/dev/ttyACM0")]);
        assert!(notice.contains("as 44:C0 after 5s"), "{notice}");
        assert!(notice.contains("none has answered with that address"), "{notice}");
        assert!(notice.contains("wartui reset --bridge 44:C0"), "{notice}");
        assert!(!notice.contains("node firmware"), "{notice}");
    }

    #[test]
    fn cli_diagnostics_says_nothing_opened_when_address_is_named_and_no_ch343_is_attached() {
        let notice = silence_notice(Some("A1:B0"), &[]);
        assert!(notice.contains("no CH343 is attached, so nothing was opened"), "{notice}");
        assert!(!notice.contains("wartui reset"), "{notice}");
    }

    #[test]
    fn last_reset_line_says_what_ran_last_when_phase_is_only_a_hint() {
        let line = |last_phase| {
            last_reset_line(&BridgeInfo {
                mac: [0; 6],
                fw_version: String::new(),
                reset_cause: ResetCause::PowerOn,
                last_phase,
                uptime_ms: 0,
                node_board: wartui_proto::bus::NodeBoard::NoCarrier,
                radio: None,
                image: None,
            })
        };
        for (phase, said) in [
            (LoopPhase::Command, "it last ran a host command"),
            (LoopPhase::DrainLink, "it last read host commands"),
            (LoopPhase::Pump, "it last wrote to the host"),
            (LoopPhase::RadioLink, "it last ran its radio link"),
            (LoopPhase::Boot, "it had not finished starting"),
        ] {
            assert_eq!(line(phase), format!("last reset  powered on; {said}"));
        }
        assert_eq!(line(LoopPhase::Unknown), "last reset  powered on");
    }

    #[test]
    fn cli_diagnostics_preserves_clean_formatting_when_generating_bridge_notice() {
        // A wrapped literal keeps the source's leading whitespace, which reads as
        // ragged gaps mid-sentence and is invisible here.
        let notice = silence_notice(Some("/dev/x"), &[]);
        for line in notice.lines() {
            // The bullets' own two-space indent is deliberate; a gap after the
            // text has started is the symptom being guarded against.
            let body = line.trim_start();
            assert!(!body.contains("  "), "gap inside {line:?}");
            assert_eq!(line, line.trim_end(), "trailing space in {line:?}");
        }
    }

    #[test]
    fn cli_formatter_formats_bullet_points_when_rendering_help_or_diagnostics() {
        let notice = silence_notice(Some("/dev/x"), &[]);
        for line in notice.lines().filter(|l| l.starts_with(' ')) {
            assert!(line.starts_with("  - "), "unexpected indent: {line:?}");
        }
    }

    #[test]
    fn cli_diagnostics_lists_node_firmware_when_bridge_is_named_by_path() {
        // A path is opened as given, so it can reach a node.
        let notice = silence_notice(Some("/dev/x"), &[]);
        for cause in ["node firmware", "wedged", "another program"] {
            assert!(notice.contains(cause), "missing {cause}: {notice}");
        }
    }
}
