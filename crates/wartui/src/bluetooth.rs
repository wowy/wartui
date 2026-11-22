//! `wartui bluetooth` — pair this PC with the bridge over Bluetooth, forget it, or let phones
//! link to it.
//!
//! **`pair` opens the window first, when it can.** The bridge lets a new host pair only inside its
//! pairing window: 120 s after it powers up, while it holds no bond, and 120 s after an
//! `OpenPairing`. A bridge that answers on USB is sent one, found as other commands find it, by
//! asking each CH343. One that does not is paired inside whatever window it has.
//!
//! **Then the OS pairs, because BlueZ never would.** On Linux `pair` scans for the id the bridge
//! advertises and pairs through BlueZ's D-Bus API (`wartui_bridge::bluez` has why). Other
//! OSes pair on their own on the first `--link bluetooth` connection inside the window, so `pair`
//! says so there and stops after the window. macOS raises a dialog for it, and that connection's
//! subscription waits on the click, mostly past the transport's 10 s; the pairing completes
//! anyway, and the next connection is served (`docs/ble-host-link-findings.md` § "macOS").
//!
//! **A pairing is proved, never assumed.** Outside the window the bridge accepts the pairing and
//! then drops the host as unbonded, so a pairing BlueZ reports done can be one the bridge did not
//! keep. `pair` connects once over the transport and asks the bridge whether this host is
//! encrypted and bonded. A PC BlueZ lists as paired is checked the same way first: a bond the
//! bridge refuses is stale, and is removed and paired afresh, since BlueZ never pairs again on
//! its own. A pairing the bridge did not keep is removed too, so BlueZ holds no one-sided bond.
//!
//! **`--fresh` is the escape for a bond only this PC kept.** A bridge whose bonds were cleared
//! drops a PC that still holds one, and that drop is weak evidence, so `pair` never removes the
//! pairing on it. `--fresh` removes this PC's pairing first, on the operator's word, then pairs as
//! usual.
//!
//! **Only the bridge's own word removes a pairing** ([`DisconnectCause::NotBonded`], or a status
//! saying this host is not bonded). A link that only dropped before serving this PC, at the time
//! the bridge drops an unbonded one, is no proof: a bonded PC waiting behind a USB host and then
//! cut off by a P4 reset or range drops the same way, and removing its bond would strand it. Then
//! `pair` removes nothing, says what it saw, and asks for another try.
//!
//! **`forget` clears every bond the bridge keeps**, over the chosen link, and removes the bridge
//! from BlueZ on Linux. The bridge disconnects a host connected on a bond it cleared, this PC
//! too over Bluetooth. Every other host that paired keeps a bond the bridge dropped, and BlueZ
//! cannot reconnect on one (`docs/c6-ble-pairing-findings.md`, "Clearing the bridge's bonds
//! strands every host"), so `forget` says each must forget the bridge too.
//!
//! **`wardrive-esp` reads or sets whether phones may link** as to a WardriveESP board, with no
//! pairing. The bridge keeps the setting on its own flash, as it keeps the pool, and is reached
//! as `wartui pool` reaches it. Turning it on says what it costs: any phone in range can then hold
//! the bridge's one Bluetooth connection and start or stop the scan.
//!
//! **None saves `wartui.toml`.** Only `run` and `flash-bridge` save the bridge
//! ([`crate::memory`]). The next `run --link bluetooth` finds a paired bridge and remembers it.
//! A remembered Bluetooth address BlueZ does not list, or one that does not answer, is followed by
//! a scan (`wartui_bridge::ble`).

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::{Args as ClapArgs, Subcommand, ValueEnum};
#[cfg(any(target_os = "linux", test))]
use wartui_bridge::DisconnectCause;
#[cfg(any(target_os = "linux", test))]
use wartui_bridge::ble::Advertiser;
use wartui_bridge::serial::{self, BridgeSpec, Probed};
use wartui_bridge::{LinkEvent, LinkHandle};
#[cfg(any(target_os = "linux", test))]
use wartui_proto::link::HOST_SILENCE_SECS;
use wartui_proto::link::{
    BluetoothStatus, BridgeToHost, HostToBridge, PAIR_WINDOW_SECS, PairWindow,
};
use wartui_proto::mac::{self, Mac};

use crate::config::Link;
#[cfg(any(target_os = "linux", test))]
use crate::status::bonds_text;
use crate::status::{pairing_text, wait_for_ready, wait_for_status, wardrive_esp_text};

/// How long the bridge has to answer a status request.
const REPLY_TIMEOUT: Duration = crate::CONNECT_NOTICE_AFTER;

/// How long `forget` listens for a fault once the bonds are cleared.
const SETTLE: Duration = Duration::from_secs(1);

/// How long `wardrive-esp` listens for a fault once the setting is sent, as `wartui pool` does.
const SETTING_SETTLE: Duration = Duration::from_millis(500);

/// What turning `wardrive-esp` on lets a stranger do.
const PHONES_NOTICE: &str = "Any phone in range can now link to the bridge with no pairing, hold \
                             its one Bluetooth connection, and start or stop the scan.";

#[derive(ClapArgs)]
pub struct Args {
    #[command(subcommand)]
    action: Action,
}

#[derive(Subcommand)]
enum Action {
    /// Pair this PC with the bridge, once. Opens the bridge's pairing window over USB when it is
    /// plugged in; otherwise pair within 120 s of the bridge powering up.
    Pair(PairArgs),
    /// Clear every Bluetooth bond the bridge keeps, and forget it on this PC. Every other host
    /// that paired with it must forget it too.
    Forget(ForgetArgs),
    /// Read or set whether phones may link to the bridge as a WardriveESP board, with no pairing.
    /// The bridge keeps it across its reboots.
    WardriveEsp(WardriveEspArgs),
}

