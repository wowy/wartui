//! `wartui bluetooth` — pair this PC with the bridge over Bluetooth, or forget it.
//!
//! **`pair` opens the window first, when it can.** The bridge lets a new host pair only inside its
//! pairing window: 120 s after it powers up, while it holds no bond, and 120 s after an
//! `OpenPairing`. A bridge that answers on USB is sent one, found as other commands find it, by
//! asking each CH343. One that does not is paired inside whatever window it has.
//!
//! **Then the OS pairs, because BlueZ never would.** On Linux `pair` scans for the bridge's
//! advertised name and pairs through BlueZ's D-Bus API (`wartui_bridge::bluez` has why). Other
//! OSes pair on their own on the first `--link bluetooth` connection inside the window, which is
//! untested, so `pair` says so there and stops after the window.
//!
//! **A pairing is proved, never assumed.** Outside the window the bridge accepts the pairing and
//! then drops the host as unbonded, so a pairing BlueZ reports done can be one the bridge did not
//! keep. `pair` connects once over the transport and asks the bridge whether this host is
//! encrypted and bonded. A PC BlueZ lists as paired is checked the same way first: a bond the
//! bridge dropped is stale, and is removed and paired afresh, since BlueZ never pairs again on
//! its own. A pairing the bridge did not keep is removed too, so BlueZ holds no one-sided bond.
//!
//! **`forget` clears every bond the bridge keeps**, over the chosen link, and removes the bridge
//! from BlueZ on Linux. Every other host that paired keeps a bond the bridge dropped, and BlueZ
//! cannot reconnect on one (`docs/c6-ble-pairing-findings.md`, "Clearing the bridge's bonds
//! strands every host"), so `forget` says each must forget the bridge too.
//!
//! **Neither saves `wartui.toml`.** Only `run` and `flash-bridge` save the bridge
//! ([`crate::memory`]). The next `run --link bluetooth` finds a paired bridge and remembers it, and
//! a remembered Bluetooth address that BlueZ no longer lists falls back to a scan.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::{Args as ClapArgs, Subcommand};
use wartui_bridge::serial::{self, BridgeSpec, Probed};
use wartui_bridge::{LinkEvent, LinkHandle};
use wartui_proto::link::{
    BluetoothStatus, BridgeToHost, HostToBridge, PAIR_WINDOW_SECS, PairWindow,
};
use wartui_proto::mac::{self, Mac};

use crate::config::Link;
use crate::status::{pairing_text, wait_for_ready, wait_for_status};

/// How long the bridge has to answer a status request.
const REPLY_TIMEOUT: Duration = crate::CONNECT_NOTICE_AFTER;

/// How long `forget` listens for a fault once the bonds are cleared.
const SETTLE: Duration = Duration::from_secs(1);

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
}

#[derive(ClapArgs)]
struct PairArgs {
    /// The bridge, by its address or the end of it (`44:C0`). The remembered bridge, else the
    /// only one heard, when omitted.
    #[arg(long, value_name = "MAC")]
    bridge: Option<String>,

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

    /// Which link reaches the bridge. `[bridge] link` in `wartui.toml` when omitted, else
    /// `usb`. A `--bridge` path is always USB.
    #[arg(long, value_name = "LINK")]
    link: Option<Link>,

    /// Read the remembered bridge from this file instead of the default `wartui.toml`. A file
    /// that does not load is no bridge remembered. Never written; see
    /// `crates/wartui/README.md` § "Config file".
    #[arg(long, value_name = "PATH")]
    config: Option<PathBuf>,
}

pub async fn run(args: Args) -> Result<()> {
    match args.action {
        Action::Pair(args) => pair(args).await,
        Action::Forget(args) => forget(args).await,
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
    pair_with(spec, preferred, &window).await
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
        let info = tokio::time::timeout(crate::CONNECT_NOTICE_AFTER, wait_for_ready(&mut link))
            .await
            .context("the bridge did not announce itself")??;
        link.send_bulk(HostToBridge::OpenPairing).context("queueing the request")?;
        link.send_bulk(HostToBridge::GetStatus).context("queueing the status request")?;
        let status = tokio::time::timeout(REPLY_TIMEOUT, wait_for_status(&mut link))
            .await
            .context("the bridge did not answer a status request")??;
        let BridgeToHost::Status { bluetooth, .. } = status else {
            unreachable!("wait_for_status only returns Status")
        };
        Ok((info.mac, bluetooth))
    }
    .await;
    link.close().await;
    opened
}

/// What `pair` says once the bridge keeps the bond.
fn paired_line(name: &str, status: &BluetoothStatus) -> String {
    let bonds = match status.bonds {
        1 => "1 bond stored".to_owned(),
        n => format!("{n} bonds stored"),
    };
    format!("{name} paired and bonded, {bonds}")
}

/// Why the bridge did not keep a pairing, and what to do: with the window opened over USB, a
/// rejection; without, most likely a closed window.
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

#[cfg(target_os = "linux")]
use linux::pair_with;

#[cfg(target_os = "linux")]
mod linux {
    use std::time::Duration;

