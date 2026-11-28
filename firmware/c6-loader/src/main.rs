//! The C6 loader: lands `firmware/p4-radio` on a factory P4-WIFI6's C6 from ESP-Hosted's OTA slot.
//!
//! - **It plans from the table it boots under.** The factory table and `otadata` say which slot
//!   holds the loader, and `wartui_proto::first_flash::plan` refuses any write of ours that lands
//!   on it. All reads and checks come before the first write.
//! - **A refusal undoes the switch.** Erasing the newest `otadata` entry boots ESP-Hosted again,
//!   so a C6 this loader cannot set up is left as it shipped. A damaged embedded image is undone
//!   the same way.
//! - **The writes follow #395's order.** Until the last step erases `otadata`, the factory table
//!   and `otadata` still boot the loader, so a power cut restarts it. Only a cut while the
//!   bootloader or the table is written needs the header.
//! - **Every write copies through RAM.** The embedded images live in flash, and the ROM's write
//!   routine must not read flash while it programs it.
//! - **`failed` repeats and never resets.** The P4 opens its UART only after resetting the C6, and
//!   a reset would rerun a flow that has already refused.

#![no_std]
#![no_main]

use core::fmt::{self, Write as _};

use esp_hal::Blocking;
use esp_hal::delay::Delay;
use esp_hal::main;
use esp_hal::uart::{Config, Uart};
use esp_storage::{FlashStorage, FlashStorageError};
use sha2::{Digest, Sha256};
use wartui_proto::first_flash::{self, Plan, Refusal, SECTOR, Span, TABLE_AT, TABLE_LEN};

mod bundle {
    include!(concat!(env!("OUT_DIR"), "/bundle.rs"));
}

use bundle::{APP, APP_SHA256, BOOTLOADER, TABLE};

esp_bootloader_esp_idf::esp_app_desc!();

/// The `otadata` partition's type and subtype, as the bootloader finds it.
const DATA: u8 = 1;
const OTA: u8 = 0;
const SECTOR_LEN: usize = SECTOR as usize;

// The bundle's lengths are the spans `plan` erases for them.
const _: () = assert!(first_flash::BOOTLOADER.start == 0 && first_flash::BOOTLOADER.end == 0x8000);
const _: () = assert!(SECTOR == 0x1000);

/// Nothing reaches the UART from here, and a reset could rerun a half-done flow.
#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    loop {}
}

#[main]
fn main() -> ! {
    let p = esp_hal::init(esp_hal::Config::default());
    let uart = Uart::new(p.UART1, Config::default().with_baudrate(115_200))
        .expect("115200 8N1 suits UART1")
        .with_tx(p.GPIO20)
        .with_rx(p.GPIO18);
    let mut link = Link(uart);
    let mut flash = FlashStorage::new(p.FLASH);
    let delay = Delay::new();

    link.line(format_args!("start"));
    let failure = match run(&mut flash, &mut link) {
        Ok(done) => {
            link.line(format_args!("{done}"));
            delay.delay_millis(100);
            esp_hal::system::software_reset()
        }
        Err(failure) => failure,
    };
    loop {
        link.line(format_args!("failed {failure}"));
        delay.delay_millis(5000);
    }
}

/// The checks, then the rehearsal or the writes.
fn run(flash: &mut FlashStorage<'_>, link: &mut Link) -> Result<Done, Failure> {
    let factory = Factory::read(flash)?;
    let running = first_flash::booted(&factory.table, &factory.entries).ok_or(Failure::NoSlot)?;
    let app_len = u32::try_from(APP.len()).unwrap_or(u32::MAX);
    let plan = match first_flash::plan(TABLE, running, app_len) {
        Ok(plan) => plan,
        Err(refusal) => return Err(Failure::Refused(refusal, factory.undo(flash))),
    };
    let sha256: [u8; 32] = Sha256::digest(APP).into();
    if sha256 != APP_SHA256 {
        return Err(Failure::Damaged(factory.undo(flash)));
    }
    if cfg!(feature = "rehearse") {
        rehearse(flash, link, &plan, &factory)
    } else {
        land(flash, link, &plan)
    }
}

/// Report each write, then boot ESP-Hosted again.
fn rehearse(
    flash: &mut FlashStorage<'_>,
    link: &mut Link,
    plan: &Plan,
    factory: &Factory,
) -> Result<Done, Failure> {
    for step in steps(plan) {
        link.line(format_args!("step would {step}"));
    }
    let at = factory.newest().ok_or(Failure::NoEntry)?;
    let step = Step { text: "erase the otadata entry", at };
    link.line(format_args!("step {step}"));
    erase(flash, at).map_err(|fault| Failure::Step(step, fault))?;
    Ok(Done::Rehearsed)
}

