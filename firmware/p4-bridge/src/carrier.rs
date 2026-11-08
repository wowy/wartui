//! The wartui-carrier: which one the P4 sits on, and its nodes' power, through the carrier's
//! I/O expander at 0x20 on I2C0 (`docs/node-bus.md` § "On the carrier board").
//!
//! **The board is read before the node buses are built**, because it picks the CS pins they
//! drive. [`detect`] runs blocking on the bus `main` brought up for the GPS. No answer at the
//! expander's address is no carrier: an `Error`, and no slot driven. Otherwise
//! `wartui_proto::carrier` decides from the registers, and [`detect`] does the I2C around it,
//! each transaction bounded by [`TRANSACTION_TIMEOUT_MS`]:
//!
//! - **warm**, a P4 reset under running nodes: nodes on stay on, and any with its output low is
//!   powered;
//! - **cold**: every port an input, the outputs low, then only the node ports outputs, and every
//!   node powered;
//! - **broken**, inputs naming no carrier: an `Error`, nothing powered and no slot driven.
//!
//! An expander that answers and then fails is an `Error` too, and leaves power control lost:
//! a power-cycle answers `Failed`. The board is then the one the registers named, or, when none
//! had, the fourteen with no slot driven, which broken also reports: a carrier is there, so its
//! fan runs. A fault goes with every announcement, as the GPS fault does, and so does a `Log`
//! line naming a carrier that came up and what it powered at boot.
//!
//! **[`run`] powers the nodes one at a time, 100 ms apart**, in slot order A1, B1, A2, B2,
//! so their inrush never adds up. Then it serves power-cycle requests forever, one at a time:
//! output low, [`POWER_OFF`], output high, and `PowerCycled` once the node is powered again.
//! Requests wait in a bitmask in [`Power`], so a request for a slot already waiting or cycling
//! joins that cycle and is answered once. The host reader answers at once what needs no
//! expander: `NoCarrier` with no carrier, `NotDriven` for a slot the board does not drive, and
//! `Failed` once control is lost.
//!
//! **After a lost node supply, [`run`] staggers the nodes again.** The P4 stays up on its own
//! USB-C while the carrier's node supply is out, and the expander, on the P4's 3V3, keeps every
//! node port high, so a returning supply would start every node at once. The P4 cannot sense
//! the supply. Its sign is every present node dropping at once, two or more, which
//! `wartui_proto::carrier::DropWatch` decides from each presence change. One node alone has no
//! inrush to stagger, and its drop looks like a crash or a node pulled from its socket.
//!
//! [`run`] then writes every node port low, between requests, and switches on one slot at a
//! time, the scout, for up to [`SCOUT_WAIT`]: first the slots that were present, then every
//! other slot the board drives, round and round. Only one node's inrush meets a supply that has
//! just returned, and a node seated in any socket ends the wait. A scout that answers means the
//! supply is back, and the other ports are powered [`STAGGER`] apart. A power-cycle request
//! ends the wait the same way, so no request waits on the supply to return.
//!
//! **A power-up into a dead supply is a lost supply too.** The P4 may boot before the node
//! supply, or a request may end the scouting while the supply is still out. Every port is then
//! raised into nothing, the supply starts every node at once, and with no node present
//! `DropWatch` never fires. So after a stagger that raises every port, [`run`] waits up to
//! [`SCOUT_WAIT`] for any node to be present. A request is served at once, also during that
//! wait, and the wait starts again after it, so no request waits out the check. Without a node,
//! the supply is taken as out, and [`run`] scouts again: after a lost supply, the slots that were
//! present first. A board with no node seated scouts forever, and a node seated later is powered
//! when the scout reaches its socket.
//!
//! A node powered over its own USB stays present while the node supply is out, so the P4 takes
//! the supply as on: a lost supply is not seen, and a returning one starts the other nodes
//! together.
//!
//! **I2C0 is shared with the GPS reader.** [`run`] locks the bus for each transaction and never
//! across a timer, so the receiver's polls go on between the steps of a power-up or a cycle.

