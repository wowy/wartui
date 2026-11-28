//! The C6 loader's decisions: which slot it runs from and what it may write.
//!
//! The loader is the first image on a new board's C6. It runs from the factory table's `ota_0` or
//! `ota_1`, and rewrites the bootloader, table, `otadata` and `ota_0` from our table. A write
//! that lands on the running slot kills the loader mid-flash and bricks the board, so the checks
//! live here, where the host tests them, as [`split`](crate::split) does for the P4.
//!
//! - **The loader trusts the bootloader's rule, not `OtaUpdater`.** The slot it runs from is the
//!   slot the bootloader chose, so [`newest`] and [`booted`] repeat ESP-IDF's rule: an `otadata`
//!   entry counts unless its seq is `u32::MAX`, its state is invalid or aborted, or its CRC
//!   differs. The slot is `ota_((seq - 1) % n)`, found by subtype, and erased `otadata` boots
//!   `ota_0`. `firmware/common/src/ota.rs` reads erased `otadata` its own way, which would
//!   misplace the loader on a factory board.
//! - **`trial` is found by label**, because `data, undefined` is not unique.
//! - **The whole region below the table is the bootloader.** The loader rewrites it all, and the
//!   trailing `0xFF` writes are no-ops, so the bootloader's length stays out of the plan.
//! - **Every span is checked against the running slot.** A table that places `ota_0`, `otadata`
//!   or `trial` on the loader is refused, never written.

use core::fmt;

use crate::crc::esp_rom_crc32_le;

/// Where the partition table lives.
pub const TABLE_AT: u32 = 0x8000;
/// The table's length: 0x60 entries of 32 bytes.
pub const TABLE_LEN: usize = 0xC00;
/// The flash's erase unit.
pub const SECTOR: u32 = 0x1000;
/// Our bootloader's span: everything below the table.
pub const BOOTLOADER: Span = Span { start: 0, end: TABLE_AT };

const ENTRY_MAGIC: [u8; 2] = [0xAA, 0x50];
const ENTRY_LEN: usize = 32;
const APP: u8 = 0;
const OTA_0: u8 = 0x10;
const OTA_LAST: u8 = 0x1F;
const STATE_INVALID: u32 = 3;
const STATE_ABORTED: u32 = 4;

/// A half-open flash range.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Span {
    pub start: u32,
    pub end: u32,
}

impl Span {
    fn overlaps(self, other: Span) -> bool {
        self.start < other.end && other.start < self.end
    }
}

impl fmt::Display for Span {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:#x}..{:#x}", self.start, self.end)
    }
}

/// One partition table entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Partition {
    pub kind: u8,
    pub subtype: u8,
    pub offset: u32,
    pub size: u32,
    pub label: [u8; 16],
}

impl Partition {
    #[must_use]
    pub fn span(&self) -> Span {
        Span { start: self.offset, end: self.offset.saturating_add(self.size) }
    }

    /// The label up to its first NUL; empty if it is not UTF-8.
    #[must_use]
    pub fn label(&self) -> &str {
        let end = self.label.iter().position(|&b| b == 0).unwrap_or(self.label.len());
        core::str::from_utf8(&self.label[..end]).unwrap_or("")
    }
}

fn le32(bytes: &[u8]) -> u32 {
    u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
}

/// Entries in order, stopping at the first without the `0xAA 0x50` magic (the MD5 row, or `0xFF`).
pub fn partitions(table: &[u8]) -> impl Iterator<Item = Partition> + '_ {
    table.as_chunks::<ENTRY_LEN>().0.iter().take_while(|entry| entry[..2] == ENTRY_MAGIC).map(
        |entry| Partition {
            kind: entry[2],
            subtype: entry[3],
            offset: le32(&entry[4..8]),
            size: le32(&entry[8..12]),
            label: entry[12..28].try_into().unwrap_or([0; 16]),
        },
    )
}