/// #395 § "Order of writes", steps 1 to 6.
fn land(flash: &mut FlashStorage<'_>, link: &mut Link, plan: &Plan) -> Result<Done, Failure> {
    let [app, check, trial, bootloader, table, otadata] = steps(plan);
    let mut step =
        |step: Step, work: &mut dyn FnMut(&mut FlashStorage<'_>) -> Result<(), Fault>| {
            link.line(format_args!("step {step}"));
            work(flash).map_err(|fault| Failure::Step(step, fault))
        };
    step(app, &mut |flash| {
        erase(flash, app.at)?;
        write(flash, app.at.start, APP)
    })?;
    step(check, &mut |flash| {
        if hash(flash, app.at.start, APP.len())? == APP_SHA256 { Ok(()) } else { Err(Fault::Hash) }
    })?;
    step(trial, &mut |flash| erase(flash, trial.at))?;
    step(bootloader, &mut |flash| rewrite(flash, bootloader.at, BOOTLOADER))?;
    step(table, &mut |flash| rewrite(flash, table.at, TABLE))?;
    step(otadata, &mut |flash| erase(flash, otadata.at))?;
    Ok(Done::Landed)
}

/// The six steps, named as their `step` lines name them.
fn steps(plan: &Plan) -> [Step; 6] {
    [
        Step { text: "write the radio image", at: plan.app },
        Step { text: "check the radio image", at: plan.app },
        Step { text: "erase trial", at: plan.trial },
        Step { text: "write the bootloader", at: plan.bootloader },
        Step { text: "write the partition table", at: plan.table },
        Step { text: "erase otadata", at: plan.otadata },
    ]
}

/// What the factory table and its `otadata` hold at boot.
struct Factory {
    table: [u8; TABLE_LEN],
    /// Where `otadata` starts.
    otadata: u32,
    /// The first entry of each `otadata` sector.
    entries: [[u8; 32]; 2],
}

impl Factory {
    fn read(flash: &mut FlashStorage<'_>) -> Result<Self, Failure> {
        let mut table = [0; TABLE_LEN];
        let at = Span { start: TABLE_AT, end: TABLE_AT + TABLE_LEN as u32 };
        flash.read(TABLE_AT, &mut table).map_err(|e| {
            Failure::Step(Step { text: "read the partition table", at }, Fault::Read(e))
        })?;
        let otadata = first_flash::partitions(&table)
            .find(|p| p.kind == DATA && p.subtype == OTA)
            .ok_or(Failure::NoOtadata)?;
        let mut entries = [[0; 32]; 2];
        for (sector, entry) in (0..).zip(entries.iter_mut()) {
            flash.read(otadata.offset + sector * SECTOR, entry).map_err(|e| {
                Failure::Step(Step { text: "read otadata", at: otadata.span() }, Fault::Read(e))
            })?;
        }
        Ok(Factory { table, otadata: otadata.offset, entries })
    }

    /// The sector holding the entry that boots this loader.
    fn newest(&self) -> Option<Span> {
        let sector = u32::try_from(first_flash::newest(&self.entries)?).ok()?;
        let start = self.otadata + sector * SECTOR;
        Some(Span { start, end: start + SECTOR })
    }

    /// Erase that entry, so ESP-Hosted boots next.
    fn undo(&self, flash: &mut FlashStorage<'_>) -> Undo {
        match self.newest() {
            None => Undo::Nothing,
            Some(at) => match erase(flash, at) {
                Ok(()) => Undo::Selected,
                Err(fault) => Undo::Failed(at, fault),
            },
        }
    }
}

fn erase(flash: &mut FlashStorage<'_>, at: Span) -> Result<(), Fault> {
    flash.erase(at.start, at.end).map_err(Fault::Erase)
}

/// A sector of RAM, word-aligned as the ROM's write routine needs.
#[repr(align(4))]
struct Bounce([u8; SECTOR_LEN]);

/// Write `bytes` at `at` a sector at a time through RAM, padding the last word with 0xFF.
fn write(flash: &mut FlashStorage<'_>, at: u32, bytes: &[u8]) -> Result<(), Fault> {
    let mut bounce = Bounce([0xFF; SECTOR_LEN]);
    for (offset, chunk) in (at..).step_by(SECTOR_LEN).zip(bytes.chunks(SECTOR_LEN)) {
        bounce.0.fill(0xFF);
        bounce.0[..chunk.len()].copy_from_slice(chunk);
        let len = chunk.len().next_multiple_of(4);
        flash.write_nor(offset, &bounce.0[..len]).map_err(Fault::Write)?;
    }
    Ok(())
}

