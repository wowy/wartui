//! Who owns the P4's host queue: the USB host on UART0, a Bluetooth central through the C6, or
//! no one. [`Owner`] is pure: the P4 feeds it each input and carries out the [`Effect`]s it
//! returns, in order (`firmware/p4-bridge/src/ble.rs`).
//!
//! **One host at a time.** There is one host stream and one credit window: the queue's frames go
//! either to UART0 or down the C6 link, never both, and a central whose stream stopped part-way
//! cannot share the window with another reader. So one host owns the queue, and taking it clears
//! the queue and announces the bridge to the new owner.
//!
//! **A UART0 frame takes the queue, subscriber or not**, so `wartui flash-bridge` over USB always
//! gets in, whatever the C6 or a central does. The central is first sent a notice
//! ([`Effect::NoticeCentral`]), then released. Once UART0 falls silent for the host gate's 15 s,
//! a central still subscribed takes the queue back. A central that subscribes while USB owns the
//! queue waits for that.
//!
//! **An `Identify` never takes the queue from a Bluetooth host.** `wartui ports`, every `--bridge`
//! lookup and the probe on another PC send `Identify` to each CH343 they find. Taking the queue
//! for one drops what was queued for the central, twice: once on taking it and once on the
//! handback 15 s later. Run 10 lost a batch from every node at each such probe
//! (`docs/c6-ble-host-findings.md`, "A UART0 frame takes the queue from Bluetooth"). So an
//! `Identify` while Bluetooth owns is answered with a `Ready` written straight to UART0
//! ([`Effect::ReadyOnUart`]), and the queue stays the central's. With no owner, an `Identify`
//! opens UART0's gate, as any UART0 frame does.

use heapless::Vec;

use crate::link::HostOwner;

/// What the P4 feeds [`Owner`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Input {
    /// A central subscribed to the host stream, on an encrypted link, holding a stored bond.
    Subscribed,
    /// The central's subscription ended, its link lost encryption or its bond, it disconnected,
    /// or the C6 started a new life.
    Unsubscribed,
    /// A `HostToBridge::Identify` decoded on UART0.
    UartIdentify,
    /// Any other host frame decoded on UART0.
    UartFrame,
    /// UART0 has sent no frame for the host gate's silence timeout.
    UartSilent,
}

/// What the P4 does for an input, in the order [`Owner::on`] lists them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Effect {
    /// Make Bluetooth the host: mark where its stream begins, clear the queue and announce.
    AttachBluetooth,
    /// Make USB the host: open UART0's gate and clear the queue. The frame that caused it
    /// announces the bridge.
    AttachUsb,
    /// Stop serving the central: clear the queue and drop its update and cache sessions.
    ReleaseBluetooth,
    /// Close UART0's gate: drop its update and cache sessions.
    ReleaseUsb,
    /// Send the central one `BridgeToHost::Error` saying the USB host took the bridge, ahead of
    /// anything else on its stream.
    NoticeCentral,
    /// Answer the `Identify` with one `Ready` written straight to UART0, leaving the queue and
    /// UART0's gate alone. The frame is not otherwise carried out.
    ReadyOnUart,
}

/// The effects of one input: three at most.
pub type Effects = Vec<Effect, 3>;

/// Who owns the host queue, and whether a central waits for it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Owner {
    owner: HostOwner,
    /// A central is subscribed and authorized, whether or not it owns the queue.
    central: bool,
}

impl Owner {
    /// No host, and no central.
    #[must_use]
    pub const fn new() -> Self {
        Self { owner: HostOwner::None, central: false }
    }

    /// Which host owns the queue.
    #[must_use]
    pub const fn owner(&self) -> HostOwner {
        self.owner
    }

    /// Whether a central is subscribed and authorized, owning the queue or waiting for it.
    #[must_use]
    pub const fn central(&self) -> bool {
        self.central
    }

    /// Take `input`, and return what the P4 must do, in order.
    pub fn on(&mut self, input: Input) -> Effects {
        let effects: &[Effect] = match (input, self.owner) {
            (Input::Subscribed, HostOwner::None) => {
                self.central = true;
                self.owner = HostOwner::Bluetooth;
                &[Effect::AttachBluetooth]
            }
            // Bluetooth already owns it, or the central waits for USB to fall silent.
            (Input::Subscribed, HostOwner::Bluetooth | HostOwner::Usb) => {
                self.central = true;
                &[]
            }
            (Input::Unsubscribed, HostOwner::Bluetooth) => {
                self.central = false;
                self.owner = HostOwner::None;
                &[Effect::ReleaseBluetooth]
            }
            (Input::Unsubscribed, HostOwner::None | HostOwner::Usb) => {
                self.central = false;
                &[]
            }
            (Input::UartIdentify, HostOwner::Bluetooth) => &[Effect::ReadyOnUart],
            (Input::UartIdentify | Input::UartFrame, HostOwner::None) => {
                self.owner = HostOwner::Usb;
                &[Effect::AttachUsb]
            }
            (Input::UartIdentify | Input::UartFrame, HostOwner::Usb) => &[],
            (Input::UartFrame, HostOwner::Bluetooth) => {
                self.owner = HostOwner::Usb;
                &[Effect::NoticeCentral, Effect::ReleaseBluetooth, Effect::AttachUsb]
            }
            (Input::UartSilent, HostOwner::Usb) if self.central => {
                self.owner = HostOwner::Bluetooth;
                &[Effect::ReleaseUsb, Effect::AttachBluetooth]
            }
            (Input::UartSilent, HostOwner::Usb) => {
                self.owner = HostOwner::None;
                &[Effect::ReleaseUsb]
            }
            (Input::UartSilent, HostOwner::None | HostOwner::Bluetooth) => &[],
        };
        // Three at most, as `Effects` holds.
        Vec::from_slice(effects).unwrap_or_default()
    }
}