/// The `otadata` sector (0 or 1) holding the newest entry the bootloader accepts: seq not
/// `u32::MAX`, state neither `INVALID` (3) nor `ABORTED` (4), and the CRC at bytes 28..32 equal to
/// `esp_rom_crc32_le(u32::MAX, seq)`. Seq 0 is accepted, as the bootloader does. On equal seqs
/// sector 1 wins.
#[must_use]
pub fn newest(otadata: &[[u8; 32]; 2]) -> Option<usize> {
    let seq = |entry: &[u8; 32]| {
        let seq = le32(&entry[..4]);
        let state = le32(&entry[24..28]);
        let valid = seq != u32::MAX
            && state != STATE_INVALID
            && state != STATE_ABORTED
            && le32(&entry[28..32]) == esp_rom_crc32_le(u32::MAX, &entry[..4]);
        valid.then_some(seq)
    };
    match (seq(&otadata[0]), seq(&otadata[1])) {
        (Some(a), Some(b)) => Some(usize::from(b >= a)),
        (Some(_), None) => Some(0),
        (None, Some(_)) => Some(1),
        (None, None) => None,
    }
}

fn is_ota_app(p: &Partition) -> bool {
    p.kind == APP && (OTA_0..=OTA_LAST).contains(&p.subtype)
}

/// The app slot the bootloader boots: the `ota_N` app partition whose `N` is the newest entry's
/// `(seq - 1) % n`, with `n` the number of `ota_N` app partitions. The bootloader indexes slots by
/// subtype, not table order. With no valid entry it boots `ota_0`.
#[must_use]
pub fn booted(table: &[u8], otadata: &[[u8; 32]; 2]) -> Option<Partition> {
    let index = match newest(otadata) {
        Some(sector) => {
            let count = u32::try_from(partitions(table).filter(is_ota_app).count()).ok()?;
            le32(&otadata[sector][..4]).wrapping_sub(1).checked_rem(count)?
        }
        None => 0,
    };
    partitions(table).filter(is_ota_app).find(|p| u32::from(p.subtype & 0x0F) == index)
}

/// What the loader writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Plan {
    pub app: Span,
    pub otadata: Span,
    pub trial: Span,
    pub table: Span,
    pub bootloader: Span,
}

/// Why the loader must not write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// Our table has no partition by this name (`ota_0`, `otadata`, `trial`).
    Missing(&'static str),
    /// The radio image is `len` bytes, and our `ota_0` holds `slot`.
    TooLarge { len: u32, slot: u32 },
    /// Writing `what` to `at` would overwrite the running loader in `running`.
    Overlaps { what: &'static str, at: Span, running: Span },
}

/// The text after `c6-loader: failed `.
impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Refusal::Missing(name) => write!(f, "our partition table has no {name} partition"),
            Refusal::TooLarge { len, slot } => {
                write!(f, "the radio image is {len} bytes, and our ota_0 holds {slot}")
            }
            Refusal::Overlaps { what, at, running } => {
                write!(f, "the {what} at {at} would overwrite this loader at {running}")
            }
        }
    }
}

