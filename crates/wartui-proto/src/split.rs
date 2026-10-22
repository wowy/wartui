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
//! - **The P4 carries the C6's image, and the C6 runs it.** A hello whose image digest
//!   differs from the bundled one is [`RadioState::Updating`]: the P4 sends the bundle over
//!   the link (`crate::spi::update`) and refuses radio commands meanwhile. The C6 reboots
//!   into the new image, and its next hello is a new life. One attempt per P4 life, so an
//!   image that will not take cannot loop: a C6 still on another build afterwards is
//!   [`RadioState::Stale`], bridged with a fault when its revision matches.
//! - **A C6 that never says hello after the install is power-cycled**, with
//!   [`PairEvent::ResetRadio`], up to [`TRIAL_BOOTS`] times. A trial image that hangs gets no
//!   watchdog, and each power cycle counts a trial boot, so the C6 rolls back to the image it
//!   ran before (`firmware/common/src/ota.rs`).
//! - **A C6 whose hello carries no digest predates the update path** and is not updated: it
//!   cannot take the image, so it needs one wired flash.

use core::fmt::Write;

use crate::link::{
    BridgeToHost, Chip, HostToBridge, LINK_PROTO_VERSION, LogStr, LoopPhase, QueueId, QueueStat,
    QueueStats, RadioInfo, SendStatus, ShortStr, SpiStats,
};
use crate::mac::Mac;
use crate::ota::TRIAL_BOOTS;
use crate::ota::image::DIGEST_LEN;
use crate::reset::ResetCause;
use crate::spi::{Hello, LINK_REVISION, RadioStatus};

/// How long the P4 waits for the C6's first hello before enumerating USB with its own MAC.
pub const RADIO_WAIT_MS: u64 = 2_000;

/// How long the P4 waits for the C6's hello after an update's install, or after resetting
/// it since, before resetting it again.
///
/// Before the new image's first hello the C6 erases the trial sector, rewrites `otadata`,
/// reboots, and writes the trial's boot count before Wi-Fi starts. Each reset counts a
/// trial boot and the third rolls back, so a wait too short rolls back a good image that
/// boots slowly.
pub const INSTALL_WAIT_MS: u64 = 10_000;

/// What the P4 knows of its C6.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RadioState {
    /// No hello yet, within [`RADIO_WAIT_MS`].
    Waiting,
    /// The C6 answered, runs the bundled image and speaks this P4's [`LINK_REVISION`].
    Up(Hello),
    /// The C6 runs another image, and the P4 is sending it the bundled one.
    Updating(Hello),
    /// The C6 runs another image after this P4 life's update, and speaks this P4's revision:
    /// bridged, with a fault.
    Stale(Hello),
    /// The C6 answered speaking another revision, or predates the update path.
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
    /// The C6 installed an update and has not said hello since: reset it.
    ResetRadio,
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
    /// The P4 was built to read a GPS receiver.
    gps: bool,
    /// The digest of the C6 image the P4 carries.
    bundled: [u8; DIGEST_LEN],
    /// This P4 life has tried to update the C6.
    attempted: bool,
    /// How much of the image the C6 holds, in percent.
    progress: u8,
    /// When the C6 was last told to install or reset, while no hello has followed.
    installed_at: Option<u64>,
    /// Resets since the install.
    resets: u32,
}

