//! CRC-32 for records on flash and in the SD log ([`crate::sd::log`]).
//!
//! A flash record outlives the build that wrote it, so it takes the 32-bit CRC every tool can
//! check. The links' packets use [`crc16`](crate::link::crc16), which is enough for a packet
//! that lives one transfer.

/// CRC-32/ISO-HDLC, the zlib and Ethernet CRC: reflected polynomial `0xEDB88320`, initial value
/// and final XOR `0xFFFFFFFF`.
#[must_use]
pub fn crc32(data: &[u8]) -> u32 {
    esp_rom_crc32_le(0, data)
}

/// `esp_rom_crc32_le(crc, data)`: [`crc32`]'s table, register `!crc` in, `!` out.
/// The bootloader checks an `otadata` entry with `esp_rom_crc32_le(u32::MAX, &seq_le)`.
#[must_use]
pub fn esp_rom_crc32_le(crc: u32, data: &[u8]) -> u32 {
    let mut crc = !crc;
    for &byte in data {
        crc = (crc >> 8) ^ TABLE[usize::from(crc as u8 ^ byte)];
    }
    !crc
}

/// [`crc32`]'s remainder for each value of the low byte, built at compile time.
static TABLE: [u32; 256] = {
    let mut table = [0u32; 256];
    let mut n = 0;
    while n < 256 {
        let mut crc = n as u32;
        let mut bit = 0;
        while bit < 8 {
            crc = if crc & 1 != 0 { (crc >> 1) ^ 0xEDB8_8320 } else { crc >> 1 };
            bit += 1;
        }
        table[n] = crc;
        n += 1;
    }
    table
};