#[derive(ClapArgs)]
struct PairArgs {
    /// The bridge, by its address or the end of it (`44:C0`). The remembered bridge, else the
    /// only one heard, when omitted.
    #[arg(long, value_name = "MAC")]
    bridge: Option<String>,

    /// Remove this PC's pairing with the bridge first, then pair again: for a bridge that has
    /// forgotten this PC.
    #[arg(long)]
    fresh: bool,

    /// Read the remembered bridge from this file instead of the default `wartui.toml`. A file
    /// that does not load is no bridge remembered. Never written; see
    /// `crates/wartui/README.md` § "Config file".
    #[arg(long, value_name = "PATH")]
    config: Option<PathBuf>,
}

#[derive(ClapArgs)]
struct ForgetArgs {
    /// The bridge, as a device path, the board's address, or the end of it (`00:08`). Detected
    /// if omitted.
    #[arg(long, value_name = "PATH|MAC")]
    bridge: Option<String>,

    #[command(flatten)]
    link_flag: crate::config::LinkFlag,

    /// Read the remembered bridge from this file instead of the default `wartui.toml`. A file
    /// that does not load is no bridge remembered. Never written; see
    /// `crates/wartui/README.md` § "Config file".
    #[arg(long, value_name = "PATH")]
    config: Option<PathBuf>,
}

#[derive(ClapArgs)]
struct WardriveEspArgs {
    /// `on` lets phones link, `off` does not. Without it, prints the setting.
    #[arg(value_name = "on|off")]
    state: Option<OnOff>,

    /// The bridge, as a device path, the board's address, or the end of it (`00:08`). Detected
    /// if omitted.
    #[arg(long, value_name = "PATH|MAC")]
    bridge: Option<String>,

    #[command(flatten)]
    link_flag: crate::config::LinkFlag,

    /// Read the remembered bridge from this file instead of the default `wartui.toml`. A file
    /// that does not load is no bridge remembered. Never written; see
    /// `crates/wartui/README.md` § "Config file".
    #[arg(long, value_name = "PATH")]
    config: Option<PathBuf>,
}

/// A setting, as the command line spells it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum OnOff {
    On,
    Off,
}

pub async fn run(args: Args) -> Result<()> {
    match args.action {
        Action::Pair(args) => pair(args).await,
        Action::Forget(args) => forget(args).await,
        Action::WardriveEsp(args) => wardrive_esp(args).await,
    }
}

/// Where the bridge's pairing window stands before pairing.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Window {
    /// Opened over USB, on the bridge that answered as `mac`.
    Opened { mac: Mac, status: BluetoothStatus },
    /// No bridge answered on USB.
    NotOnUsb,
    /// Several bridges answered on USB and none was named, as these labels.
    Several(String),
    /// A bridge answered on USB and its window did not open, for this reason.
    Failed(String),
}

impl Window {
    /// The bridge the window was opened on, which names the one to pair with.
    fn mac(&self) -> Option<Mac> {
        match self {
            Self::Opened { mac, .. } => Some(*mac),
            Self::NotOnUsb | Self::Several(_) | Self::Failed(_) => None,
        }
    }

    /// What `pair` says about the window.
    fn line(&self) -> String {
        match self {
            Self::Opened { mac, status } => format!(
                "opened the pairing window of bridge {} over USB: {}",
                mac::full(mac),
                pairing_text(status)
            ),
            Self::NotOnUsb => format!(
                "the bridge is not on USB, so its pairing window is open only for \
                 {PAIR_WINDOW_SECS} s after it powers up, and while it holds no bond"
            ),
            Self::Several(labels) => format!(
                "several bridges answer on USB ({labels}); name one with --bridge to open its \
                 pairing window over USB"
            ),
            Self::Failed(why) => format!("could not open the pairing window over USB: {why}"),
        }
    }
}

/// The bridge on USB that `pair` opens the window of.
#[derive(Debug, PartialEq, Eq)]
enum OnUsb<'a> {
    One(&'a Probed),
    None,
    Several(String),
}

/// The one bridge among the CH343s `probed` to open the window of: the one `named`, else the one
/// `remembered`, else the only one that answered.
fn usb_bridge<'a>(
    probed: &'a [Probed],
    named: Option<&BridgeSpec>,
    remembered: Option<Mac>,
) -> OnUsb<'a> {
    let answered = probed.iter().filter(|board| board.mac.is_some());
    let named: Vec<&Probed> = match named {
        Some(spec) => {
            answered.filter(|board| board.mac.is_some_and(|m| spec.matches_address(&m))).collect()
        }
        None => {
            let answered: Vec<&Probed> = answered.collect();
            match answered.iter().find(|board| board.mac.is_some() && board.mac == remembered) {
                Some(board) => vec![*board],
                None => answered,
            }
        }
    };
    match named.as_slice() {
        [] => OnUsb::None,
        [only] => OnUsb::One(only),
        several => {
            OnUsb::Several(several.iter().map(|board| board.label()).collect::<Vec<_>>().join(", "))
        }
    }
}

async fn pair(args: PairArgs) -> Result<()> {
    let remembered = crate::config::remembered_bridge(args.config.as_deref());
    let named = args.bridge.as_deref().map(crate::spec);
    if let Some(BridgeSpec::Path(path)) = &named {
        bail!("--bridge {path} is a USB port; name the bridge by its address: --bridge XX:YY");
    }
    let window = open_window(named.as_ref(), remembered.mac).await;
    println!("{}", window.line());
    let spec = window.mac().map(BridgeSpec::Mac).or(named);
    let preferred = spec.clone().or(remembered.mac.map(BridgeSpec::Mac));
    pair_with(spec, preferred, &window, args.fresh).await
}