use core::cell::Cell;
use core::fmt::{self, Write};
use core::future::pending;

use embassy_futures::select::select;
use embassy_sync::blocking_mutex::raw::NoopRawMutex;
use embassy_sync::signal::Signal;
use embassy_time::{Duration, Timer, with_timeout};
use esp_hal::Blocking;
use esp_hal::i2c::master::{Error, I2c, SoftwareTimeout};
use wartui_firmware::gps::bus_config;
use wartui_proto::bus::{BUSES, NodeBoard, Slot};
use wartui_proto::carrier::{
    ADDRESS, ALL_INPUTS, Decision, DropWatch, REG_CONFIG, REG_INPUT, REG_OUTPUT, decide, port,
    ports, warm_board,
};
use wartui_proto::link::{BridgeToHost, LogStr, PowerOutcome};

use crate::{I2cBus, Shared, now_ms};

/// The gap between powering one node and the next.
pub const STAGGER: Duration = Duration::from_millis(100);
/// The gap between a cycle's tries at switching its node back on.
const ON_RETRY: Duration = Duration::from_millis(200);
/// How long a scout has to answer after a lost supply, and any node after a stagger that raised
/// every port. A node boots in under a second, and an absent slot is probed every 2 s.
const SCOUT_WAIT: Duration = Duration::from_secs(5);
/// How long a power-cycle holds a node off.
pub const POWER_OFF: Duration = Duration::from_secs(1);
/// The longest one expander transaction may take. Three bytes at 400 kHz take well under a
/// millisecond.
pub const TRANSACTION_TIMEOUT_MS: u64 = 10;
/// Tries at switching a node back on before a cycle gives up and answers `Failed`.
const ON_TRIES: u8 = 3;
/// Tries at each expander transaction at boot, so one glitch neither hides a carrier nor loses
/// its power control. With no carrier, each of the first reads is NACKed at once, so they cost
/// little.
const DETECT_TRIES: u8 = 3;

/// The fault when the expander answered and then failed at boot.
const LOST: &str = "the carrier's I/O expander stopped answering, so node power is out of reach";
/// The fault when nothing answers at the expander's address.
const NONE: &str = "no wartui-carrier answered at 0x20, so no node is driven";
/// The fault when I2C0 refused its configuration, so the expander was never asked.
const UNREAD: &str =
    "I2C0 refused its configuration, so the carrier was not read and no node is driven";
/// The fault when the expander's inputs name no carrier.
const BROKEN: &str =
    "the carrier's I/O expander reads as no known carrier, so no node is powered or driven";
/// The fault while a failed write may have left a node off. The next write of the wanted state
/// that lands clears it.
const UNPOWERED: &str = "the carrier's I/O expander did not answer, so a node may be unpowered";

/// What [`detect`] found.
pub struct Carrier {
    /// What `Ready` reports.
    board: NodeBoard,
    /// Slots per bus the node buses drive: the board's, or none when its power is out of reach
    /// and the registers named no board.
    per_bus: u8,
    /// The output registers as last read or written.
    output: u16,
    /// Node ports to power, staggered.
    pending: u16,
    /// Whether the expander takes power requests.
    control: bool,
    /// Why not, or why nothing is driven.
    fault: Option<&'static str>,
}

impl Carrier {
    /// No carrier: nothing answered at the expander's address.
    const fn none() -> Self {
        let board = NodeBoard::NoCarrier;
        Self { board, per_bus: 0, output: 0, pending: 0, control: false, fault: Some(NONE) }
    }

    /// No carrier read, on a bus that refused its configuration.
    pub const fn unread() -> Self {
        Self { fault: Some(UNREAD), ..Self::none() }
    }

    /// A carrier that came up, with `output` in its registers and `pending` still to power.
    const fn up(board: NodeBoard, output: u16, pending: u16) -> Self {
        Self { board, per_bus: board.per_bus(), output, pending, control: true, fault: None }
    }

