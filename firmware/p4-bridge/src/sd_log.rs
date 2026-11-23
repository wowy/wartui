//! The diagnostics log on the microSD card: what the bridge records, and the ring that holds it
//! until the card takes it. `src/sd.rs` writes it to the card; `wartui_proto::sd::log` has the
//! records and `wartui_proto::sd::append` the FAT32 writer.
//!
//! **What is logged**, while the setting is on:
//!
//! - every frame the bridge produces for a host but node records, GPS bytes and delivery outcomes
//!   (`log::logged`), whether or not a host holds the queue, so a phone session, which reads no
//!   frames, still leaves its errors and logs;
//! - a `Status` every [`SNAPSHOT_EVERY`], when a file opens and after each change of who holds the
//!   host queue. It is built from the C6's counters as last received: it never asks the C6 for
//!   fresh ones, so it adds no radio-link traffic and waits for nothing;
//! - each change of who holds the host queue (`src/ble.rs`, `own`);
//! - a phone's commands, and the lines it is sent but its `WD:AP` and `WD:BT` sightings.
//!
//! **The log is the lowest-priority work on the P4, and never makes anything wait.** A producer
//! encodes one record, copies it into the ring and returns. A full ring evicts its oldest records
//! and counts them, and the next flush says how many in a `Dropped` record; nothing is ever
//! back-pressured. The ring is PSRAM of its own, split off after the host bulk ring
//! ([`SD_LOG_BYTES`](crate::config::SD_LOG_BYTES)), and takes none of the host queue's space or
//! credit. With the setting off a producer reads one `Cell` and returns. The writer yields between
//! card transfers and pauses while an update or a node image upload runs (`src/sd.rs`). While no
//! card takes the log, the ring keeps the newest records for the next card's first file.
//!
//! **Nothing pushes to the host, or logs, while the ring is borrowed.** `Shared::push_host` tees
//! into the ring, so a push from inside a borrow would borrow it twice, which panics, and a panic
//! reboots. Each borrow covers one push or one read and nothing else, and a push that finds the
//! ring borrowed anyway drops its record and counts it.
//!
//! **Turning the setting off** stops the producers at once. The writer then flushes what the ring
//! holds, ends the file with a `Closed` record and empties the ring; with no file open the ring is
//! emptied at once. Turning it on opens a new file.

use core::cell::{Cell, RefCell};
use core::fmt::Write as _;

use embassy_futures::select::select;
use embassy_sync::blocking_mutex::raw::NoopRawMutex;
use embassy_sync::signal::Signal;
use embassy_time::{Duration, Instant, Timer};
use wartui_proto::frame_ring::FrameRingIn;
use wartui_proto::heapless::String;
use wartui_proto::host_owner::{Holder, Input};
use wartui_proto::link::{self, LogStr, LoopPhase, MAX_FRAME, SdLogState, ShortStr, encode_frame};
use wartui_proto::ota::image::DIGEST_LEN;
use wartui_proto::reset::ResetCause;
use wartui_proto::sd::log::{self, FORMAT_VERSION, Kind, Opened, RECORD_MAX};
use wartui_proto::wardrive_esp::session::Class;

use crate::{Shared, now_ms};

/// How often a `Status` snapshot is logged, besides when a file opens and the owner changes.
pub const SNAPSHOT_EVERY: Duration = Duration::from_secs(30);

/// Bytes queued past which a producer wakes the writer before its next tick.
const URGE_BYTES: usize = 12 * 1024;

/// The phone's sighting lines, which the log leaves out.
const SIGHTING_LINES: [&[u8]; 2] = [b"WD:AP ", b"WD:BT "];

/// Encoded records waiting for the card, over PSRAM.
pub type Ring = FrameRingIn<&'static mut [u8]>;

/// What this boot is, for the `Opened` record at the head of every file.
pub struct Boot {
    pub fw_version: ShortStr,
    pub cause: ResetCause,
    pub last_phase: LoopPhase,
    pub image: Option<[u8; DIGEST_LEN]>,
}