/// What `pair` does first with a bridge this PC is `paired` with or not, given `--fresh`.
#[cfg(any(target_os = "linux", test))]
#[derive(Debug, PartialEq, Eq)]
enum First {
    /// Check that the bridge keeps the pairing, and pair again only if it refuses it.
    Check,
    /// Remove the pairing, on `--fresh`, and pair again.
    Remove,
    /// Pair: there is no pairing to check or remove.
    Pair,
}

#[cfg(any(target_os = "linux", test))]
fn first_step(paired: bool, fresh: bool) -> First {
    match (paired, fresh) {
        (true, false) => First::Check,
        (true, true) => First::Remove,
        (false, _) => First::Pair,
    }
}

/// The line that ends a failure to prove a pairing BlueZ held: the escape when the bridge has
/// forgotten this PC, naming the bridge by the `octets` it advertises.
#[cfg(any(target_os = "linux", test))]
fn fresh_hint(octets: [u8; 2]) -> String {
    format!(
        "If the bridge has forgotten this PC, run `wartui bluetooth pair --fresh --bridge {}`",
        mac::octets(&octets)
    )
}

/// Open the pairing window of the bridge on USB, if one answers there.
async fn open_window(named: Option<&BridgeSpec>, remembered: Option<Mac>) -> Window {
    let attached = serial::discover_bridges().unwrap_or_default();
    if attached.is_empty() {
        return Window::NotOnUsb;
    }
    let probed = serial::probe_all(&attached).await;
    let board = match usb_bridge(&probed, named, remembered) {
        OnUsb::One(board) => board,
        OnUsb::None => return Window::NotOnUsb,
        OnUsb::Several(labels) => return Window::Several(labels),
    };
    match open_over_usb(&board.candidate.path).await {
        Ok((mac, status)) if status.window != PairWindow::Closed => Window::Opened { mac, status },
        Ok(_) => Window::Failed("the bridge reported its window still closed".to_owned()),
        Err(why) => Window::Failed(format!("{why:#}")),
    }
}

/// Send `OpenPairing` to the bridge on `path`, and read back its Bluetooth host link.
async fn open_over_usb(path: &str) -> Result<(Mac, BluetoothStatus)> {
    let memory = wartui_bridge::remember::BridgeMemory::none();
    let mut link = crate::open(Some(path), None, Link::Usb, memory)?;
    let opened = async {
        let info = wait_for_ready(&mut link, Link::Usb, crate::CONNECT_NOTICE_AFTER)
            .await?
            .context("the bridge did not announce itself")?;
        link.send_bulk(HostToBridge::OpenPairing).context("queueing the request")?;
        Ok((info.mac, bluetooth_status(&mut link).await?))
    }
    .await;
    link.close().await;
    opened
}

/// Ask the bridge for its status, and return its Bluetooth host link.
async fn bluetooth_status(link: &mut LinkHandle) -> Result<BluetoothStatus> {
    link.send_bulk(HostToBridge::GetStatus).context("queueing the status request")?;
    let status = tokio::time::timeout(REPLY_TIMEOUT, wait_for_status(link))
        .await
        .context("the bridge did not answer a status request")??;
    let BridgeToHost::Status { bluetooth, .. } = status else {
        unreachable!("wait_for_status only returns Status")
    };
    Ok(bluetooth)
}

/// What `pair` says once the bridge `found` keeps the bond.
#[cfg(any(target_os = "linux", test))]
fn paired_line(found: &Advertiser, status: &BluetoothStatus) -> String {
    format!("{} paired and bonded, {}", found.label(), bonds_text(status.bonds))
}

/// Why a pairing BlueZ could not finish failed, and what to do: the bridge did not refuse it, so
/// the next try may well succeed.
#[cfg(any(target_os = "linux", test))]
fn pair_failed(found: &Advertiser, why: &str) -> String {
    format!(
        "could not pair with {} (BlueZ: {why}). The bridge may be starting or out of range; \
         run `wartui bluetooth pair` again",
        found.label()
    )
}

/// Why the bridge did not keep a pairing, and what to do: with the window opened over USB, a
/// rejection; without, most likely a closed window.
#[cfg(any(target_os = "linux", test))]
fn not_kept(window: &Window, why: &str) -> String {
    match window {
        Window::Opened { .. } => format!(
            "the bridge did not keep the pairing although its window was open ({why}). Run \
             `wartui bluetooth pair` again; `wartui status` over USB shows its pairing counters"
        ),
        _ => format!(
            "the bridge did not keep the pairing ({why}), so its pairing window is most likely \
             closed. Plug in its USB cable and run `wartui bluetooth pair` again, or power-cycle \
             the bridge and pair within {PAIR_WINDOW_SECS} s of it powering up"
        ),
    }
}

/// What one link event says while `pair` waits for the bridge to serve this PC.
#[cfg(any(target_os = "linux", test))]
#[derive(Debug, PartialEq, Eq)]
enum Served<'a> {
    /// The bridge announced itself.
    Yes,
    /// The bridge refused this PC for holding no bond: proof enough to remove the pairing.
    Refused,
    /// The link dropped as the bridge drops an unbonded PC, which proves nothing.
    Dropped,
    /// The link failed some other way, for this reason.
    Failed(&'a str),
    /// Nothing yet.
    Waiting,
}

