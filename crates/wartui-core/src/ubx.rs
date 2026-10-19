//! The u-blox side of a bridge's GPS receiver: the configuration the host writes to it, and the
//! split of what it sends back into NMEA and UBX.
//!
//! The bridge is a byte pipe (`firmware/common/src/gps.rs`), so everything that knows the receiver
//! speaks UBX lives here. A change to the rate or the sentence set then costs a `cargo run`, not a
//! reflash.
//!
//! **The configuration goes to RAM only.** [`valset_rate`] writes layer RAM and never BBR or flash,
//! so nothing persists on the receiver: a power cycle returns it to its defaults, and the host
//! writes the configuration again rather than trusting what the receiver remembers.
//!
//! **The receiver's stream interleaves NMEA and UBX**, and the bridge splits it wherever an I2C
//! read ended. [`Demux`] reassembles a UBX packet across those splits and passes every other byte
//! on as NMEA. Without it, every acknowledgment the receiver sends reads as a rejected NMEA line.
//! A packet's length is capped at [`MAX_PAYLOAD`], so a stray `B5 62` in the stream costs at most
//! that many bytes before the demux resynchronizes.

use std::ops::RangeInclusive;

/// The update rates [`valset_rate`] accepts, in hertz. Out-of-range rates clamp to it.
pub const RATE_HZ: RangeInclusive<u8> = 1..=5;

/// The rate the host asks for when the operator names none: the fastest, since a position at
/// driving speed ages by meters per fix.
pub const DEFAULT_RATE_HZ: u8 = 5;

/// The longest UBX payload [`Demux`] reassembles. The receiver is configured for NMEA, so its
/// UBX is acknowledgments and the occasional poll reply, all far shorter. A longer length is a
/// stray sync in the stream, and the packet is dropped.
pub const MAX_PAYLOAD: usize = 512;

const SYNC1: u8 = 0xB5;
const SYNC2: u8 = 0x62;
/// Sync, class, id and length.
const HEADER: usize = 6;
const CHECKSUM: usize = 2;

const CLASS_ACK: u8 = 0x05;
const ID_ACK_NAK: u8 = 0x00;
const ID_ACK_ACK: u8 = 0x01;
const CLASS_CFG: u8 = 0x06;
const ID_CFG_VALSET: u8 = 0x8A;

/// VALSET's layer bit for RAM: applied now, lost at power-off.
const LAYER_RAM: u8 = 0x01;

/// Configuration keys, from the u-blox M10 interface description.
const CFG_RATE_MEAS: u32 = 0x3021_0001;
const CFG_RATE_NAV: u32 = 0x3021_0002;
const CFG_MSGOUT_NMEA_GGA_I2C: u32 = 0x2091_00BA;
const CFG_MSGOUT_NMEA_RMC_I2C: u32 = 0x2091_00AB;
const CFG_MSGOUT_NMEA_GSA_I2C: u32 = 0x2091_00BF;
const CFG_MSGOUT_NMEA_GSV_I2C: u32 = 0x2091_00C4;
const CFG_MSGOUT_NMEA_GLL_I2C: u32 = 0x2091_00C9;
const CFG_MSGOUT_NMEA_VTG_I2C: u32 = 0x2091_00B0;
const CFG_UART1OUTPROT_UBX: u32 = 0x1074_0001;
const CFG_UART1OUTPROT_NMEA: u32 = 0x1074_0002;