/// A stretch of the writer's synchronous work between two yields, which [`note_busy`] times.
#[derive(Clone, Copy)]
pub enum Window {
    /// The appender's `next`, and copying a block it writes.
    Next,
    /// The appender taking a block a `Read` brought back.
    Fed,
    /// The appender taking one block of a `ReadRun`.
    RunFed,
    /// A flush's head: the file's tail and a `Dropped` record.
    Head,
    /// A flush's head with the new file's `Opened` record.
    Opened,
    /// One slice of a flush's records copied from the ring, a block or so.
    Copy,
    /// A flush's end: a `Closed` record, the padding and the appender planning the flush.
    Plan,
}

impl Window {
    /// Every window in declaration order, so a window's `as usize` is its index here.
    const ALL: [Self; 7] =
        [Self::Next, Self::Fed, Self::RunFed, Self::Head, Self::Opened, Self::Copy, Self::Plan];
    const COUNT: usize = Self::ALL.len();

    const fn name(self) -> &'static str {
        match self {
            Self::Next => "next",
            Self::Fed => "fed",
            Self::RunFed => "run",
            Self::Head => "head",
            Self::Opened => "opened",
            Self::Copy => "copy",
            Self::Plan => "plan",
        }
    }
}

/// The start of [`say_longest`]'s line.
const LONGEST: &str = "sd log: longest µs";

// [`say_longest`]'s line fits a [`LogStr`] while every maximum is under 100 ms (five digits).
const _: () = {
    let mut len = LONGEST.len();
    let mut i = 0;
    while i < Window::COUNT {
        len += 1 + Window::ALL[i].name().len() + 1 + 5;
        i += 1;
    }
    assert!(len <= capacity(None::<&LogStr>));
};

/// The capacity of a `String<N>`, where a constant needs it.
const fn capacity<const N: usize>(_: Option<&String<N>>) -> usize {
    N
}

/// What the writer reports in `SdStatus.log`, beside the setting and the drops.
#[derive(Clone, Copy)]
pub struct Stats {
    pub state: SdLogState,
    pub file: Option<u32>,
    pub written: u64,
    pub errors: u32,
    pub busy_max_us: u32,
}

/// The log as the producers and the writer share it.
pub struct SdLog {
    /// The setting `SetSdLog` stores.
    on: Cell<bool>,
    /// Changes of the setting since boot, so the writer sees an off and an on between two looks.
    toggles: Cell<u32>,
    ring: RefCell<Ring>,
    /// Records lost outside the ring's own count: taken for a flush that failed, or pushed while
    /// the ring was borrowed.
    lost: Cell<u32>,
    /// Raised past [`URGE_BYTES`] and by a change of the setting, so the writer looks before its
    /// next tick.
    pub urge: Signal<NoopRawMutex, ()>,
    /// Raised for a snapshot ahead of the next [`SNAPSHOT_EVERY`].
    snapshot: Signal<NoopRawMutex, ()>,
    stats: Cell<Stats>,
    /// Each [`Window`]'s longest stretch, by its index.
    maxima: [Cell<u32>; Window::COUNT],
    /// Whether a maximum grew since [`say_longest`] last said them.
    unsaid: Cell<bool>,
    boot: Boot,
}

impl SdLog {
    /// The ring over `ring`, this boot's facts, and the setting off until [`Self::boot_setting`].
    pub fn new(ring: &'static mut [u8], boot: Boot) -> Self {
        Self {
            on: Cell::new(false),
            toggles: Cell::new(0),
            ring: RefCell::new(FrameRingIn::over(ring)),
            lost: Cell::new(0),
            urge: Signal::new(),
            snapshot: Signal::new(),
            stats: Cell::new(Stats {
                state: SdLogState::Waiting,
                file: None,
                written: 0,
                errors: 0,
                busy_max_us: 0,
            }),
            maxima: [const { Cell::new(0) }; Window::COUNT],
            unsaid: Cell::new(false),
            boot,
        }
    }

    /// Take the setting the settings sector holds at boot.
    pub fn boot_setting(&self, on: bool) {
        self.on.set(on);
    }

    /// Why this boot started.
    pub fn reset_cause(&self) -> ResetCause {
        self.boot.cause
    }
}

/// Whether the bridge logs to the card.
pub fn on(shared: &Shared) -> bool {
    shared.sd_log.on.get()
}

/// The setting's changes since boot.
pub fn toggles(shared: &Shared) -> u32 {
    shared.sd_log.toggles.get()
}