/// What `event` says of the proof.
#[cfg(any(target_os = "linux", test))]
fn served_by(event: &LinkEvent) -> Served<'_> {
    match event {
        LinkEvent::Connected(_) => Served::Yes,
        LinkEvent::Disconnected { cause: DisconnectCause::NotBonded, .. } => Served::Refused,
        LinkEvent::Disconnected { cause: DisconnectCause::MaybeNotBonded, .. } => Served::Dropped,
        // Still connected, and waiting on the OS or, past it, on the bridge.
        LinkEvent::Disconnected {
            cause: DisconnectCause::AwaitingPairing | DisconnectCause::AwaitingReady,
            ..
        } => Served::Waiting,
        LinkEvent::Disconnected { reason, .. } => Served::Failed(reason),
        LinkEvent::Message(_) | LinkEvent::Garbled(_) => Served::Waiting,
    }
}

/// Why `pair` stops without removing anything, when the bridge `found` did not serve this PC: a
/// USB host may hold it, and a link that `dropped` as the bridge drops an unbonded PC proves
/// nothing either way.
#[cfg(any(target_os = "linux", test))]
fn unserved(found: &Advertiser, dropped: bool) -> String {
    let dropped = if dropped {
        ", and dropped it before serving it as it drops a PC it holds no bond for, which a P4 \
         reset or a weak signal does too"
    } else {
        ""
    };
    format!(
        "{} did not serve this PC over Bluetooth{dropped}. A USB host may hold the bridge: \
         it serves this PC {HOST_SILENCE_SECS} s after its USB link goes quiet. Nothing was \
         removed; run `wartui bluetooth pair` again.\n{}",
        found.label(),
        fresh_hint(found.octets)
    )
}

#[cfg(target_os = "linux")]
use linux::pair_with;

#[cfg(target_os = "linux")]
mod linux {
    use std::time::Duration;

    use anyhow::{Context, Result, anyhow, bail};
    use wartui_bridge::LinkHandle;
    use wartui_bridge::ble::{Advertiser, BleTransport, find_advertiser};
    use wartui_bridge::bluez::{self, BluezError, Device};
    use wartui_bridge::serial::BridgeSpec;
    use wartui_proto::link::{BluetoothStatus, HOST_SILENCE_SECS};
    use wartui_proto::mac::{self, Mac};

    use super::{
        First, Served, Window, bluetooth_status, first_step, not_kept, pair_failed, paired_line,
        served_by, unserved,
    };

    /// How long the proof waits for the bridge to serve this PC: a connect, and the
    /// [`HOST_SILENCE_SECS`] a USB host that just opened the window holds the bridge for, with
    /// room.
    const PROVE_WITHIN: Duration = Duration::from_secs(40);

    /// How many times `pair` asks BlueZ to pair while BlueZ does not reach the bridge.
    const PAIR_TRIES: u32 = 2;

    /// How long `pair` waits before its second try. The bridge restarts its advertising when its
    /// bonds or its pairing window change, and a connect BlueZ sends then is lost.
    const SECOND_TRY_AFTER: Duration = Duration::from_secs(1);

    /// What the bridge said of this PC once connected.
    enum Proof {
        /// Encrypted and bonded, beside these bonds.
        Bonded(BluetoothStatus),
        /// Refused as holding no bond, in the bridge's own words.
        NotBonded,
        /// Not served within [`PROVE_WITHIN`], and whether a link dropped as the bridge drops an
        /// unbonded PC, which proves nothing.
        Unserved { dropped: bool },
    }

    /// Find the bridge, pair through BlueZ unless a pairing it keeps is already there, and prove
    /// the bond.
    pub(super) async fn pair_with(
        spec: Option<BridgeSpec>,
        preferred: Option<BridgeSpec>,
        window: &Window,
        fresh: bool,
    ) -> Result<()> {
        let (spec, preferred) = (spec.as_ref(), preferred.as_ref());
        let (mut found, mut device) = find_listed(spec, preferred).await?;
        let first = first_step(device.paired, fresh);
        match first {
            First::Remove => {
                println!("removing this PC's pairing with {}, as --fresh asks", found.label());
                (found, device) = remove_and_find(device, spec, preferred).await?;
            }
            First::Pair if fresh => println!("this PC holds no pairing with {}", found.label()),
            First::Pair | First::Check => {}
        }
        if first == First::Check {
            let label = found.label();
            println!("this PC is paired with {label} already; checking the bridge keeps it");
            let proof =
                prove(&found, window).await.with_context(|| super::fresh_hint(found.octets))?;
            match proof {
                Proof::Bonded(status) => {
                    println!("{}", paired_line(&found, &status));
                    return Ok(());
                }
                Proof::NotBonded => {
                    println!("the bridge holds no bond for this PC; removing it and pairing again");
                    (found, device) = remove_and_find(device, spec, preferred).await?;
                }
                Proof::Unserved { dropped } => bail!("{}", unserved(&found, dropped)),
            }
        }
        println!("pairing with {}", found.label());
        let pairing = {
            let (device, label) = (device.clone(), found.label());
            blocking(move || try_pairing(|| bluez::pair(&device), SECOND_TRY_AFTER, &label)).await?
        };
        match pairing {
            Ok(()) => {}
            Err(BluezError::Rejected(why)) => {
                remove_quietly(device).await;
                bail!("{}", not_kept(window, &why));
            }
            Err(BluezError::Failed(why) | BluezError::NotReached(why)) => {
                bail!("{}", pair_failed(&found, &why));
            }
            Err(error) => bail!("{error}"),
        }
        match prove(&found, window).await? {
            Proof::Bonded(status) => {
                println!("{}", paired_line(&found, &status));
                Ok(())
            }
            Proof::NotBonded => {
                remove_quietly(device).await;
                bail!("{}", not_kept(window, "it refused this PC as unbonded"))
            }
            Proof::Unserved { dropped } => bail!("{}", unserved(&found, dropped)),
        }
    }