/// One `UBX-CFG-VALSET` that sets the receiver's navigation rate and the NMEA it sends on I2C.
///
/// `GGA` and `RMC` arrive every solution, at `rate_hz` (clamped to [`RATE_HZ`]): they carry the
/// position. `GSA` and `GSV` arrive once a second whatever the rate. They feed only the sky view,
/// and at the full rate they would be most of the I2C traffic. `GLL` and `VTG` repeat what `RMC`
/// says, so they are off.
///
/// **UART1 output is off.** The receiver's transmit buffers are shared across its ports, and by
/// default UART1 sends the same NMEA at 9600 baud, about 960 bytes a second. Nobody reads it, since
/// the bridge reads I2C only, and at 5 Hz it cannot keep up: its backlog fills the shared pool, and
/// the receiver drops I2C sentences and reports `txbuf alloc` in a `TXT` sentence.
#[must_use]
pub fn valset_rate(rate_hz: u8) -> Vec<u8> {
    let rate = rate_hz.clamp(*RATE_HZ.start(), *RATE_HZ.end());
    let mut payload = vec![0x00, LAYER_RAM, 0x00, 0x00];
    let mut u2 = |key: u32, value: u16| {
        payload.extend_from_slice(&key.to_le_bytes());
        payload.extend_from_slice(&value.to_le_bytes());
    };
    u2(CFG_RATE_MEAS, 1000 / u16::from(rate));
    u2(CFG_RATE_NAV, 1);
    for (key, every) in [
        (CFG_MSGOUT_NMEA_GGA_I2C, 1),
        (CFG_MSGOUT_NMEA_RMC_I2C, 1),
        (CFG_MSGOUT_NMEA_GSA_I2C, rate),
        (CFG_MSGOUT_NMEA_GSV_I2C, rate),
        (CFG_MSGOUT_NMEA_GLL_I2C, 0),
        (CFG_MSGOUT_NMEA_VTG_I2C, 0),
        (CFG_UART1OUTPROT_UBX, 0),
        (CFG_UART1OUTPROT_NMEA, 0),
    ] {
        payload.extend_from_slice(&key.to_le_bytes());
        payload.push(every);
    }
    encode(CLASS_CFG, ID_CFG_VALSET, &payload)
}

/// A UBX frame: sync, class, id, little-endian length, payload and checksum.
fn encode(class: u8, id: u8, payload: &[u8]) -> Vec<u8> {
    let len = u16::try_from(payload.len()).expect("a UBX payload fits its u16 length");
    let mut out = Vec::with_capacity(HEADER + payload.len() + CHECKSUM);
    out.extend_from_slice(&[SYNC1, SYNC2, class, id]);
    out.extend_from_slice(&len.to_le_bytes());
    out.extend_from_slice(payload);
    let (a, b) = fletcher8(&out[2..]);
    out.extend_from_slice(&[a, b]);
    out
}

/// UBX's 8-bit Fletcher checksum, over class through the end of the payload.
fn fletcher8(bytes: &[u8]) -> (u8, u8) {
    bytes.iter().fold((0u8, 0u8), |(a, b), &byte| {
        let a = a.wrapping_add(byte);
        (a, b.wrapping_add(a))
    })
}

/// One UBX packet the receiver sent, checksum verified.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Packet<'a> {
    /// Message class.
    pub class: u8,
    /// Message id within the class.
    pub id: u8,
    /// The payload, without header or checksum.
    pub payload: &'a [u8],
}

/// The receiver's answer to a [`valset_rate`] frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValsetReply {
    /// `ACK-ACK`: the configuration took.
    Accepted,
    /// `ACK-NAK`: the receiver refused it.
    Refused,
}

impl Packet<'_> {
    /// Whether this packet acknowledges a `CFG-VALSET`, and which way.
    #[must_use]
    pub fn valset_reply(&self) -> Option<ValsetReply> {
        if self.class != CLASS_ACK || self.payload != [CLASS_CFG, ID_CFG_VALSET] {
            return None;
        }
        match self.id {
            ID_ACK_ACK => Some(ValsetReply::Accepted),
            ID_ACK_NAK => Some(ValsetReply::Refused),
            _ => None,
        }
    }
}

/// Splits a receiver's byte stream into NMEA bytes and UBX packets.
///
/// Fed in whatever pieces the bridge read, it holds a partial UBX packet across calls. NMEA
/// bytes pass through in order, unparsed, for the line reader.
#[derive(Debug, Default)]
pub struct Demux {
    /// The packet being reassembled, from its first sync byte. Empty while scanning NMEA.
    packet: Vec<u8>,
}