/// Log to the card when `on`, from now. Off, the ring is emptied at once when no file is open,
/// and by the writer's last flush when one is.
pub fn set(shared: &Shared, on: bool) {
    let log = &shared.sd_log;
    if log.on.replace(on) == on {
        return;
    }
    log.toggles.set(log.toggles.get().wrapping_add(1));
    if !on && log.stats.get().file.is_none() {
        clear(shared);
    }
    log.urge.signal(());
}

/// Forget every record in the ring.
pub fn clear(shared: &Shared) {
    if let Ok(mut ring) = shared.sd_log.ring.try_borrow_mut() {
        ring.clear();
    }
}

/// Whether the ring holds a record.
pub fn pending(shared: &Shared) -> bool {
    shared.sd_log.ring.try_borrow().is_ok_and(|ring| !ring.is_empty())
}

/// Records the log lost since boot: the ring's evictions and refusals, and [`lose`]'s.
pub fn dropped(shared: &Shared) -> u32 {
    let ring = shared.sd_log.ring.try_borrow().map_or(0, |ring| ring.dropped());
    ring.wrapping_add(shared.sd_log.lost.get())
}

/// Count `records` lost outside the ring.
pub fn lose(shared: &Shared, records: u32) {
    let lost = &shared.sd_log.lost;
    lost.set(lost.get().wrapping_add(records));
}

/// Run `f` on the ring, or nothing when it is borrowed already. `f` must not push to the host or
/// log (see the module docs).
pub fn with_ring<R>(shared: &Shared, f: impl FnOnce(&mut Ring) -> R) -> Option<R> {
    let mut ring = shared.sd_log.ring.try_borrow_mut().ok()?;
    Some(f(&mut ring))
}

/// The writer's figures.
pub fn stats(shared: &Shared) -> Stats {
    shared.sd_log.stats.get()
}

/// Change the writer's figures.
pub fn update(shared: &Shared, f: impl FnOnce(&mut Stats)) {
    let mut stats = shared.sd_log.stats.get();
    f(&mut stats);
    shared.sd_log.stats.set(stats);
}

/// Keep `start`'s elapsed time as `window`'s longest synchronous stretch, and as the longest of
/// all, if it is, for [`say_longest`]. It logs nothing itself, since a caller may hold the ring.
pub fn note_busy(shared: &Shared, start: Instant, window: Window) {
    let us = u32::try_from(start.elapsed().as_micros()).unwrap_or(u32::MAX);
    let max = &shared.sd_log.maxima[window as usize];
    if us > max.get() {
        max.set(us);
        shared.sd_log.unsaid.set(true);
    }
    if us > stats(shared).busy_max_us {
        update(shared, |stats| stats.busy_max_us = us);
    }
}

/// Say every window's longest stretch in one log line, once after any of them grows: `sd log:
/// longest µs next 41 fed 12 …`. Called by the writer where it holds no borrow.
pub fn say_longest(shared: &Shared) {
    if !shared.sd_log.unsaid.replace(false) {
        return;
    }
    let mut line = LogStr::new();
    let _ = line.push_str(LONGEST);
    for (window, max) in Window::ALL.iter().zip(&shared.sd_log.maxima) {
        let _ = write!(line, " {} {}", window.name(), max.get());
    }
    shared.push_log(&line);
}

/// The log, for `SdStatus.log`.
pub fn report(shared: &Shared) -> link::SdLog {
    let on = on(shared);
    let stats = stats(shared);
    link::SdLog {
        on,
        state: if on { stats.state } else { SdLogState::Off },
        file: stats.file,
        written: stats.written,
        dropped: dropped(shared),
        errors: stats.errors,
        busy_max_us: stats.busy_max_us,
    }
}

/// Ask for a snapshot ahead of the next [`SNAPSHOT_EVERY`].
pub fn ask_snapshot(shared: &Shared) {
    shared.sd_log.snapshot.signal(());
}

/// The P4's uptime, as a record carries it.
fn ms() -> u32 {
    // The low 32 bits; the reader unwraps them from `Opened`.
    now_ms() as u32
}