    /// Pair through `pair`, and once more `pause` later when BlueZ does not reach the bridge
    /// `label` names. Any other outcome, and the second try's, is returned as it is.
    fn try_pairing(
        mut pair: impl FnMut() -> Result<(), BluezError>,
        pause: Duration,
        label: &str,
    ) -> Result<(), BluezError> {
        let mut tries = 1;
        loop {
            match pair() {
                Err(BluezError::NotReached(_)) if tries < PAIR_TRIES => {
                    println!("BlueZ did not reach {label}; trying once more");
                    std::thread::sleep(pause);
                    tries += 1;
                }
                outcome => return outcome,
            }
        }
    }

    /// Scan for the bridge `spec` names, preferring one `preferred` names, and the device BlueZ
    /// lists for what the scan heard.
    async fn find_listed(
        spec: Option<&BridgeSpec>,
        preferred: Option<&BridgeSpec>,
    ) -> Result<(Advertiser, Device)> {
        let found = find_advertiser(spec, preferred).await?;
        let address = found.address.clone();
        let device = blocking(move || bluez::device_at(&address)).await??.ok_or_else(|| {
            anyhow!(
                "BlueZ does not list {} at {}, which a scan just heard",
                found.label(),
                found.address
            )
        })?;
        Ok((found, device))
    }

    /// Remove `device`'s pairing from BlueZ, then find the bridge again ([`find_listed`]).
    async fn remove_and_find(
        device: Device,
        spec: Option<&BridgeSpec>,
        preferred: Option<&BridgeSpec>,
    ) -> Result<(Advertiser, Device)> {
        blocking(move || bluez::remove(&device)).await??;
        find_listed(spec, preferred).await
    }

    /// Remove a pairing the bridge did not keep, so BlueZ holds no one-sided bond.
    async fn remove_quietly(device: Device) {
        match blocking(move || bluez::remove(&device)).await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => tracing::warn!(%error, "could not remove the unkept pairing"),
            Err(error) => tracing::warn!(%error, "could not remove the unkept pairing"),
        }
    }

    /// Connect to the bridge `found` names, and ask whether this PC is encrypted and bonded.
    ///
    /// Named by its advertised octets, so a retry that scans reaches that bridge and no other.
    async fn prove(found: &Advertiser, window: &Window) -> Result<Proof> {
        let mut link = BleTransport::with_spec(BridgeSpec::Tail(found.octets.to_vec())).start()?;
        if matches!(window, Window::Opened { .. }) {
            println!(
                "waiting for the bridge to serve this PC, up to {HOST_SILENCE_SECS} s after its \
                 USB link went quiet"
            );
        }
        let mut dropped = false;
        let proof = match tokio::time::timeout(PROVE_WITHIN, served(&mut link, &mut dropped)).await
        {
            Err(_) => Ok(Proof::Unserved { dropped }),
            Ok(Err(error)) => Err(error),
            Ok(Ok(false)) => Ok(Proof::NotBonded),
            Ok(Ok(true)) => bonded(&mut link).await,
        };
        link.close().await;
        proof
    }

    /// Wait for the bridge's `Ready`: `true`, or `false` when it refuses this PC as unbonded.
    /// Other failures are retried by the transport, and a drop that only may mean no bond sets
    /// `dropped`.
    async fn served(link: &mut LinkHandle, dropped: &mut bool) -> Result<bool> {
        loop {
            let Some(event) = link.recv().await else { bail!("the link closed") };
            match served_by(&event) {
                Served::Yes => return Ok(true),
                Served::Refused => return Ok(false),
                Served::Dropped => {
                    *dropped = true;
                    tracing::info!("the bridge dropped the proof's connection; retrying");
                }
                Served::Failed(reason) => {
                    tracing::info!(%reason, "the proof's connection failed; retrying");
                }
                Served::Waiting => {}
            }
        }
    }

    /// What the bridge's `Status` says of this host.
    async fn bonded(link: &mut LinkHandle) -> Result<Proof> {
        let bluetooth = bluetooth_status(link).await?;
        Ok(match bluetooth.central {
            Some(central) if central.encrypted && central.bonded => Proof::Bonded(bluetooth),
            _ => Proof::NotBonded,
        })
    }

    /// Remove the bridge `mac` from BlueZ, by its advertised octets or the `address` remembered
    /// for it, and say what was done.
    pub(super) async fn forget_here(mac: Mac, address: Option<String>) {
        let name = format!("bridge {}", mac::short(&mac));
        let octets = [mac[4], mac[5]];
        let listed = blocking(move || -> Result<Vec<Device>, BluezError> {
            let devices = bluez::devices()?;
            Ok(devices
                .into_iter()
                .filter(|d| super::is_this_bridge(d.octets, &d.address, octets, address.as_deref()))
                .collect())
        })
        .await;
        let devices = match listed {
            Ok(Ok(devices)) => devices,
            Ok(Err(error)) => return println!("could not forget {name} on this PC: {error}"),
            Err(error) => return println!("could not forget {name} on this PC: {error:#}"),
        };
        if devices.is_empty() {
            return println!("this PC held no pairing with {name}");
        }
        for device in devices {
            match blocking(move || bluez::remove(&device)).await {
                Ok(Ok(())) => println!("forgot {name} on this PC"),
                Ok(Err(error)) => println!("could not forget {name} on this PC: {error}"),
                Err(error) => println!("could not forget {name} on this PC: {error:#}"),
            }
        }
    }

    /// Run a blocking BlueZ call off the async runtime.
    async fn blocking<T: Send + 'static>(call: impl FnOnce() -> T + Send + 'static) -> Result<T> {
        tokio::task::spawn_blocking(call).await.context("a BlueZ call panicked")
    }

    #[cfg(test)]
    mod tests {
        use std::time::Duration;

        use wartui_bridge::bluez::BluezError;

        use super::try_pairing;

        const PAGE_TIMEOUT: &str = "ConnectionAttemptFailed: Page Timeout";

        /// Pair through [`try_pairing`] with no pause, BlueZ answering `outcomes` in turn: what it
        /// returns, and how many times it asked.
        fn paired(outcomes: &[Result<(), BluezError>]) -> (Result<(), BluezError>, usize) {
            let mut calls = 0;
            let outcome = try_pairing(
                || {
                    calls += 1;
                    outcomes[calls - 1].clone()
                },
                Duration::ZERO,
                "bridge 44:C0",
            );
            (outcome, calls)
        }

        fn not_reached() -> Result<(), BluezError> {
            Err(BluezError::NotReached(PAGE_TIMEOUT.to_owned()))
        }

        #[test]
        fn bluetooth_pair_pairs_on_second_try_when_bluez_first_does_not_reach_the_bridge() {
            assert_eq!(paired(&[not_reached(), Ok(())]), (Ok(()), 2));
        }

        #[test]
        fn bluetooth_pair_returns_second_failure_when_bluez_never_reaches_the_bridge() {
            assert_eq!(paired(&[not_reached(), not_reached(), Ok(())]), (not_reached(), 2));
        }

        #[test]
        fn bluetooth_pair_tries_once_when_bridge_rejects_the_pairing() {
            let rejected = Err(BluezError::Rejected("AuthenticationCanceled".to_owned()));
            assert_eq!(paired(&[rejected.clone(), Ok(())]), (rejected, 1));
        }

        #[test]
        fn bluetooth_pair_tries_once_when_bluez_fails_otherwise() {
            let failed = Err(BluezError::Failed("InProgress: In Progress".to_owned()));
            assert_eq!(paired(&[failed.clone(), Ok(())]), (failed, 1));
        }

        #[test]
        fn bluetooth_pair_tries_once_when_pairing_succeeds() {
            assert_eq!(paired(&[Ok(()), not_reached()]), (Ok(()), 1));
        }
    }
}