    /// A carrier whose power is out of reach, driving `board`'s slots, or, when the registers
    /// named no board, reported as the fourteen and driving none.
    const fn lost(board: Option<NodeBoard>, fault: &'static str) -> Self {
        let (board, per_bus) = match board {
            Some(board) => (board, board.per_bus()),
            None => (NodeBoard::Carrier14, 0),
        };
        Self { board, per_bus, output: 0, pending: 0, control: false, fault: Some(fault) }
    }

    /// The board, as `Ready` reports it.
    pub const fn board(&self) -> NodeBoard {
        self.board
    }

    /// Slots per bus the node buses drive.
    pub const fn per_bus(&self) -> u8 {
        self.per_bus
    }
}

/// Read the expander on `i2c`, set it up, and say what to power. Called once from `main`,
/// before the node buses are built.
///
/// Out of line, so its frame is not `main`'s.
#[inline(never)]
pub fn detect(i2c: &mut I2c<'_, Blocking>) -> Carrier {
    let bounded = bus_config().with_software_timeout(SoftwareTimeout::Transaction(
        esp_hal::time::Duration::from_millis(TRANSACTION_TIMEOUT_MS),
    ));
    if i2c.apply_config(&bounded).is_err() {
        return Carrier::unread();
    }
    let found = read_expander(i2c);
    // The shared bus's users bound their own transactions.
    let _ = i2c.apply_config(&bus_config());
    found
}

/// The first read decides whether a carrier is there.
fn read_expander(i2c: &mut I2c<'_, Blocking>) -> Carrier {
    let Ok(config) = tried(|| read16(i2c, REG_CONFIG)) else { return Carrier::none() };
    let warm = warm_board(config);
    let Ok(output) = tried(|| read16(i2c, REG_OUTPUT)) else { return Carrier::lost(warm, LOST) };
    if let Some(board) = warm {
        // The inputs are not read on this path.
        return Carrier::up(board, output, decide(config, output, 0).power());
    }
    // Every port an input first, which turns every node off, so the inputs name the carrier.
    if config != ALL_INPUTS && tried(|| write16(i2c, REG_CONFIG, ALL_INPUTS)).is_err() {
        return Carrier::lost(None, LOST);
    }
    let Ok(input) = tried(|| read16(i2c, REG_INPUT)) else { return Carrier::lost(None, LOST) };
    match decide(ALL_INPUTS, output, input) {
        Decision::Cold { board } => {
            // Low before they become outputs, so no node switches on early.
            if tried(|| write16(i2c, REG_OUTPUT, 0)).is_err()
                || tried(|| write16(i2c, REG_CONFIG, !ports(board))).is_err()
            {
                return Carrier::lost(Some(board), LOST);
            }
            Carrier::up(board, 0, ports(board))
        }
        Decision::Broken | Decision::Warm { .. } => Carrier::lost(None, BROKEN),
    }
}

/// `transaction`, tried up to [`DETECT_TRIES`] times until it lands.
fn tried<T>(mut transaction: impl FnMut() -> Result<T, Error>) -> Result<T, Error> {
    let mut tries = 1;
    loop {
        match transaction() {
            Err(_) if tries < DETECT_TRIES => tries += 1,
            result => return result,
        }
    }
}

/// A register pair, port 0 then port 1, as one value.
fn read16(i2c: &mut I2c<'_, Blocking>, reg: u8) -> Result<u16, Error> {
    let mut pair = [0; 2];
    i2c.write_read(ADDRESS, &[reg], &mut pair)?;
    Ok(u16::from_le_bytes(pair))
}

fn write16(i2c: &mut I2c<'_, Blocking>, reg: u8, value: u16) -> Result<(), Error> {
    let [low, high] = value.to_le_bytes();
    i2c.write(ADDRESS, &[reg, low, high])
}