impl Demux {
    /// A demux expecting the start of a stream.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Consume the next piece of the stream.
    ///
    /// `nmea` gets runs of bytes outside any UBX packet. `ubx` gets each packet whose checksum
    /// holds. A packet with a bad checksum or an oversize length is dropped, with the bytes it
    /// consumed, and scanning resumes after it.
    pub fn push(&mut self, bytes: &[u8], mut nmea: impl FnMut(&[u8]), mut ubx: impl FnMut(Packet)) {
        // Start of the NMEA run in `bytes`, while scanning.
        let mut run = 0;
        for (i, &byte) in bytes.iter().enumerate() {
            // A lone first sync byte was not a packet after all.
            if self.packet.len() == 1 && byte != SYNC2 {
                nmea(&[SYNC1]);
                self.packet.clear();
                run = i;
            }
            if self.packet.is_empty() {
                if byte == SYNC1 {
                    if run < i {
                        nmea(&bytes[run..i]);
                    }
                    self.packet.push(byte);
                }
                continue;
            }
            self.packet.push(byte);
            if self.packet.len() < HEADER {
                continue;
            }
            let len = usize::from(u16::from_le_bytes([self.packet[4], self.packet[5]]));
            if len > MAX_PAYLOAD {
                self.packet.clear();
                run = i + 1;
                continue;
            }
            if self.packet.len() == HEADER + len + CHECKSUM {
                let (body, sum) = self.packet.split_at(HEADER + len);
                if fletcher8(&body[2..]) == (sum[0], sum[1]) {
                    ubx(Packet { class: body[2], id: body[3], payload: &body[HEADER..] });
                }
                self.packet.clear();
                run = i + 1;
            }
        }
        if self.packet.is_empty() && run < bytes.len() {
            nmea(&bytes[run..]);
        }
    }
}

/// The receiver's `ACK-ACK` to a `CFG-VALSET`, as it arrives on the wire.
#[cfg(test)]
pub(crate) const ACK_VALSET: [u8; 10] =
    [0xB5, 0x62, 0x05, 0x01, 0x02, 0x00, 0x06, 0x8A, 0x98, 0xC1];

/// The receiver's `ACK-NAK` to a `CFG-VALSET`, as it arrives on the wire.
#[cfg(test)]
pub(crate) const NAK_VALSET: [u8; 10] =
    [0xB5, 0x62, 0x05, 0x00, 0x02, 0x00, 0x06, 0x8A, 0x97, 0xBC];

#[cfg(test)]
mod tests {
    use super::*;
    use wartui_proto::link::GPS_WRITE_MAX;

    #[derive(Debug, Default)]
    struct Seen {
        nmea: Vec<u8>,
        packets: Vec<(u8, u8, Vec<u8>)>,
    }

    fn feed(demux: &mut Demux, seen: &mut Seen, bytes: &[u8]) {
        demux.push(
            bytes,
            |run| seen.nmea.extend_from_slice(run),
            |packet| seen.packets.push((packet.class, packet.id, packet.payload.to_vec())),
        );
    }

    fn demux_all(chunks: &[&[u8]]) -> Seen {
        let mut demux = Demux::new();
        let mut seen = Seen::default();
        for chunk in chunks {
            feed(&mut demux, &mut seen, chunk);
        }
        seen
    }

    #[test]
    fn ubx_encodes_valset_byte_for_byte_when_rate_is_1_hz() {
        // Worked out independently of `encode`: CFG-RATE-MEAS 1000 ms, GSA and GSV every epoch,
        // UART1 UBX and NMEA output off.
        let expected = [
            0xB5, 0x62, 0x06, 0x8A, 0x38, 0x00, 0x00, 0x01, 0x00, 0x00, 0x01, 0x00, 0x21, 0x30,
            0xE8, 0x03, 0x02, 0x00, 0x21, 0x30, 0x01, 0x00, 0xBA, 0x00, 0x91, 0x20, 0x01, 0xAB,
            0x00, 0x91, 0x20, 0x01, 0xBF, 0x00, 0x91, 0x20, 0x01, 0xC4, 0x00, 0x91, 0x20, 0x01,
            0xC9, 0x00, 0x91, 0x20, 0x00, 0xB0, 0x00, 0x91, 0x20, 0x00, 0x01, 0x00, 0x74, 0x10,
            0x00, 0x02, 0x00, 0x74, 0x10, 0x00, 0xF0, 0x58,
        ];
        assert_eq!(valset_rate(1), expected);
    }

    #[test]
    fn ubx_encodes_valset_byte_for_byte_when_rate_is_5_hz() {
        // CFG-RATE-MEAS 200 ms, GSA and GSV every fifth epoch, UART1 output off.
        let expected = [
            0xB5, 0x62, 0x06, 0x8A, 0x38, 0x00, 0x00, 0x01, 0x00, 0x00, 0x01, 0x00, 0x21, 0x30,
            0xC8, 0x00, 0x02, 0x00, 0x21, 0x30, 0x01, 0x00, 0xBA, 0x00, 0x91, 0x20, 0x01, 0xAB,
            0x00, 0x91, 0x20, 0x01, 0xBF, 0x00, 0x91, 0x20, 0x05, 0xC4, 0x00, 0x91, 0x20, 0x05,
            0xC9, 0x00, 0x91, 0x20, 0x00, 0xB0, 0x00, 0x91, 0x20, 0x00, 0x01, 0x00, 0x74, 0x10,
            0x00, 0x02, 0x00, 0x74, 0x10, 0x00, 0xD5, 0x87,
        ];
        assert_eq!(valset_rate(5), expected);
    }