    use anyhow::{Context, Result, anyhow, bail};
    use wartui_bridge::ble::{Advertiser, BleTransport, find_advertiser, is_not_bonded};
    use wartui_bridge::bluez::{self, BluezError, Device};
    use wartui_bridge::remember::BridgeMemory;
    use wartui_bridge::serial::BridgeSpec;
    use wartui_bridge::{LinkEvent, LinkHandle};
    use wartui_proto::link::{BluetoothStatus, BridgeToHost, HostToBridge};
    use wartui_proto::mac::Mac;

    use super::{REPLY_TIMEOUT, Window, not_kept, paired_line};
    use crate::status::wait_for_status;

    /// How long the proof waits for the bridge to serve this PC: a connect, and the 15 s a USB
    /// host that just opened the window holds the bridge for, with room.
    const PROVE_WITHIN: Duration = Duration::from_secs(40);

    /// What the bridge said of this PC once connected.
    enum Proof {
        /// Encrypted and bonded, beside these bonds.
        Bonded(BluetoothStatus),
        /// Refused or reported as holding no bond.
        NotBonded,
    }

    /// Find the bridge, pair through BlueZ unless a pairing it keeps is already there, and prove
    /// the bond.
    pub(super) async fn pair_with(
        spec: Option<BridgeSpec>,
        preferred: Option<BridgeSpec>,
        window: &Window,
    ) -> Result<()> {
        let mut found = find_advertiser(spec.as_ref(), preferred.as_ref()).await?;
        let mut device = listed(&found).await?;
        if device.paired {
            println!("this PC is paired with {} already; checking the bridge keeps it", found.name);
            match prove(&found, window).await? {
                Proof::Bonded(status) => {
                    println!("{}", paired_line(&found.name, &status));
                    return Ok(());
                }
                Proof::NotBonded => {
                    println!("the bridge holds no bond for this PC; removing it and pairing again");
                    blocking(move || bluez::remove(&device)).await??;
                    found = find_advertiser(spec.as_ref(), preferred.as_ref()).await?;
                    device = listed(&found).await?;
                }
            }
        }
        println!("pairing with {}", found.name);
        let pairing = {
            let device = device.clone();
            blocking(move || bluez::pair(&device)).await?
        };
        match pairing {
            Ok(()) => {}
            Err(BluezError::Rejected(why)) => {
                remove_quietly(device).await;
                bail!("{}", not_kept(window, &why));
            }
            Err(error) => bail!("{error}"),
        }
        match prove(&found, window).await? {
            Proof::Bonded(status) => {
                println!("{}", paired_line(&found.name, &status));
                Ok(())
            }
            Proof::NotBonded => {
                remove_quietly(device).await;
                bail!("{}", not_kept(window, "it dropped this PC as unbonded"))
            }
        }
    }

    /// The device BlueZ lists for what the scan heard.
    async fn listed(found: &Advertiser) -> Result<Device> {
        let address = found.address.clone();
        blocking(move || bluez::device_at(&address)).await??.ok_or_else(|| {
            anyhow!(
                "BlueZ does not list {} at {}, which a scan just heard",
                found.name,
                found.address
            )
        })
    }

    /// Remove a pairing the bridge did not keep, so BlueZ holds no one-sided bond.
    async fn remove_quietly(device: Device) {
        match blocking(move || bluez::remove(&device)).await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => tracing::warn!(%error, "could not remove the unkept pairing"),
            Err(error) => tracing::warn!(%error, "could not remove the unkept pairing"),
        }
    }

    /// Connect once to the bridge `found` names, and ask whether this PC is encrypted and bonded.
    async fn prove(found: &Advertiser, window: &Window) -> Result<Proof> {
        let memory = BridgeMemory::none().on_bluetooth(Some(found.address.clone()));
        let mut link = BleTransport::new().remember_in(memory).start()?;
        if matches!(window, Window::Opened { .. }) {
            println!(
                "waiting for the bridge to serve this PC, up to 15 s after its USB link went quiet"
            );
        }
        let proof = match tokio::time::timeout(PROVE_WITHIN, served(&mut link)).await {
            Err(_) => Err(anyhow!(
                "{} did not serve this PC over Bluetooth within {} s",
                found.name,
                PROVE_WITHIN.as_secs()
            )),
            Ok(Err(error)) => Err(error),
            Ok(Ok(false)) => Ok(Proof::NotBonded),
            Ok(Ok(true)) => bonded(&mut link).await,
        };
        link.close().await;
        proof
    }

    /// Wait for the bridge's `Ready`: `true`, or `false` when it refuses this PC as unbonded.
    /// Other failures are retried by the transport.
    async fn served(link: &mut LinkHandle) -> Result<bool> {
        loop {
            match link.recv().await {
                Some(LinkEvent::Connected(_)) => return Ok(true),
                Some(LinkEvent::Disconnected { reason }) if is_not_bonded(&reason) => {
                    return Ok(false);
                }
                Some(LinkEvent::Disconnected { reason }) => {
                    tracing::info!(%reason, "the proof's connection failed; retrying");
                }
                Some(_) => {}
                None => bail!("the link closed"),
            }
        }
    }

    /// What the bridge's `Status` says of this host.
    async fn bonded(link: &mut LinkHandle) -> Result<Proof> {
        link.send_bulk(HostToBridge::GetStatus).context("queueing the status request")?;
        let status = tokio::time::timeout(REPLY_TIMEOUT, wait_for_status(link))
            .await
            .context("the bridge did not answer a status request")??;
        let BridgeToHost::Status { bluetooth, .. } = status else {
            unreachable!("wait_for_status only returns Status")
        };
        Ok(match bluetooth.central {
            Some(central) if central.encrypted && central.bonded => Proof::Bonded(bluetooth),
            _ => Proof::NotBonded,
        })
    }

    /// Remove the bridge `mac` from BlueZ, by its advertised name or the `address` remembered
    /// for it, and say what was done.
    pub(super) async fn forget_here(mac: Mac, address: Option<String>) {
        let name = wartui_bridge::ble::advertised_name(&BridgeSpec::Mac(mac)).unwrap_or_default();
        let listed = {
            let name = name.clone();
            blocking(move || -> Result<Vec<Device>, BluezError> {
                let devices = bluez::devices()?;
                Ok(devices
                    .into_iter()
                    .filter(|d| {
                        super::is_this_bridge(
                            d.name.as_deref(),
                            &d.address,
                            &name,
                            address.as_deref(),
                        )
                    })
                    .collect())
            })
            .await
        };
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
}

