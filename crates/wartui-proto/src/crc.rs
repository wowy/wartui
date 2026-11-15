//! CRC-32 for records on flash.
//!
//! A flash record outlives the build that wrote it, so it takes the 32-bit CRC every tool can
//! check. The links' packets use [`crc16`](crate::link::crc16), which is enough for a packet
//! that lives one transfer.

/// CRC-32/ISO-HDLC, the zlib and Ethernet CRC: reflected polynomial `0xEDB88320`, initial value
/// and final XOR `0xFFFFFFFF`.
#[must_use]
pub fn crc32(data: &[u8]) -> u32 {
    let mut crc = !0u32;
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