/// The carrier's state as the host reader and [`run`] share it.
pub struct Power {
    board: NodeBoard,
    control: bool,
    /// Bit `n` is set while slot byte `n` waits for a cycle or is in one.
    requests: Cell<u16>,
    /// The slot in a cycle now, by the same bits.
    cycling: Cell<u16>,
    /// Raised by each request and each lost supply, so a waiting [`run`] looks again.
    wake: Signal<NoopRawMutex, ()>,
    /// Watches presence for a lost supply.
    watch: Cell<DropWatch>,
    /// The slots present before every node dropped, while [`run`] has yet to recover them.
    lost: Cell<u16>,
    /// Raised by each presence change, so a waiting scout looks again.
    presence: Signal<NoopRawMutex, ()>,
    /// Said with every announcement while set.
    fault: Cell<Option<&'static str>>,
    /// The nodes a carrier that came up powers at boot, and those it found on.
    boot: Option<(u8, u8)>,
}

impl Power {
    /// The state `carrier` starts with.
    pub fn new(carrier: &Carrier) -> Self {
        let boot = carrier.control.then(|| {
            let powered = carrier.pending.count_ones() as u8;
            (powered, carrier.board.slot_count() - powered)
        });
        Self {
            board: carrier.board,
            control: carrier.control,
            requests: Cell::new(0),
            cycling: Cell::new(0),
            wake: Signal::new(),
            watch: Cell::new(DropWatch::new()),
            lost: Cell::new(0),
            presence: Signal::new(),
            fault: Cell::new(carrier.fault),
            boot,
        }
    }
}

fn request_bit(slot: Slot) -> u16 {
    1 << slot.as_u8()
}

/// Take the host's request to power-cycle `slot`: queue it for [`run`], or answer at once.
pub fn request(shared: &Shared, slot: Slot) {
    let power = &shared.power;
    let refused = if !power.board.is_carrier() {
        Some(PowerOutcome::NoCarrier)
    } else if !power.board.drives(slot) {
        Some(PowerOutcome::NotDriven)
    } else if !power.control {
        Some(PowerOutcome::Failed)
    } else {
        None
    };
    match refused {
        Some(outcome) => answer(shared, slot, outcome),
        None => {
            power.requests.set(power.requests.get() | request_bit(slot));
            power.wake.signal(());
        }
    }
}

/// Note that the slots in `present` hold a node, a mask by slot byte, and have [`run`] recover
/// them when every node dropped at once. Nothing without power control.
pub fn presence(shared: &Shared, present: u32) {
    let power = &shared.power;
    if !power.control {
        return;
    }
    // The expander's masks are 16 bits, and a carrier's slot bytes all fit in them.
    let present = present as u16;
    let mut watch = power.watch.get();
    if let Some(lost) = watch.note(present, power.cycling.get(), now_ms()) {
        power.lost.set(power.lost.get() | lost);
        power.wake.signal(());
    }
    power.watch.set(watch);
    power.presence.signal(());
}

#[inline(never)]
fn answer(shared: &Shared, slot: Slot, outcome: PowerOutcome) {
    shared.push_host(&BridgeToHost::PowerCycled { slot, outcome });
}

/// What the carrier has to tell a host: its fault, or the line naming it.
pub fn say(shared: &Shared) {
    let power = &shared.power;
    if let Some(fault) = power.fault.get() {
        shared.push_error(fault);
    }
    if let Some((powered, on)) = power.boot {
        let mut line = LogStr::new();
        let _ = write!(
            line,
            "{}: {} node slots; at boot {powered} powered, {on} already on",
            power.board,
            power.board.slot_count()
        );
        shared.push_log(&line);
    }
}

/// Send `args` to the host as a `Log` line.
fn log(shared: &Shared, args: fmt::Arguments<'_>) {
    let mut line = LogStr::new();
    let _ = line.write_fmt(args);
    shared.push_log(&line);
}

/// `board`'s slots in the order they are powered: A1, B1, A2, B2.
fn stagger(board: NodeBoard) -> impl Iterator<Item = Slot> + Clone {
    (0..board.per_bus()).flat_map(|index| (0..BUSES).filter_map(move |bus| Slot::new(bus, index)))
}