/// What other OSes do instead of `pair`.
#[cfg(not(target_os = "linux"))]
const OS_PAIRS: &str = "On this OS, wartui does not pair itself: the OS pairs on the first \
     `--link bluetooth` connection made inside the bridge's pairing window, such as \
     `wartui status --link bluetooth`. On macOS, click Connect in the dialog it raises. The \
     command says when the dialog may be up, and waits up to 30 s for the click.";

#[cfg(not(target_os = "linux"))]
async fn pair_with(
    _spec: Option<BridgeSpec>,
    _preferred: Option<BridgeSpec>,
    _window: &Window,
    fresh: bool,
) -> Result<()> {
    if fresh {
        println!(
            "On this OS, remove the bridge's pairing in the system's Bluetooth settings first."
        );
    }
    println!("{OS_PAIRS}");
    Ok(())
}

/// Whether a device the OS lists, advertising `listed_octets` at `listed_address`, is the bridge
/// that advertises `octets`, or sits at the Bluetooth address remembered for it. Every device
/// with the octets is taken, so two paired bridges that share their last two octets are both
/// removed. BlueZ keeps no service data across its own restart, so a paired bridge it has not
/// heard since is matched by the remembered address alone.
#[cfg(any(target_os = "linux", test))]
fn is_this_bridge(
    listed_octets: Option<[u8; 2]>,
    listed_address: &str,
    octets: [u8; 2],
    remembered: Option<&str>,
) -> bool {
    listed_octets == Some(octets)
        || remembered.is_some_and(|r| r.eq_ignore_ascii_case(listed_address))
}

async fn forget(args: ForgetArgs) -> Result<()> {
    let remembered = crate::config::remembered_bridge(args.config.as_deref());
    let over = crate::link_for(args.link_flag.link, args.bridge.as_deref(), remembered.link)?;
    let (mut link, info) =
        crate::connect_one(args.bridge.as_deref(), args.link_flag.link, args.config.as_deref())
            .await?;
    let name = format!("bridge {}", mac::short(&info.mac));
    link.send_bulk(HostToBridge::ClearBonds).context("queueing the request")?;
    println!("clearing every Bluetooth bond on bridge {}", mac::full(&info.mac));
    let cleared = cleared(&mut link, over).await;
    link.close().await;
    cleared?;
    #[cfg(target_os = "linux")]
    {
        let address = remembered.bluetooth_address.filter(|_| remembered.mac == Some(info.mac));
        linux::forget_here(info.mac, address).await;
    }
    println!("{}", stranded_notice(&name));
    Ok(())
}

