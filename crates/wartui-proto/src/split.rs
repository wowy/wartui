//! The P4 bridge's view of its C6: whether the radio is up, what `Ready` says to the host,
//! where each host command goes, and what `Status` reports.
//!
//! The firmware consults [`Pair`] for these decisions so they are testable on the host.
//!
//! - **`Ready.mac` is the C6's**, because that is the address nodes see as the bridge. The
//!   P4 therefore waits [`RADIO_WAIT_MS`] for the C6's first hello before enumerating USB.
//!   Only if the C6 never answers does it announce its own MAC, with a fault.
//! - **`Ready.uptime_ms` is the C6's uptime now.** The host detects a bridge restart by
//!   that value going backwards (`is_a_new_life` in `crates/wartui-bridge/src/serial.rs`).
//!   A C6 restart under a running P4 loses the peer table and transmit power, so it must
//!   look like a bridge restart; a hello arriving after the link was up is
//!   [`PairEvent::NewLife`], and the firmware answers with a fresh `Ready`.
//! - **The C6 repeats its hello** in every transfer until it hears the P4's, so [`Pair`]
//!   tells a new life from a repeat by the transfer seq; see [`Pair::on_hello`]. A
//!   restarted C6 boots through the same code to the same first hello, often within a
//!   millisecond of the last life's uptime, so equal uptimes prove nothing.
//! - **`Ready.reset_cause` is the C6's after a C6 restart**, since that restart is the
//!   bridge restart the host sees. The P4's own boot reports the P4's cause.
//! - **A link revision mismatch does not bridge.** The hello is frozen, so the P4 can
//!   always read it and say so, but radio commands are refused.

use core::fmt::Write;

use crate::link::{
    BridgeToHost, Chip, HostToBridge, LINK_PROTO_VERSION, LogStr, LoopPhase, QueueId, QueueStat,
    QueueStats, RadioInfo, SendStatus, ShortStr, SpiStats,
};
use crate::mac::Mac;
use crate::reset::ResetCause;
use crate::spi::{Hello, LINK_REVISION, RadioStatus};

/// How long the P4 waits for the C6's first hello before enumerating USB with its own MAC.
pub const RADIO_WAIT_MS: u64 = 2_000;

/// What the P4 knows of its C6.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RadioState {
    /// No hello yet, within [`RADIO_WAIT_MS`].
    Waiting,
    /// The C6 answered and speaks this P4's [`LINK_REVISION`].
    Up(Hello),
    /// The C6 answered speaking another revision.
    Mismatch(Hello),
    /// No hello arrived in time.
    Absent,
}

/// What a hello or a tick changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PairEvent {
    /// Nothing the host needs to hear.
    None,
    /// A new bridge life begins from the host's view: announce a fresh `Ready`.
    NewLife,
}

/// Where a host command goes.
// `Refuse` carries a whole frame, but boxing needs an allocator, which this crate avoids.
// A route is built, matched and dropped at once.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Route {
    /// The P4 handles it.
    Local,
    /// Send it down to the C6.
    Forward,
    /// Do not forward it; send the host this answer, if any.
    Refuse(Option<BridgeToHost>),
}

/// The P4's pair state.
#[derive(Debug, Clone)]
pub struct Pair {
    state: RadioState,
    /// The P4 clock when the current hello arrived.
    hello_at_ms: u64,
    /// The seq of the last C6 transfer, of any kind.
    c6_seq: u32,
    /// The current life began with a C6 restart under a running P4.
    life_from_radio: bool,
    status: Option<RadioStatus>,
    own_mac: Mac,
    own_fw: ShortStr,
    own_cause: ResetCause,
    own_phase: LoopPhase,
    boot_ms: u64,
}

impl Pair {
    /// A pair that has heard nothing yet. `boot_ms` is the P4 clock at its start.
    pub fn new(
        own_mac: Mac,
        own_fw: ShortStr,
        own_cause: ResetCause,
        own_phase: LoopPhase,
        boot_ms: u64,
    ) -> Self {
        Self {
            state: RadioState::Waiting,
            hello_at_ms: boot_ms,
            c6_seq: 0,
            life_from_radio: false,
            status: None,
            own_mac,
            own_fw,
            own_cause,
            own_phase,
            boot_ms,
        }
    }

