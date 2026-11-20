//! Who owns the P4's host queue: the USB host on UART0, a Bluetooth central through the C6, a
//! phone on the WardriveESP service through the C6, or no one. [`Owner`] is pure: the P4 feeds it
//! each input and carries out the [`Effect`]s it returns, in order
//! (`firmware/p4-bridge/src/ble.rs`).
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
//!
//! **A phone counts as the host, and holds the queue alone** ([`Holder::Phone`]). It subscribes
//! with no owner, or not at all: while a central or USB holds the queue the phone is ignored.
//! Any UART0 frame but `Identify` takes the queue from it, as from a central, with no notice,
//! since the notice is a COBS frame and a phone reads text lines. An `Identify` gets its `Ready`
//! on UART0 and leaves the phone alone. A phone never waits for USB: while USB holds the queue
//! [`phones_admitted`] is false, the C6 drops the phone, and the phone relinks by itself once
//! UART0 has been silent for the gate's 15 s. A central subscribed while a phone holds the queue
//! waits, and takes it when the phone lets go. No wartui host can see a phone hold the queue,
//! since any frame it sends takes it, so [`Owner::owner`] reads a phone as [`HostOwner::None`].

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
    /// A phone subscribed to the WardriveESP stream while the setting admits phones.
    PhoneSubscribed,
    /// The phone's subscription ended, it disconnected, the C6 started a new life, or the setting
    /// stopped admitting it.
    PhoneUnsubscribed,
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
    /// Make the phone the host: mark where its stream begins, clear the queue and start its
    /// session.
    AttachPhone,
    /// Stop serving the phone: end its session, stop the scan and clear the queue.
    ReleasePhone,
}

/// The effects of one input: three at most.
pub type Effects = Vec<Effect, 3>;

/// Who holds the host queue.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Holder {
    /// No host: the P4 queues nothing.
    #[default]
    None,
    /// A host on USB, through the CH343.
    Usb,
    /// A bonded central over Bluetooth, through the C6.
    Bluetooth,
    /// A phone on the WardriveESP service, through the C6.
    Phone,
}

impl Holder {
    /// Whether the holder reads the queue through the C6, down the radio link, rather than on
    /// UART0 or not at all.
    #[must_use]
    pub const fn through_c6(self) -> bool {
        match self {
            Self::Bluetooth | Self::Phone => true,
            Self::None | Self::Usb => false,
        }
    }
}

/// Whether the C6 may admit a phone: the setting is on and USB does not hold the queue.
#[must_use]
pub const fn phones_admitted(enabled: bool, holder: Holder) -> bool {
    enabled && !matches!(holder, Holder::Usb)
}

/// Who holds the host queue, and whether a central waits for it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Owner {
    holder: Holder,
    /// A central is subscribed and authorized, whether or not it owns the queue.
    central: bool,
}

impl Owner {
    /// No host, and no central.
    #[must_use]
    pub const fn new() -> Self {
        Self { holder: Holder::None, central: false }
    }

    /// Who holds the queue.
    #[must_use]
    pub const fn holder(&self) -> Holder {
        self.holder
    }

    /// Which wartui host owns the queue, as `Status` reports it: a phone reads as none.
    #[must_use]
    pub const fn owner(&self) -> HostOwner {
        match self.holder {
            Holder::None | Holder::Phone => HostOwner::None,
            Holder::Usb => HostOwner::Usb,
            Holder::Bluetooth => HostOwner::Bluetooth,
        }
    }

    /// Whether a central is subscribed and authorized, owning the queue or waiting for it.
    #[must_use]
    pub const fn central(&self) -> bool {
        self.central
    }

    /// Take `input`, and return what the P4 must do, in order.
    pub fn on(&mut self, input: Input) -> Effects {
        let effects: &[Effect] = match (input, self.holder) {
            (Input::Subscribed, Holder::None) => {
                self.central = true;
                self.holder = Holder::Bluetooth;
                &[Effect::AttachBluetooth]
            }
            // Bluetooth already owns it, or the central waits for USB to fall silent or the phone
            // to let go.
            (Input::Subscribed, Holder::Bluetooth | Holder::Usb | Holder::Phone) => {
                self.central = true;
                &[]
            }
            (Input::Unsubscribed, Holder::Bluetooth) => {
                self.central = false;
                self.holder = Holder::None;
                &[Effect::ReleaseBluetooth]
            }
            (Input::Unsubscribed, Holder::None | Holder::Usb | Holder::Phone) => {
                self.central = false;
                &[]
            }
            (Input::UartIdentify, Holder::Bluetooth | Holder::Phone) => &[Effect::ReadyOnUart],
            (Input::UartIdentify | Input::UartFrame, Holder::None) => {
                self.holder = Holder::Usb;
                &[Effect::AttachUsb]
            }
            (Input::UartIdentify | Input::UartFrame, Holder::Usb) => &[],
            (Input::UartFrame, Holder::Bluetooth) => {
                self.holder = Holder::Usb;
                &[Effect::NoticeCentral, Effect::ReleaseBluetooth, Effect::AttachUsb]
            }
            (Input::UartFrame, Holder::Phone) => {
                self.holder = Holder::Usb;
                &[Effect::ReleasePhone, Effect::AttachUsb]
            }
            (Input::UartSilent, Holder::Usb) if self.central => {
                self.holder = Holder::Bluetooth;
                &[Effect::ReleaseUsb, Effect::AttachBluetooth]
            }
            (Input::UartSilent, Holder::Usb) => {
                self.holder = Holder::None;
                &[Effect::ReleaseUsb]
            }
            (Input::UartSilent, Holder::None | Holder::Bluetooth | Holder::Phone) => &[],
            (Input::PhoneSubscribed, Holder::None) => {
                self.holder = Holder::Phone;
                &[Effect::AttachPhone]
            }
            (Input::PhoneSubscribed, Holder::Usb | Holder::Bluetooth | Holder::Phone) => &[],
            (Input::PhoneUnsubscribed, Holder::Phone) if self.central => {
                self.holder = Holder::Bluetooth;
                &[Effect::ReleasePhone, Effect::AttachBluetooth]
            }
            (Input::PhoneUnsubscribed, Holder::Phone) => {
                self.holder = Holder::None;
                &[Effect::ReleasePhone]
            }
            (Input::PhoneUnsubscribed, Holder::None | Holder::Usb | Holder::Bluetooth) => &[],
        };
        // Three at most, as `Effects` holds.
        Vec::from_slice(effects).unwrap_or_default()
    }
}