    #[test]
    fn ubx_clamps_valset_rate_when_out_of_range() {
        assert_eq!(valset_rate(0), valset_rate(1));
        assert_eq!(valset_rate(9), valset_rate(5));
    }

    #[test]
    fn ubx_valset_fits_gps_write_when_at_any_rate() {
        for rate in RATE_HZ {
            assert!(valset_rate(rate).len() <= GPS_WRITE_MAX);
        }
    }

    #[test]
    fn ubx_demux_separates_ack_from_nmea_when_interleaved() {
        let mut stream = b"$GNGGA,1*00\r\n".to_vec();
        stream.extend_from_slice(&ACK_VALSET);
        stream.extend_from_slice(b"$GNRMC,2*00\r\n");

        let seen = demux_all(&[&stream]);
        assert_eq!(seen.nmea, b"$GNGGA,1*00\r\n$GNRMC,2*00\r\n");
        assert_eq!(seen.packets, [(0x05, 0x01, vec![0x06, 0x8A])]);
    }

    #[test]
    fn ubx_demux_reassembles_packet_when_split_at_any_offset() {
        let mut stream = b"$A\r\n".to_vec();
        stream.extend_from_slice(&ACK_VALSET);
        stream.extend_from_slice(b"$B\r\n");
        for cut in 0..=stream.len() {
            let (head, tail) = stream.split_at(cut);
            let seen = demux_all(&[head, tail]);
            assert_eq!(seen.nmea, b"$A\r\n$B\r\n", "cut at {cut}");
            assert_eq!(seen.packets, [(0x05, 0x01, vec![0x06, 0x8A])], "cut at {cut}");
        }
    }

    #[test]
    fn ubx_recognizes_valset_refusal_when_receiver_naks() {
        let mut demux = Demux::new();
        let mut replies = Vec::new();
        demux.push(&NAK_VALSET, |_| {}, |packet| replies.push(packet.valset_reply()));
        demux.push(&ACK_VALSET, |_| {}, |packet| replies.push(packet.valset_reply()));
        assert_eq!(replies, [Some(ValsetReply::Refused), Some(ValsetReply::Accepted)]);
    }

    #[test]
    fn ubx_ignores_ack_when_it_answers_another_message() {
        let packet = Packet { class: 0x05, id: 0x01, payload: &[0x06, 0x01] };
        assert_eq!(packet.valset_reply(), None);
    }

    #[test]
    fn ubx_demux_drops_packet_and_resyncs_when_checksum_is_bad() {
        let mut bad = ACK_VALSET;
        bad[9] ^= 0xFF;
        let mut stream = bad.to_vec();
        stream.extend_from_slice(b"$A\r\n");
        stream.extend_from_slice(&ACK_VALSET);

        let seen = demux_all(&[&stream]);
        assert_eq!(seen.nmea, b"$A\r\n");
        assert_eq!(seen.packets, [(0x05, 0x01, vec![0x06, 0x8A])]);
    }

    #[test]
    fn ubx_demux_resyncs_after_header_when_length_is_oversize() {
        // A stray sync with a length past the cap: the header goes, the stream after it stays.
        let mut stream = vec![0xB5, 0x62, 0x01, 0x02, 0xFF, 0xFF];
        stream.extend_from_slice(b"$A\r\n");
        stream.extend_from_slice(&ACK_VALSET);

        let seen = demux_all(&[&stream]);
        assert_eq!(seen.nmea, b"$A\r\n");
        assert_eq!(seen.packets.len(), 1);
    }

    #[test]
    fn ubx_demux_passes_lone_sync_byte_as_nmea_when_not_followed_by_second() {
        let seen = demux_all(&[&[b'$', 0xB5], &[b'A', 0xB5, 0xB5, 0x62], &ACK_VALSET[2..]]);
        assert_eq!(seen.nmea, [b'$', 0xB5, b'A', 0xB5]);
        assert_eq!(seen.packets.len(), 1);
    }
}