/// What the loader writes, from our table and the radio app's length, or why it must not.
/// `app` runs from `ota_0`'s offset over the app length rounded up to [`SECTOR`].
pub fn plan(ours: &[u8], running: Partition, app_len: u32) -> Result<Plan, Refusal> {
    let find = |name: &'static str| {
        partitions(ours).find(|p| p.label() == name).ok_or(Refusal::Missing(name))
    };
    let ota_0 = find("ota_0")?;
    let otadata = find("otadata")?;
    let trial = find("trial")?;
    if app_len > ota_0.size {
        return Err(Refusal::TooLarge { len: app_len, slot: ota_0.size });
    }
    let rounded = app_len.div_ceil(SECTOR).saturating_mul(SECTOR);
    let plan = Plan {
        app: Span { start: ota_0.offset, end: ota_0.offset.saturating_add(rounded) },
        otadata: otadata.span(),
        trial: trial.span(),
        table: Span { start: TABLE_AT, end: TABLE_AT + SECTOR },
        bootloader: BOOTLOADER,
    };
    let running = running.span();
    let writes = [
        ("radio image", plan.app),
        ("bootloader", plan.bootloader),
        ("partition table", plan.table),
        ("otadata", plan.otadata),
        ("trial", plan.trial),
    ];
    for (what, span) in writes {
        if span.overlaps(running) {
            return Err(Refusal::Overlaps { what, at: span, running });
        }
    }
    Ok(plan)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crc;

    extern crate std;
    use std::vec::Vec;

    fn entry(label: &str, kind: u8, subtype: u8, offset: u32, size: u32) -> [u8; 32] {
        let mut e = [0u8; 32];
        e[..2].copy_from_slice(&ENTRY_MAGIC);
        e[2] = kind;
        e[3] = subtype;
        e[4..8].copy_from_slice(&offset.to_le_bytes());
        e[8..12].copy_from_slice(&size.to_le_bytes());
        e[12..12 + label.len()].copy_from_slice(label.as_bytes());
        e
    }

    fn ota_entry(seq: u32, state: u32, crc: u32) -> [u8; 32] {
        let mut e = [0xFF; 32];
        e[..4].copy_from_slice(&seq.to_le_bytes());
        e[24..28].copy_from_slice(&state.to_le_bytes());
        e[28..32].copy_from_slice(&crc.to_le_bytes());
        e
    }

    fn table(rows: &[[u8; 32]]) -> Vec<u8> {
        let mut t = Vec::new();
        for row in rows {
            t.extend_from_slice(row);
        }
        let mut md5 = [0xFF; 32];
        md5[..2].copy_from_slice(&[0xEB, 0xEB]);
        t.extend_from_slice(&md5);
        t.resize(TABLE_LEN, 0xFF);
        t
    }

    fn factory() -> Vec<u8> {
        table(&[
            entry("nvs", 1, 2, 0x9000, 0x4000),
            entry("otadata", 1, 0, 0xD000, 0x2000),
            entry("phy_init", 1, 1, 0xF000, 0x1000),
            entry("ota_0", 0, 0x10, 0x10000, 0x18_0000),
            entry("ota_1", 0, 0x11, 0x19_0000, 0x18_0000),
        ])
    }

    fn ours() -> Vec<u8> {
        table(&[
            entry("nvs", 1, 2, 0x9000, 0x4000),
            entry("otadata", 1, 0, 0xD000, 0x2000),
            entry("phy_init", 1, 1, 0xF000, 0x1000),
            entry("ota_0", 0, 0x10, 0x1_0000, 0x1E_0000),
            entry("ota_1", 0, 0x11, 0x1F_0000, 0x1E_0000),
            entry("trial", 1, 0xFF, 0x3D_0000, 0x1000),
        ])
    }

    fn factory_slot(n: usize) -> Partition {
        partitions(&factory()).filter(|p| p.kind == APP).nth(n).unwrap()
    }

    #[test]
    fn first_flash_computes_otadata_crc_when_given_bench_entries() {
        // Read off a bench board's otadata on 2026-11-27.
        assert_eq!(crc::esp_rom_crc32_le(u32::MAX, &1u32.to_le_bytes()), 0x4743_989A);
        assert_eq!(crc::esp_rom_crc32_le(u32::MAX, &2u32.to_le_bytes()), 0x55F6_3774);
    }

    #[test]
    fn first_flash_finds_booted_slot_when_newest_entry_selects_ota_1() {
        let otadata = [ota_entry(1, 2, 0x4743_989A), ota_entry(2, 0xFFFF_FFFF, 0x55F6_3774)];
        let slot = booted(&factory(), &otadata).unwrap();
        assert_eq!((slot.offset, slot.size), (0x19_0000, 0x18_0000));
        assert_eq!(newest(&otadata), Some(1));
    }

    #[test]
    fn first_flash_finds_ota_0_when_otadata_is_erased() {
        let otadata = [[0xFF; 32]; 2];
        assert_eq!(booted(&factory(), &otadata).unwrap().offset, 0x1_0000);
        assert_eq!(newest(&otadata), None);
    }

    #[test]
    fn first_flash_ignores_entry_when_crc_does_not_match() {
        let otadata = [ota_entry(1, 2, 0x4743_989A), ota_entry(2, 0xFFFF_FFFF, 0)];
        assert_eq!(booted(&factory(), &otadata).unwrap().offset, 0x1_0000);
        assert_eq!(newest(&otadata), Some(0));
    }

    #[test]
    fn first_flash_stops_listing_partitions_when_entry_lacks_magic() {
        assert_eq!(partitions(&factory()).count(), 5);
    }

    #[test]
    fn first_flash_plans_writes_when_loader_runs_from_factory_ota_1() {
        // 658_208 bytes: p4-radio's app on 2026-11-27.
        let plan = plan(&ours(), factory_slot(1), 658_208).unwrap();
        assert_eq!(plan.app, Span { start: 0x10000, end: 0xB1000 });
        assert_eq!(plan.otadata, Span { start: 0xD000, end: 0xF000 });
        assert_eq!(plan.trial, Span { start: 0x3D_0000, end: 0x3D_1000 });
        assert_eq!(plan.table, Span { start: 0x8000, end: 0x9000 });
        assert_eq!(plan.bootloader, Span { start: 0, end: 0x8000 });
    }

    #[test]
    fn first_flash_refuses_plan_when_running_slot_is_our_ota_0_span() {
        let err = plan(&ours(), factory_slot(0), 658_208).unwrap_err();
        assert!(matches!(err, Refusal::Overlaps { what: "radio image", .. }));
    }

    #[test]
    fn first_flash_refuses_plan_when_image_reaches_running_slot() {
        let err = plan(&ours(), factory_slot(1), 0x18_0001).unwrap_err();
        assert!(matches!(err, Refusal::Overlaps { what: "radio image", .. }));
    }

    #[test]
    fn first_flash_refuses_plan_when_image_exceeds_our_ota_0() {
        let err = plan(&ours(), factory_slot(1), 0x1E_0001).unwrap_err();
        assert_eq!(err, Refusal::TooLarge { len: 0x1E_0001, slot: 0x1E_0000 });
    }

    #[test]
    fn first_flash_refuses_plan_when_our_table_lacks_trial() {
        let no_trial = table(&[
            entry("otadata", 1, 0, 0xD000, 0x2000),
            entry("ota_0", 0, 0x10, 0x1_0000, 0x1E_0000),
        ]);
        assert_eq!(plan(&no_trial, factory_slot(1), 1).unwrap_err(), Refusal::Missing("trial"));
    }

    #[test]
    fn first_flash_words_refusal_when_displayed() {
        use std::string::ToString;
        let err = plan(&ours(), factory_slot(1), 0x18_0001).unwrap_err();
        assert_eq!(
            err.to_string(),
            "the radio image at 0x10000..0x191000 would overwrite this loader at \
             0x190000..0x310000"
        );
    }

    fn good(seq: u32, state: u32) -> [u8; 32] {
        ota_entry(seq, state, crc::esp_rom_crc32_le(u32::MAX, &seq.to_le_bytes()))
    }

    #[test]
    fn first_flash_skips_entry_when_state_is_aborted() {
        let otadata = [good(1, 0xFFFF_FFFF), good(2, 4)];
        assert_eq!(newest(&otadata), Some(0));
        assert_eq!(booted(&factory(), &otadata).unwrap().offset, 0x1_0000);
    }

    #[test]
    fn first_flash_skips_entry_when_seq_is_erased_value_with_matching_crc() {
        let otadata = [good(u32::MAX, 0xFFFF_FFFF), good(2, 0xFFFF_FFFF)];
        assert_eq!(newest(&otadata), Some(1));
    }

    #[test]
    fn first_flash_prefers_sector_0_when_it_holds_the_newer_entry() {
        let otadata = [good(3, 0xFFFF_FFFF), good(2, 0xFFFF_FFFF)];
        assert_eq!(newest(&otadata), Some(0));
        assert_eq!(booted(&factory(), &otadata).unwrap().offset, 0x1_0000);
    }

    #[test]
    fn first_flash_indexes_slot_by_subtype_when_table_lists_ota_1_first() {
        let swapped = table(&[
            entry("ota_1", 0, 0x11, 0x19_0000, 0x18_0000),
            entry("ota_0", 0, 0x10, 0x1_0000, 0x18_0000),
        ]);
        let otadata = [good(2, 0xFFFF_FFFF), [0xFF; 32]];
        assert_eq!(booted(&swapped, &otadata).unwrap().offset, 0x19_0000);
    }

    #[test]
    fn first_flash_refuses_plan_when_running_slot_covers_trial() {
        let running = Partition {
            kind: APP,
            subtype: 0x11,
            offset: 0x3C_0000,
            size: 0x2_0000,
            label: [0; 16],
        };
        let err = plan(&ours(), running, 1).unwrap_err();
        assert!(matches!(err, Refusal::Overlaps { what: "trial", .. }));
    }
}