async fn wardrive_esp(args: WardriveEspArgs) -> Result<()> {
    let (mut link, info) =
        crate::connect_one(args.bridge.as_deref(), args.link_flag.link, args.config.as_deref())
            .await?;
    let wanted = args.state.map(|state| state == OnOff::On);
    if let Some(on) = wanted {
        link.send_bulk(HostToBridge::SetWardriveEsp { on }).context("queueing the setting")?;
        let state = if on { "on" } else { "off" };
        println!("turning wardrive esp {state} on bridge {}", mac::full(&info.mac));
        crate::settle(&mut link, SETTING_SETTLE).await?;
    }
    let bluetooth = bluetooth_status(&mut link).await?;
    println!("phones     {}", wardrive_esp_text(&bluetooth));
    if wanted.is_some_and(|on| on != bluetooth.wardrive_esp) {
        bail!("the bridge did not take the setting");
    }
    if wanted == Some(true) {
        println!("{PHONES_NOTICE}");
    }
    Ok(())
}

/// Wait for the bonds to clear. Over USB a status shows them gone. Over Bluetooth this PC's bond
/// went with them and the bridge disconnects it, so that disconnect is the expected end, and only
/// a fault is listened for until it, or for [`SETTLE`].
async fn cleared(link: &mut LinkHandle, over: Link) -> Result<()> {
    if over == Link::Bluetooth {
        let until = tokio::time::Instant::now() + SETTLE;
        loop {
            match tokio::time::timeout_at(until, link.recv()).await {
                Ok(Some(LinkEvent::Message(BridgeToHost::Error { message }))) => {
                    println!("fault      {message}");
                }
                Ok(Some(LinkEvent::Disconnected { .. }) | None) | Err(_) => return Ok(()),
                Ok(Some(_)) => {}
            }
        }
    }
    crate::settle(link, SETTLE).await?;
    let bluetooth = bluetooth_status(link).await?;
    println!("pairing    {}", pairing_text(&bluetooth));
    if bluetooth.bonds > 0 {
        bail!("the bridge did not clear its bonds");
    }
    Ok(())
}

/// What `forget` says every other host must do.
fn stranded_notice(name: &str) -> String {
    #[cfg(target_os = "linux")]
    let here = "This PC pairs again with `wartui bluetooth pair`.";
    #[cfg(not(target_os = "linux"))]
    let here = "Remove it in this PC's Bluetooth settings too; this PC pairs again on its next \
                `--link bluetooth` connection inside the bridge's pairing window.";
    format!(
        "Every other host that paired with {name} keeps a bond the bridge no longer holds, and \
         cannot connect until it forgets the bridge: remove it in that host's Bluetooth settings, \
         then pair again inside the bridge's pairing window. {here}"
    )
}

#[cfg(test)]
mod tests {
    use wartui_bridge::ble::Advertiser;
    use wartui_bridge::ports::{CH343_PID, CH343_VID, candidate};
    use wartui_bridge::serial::{BridgeSpec, Probed};
    use wartui_bridge::{DisconnectCause, LinkEvent};
    use wartui_proto::link::{BluetoothStatus, PairWindow};

    use super::{
        First, OnUsb, Served, Window, first_step, is_this_bridge, not_kept, pair_failed,
        paired_line, served_by, stranded_notice, unserved, usb_bridge,
    };

    const BRIDGE: [u8; 6] = [0x02, 0x00, 0x5E, 0x10, 0x44, 0xC0];
    const OTHER: [u8; 6] = [0x02, 0x00, 0x5E, 0x10, 0xA1, 0xB0];

    /// The bridge a scan heard, with the name the OS heard.
    fn found(name: Option<&str>) -> Advertiser {
        Advertiser {
            octets: [0x44, 0xC0],
            name: name.map(str::to_owned),
            address: "02:00:5E:10:44:C0".to_owned(),
        }
    }

    fn answered(device: &str, mac: [u8; 6]) -> Probed {
        let ch343 = candidate(device, Some(CH343_VID), Some(CH343_PID), Some(device));
        Probed { candidate: ch343, opened: true, mac: Some(mac) }
    }

    fn silent(device: &str) -> Probed {
        let ch343 = candidate(device, Some(CH343_VID), Some(CH343_PID), Some(device));
        Probed { candidate: ch343, opened: true, mac: None }
    }

    #[test]
    fn bluetooth_pair_opens_window_on_only_usb_bridge_when_none_is_named() {
        let probed = [silent("/dev/ttyACM0"), answered("/dev/ttyACM1", BRIDGE)];
        assert_eq!(usb_bridge(&probed, None, None), OnUsb::One(&probed[1]));
    }

    #[test]
    fn bluetooth_pair_opens_window_on_named_or_remembered_bridge_when_several_answer() {
        let probed = [answered("/dev/ttyACM0", OTHER), answered("/dev/ttyACM1", BRIDGE)];
        let named = "44:C0".parse::<BridgeSpec>().unwrap_or_else(|never| match never {});
        assert_eq!(usb_bridge(&probed, Some(&named), None), OnUsb::One(&probed[1]));
        assert_eq!(usb_bridge(&probed, None, Some(BRIDGE)), OnUsb::One(&probed[1]));
        assert!(matches!(usb_bridge(&probed, None, None), OnUsb::Several(_)));
    }

    #[test]
    fn bluetooth_pair_skips_usb_when_named_bridge_is_not_on_usb() {
        let probed = [answered("/dev/ttyACM0", OTHER)];
        let named = "44:C0".parse::<BridgeSpec>().unwrap_or_else(|never| match never {});
        assert_eq!(usb_bridge(&probed, Some(&named), Some(OTHER)), OnUsb::None);
        assert_eq!(usb_bridge(&[silent("/dev/ttyACM0")], None, None), OnUsb::None);
    }