/// Power the nodes [`detect`] left to power, then serve power-cycle requests forever, and
/// recover a lost supply between them. Waits forever with no carrier, or with power out of reach.
pub async fn run(carrier: Carrier, i2c: Option<&'static I2cBus>, shared: &Shared) -> ! {
    let Some(i2c) = i2c.filter(|_| carrier.control) else { pending().await };
    let board = carrier.board;
    let mut output = carrier.output;
    let mut first = true;
    for slot in stagger(board).filter(|&slot| carrier.pending & port(slot) != 0) {
        if !first {
            Timer::after(STAGGER).await;
        }
        first = false;
        output |= port(slot);
        let landed = write_output(i2c, output).await;
        note_powered(shared, landed);
    }
    let power = &shared.power;
    let mut supply = if carrier.pending != 0 { Supply::Unchecked(0) } else { Supply::On };
    loop {
        let lost = power.lost.take();
        if lost != 0 {
            let why = "every node dropped at once, so the node supply was likely lost";
            log(shared, format_args!("{board}: {why}"));
            supply = recover(i2c, shared, board, output, lost).await;
            continue;
        }
        let requests = power.requests.get();
        if let Some(slot) = Slot::from_u8(requests.trailing_zeros() as u8) {
            power.cycling.set(request_bit(slot));
            let outcome = cycle(i2c, shared, output, slot).await;
            let mut watch = power.watch.get();
            watch.cycled(request_bit(slot), now_ms());
            power.watch.set(watch);
            power.cycling.set(0);
            power.requests.set(power.requests.get() & !request_bit(slot));
            answer(shared, slot, outcome);
            continue;
        }
        // Only once no request waits: a waiting one would end the scouting or the check at once.
        // A fresh lost supply above replaces the slots carried here.
        supply = match supply {
            Supply::Unchecked(lost) => answered(shared, board, lost).await,
            Supply::Out(lost) => recover(i2c, shared, board, output, lost).await,
            Supply::On => {
                power.wake.wait().await;
                Supply::On
            }
        };
    }
}

/// What [`run`] takes the node supply to be. `Unchecked` and `Out` carry the slots to scout
/// first, by request bit: those present before a lost supply, or none.
#[derive(Clone, Copy)]
enum Supply {
    /// On, as far as [`run`] knows: a node answered after the last power-up that raised every
    /// port, or none has run.
    On,
    /// Unknown: every port was raised and no node has answered yet, so [`answered`] decides.
    Unchecked(u16),
    /// Out: [`recover`] scouts.
    Out(u16),
}

/// Whether a node is present within [`SCOUT_WAIT`], after a power-up that raised every port.
/// A power-cycle request ends the wait unchecked, so [`run`] serves it and checks again after.
/// Neither within the wait takes the node supply as out, and that is logged. `lost` is carried
/// through.
async fn answered(shared: &Shared, board: NodeBoard, lost: u16) -> Supply {
    let power = &shared.power;
    let wait = async {
        loop {
            if shared.nodes.present() != 0 {
                return Supply::On;
            }
            if power.requests.get() != 0 {
                return Supply::Unchecked(lost);
            }
            // A stale signal only costs another look.
            select(power.presence.wait(), power.wake.wait()).await;
        }
    };
    if let Ok(supply) = with_timeout(SCOUT_WAIT, wait).await {
        return supply;
    }
    let why = "no node answered after power-up, so the node supply may be out";
    log(shared, format_args!("{board}: {why}"));
    Supply::Out(lost)
}

/// Switch `slot`'s node off, then back to `output`, the registers' wanted state, which also
/// powers any node a failed write left off.
async fn cycle(i2c: &I2cBus, shared: &Shared, output: u16, slot: Slot) -> PowerOutcome {
    let bit = port(slot);
    if !write_output(i2c, output & !bit).await {
        return PowerOutcome::Failed;
    }
    Timer::after(POWER_OFF).await;
    for _ in 0..ON_TRIES {
        if write_output(i2c, output).await {
            note_powered(shared, true);
            return PowerOutcome::Done;
        }
        Timer::after(ON_RETRY).await;
    }
    note_powered(shared, false);
    PowerOutcome::Failed
}