impl Pair {
    /// A pair that has heard nothing yet. `boot_ms` is the P4 clock at its start, `gps`
    /// says whether the P4 was built to read a GPS receiver, which `Ready` reports, and
    /// `bundled` is the digest of the C6 image the P4 carries.
    pub fn new(
        own_mac: Mac,
        own_fw: ShortStr,
        own_cause: ResetCause,
        own_phase: LoopPhase,
        boot_ms: u64,
        gps: bool,
        bundled: [u8; DIGEST_LEN],
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
            gps,
            bundled,
            attempted: false,
            progress: 0,
            installed_at: None,
            resets: 0,
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
    ///
    /// A new life's state follows from its image digest; see the module documentation.
    pub fn on_hello(&mut self, hello: Hello, seq: u32, now_ms: u64) -> PairEvent {
        let last_seq = core::mem::replace(&mut self.c6_seq, seq);
        let running = match &self.state {
            RadioState::Up(old)
            | RadioState::Updating(old)
            | RadioState::Stale(old)
            | RadioState::Mismatch(old) => {
                let same =
                    old.mac == hello.mac && old.link_rev == hello.link_rev && seq >= last_seq;
                if same {
                    return PairEvent::None;
                }
                true
            }
            RadioState::Waiting | RadioState::Absent => false,
        };
        self.state = self.judge(hello);
        self.installed_at = None;
        self.hello_at_ms = now_ms;
        self.life_from_radio = running;
        // A new C6 life: counters from the last one no longer apply.
        self.status = None;
        PairEvent::NewLife
    }

    /// The state a new life's hello puts the pair in.
    fn judge(&mut self, hello: Hello) -> RadioState {
        let revision = hello.link_rev == LINK_REVISION;
        match hello.image {
            Some(image) if image == self.bundled && revision => RadioState::Up(hello),
            Some(image) if image != self.bundled && !self.attempted => {
                self.attempted = true;
                self.progress = 0;
                RadioState::Updating(hello)
            }
            Some(image) if image != self.bundled && revision => RadioState::Stale(hello),
            Some(_) | None => RadioState::Mismatch(hello),
        }
    }

    /// The update reached `percent` of the image.
    pub fn note_update_progress(&mut self, percent: u8) {
        self.progress = percent;
    }

    /// How much of the image the C6 holds, in percent, as last noted.
    pub fn update_progress(&self) -> u8 {
        self.progress
    }

    /// The P4 sent the C6 `Install` at `now_ms`. Until a hello follows, [`Pair::on_tick`]
    /// resets the C6 every [`INSTALL_WAIT_MS`], and [`Pair::fault`] says it is installing.
    pub fn note_installed(&mut self, now_ms: u64) {
        if matches!(self.state, RadioState::Updating(_)) {
            self.installed_at = Some(now_ms);
            self.resets = 0;
        }
    }

    /// The update failed: the C6 refused the image or its install, or stopped answering. It is
    /// [`RadioState::Stale`] if its revision matches, so it still bridges, and no reset
    /// follows.
    pub fn on_update_failed(&mut self) {
        self.installed_at = None;
        if let RadioState::Updating(hello) = &self.state {
            let hello = hello.clone();
            let revision = hello.link_rev == LINK_REVISION;
            self.state =
                if revision { RadioState::Stale(hello) } else { RadioState::Mismatch(hello) };
        }
    }

    /// The C6 sent a transfer other than a hello, numbered `seq`.
    pub fn note_seq(&mut self, seq: u32) {
        self.c6_seq = seq;
    }

    /// The P4 clock reached `now_ms`.
    ///
    /// Gives up on a C6 silent since boot, and resets one silent since an install, at most
    /// [`TRIAL_BOOTS`] times before giving up on it too.
    pub fn on_tick(&mut self, now_ms: u64) -> PairEvent {
        if self.state == RadioState::Waiting && now_ms.saturating_sub(self.boot_ms) >= RADIO_WAIT_MS
        {
            self.state = RadioState::Absent;
            return PairEvent::NewLife;
        }
        if let Some(at) = self.installed_at
            && now_ms.saturating_sub(at) >= INSTALL_WAIT_MS
        {
            if self.resets < TRIAL_BOOTS {
                self.resets += 1;
                self.installed_at = Some(now_ms);
                return PairEvent::ResetRadio;
            }
            self.installed_at = None;
            self.state = RadioState::Absent;
            return PairEvent::NewLife;
        }
        PairEvent::None
    }

    /// The earliest P4 time at which [`Pair::on_tick`] acts, or `None` while it has nothing
    /// to wait for. A caller sleeps until then instead of polling.
    pub fn next_deadline_ms(&self) -> Option<u64> {
        let waiting = (self.state == RadioState::Waiting).then_some(self.boot_ms + RADIO_WAIT_MS);
        let installed = self.installed_at.map(|at| at + INSTALL_WAIT_MS);
        match (waiting, installed) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }

    /// The C6's current state.
    pub fn state(&self) -> &RadioState {
        &self.state
    }

    /// Whether radio commands and records flow: the C6 is up, or stale with a matching
    /// revision.
    pub fn bridges(&self) -> bool {
        matches!(self.state, RadioState::Up(_) | RadioState::Stale(_))
    }

    /// Whether the C6's records are read: while it bridges, and while it updates speaking this
    /// P4's revision, so its lines about the update reach the host.
    pub fn reads_records(&self) -> bool {
        match &self.state {
            RadioState::Updating(hello) => hello.link_rev == LINK_REVISION,
            _ => self.bridges(),
        }
    }

    fn radio_hello(&self) -> Option<&Hello> {
        match &self.state {
            RadioState::Up(hello)
            | RadioState::Updating(hello)
            | RadioState::Stale(hello)
            | RadioState::Mismatch(hello) => Some(hello),
            RadioState::Waiting | RadioState::Absent => None,
        }
    }

    /// The MAC for the USB serial number: `None` until the P4 knows which to use.
    pub fn usb_serial_mac(&self) -> Option<Mac> {
        match &self.state {
            RadioState::Waiting => None,
            RadioState::Absent => Some(self.own_mac),
            RadioState::Up(hello)
            | RadioState::Updating(hello)
            | RadioState::Stale(hello)
            | RadioState::Mismatch(hello) => Some(hello.mac),
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
            gps: self.gps,
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
            RadioState::Mismatch(hello) if hello.image.is_none() => {
                let _ = write!(
                    text,
                    "radio predates bundled updates; flash it through the header once"
                );
            }
            RadioState::Mismatch(hello) => {
                let _ = write!(
                    text,
                    "radio speaks SPI link revision {}, this P4 speaks {}; update the radio",
                    hello.link_rev, LINK_REVISION
                );
            }
            RadioState::Updating(_) if self.installed_at.is_some() => {
                let _ = write!(text, "updating the radio: installing the bundled image");
            }
            RadioState::Updating(_) => {
                let _ =
                    write!(text, "updating the radio to the bundled image ({}%)", self.progress);
            }
            RadioState::Stale(_) => {
                let _ = write!(text, "radio runs another build; the bundled image did not take");
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
                if self.bridges() =>
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
