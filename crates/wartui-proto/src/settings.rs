//! The settings the P4 keeps on its flash: the lid fan's duty and the channel pool, in the first
//! sector of its `nvs` partition.
//!
//! **No ESP-IDF NVS reads that partition**, and neither a USB flash nor an update over the host
//! link writes it, so the layout is this module's own.
//!
//! **One 4-byte record a change, written at the first erased word**: a tag naming the setting,
//! its value, the value inverted, then 0 ([`record`]). NOR flash programs only by clearing bits,
//! so a change costs a word written and nothing erased. Reading ([`read`]) walks the words up to
//! the first erased one, and each tag's last valid record wins. A word a power cut tore fails its
//! check and is skipped, and so is a tag or a value this build does not know.
//!
//! **A full sector erases and rewrites what it holds** ([`Write::Erase`]): every setting [`read`]
//! gives, with the new one in its place, so one setting never costs the other. A power cut
//! between the erase and the rewrite loses both, and the next boot runs the fan at
//! [`FanDuty::FULL`] and the pool at [`ChannelPool::All`].

use crate::fan::FanDuty;
use crate::ota::SECTOR;
use crate::plan::ChannelPool;

/// The tag of a fan duty's record.
pub const FAN: u8 = b'F';

/// The tag of a channel pool's record.
pub const POOL: u8 = b'P';

/// Bytes in a record: one flash word.
pub const WORD: usize = 4;

/// Records a sector holds.
pub const RECORDS: usize = SECTOR / WORD;

/// Settings a sector holds, one record each after an erase.
pub const KINDS: usize = 2;

/// One setting, as a record stores it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Setting {
    /// The lid fan's duty.
    Fan(FanDuty),
    /// The channel pool.
    Pool(ChannelPool),
}

/// What a sector holds: each setting's last valid record, or `None` for one with none.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Stored {
    /// The lid fan's duty.
    pub fan: Option<FanDuty>,
    /// The channel pool.
    pub pool: Option<ChannelPool>,
}

impl Stored {
    /// These settings with `setting` in its tag's place.
    #[must_use]
    pub const fn with(self, setting: Setting) -> Self {
        match setting {
            Setting::Fan(duty) => Self { fan: Some(duty), ..self },
            Setting::Pool(pool) => Self { pool: Some(pool), ..self },
        }
    }
}

/// The flash write a setting takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Write {
    /// Write `record` at `offset`, the sector's first erased word.
    At {
        /// The byte offset in the sector.
        offset: usize,
        /// The word to write.
        record: [u8; WORD],
    },
    /// Erase the sector, then write `records[..len]` at 0.
    Erase {
        /// Every setting the sector holds, fan then pool, the new one in its place.
        records: [u8; KINDS * WORD],
        /// Bytes of `records` to write.
        len: usize,
    },
}

/// The byte a pool's record stores.
const fn pool_byte(pool: ChannelPool) -> u8 {
    match pool {
        ChannelPool::Us => 0,
        ChannelPool::Eu => 1,
        ChannelPool::All => 2,
    }
}

/// The pool `byte` stores, or `None` for a byte no pool stores.
const fn pool_from(byte: u8) -> Option<ChannelPool> {
    match byte {
        0 => Some(ChannelPool::Us),
        1 => Some(ChannelPool::Eu),
        2 => Some(ChannelPool::All),
        _ => None,
    }
}

/// The record for `setting`.
#[must_use]
pub const fn record(setting: Setting) -> [u8; WORD] {
    let (tag, value) = match setting {
        Setting::Fan(duty) => (FAN, duty.get()),
        Setting::Pool(pool) => (POOL, pool_byte(pool)),
    };
    [tag, value, !value, 0]
}

/// The setting `word` records, or `None` for an erased, torn or unknown word.
fn decode(word: &[u8; WORD]) -> Option<Setting> {
    let [tag, value, check, 0] = *word else { return None };
    if check != !value {
        return None;
    }
    match tag {
        FAN => FanDuty::new(value).map(Setting::Fan),
        POOL => pool_from(value).map(Setting::Pool),
        _ => None,
    }
}

/// The words up to the first erased one.
fn written(sector: &[u8; SECTOR]) -> impl Iterator<Item = &[u8; WORD]> {
    sector.as_chunks::<WORD>().0.iter().take_while(|word| **word != [0xFF; WORD])
}

/// What `sector` holds: each tag's last valid record.
#[must_use]
pub fn read(sector: &[u8; SECTOR]) -> Stored {
    written(sector).filter_map(decode).fold(Stored::default(), Stored::with)
}

/// The write that stores `setting` in `sector`, or `None` when the sector already holds it.
///
/// At the first erased word while one is left; once the sector is full, an erase and every
/// setting it holds rewritten, fan then pool, with `setting` in its place.
#[must_use]
pub fn write_for(sector: &[u8; SECTOR], setting: Setting) -> Option<Write> {
    let stored = read(sector);
    if stored.with(setting) == stored {
        return None;
    }
    let used = written(sector).count();
    if used < RECORDS {
        return Some(Write::At { offset: used * WORD, record: record(setting) });
    }
    let kept = stored.with(setting);
    let mut records = [0xFF; KINDS * WORD];
    let mut len = 0;
    let fan = kept.fan.map(Setting::Fan);
    let pool = kept.pool.map(Setting::Pool);
    for setting in [fan, pool].into_iter().flatten() {
        records[len..len + WORD].copy_from_slice(&record(setting));
        len += WORD;
    }
    Some(Write::Erase { records, len })
}