/// Power `board`'s nodes again with the node supply taken as out: one scout at a time, the
/// slots in `lost` first, then the rest the board drives, until one answers or a power-cycle
/// request waits. Then the rest of `output`, the wanted state, [`STAGGER`] apart. A scout that
/// answered shows the supply on. After a request it is unchecked, and `lost` is carried, so a
/// later scouting starts at the same slots.
async fn recover(
    i2c: &I2cBus,
    shared: &Shared,
    board: NodeBoard,
    output: u16,
    lost: u16,
) -> Supply {
    let off = output & !ports(board);
    // A write that fails leaves nodes on, and the scout's write settles them.
    let _ = write_output(i2c, off).await;
    let was_lost = move |slot: &Slot| lost & request_bit(*slot) != 0;
    let scouts =
        stagger(board).filter(was_lost).chain(stagger(board).filter(move |s| !was_lost(s)));
    let (scout, ended) = 'found: loop {
        for slot in scouts.clone() {
            match try_scout(i2c, shared, off, slot).await {
                Scouted::Silent => {}
                ended => break 'found (slot, ended),
            }
        }
    };
    let mut on = off | port(scout);
    let mut powered = 1;
    for slot in stagger(board).filter(|&slot| slot != scout && output & port(slot) != 0) {
        Timer::after(STAGGER).await;
        on |= port(slot);
        note_powered(shared, write_output(i2c, on).await);
        powered += 1;
    }
    let label = board.label(scout);
    let why = match ended {
        Scouted::Answered => format_args!("{label} answered, so the node supply is back"),
        _ => format_args!("a power-cycle request ended the wait"),
    };
    log(shared, format_args!("{board}: {why}; {powered} node slots powered again"));
    // After a request, the supply may still be out, and `run` checks once it has served it.
    if ended == Scouted::Answered { Supply::On } else { Supply::Unchecked(lost) }
}

/// How one scout's wait ended.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Scouted {
    /// Its node answered.
    Answered,
    /// A power-cycle request is waiting.
    Requested,
    /// Neither within [`SCOUT_WAIT`], so it is off again.
    Silent,
}

/// Switch on `slot`'s node over `off`, and wait up to [`SCOUT_WAIT`] for it to answer or a
/// power-cycle request to arrive. Without either, it is switched off again.
async fn try_scout(i2c: &I2cBus, shared: &Shared, off: u16, slot: Slot) -> Scouted {
    let power = &shared.power;
    power.presence.reset();
    note_powered(shared, write_output(i2c, off | port(slot)).await);
    let bit = request_bit(slot);
    let wait = async {
        loop {
            if shared.nodes.present() & u32::from(bit) != 0 {
                return Scouted::Answered;
            }
            if power.requests.get() != 0 {
                return Scouted::Requested;
            }
            // `run` reads the requests and the lost slots themselves, not the wake.
            select(power.presence.wait(), power.wake.wait()).await;
        }
    };
    let ended = with_timeout(SCOUT_WAIT, wait).await.unwrap_or(Scouted::Silent);
    if ended == Scouted::Silent {
        let _ = write_output(i2c, off).await;
    }
    ended
}

/// Note whether a write of the wanted state landed. One that did powers every node a failed
/// write left off, so it clears [`UNPOWERED`], and only that fault. One that did not may leave
/// a node off.
fn note_powered(shared: &Shared, landed: bool) {
    let fault = &shared.power.fault;
    if !landed {
        fault.set(Some(UNPOWERED));
        shared.push_error(UNPOWERED);
    } else if fault.get() == Some(UNPOWERED) {
        fault.set(None);
    }
}

/// Write both output registers, saying whether the expander took them in time.
async fn write_output(i2c: &I2cBus, output: u16) -> bool {
    let [low, high] = output.to_le_bytes();
    let bytes = [REG_OUTPUT, low, high];
    let mut i2c = i2c.lock().await;
    let write = i2c.write_async(ADDRESS, &bytes);
    matches!(with_timeout(Duration::from_millis(TRANSACTION_TIMEOUT_MS), write).await, Ok(Ok(())))
}
