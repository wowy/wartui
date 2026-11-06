//! The wartui-carrier: which one the P4 sits on, and its nodes' power, through the carrier's
//! I/O expander at 0x20 on I2C0 (`docs/node-bus.md` § "On the carrier board").
//!
//! **The board is read before the node buses are built**, because it picks the CS pins and
//! IRQ's polarity. [`detect`] runs blocking on the bus `main` brought up for the GPS. No answer
//! at the expander's address is the bench. Otherwise `wartui_proto::carrier` decides from the
//! registers, and [`detect`] does the I2C around it, each transaction bounded by
//! [`TRANSACTION_TIMEOUT_MS`]:
//!
//! - **warm**, a P4 reset under running nodes: nodes on stay on, and any with its output low is
//!   powered;
//! - **cold**: every port an input, the outputs low, then only the node ports outputs, and every
//!   node powered;
//! - **broken**, inputs naming no carrier: an `Error`, nothing powered and no slot driven.
//!
//! An expander that answers and then fails is an `Error` too, and leaves power control lost:
//! every power-cycle answers `Failed`. The board is then the one the registers named, or, when
//! none had, the fourteen with no slot driven, which broken also reports. A fault goes with
//! every announcement, as the GPS fault does, and so does an `Info` line naming a carrier that
//! came up and what it powered.
//!
//! **[`run`] powers the nodes one at a time, [`STAGGER`] apart**, in slot order A1, B1, A2, B2,
//! so their inrush never adds up. Then it serves power-cycle requests forever, one at a time:
//! output low, [`POWER_OFF`], output high, and `PowerCycled` once the node is powered again.
//! Requests wait in a bitmask in [`Power`], so a request for a slot already waiting or cycling
//! joins that cycle and is answered once. The host reader answers at once what needs no
//! expander: `NoCarrier` on the bench, `NotDriven` for a slot the board does not drive, and
//! `Failed` once control is lost.
//!
//! **I2C0 is shared with the GPS reader.** [`run`] locks the bus for each transaction and never
//! across a timer, so the receiver's polls go on between the steps of a power-up or a cycle.

use core::cell::Cell;
use core::fmt::Write;
use core::future::pending;

use embassy_sync::blocking_mutex::raw::NoopRawMutex;
use embassy_sync::signal::Signal;
use embassy_time::{Duration, Timer, with_timeout};
use esp_hal::Blocking;
use esp_hal::i2c::master::{Error, I2c, SoftwareTimeout};
use wartui_firmware::gps::bus_config;
use wartui_proto::bus::{BUSES, NodeBoard, Slot};
use wartui_proto::carrier::{
    ADDRESS, ALL_INPUTS, Decision, REG_CONFIG, REG_INPUT, REG_OUTPUT, decide, port, ports,
    warm_board,
};
use wartui_proto::link::{BridgeToHost, LogStr, PowerOutcome};

use crate::{I2cBus, Shared};

/// The gap between powering one node and the next.
pub const STAGGER: Duration = Duration::from_millis(200);
/// How long a power-cycle holds a node off.
pub const POWER_OFF: Duration = Duration::from_secs(1);
/// The longest one expander transaction may take. Three bytes at 400 kHz take well under a
/// millisecond.
pub const TRANSACTION_TIMEOUT_MS: u64 = 10;
/// Tries at switching a node back on before a cycle gives up and answers `Failed`.
const ON_TRIES: u8 = 3;

/// The fault when the expander answered and then failed at boot.
const LOST: &str = "the carrier's I/O expander stopped answering, so node power is out of reach";
/// The fault when the expander's inputs name no carrier.
const BROKEN: &str =
    "the carrier's I/O expander reads as no known carrier, so no node is powered or driven";
/// The fault when a node could not be powered at boot.
const UNPOWERED: &str = "the carrier's I/O expander did not answer, so a node may be unpowered";

/// What [`detect`] found.
pub struct Carrier {
    /// What `Ready` reports.
    board: NodeBoard,
    /// Slots per bus the node buses drive.
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
    /// No carrier: the bench, or a bus that refused its configuration.
    pub const fn bench() -> Self {
        let board = NodeBoard::Bench;
        Self { board, per_bus: board.per_bus(), output: 0, pending: 0, control: false, fault: None }
    }

    /// A carrier that came up, with `output` in its registers and `pending` still to power.
    const fn up(board: NodeBoard, output: u16, pending: u16) -> Self {
        Self { board, per_bus: board.per_bus(), output, pending, control: true, fault: None }
    }

    /// A carrier whose power is out of reach, driving `board`'s slots, or none when the
    /// registers named no carrier.
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
        return Carrier::bench();
    }
    let found = read_expander(i2c);
    // The shared bus's users bound their own transactions.
    let _ = i2c.apply_config(&bus_config());
    found
}