/// What other OSes do instead of `pair`.
#[cfg(not(target_os = "linux"))]
const OS_PAIRS: &str = "On this OS, wartui does not pair itself: the OS pairs on the first \
     `--link bluetooth` connection made inside the bridge's pairing window, such as \
     `wartui status --link bluetooth`. That path is untested.";

#[cfg(not(target_os = "linux"))]
async fn pair_with(
    _spec: Option<BridgeSpec>,
    _preferred: Option<BridgeSpec>,
    _window: &Window,
) -> Result<()> {
    println!("{OS_PAIRS}");
    Ok(())
}

/// Whether a device the OS lists, named `listed_name` at `listed_address`, is the bridge that
/// advertises `name`, or sits at the Bluetooth address remembered for it.
fn is_this_bridge(
    listed_name: Option<&str>,
    listed_address: &str,
    name: &str,
    remembered: Option<&str>,
) -> bool {
    listed_name == Some(name) || remembered.is_some_and(|r| r.eq_ignore_ascii_case(listed_address))
}

async fn forget(args: ForgetArgs) -> Result<()> {
    let remembered = crate::config::remembered_bridge(args.config.as_deref());
    let over = crate::link_for(args.link, args.bridge.as_deref(), remembered.link)?;
    let (mut link, info) =
        crate::connect_one(args.bridge.as_deref(), args.link, args.config.as_deref()).await?;
    let name = wartui_bridge::ble::advertised_name(&BridgeSpec::Mac(info.mac)).unwrap_or_default();
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

/// Wait for the bonds to clear. Over USB a status shows them gone. Over Bluetooth this PC's bond
/// went with them, so the bridge may stop serving it before it answers, and only a fault is
/// listened for.
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
    link.send_bulk(HostToBridge::GetStatus).context("queueing the status request")?;
    let status = tokio::time::timeout(REPLY_TIMEOUT, wait_for_status(link))
        .await
        .context("the bridge did not answer a status request")??;
    let BridgeToHost::Status { bluetooth, .. } = status else {
        unreachable!("wait_for_status only returns Status")
    };
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
    use wartui_bridge::ports::{CH343_PID, CH343_VID, candidate};
    use wartui_bridge::serial::{BridgeSpec, Probed};
    use wartui_proto::link::{BluetoothStatus, PairWindow};

    use super::{
        OnUsb, Window, is_this_bridge, not_kept, paired_line, stranded_notice, usb_bridge,
    };

    const BRIDGE: [u8; 6] = [0x02, 0x00, 0x5E, 0x10, 0x44, 0xC0];
    const OTHER: [u8; 6] = [0x02, 0x00, 0x5E, 0x10, 0xA1, 0xB0];

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
        let text = not_kept(&Window::NotOnUsb, "it dropped this PC as unbonded");
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
        assert_eq!(
            paired_line("wartui 44:C0", &status),
            "wartui 44:C0 paired and bonded, 2 bonds stored"
        );
    }

    #[test]
    fn bluetooth_forget_matches_device_by_name_or_remembered_address() {
        let name = "wartui 44:C0";
        assert!(is_this_bridge(Some(name), "02:00:5E:10:44:C2", name, None));
        assert!(is_this_bridge(None, "02:00:5E:10:44:C2", name, Some("02:00:5e:10:44:c2")));
        assert!(!is_this_bridge(Some("wartui A1:B0"), "02:00:5E:10:A1:B2", name, None));
    }

    #[test]
    fn bluetooth_forget_tells_every_other_host_to_forget_when_bonds_are_cleared() {
        let notice = stranded_notice("wartui 44:C0");
        assert!(notice.contains("Every other host that paired with wartui 44:C0"), "{notice}");
        assert!(notice.contains("forgets the bridge"), "{notice}");
    }
}