/// Read `bytes.len()` bytes back from `at` and compare.
fn verify(flash: &mut FlashStorage<'_>, at: u32, bytes: &[u8]) -> Result<(), Fault> {
    let mut back = [0; SECTOR_LEN];
    for (offset, chunk) in (at..).step_by(SECTOR_LEN).zip(bytes.chunks(SECTOR_LEN)) {
        let back = &mut back[..chunk.len()];
        flash.read(offset, back).map_err(Fault::Read)?;
        if back != chunk {
            return Err(Fault::Differs);
        }
    }
    Ok(())
}

/// Erase `at`, write `bytes` over it and read them back.
fn rewrite(flash: &mut FlashStorage<'_>, at: Span, bytes: &[u8]) -> Result<(), Fault> {
    erase(flash, at)?;
    write(flash, at.start, bytes)?;
    verify(flash, at.start, bytes)
}

/// SHA-256 of `len` bytes of flash from `at`, read a sector at a time.
fn hash(flash: &mut FlashStorage<'_>, at: u32, len: usize) -> Result<[u8; 32], Fault> {
    let mut sha = Sha256::new();
    let mut back = [0; SECTOR_LEN];
    let mut offset = at;
    let mut left = len;
    while left > 0 {
        let n = left.min(SECTOR_LEN);
        flash.read(offset, &mut back[..n]).map_err(Fault::Read)?;
        sha.update(&back[..n]);
        offset += SECTOR;
        left -= n;
    }
    Ok(sha.finalize().into())
}

/// The link UART: TX GPIO20 to the P4's GPIO14, 115200 8N1.
struct Link(Uart<'static, Blocking>);

impl Link {
    /// One `c6-loader: ` line, sent before this returns.
    fn line(&mut self, args: fmt::Arguments<'_>) {
        let _ = write!(self, "c6-loader: {args}\r\n");
        let _ = self.0.flush();
    }
}

impl fmt::Write for Link {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        let mut rest = s.as_bytes();
        while !rest.is_empty() {
            let sent = self.0.write(rest).map_err(|_| fmt::Error)?;
            rest = rest.get(sent..).unwrap_or_default();
        }
        Ok(())
    }
}

/// How the flow ended well.
enum Done {
    Landed,
    Rehearsed,
}

impl fmt::Display for Done {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Done::Landed => f.write_str("done"),
            Done::Rehearsed => f.write_str("done rehearsal"),
        }
    }
}

/// One step and the flash it touches: `write the bootloader at 0x0..0x8000`.
#[derive(Clone, Copy)]
struct Step {
    text: &'static str,
    at: Span,
}

impl fmt::Display for Step {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} at {}", self.text, self.at)
    }
}

/// What went wrong within a step.
enum Fault {
    Erase(FlashStorageError),
    Write(FlashStorageError),
    Read(FlashStorageError),
    Differs,
    Hash,
}

impl fmt::Display for Fault {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Fault::Erase(e) => write!(f, "the erase failed ({e:?})"),
            Fault::Write(e) => write!(f, "the write failed ({e:?})"),
            Fault::Read(e) => write!(f, "the read failed ({e:?})"),
            Fault::Differs => f.write_str("the read-back differs"),
            Fault::Hash => f.write_str("the read-back's SHA-256 differs"),
        }
    }
}

/// What erasing the newest `otadata` entry did.
enum Undo {
    Selected,
    Nothing,
    Failed(Span, Fault),
}

/// The tail of a `failed` line that undid the switch.
impl fmt::Display for Undo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Undo::Selected => f.write_str(", and ESP-Hosted is selected again"),
            Undo::Nothing => f.write_str(", and otadata holds no entry to erase"),
            Undo::Failed(at, fault) => write!(f, ", and erasing otadata at {at} failed: {fault}"),
        }
    }
}

/// The text after `c6-loader: failed `.
enum Failure {
    NoOtadata,
    NoSlot,
    NoEntry,
    Refused(Refusal, Undo),
    Damaged(Undo),
    Step(Step, Fault),
}

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Failure::NoOtadata => f.write_str("the running partition table has no otadata"),
            Failure::NoSlot => f.write_str("no app slot boots under this table"),
            Failure::NoEntry => f.write_str("otadata holds no entry to erase"),
            Failure::Refused(refusal, undo) => write!(f, "{refusal}{undo}"),
            Failure::Damaged(undo) => write!(f, "the embedded image is damaged{undo}"),
            Failure::Step(step, fault) => write!(f, "{step}, {fault}"),
        }
    }
}