fn read_expander(i2c: &mut I2c<'_, Blocking>) -> Carrier {
    let Ok(config) = read16(i2c, REG_CONFIG) else { return Carrier::bench() };
    let warm = warm_board(config);
    let Ok(output) = read16(i2c, REG_OUTPUT) else { return Carrier::lost(warm, LOST) };
    if let Some(board) = warm {
        // The inputs are not read on this path.
        return Carrier::up(board, output, decide(config, output, 0).power());
    }
    // Every port an input first, which turns every node off, so the inputs name the carrier.
    if config != ALL_INPUTS && write16(i2c, REG_CONFIG, ALL_INPUTS).is_err() {
        return Carrier::lost(None, LOST);
    }
    let Ok(input) = read16(i2c, REG_INPUT) else { return Carrier::lost(None, LOST) };
    match decide(ALL_INPUTS, output, input) {
        Decision::Cold { board } => {
            // Low before they become outputs, so no node switches on early.
            if write16(i2c, REG_OUTPUT, 0).is_err()
                || write16(i2c, REG_CONFIG, !ports(board)).is_err()
            {
                return Carrier::lost(Some(board), LOST);
            }
            Carrier::up(board, 0, ports(board))
        }
        Decision::Broken | Decision::Warm { .. } => Carrier::lost(None, BROKEN),
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
    /// Raised by each request, so a waiting [`run`] looks again.
    wake: Signal<NoopRawMutex, ()>,
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
            wake: Signal::new(),
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

#[inline(never)]
fn answer(shared: &Shared, slot: Slot, outcome: PowerOutcome) {
    shared.push_host(&BridgeToHost::PowerCycled { slot, outcome });
}

/// What the carrier has to tell a host: its fault, or the line naming it. Nothing on the bench.
pub fn say(shared: &Shared) {
    let power = &shared.power;
    if let Some(fault) = power.fault.get() {
        shared.push_error(fault);
    }
    if let Some((powered, on)) = power.boot {
        let mut line = LogStr::new();
        let _ = write!(
            line,
            "{}: {} node slots, {powered} powered now, {on} already on",
            power.board,
            power.board.slot_count()
        );
        shared.push_log(&line);
    }
}

/// Power the nodes [`detect`] left to power, then serve power-cycle requests forever. Waits
/// forever on the bench, or with power out of reach.
pub async fn run(carrier: Carrier, i2c: Option<&'static I2cBus>, shared: &Shared) -> ! {
    let Some(i2c) = i2c.filter(|_| carrier.control) else { pending().await };
    let mut output = carrier.output;
    let mut first = true;
    for index in 0..carrier.board.per_bus() {
        for bus in 0..BUSES {
            let Some(slot) = Slot::new(bus, index) else { continue };
            if carrier.pending & port(slot) == 0 {
                continue;
            }
            if !first {
                Timer::after(STAGGER).await;
            }
            first = false;
            output |= port(slot);
            if !write_output(i2c, output).await {
                shared.power.fault.set(Some(UNPOWERED));
                shared.push_error(UNPOWERED);
            }
        }
    }
    let power = &shared.power;
    loop {
        let requests = power.requests.get();
        let Some(slot) = Slot::from_u8(requests.trailing_zeros() as u8) else {
            power.wake.wait().await;
            continue;
        };
        let outcome = cycle(i2c, &mut output, slot).await;
        power.requests.set(power.requests.get() & !request_bit(slot));
        answer(shared, slot, outcome);
    }
}

/// Switch `slot`'s node off, then on. `output` keeps the registers' wanted state, so a node a
/// failed write left off is powered by the next write that lands.
async fn cycle(i2c: &I2cBus, output: &mut u16, slot: Slot) -> PowerOutcome {
    let bit = port(slot);
    if !write_output(i2c, *output & !bit).await {
        return PowerOutcome::Failed;
    }
    Timer::after(POWER_OFF).await;
    for _ in 0..ON_TRIES {
        if write_output(i2c, *output).await {
            return PowerOutcome::Done;
        }
        Timer::after(STAGGER).await;
    }
    PowerOutcome::Failed
}

/// Write both output registers, saying whether the expander took them in time.
async fn write_output(i2c: &I2cBus, output: u16) -> bool {
    let [low, high] = output.to_le_bytes();
    let bytes = [REG_OUTPUT, low, high];
    let mut i2c = i2c.lock().await;
    let write = i2c.write_async(ADDRESS, &bytes);
    matches!(with_timeout(Duration::from_millis(TRANSACTION_TIMEOUT_MS), write).await, Ok(Ok(())))
}