    #[test]
    fn bluetooth_pair_says_window_and_bonds_when_opened_over_usb() {
        let status = BluetoothStatus {
            window: PairWindow::Open { secs: 120 },
            bonds: 1,
            ..BluetoothStatus::default()
        };
        let line = Window::Opened { mac: BRIDGE, status }.line();
        assert!(line.contains("over USB: open for 120 s more, 1 bond stored"), "{line}");
        assert!(Window::NotOnUsb.line().contains("120 s after it powers up"));
    }

    #[test]
    fn bluetooth_pair_names_closed_window_and_both_fixes_when_not_opened_over_usb() {
        let text = not_kept(&Window::NotOnUsb, "it refused this PC as unbonded");
        assert!(text.contains("pairing window is most likely closed"), "{text}");
        assert!(text.contains("Plug in its USB cable"), "{text}");
        assert!(text.contains("power-cycle the bridge and pair within 120 s"), "{text}");
    }

    #[test]
    fn bluetooth_pair_calls_failure_a_rejection_when_window_was_opened() {
        let window = Window::Opened { mac: BRIDGE, status: BluetoothStatus::default() };
        let text = not_kept(&window, "AuthenticationFailed");
        assert!(text.contains("although its window was open (AuthenticationFailed)"), "{text}");
        assert!(!text.contains("most likely closed"), "{text}");
    }

    #[test]
    fn bluetooth_pair_prints_name_and_bonds_when_bridge_keeps_the_bond() {
        let status = BluetoothStatus { bonds: 2, ..BluetoothStatus::default() };
        let line = paired_line(&found(Some("wartui 44:C0")), &status);
        assert_eq!(line, "wartui 44:C0 paired and bonded, 2 bonds stored");
        let line = paired_line(&found(None), &status);
        assert_eq!(line, "bridge 44:C0 paired and bonded, 2 bonds stored", "no name heard");
    }

    #[test]
    fn bluetooth_pair_asks_for_another_try_when_bluez_could_not_reach_the_bridge() {
        let text = pair_failed(&found(None), "ConnectionAttemptFailed: Page Timeout");
        assert!(text.contains("(BlueZ: ConnectionAttemptFailed: Page Timeout)"), "{text}");
        assert!(text.ends_with("run `wartui bluetooth pair` again"), "{text}");
        assert!(!text.contains("power-cycle"), "{text}");
    }

    #[test]
    fn bluetooth_forget_matches_device_when_octets_or_remembered_address_agree() {
        let octets = [0x44, 0xC0];
        assert!(is_this_bridge(Some(octets), "02:00:5E:10:44:C2", octets, None));
        // BlueZ holds no service data for a bridge it has not heard since it restarted.
        assert!(is_this_bridge(None, "02:00:5E:10:44:C2", octets, Some("02:00:5e:10:44:c2")));
        assert!(!is_this_bridge(None, "02:00:5E:10:44:C2", octets, None));
        assert!(!is_this_bridge(Some([0xA1, 0xB0]), "02:00:5E:10:A1:B2", octets, None));
    }

    fn down(cause: DisconnectCause) -> LinkEvent {
        LinkEvent::Disconnected { reason: "gone".to_owned(), cause }
    }

    #[test]
    fn bluetooth_pair_reads_refusal_only_when_bridge_refused_this_pc_itself() {
        assert_eq!(served_by(&down(DisconnectCause::NotBonded)), Served::Refused);
        // A drop at the time the bridge drops an unbonded PC proves nothing.
        assert_eq!(served_by(&down(DisconnectCause::MaybeNotBonded)), Served::Dropped);
        assert_eq!(served_by(&down(DisconnectCause::Other)), Served::Failed("gone"));
        assert_eq!(served_by(&down(DisconnectCause::UsbTookBridge)), Served::Failed("gone"));
        assert_eq!(served_by(&down(DisconnectCause::AwaitingPairing)), Served::Waiting);
        assert_eq!(served_by(&down(DisconnectCause::AwaitingReady)), Served::Waiting);
    }

    #[test]
    fn bluetooth_pair_removes_pairing_first_only_when_fresh_and_paired() {
        assert_eq!(first_step(true, false), First::Check);
        assert_eq!(first_step(true, true), First::Remove);
        assert_eq!(first_step(false, true), First::Pair);
        assert_eq!(first_step(false, false), First::Pair);
    }

    #[test]
    fn bluetooth_pair_names_fresh_escape_when_bridge_did_not_serve_this_pc() {
        let text = unserved(&found(Some("wartui 44:C0")), false);
        let last = text.lines().last().unwrap_or_default();
        assert_eq!(
            last,
            "If the bridge has forgotten this PC, run `wartui bluetooth pair --fresh --bridge 44:C0`"
        );
    }

    #[test]
    fn bluetooth_pair_removes_nothing_and_asks_again_when_bridge_did_not_serve_this_pc() {
        let text = unserved(&found(Some("wartui 44:C0")), true);
        assert!(text.contains("wartui 44:C0 did not serve this PC over Bluetooth"), "{text}");
        assert!(text.contains("as it drops a PC it holds no bond for"), "{text}");
        assert!(text.contains("A USB host may hold the bridge"), "{text}");
        assert!(text.contains("Nothing was removed"), "{text}");
        assert!(!unserved(&found(None), false).contains("dropped"));
    }

    #[test]
    fn bluetooth_forget_tells_every_other_host_to_forget_when_bonds_are_cleared() {
        let notice = stranded_notice("bridge 44:C0");
        assert!(notice.contains("Every other host that paired with bridge 44:C0"), "{notice}");
        assert!(notice.contains("forgets the bridge"), "{notice}");
    }
}