    /// The C6 sent a hello in its transfer `seq`, at P4 time `now_ms`.
    ///
    /// The C6 repeats its hello in every transfer until it has heard the P4's, so most
    /// calls are the same life again. A hello is a [`PairEvent::NewLife`] when any of these
    /// holds:
    ///
    /// - it is the first, or its MAC or link revision changed;
    /// - its seq is lower than the last C6 transfer's ([`Pair::note_seq`]). The C6 counts
    ///   transfers from 0 at every boot and only upwards within a life, so a lower seq is a
    ///   restart, wherever in the last life it came.
    ///
    /// The seq is a `u32` and wraps after about 31 days at 1,600 transfers a second. A wrap
    /// comes mid-life, on a transfer that is not a hello, since the C6 sends hellos only
    /// until the P4 answers. So a hello's seq is lower than the last only after a restart.
    ///
    /// Anything else leaves the anchor and counters alone, so `Ready.uptime_ms` never steps
    /// back.
    pub fn on_hello(&mut self, hello: Hello, seq: u32, now_ms: u64) -> PairEvent {
        let last_seq = core::mem::replace(&mut self.c6_seq, seq);
        let running = match &self.state {
            RadioState::Up(old) | RadioState::Mismatch(old) => {
                let same =
                    old.mac == hello.mac && old.link_rev == hello.link_rev && seq >= last_seq;
                if same {
                    return PairEvent::None;
                }
                true
            }
            RadioState::Waiting | RadioState::Absent => false,
        };
        let matches = hello.link_rev == LINK_REVISION;
        self.state = if matches { RadioState::Up(hello) } else { RadioState::Mismatch(hello) };
        self.hello_at_ms = now_ms;
        self.life_from_radio = running;
        // A new C6 life: counters from the last one no longer apply.
        self.status = None;
        PairEvent::NewLife
    }

    /// The C6 sent a transfer other than a hello, numbered `seq`.
    pub fn note_seq(&mut self, seq: u32) {
        self.c6_seq = seq;
    }

    /// The P4 clock reached `now_ms`.
    pub fn on_tick(&mut self, now_ms: u64) -> PairEvent {
        if self.state == RadioState::Waiting && now_ms.saturating_sub(self.boot_ms) >= RADIO_WAIT_MS
        {
            self.state = RadioState::Absent;
            return PairEvent::NewLife;
        }
        PairEvent::None
    }

    /// The C6's current state.
    pub fn state(&self) -> &RadioState {
        &self.state
    }

    fn radio_hello(&self) -> Option<&Hello> {
        match &self.state {
            RadioState::Up(hello) | RadioState::Mismatch(hello) => Some(hello),
            RadioState::Waiting | RadioState::Absent => None,
        }
    }

    /// The MAC for the USB serial number: `None` until the P4 knows which to use.
    pub fn usb_serial_mac(&self) -> Option<Mac> {
        match &self.state {
            RadioState::Waiting => None,
            RadioState::Absent => Some(self.own_mac),
            RadioState::Up(hello) | RadioState::Mismatch(hello) => Some(hello.mac),
        }
    }

    /// Milliseconds since this bridge life began, as the host counts it.
    fn uptime_ms(&self, now_ms: u64) -> u32 {
        match self.radio_hello() {
            Some(hello) => {
                hello.uptime_ms.saturating_add(clamp_u32(now_ms.saturating_sub(self.hello_at_ms)))
            }
            None => clamp_u32(now_ms.saturating_sub(self.boot_ms)),
        }
    }