/// Copy one encoded record into the ring, evicting the oldest to fit, and wake the writer once
/// enough is queued.
fn push(shared: &Shared, record: &[u8]) {
    let log = &shared.sd_log;
    let queued = match log.ring.try_borrow_mut() {
        Ok(mut ring) => {
            ring.push(record);
            ring.len_bytes()
        }
        Err(_) => {
            lose(shared, 1);
            return;
        }
    };
    if queued >= URGE_BYTES {
        log.urge.signal(());
    }
}

/// Log a `kind` record of `frame`, a link frame as `encode_frame` wrote it without its
/// terminator. The caller checks the setting.
///
/// Out of line, with its record buffer, so `Shared::push_host` holds only its own frame.
#[inline(never)]
pub fn record_frame(shared: &Shared, kind: Kind, frame: &[u8]) {
    let mut record = [0; RECORD_MAX];
    if let Some(len) = log::encode(kind, ms(), frame, &mut record) {
        push(shared, &record[..len]);
    }
}

/// Log a line the phone is sent, unless it is a sighting.
pub fn phone_line(shared: &Shared, class: Class, line: &[u8]) {
    if on(shared) && !SIGHTING_LINES.iter().any(|prefix| line.starts_with(prefix)) {
        phone_line_record(shared, class, line);
    }
}

#[inline(never)]
fn phone_line_record(shared: &Shared, class: Class, line: &[u8]) {
    // The text alone: the record ends it.
    let text = line.strip_suffix(b"\n").unwrap_or(line);
    let mut record = [0; RECORD_MAX];
    if let Some(len) = log::encode_phone_line(class, text, ms(), &mut record) {
        push(shared, &record[..len]);
    }
}

/// Log a command a phone wrote.
pub fn phone_command(shared: &Shared, text: &[u8]) {
    if on(shared) {
        phone_command_record(shared, text);
    }
}

#[inline(never)]
fn phone_command_record(shared: &Shared, text: &[u8]) {
    let mut record = [0; RECORD_MAX];
    if let Some(len) = log::encode(Kind::PhoneCommand, ms(), text, &mut record) {
        push(shared, &record[..len]);
    }
}

/// Log that `input` moved the host queue from `before` to `after`.
#[inline(never)]
pub fn owner(shared: &Shared, input: Input, before: Holder, after: Holder) {
    if !on(shared) {
        return;
    }
    // A header, three bytes, a CRC and COBS's overhead.
    let mut record = [0; 32];
    if let Some(len) = log::encode_owner(input, before, after, ms(), &mut record) {
        push(shared, &record[..len]);
    }
}

/// Encode the `Opened` record of file `file`, this boot's `part`th, on a card of `card_mb` with
/// `free_mb` free, into `out`. Returns its length, or `None` when `out` is too short.
#[inline(never)]
pub fn encode_opened(
    shared: &Shared,
    file: u32,
    part: u16,
    card_mb: u32,
    free_mb: Option<u32>,
    out: &mut [u8],
) -> Option<usize> {
    let boot = &shared.sd_log.boot;
    let opened = Opened {
        version: FORMAT_VERSION,
        file,
        part,
        opened_ms: now_ms(),
        fw_version: boot.fw_version.clone(),
        reset_cause: boot.cause,
        last_phase: boot.last_phase,
        slot: ShortStr::try_from(crate::update::slot(shared)).unwrap_or_default(),
        trial: crate::update::on_trial(shared),
        image: boot.image,
        card_mb,
        free_mb,
    };
    log::encode_opened(&opened, ms(), out)
}

/// Log a `Status` snapshot every [`SNAPSHOT_EVERY`], and when one is asked for, while the setting
/// is on.
pub async fn run(shared: &Shared) -> ! {
    loop {
        select(Timer::after(SNAPSHOT_EVERY), shared.sd_log.snapshot.wait()).await;
        if on(shared) {
            snapshot(shared);
        }
    }
}

/// Log the `Status` a poll would get, from the counters last received.
///
/// Out of line, with the frame, so [`run`]'s future holds neither.
#[inline(never)]
fn snapshot(shared: &Shared) {
    let status = shared.status_msg();
    let mut frame = [0; MAX_FRAME];
    // Every `BridgeToHost` fits `MAX_FRAME`.
    let Ok(len) = encode_frame(&status, &mut frame) else { return };
    // The frame ends in its terminator, which the record leaves out.
    record_frame(shared, Kind::Snapshot, &frame[..len - 1]);
}