    /// The `Ready` to announce, or `None` while the P4 is still waiting for the C6.
    pub fn ready(&self, now_ms: u64) -> Option<BridgeToHost> {
        let mac = self.usb_serial_mac()?;
        let hello = self.radio_hello();
        let (reset_cause, last_phase) = match hello {
            Some(hello) if self.life_from_radio => (hello.reset_cause, hello.last_phase),
            _ => (self.own_cause, self.own_phase),
        };
        Some(BridgeToHost::Ready {
            chip: Chip::Esp32P4,
            mac,
            fw_version: self.own_fw.clone(),
            proto_version: LINK_PROTO_VERSION,
            reset_cause,
            last_phase,
            heap_free: hello.map_or(0, |hello| hello.heap_free),
            uptime_ms: self.uptime_ms(now_ms),
            panel: None,
            gps: false,
            radio: hello.map(|hello| RadioInfo {
                chip: hello.chip,
                mac: hello.mac,
                fw_version: hello.fw_version.clone(),
                reset_cause: hello.reset_cause,
                last_phase: hello.last_phase,
                uptime_ms: hello.uptime_ms,
            }),
        })
    }

    /// The fault to tell the host about, if the radio is not usable.
    pub fn fault(&self) -> Option<LogStr> {
        let mut text = LogStr::new();
        match &self.state {
            RadioState::Mismatch(hello) => {
                let _ = write!(
                    text,
                    "radio speaks SPI link revision {}, this P4 speaks {}; update the radio",
                    hello.link_rev, LINK_REVISION
                );
            }
            RadioState::Absent => {
                let _ = write!(text, "the radio coprocessor is not answering");
            }
            RadioState::Waiting | RadioState::Up(_) => return None,
        }
        Some(text)
    }

    /// Where `cmd` goes.
    pub fn route(&self, cmd: &HostToBridge) -> Route {
        match cmd {
            HostToBridge::Identify
            | HostToBridge::GetStatus
            | HostToBridge::Reset
            | HostToBridge::ShowPanel { .. }
            | HostToBridge::GpsWrite { .. } => Route::Local,
            // Listed rather than caught, so a new command is a compile error until it is
            // placed.
            HostToBridge::SendEspNow { .. }
            | HostToBridge::AddPeer { .. }
            | HostToBridge::RemovePeer { .. }
            | HostToBridge::SetTxPower { .. }
                if matches!(self.state, RadioState::Up(_)) =>
            {
                Route::Forward
            }
            HostToBridge::SendEspNow { id, .. } => Route::Refuse(Some(BridgeToHost::SendResult {
                id: *id,
                status: SendStatus::Rejected,
                tx_us: 0,
            })),
            HostToBridge::AddPeer { .. }
            | HostToBridge::RemovePeer { .. }
            | HostToBridge::SetTxPower { .. } => Route::Refuse(None),
        }
    }

    /// The C6 reported its counters.
    pub fn note_radio_status(&mut self, status: RadioStatus) {
        self.status = Some(status);
    }

    /// The `Status` to answer a poll with. `host` and `down` are the P4's own queues;
    /// `host_dropped` is what its outbox dropped.
    pub fn status(
        &self,
        now_ms: u64,
        host: [QueueStat; 2],
        down: QueueStat,
        mut spi: SpiStats,
        host_dropped: u32,
    ) -> BridgeToHost {
        let mut queues = QueueStats::new();
        let mut radio_up_drops = 0u32;
        if let Some(status) = &self.status {
            for stat in &status.queues {
                if matches!(stat.queue, QueueId::RadioUp | QueueId::RadioUpPriority) {
                    radio_up_drops = radio_up_drops.saturating_add(stat.dropped);
                }
                let _ = queues.push(*stat);
            }
            spi.radio_crc_fail = status.crc_fail;
            spi.radio_bad_magic = status.bad_magic;
            spi.radio_seq_gap = status.seq_gap;
        }
        // The C6's queues leave room for these: 4 + 3 fits in 8.
        let _ = queues.push(down);
        for stat in host {
            let _ = queues.push(stat);
        }
        let counters = self.status.as_ref();
        BridgeToHost::Status {
            peer_count: counters.map_or(0, |status| status.peer_count),
            rx_count: counters.map_or(0, |status| status.rx_count),
            dropped_tx: host_dropped.saturating_add(radio_up_drops),
            uptime_ms: self.uptime_ms(now_ms),
            rx_oversize: counters.map_or(0, |status| status.rx_oversize),
            queues,
            spi: Some(spi),
        }
    }
}

fn clamp_u32(value: u64) -> u32 {
    u32::try_from(value).unwrap_or(u32::MAX)
}
