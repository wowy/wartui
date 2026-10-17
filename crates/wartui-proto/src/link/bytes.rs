//! Serde for a link message's byte payloads, through `#[serde(with = "bytes")]`.
//!
//! A heapless `Vec<u8, N>` serializes as a sequence, which postcard writes one element at
//! a time. Postcard writes a byte string as the same varint length and the same bytes, so
//! the wire is unchanged, but as one copy. On the bridge that is the difference that
//! matters: it encodes every frame it forwards, and pumps no USB while it does.
//!
//! Decoding takes a sequence too, so a self-describing format that hands one back still
//! works.

use core::fmt;
use core::marker::PhantomData;

use heapless::Vec;
use serde::de::{Deserializer, Error, SeqAccess, Visitor};
use serde::ser::Serializer;

/// Write `bytes` as a byte string.
///
/// # Errors
/// Whatever the serializer refuses.
pub fn serialize<S: Serializer, const N: usize>(
    bytes: &Vec<u8, N>,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    serializer.serialize_bytes(bytes)
}

/// Read a byte string, or a sequence of bytes, of at most `N`.
///
/// # Errors
/// An `invalid_length` past `N`, or whatever the deserializer refuses.
pub fn deserialize<'de, D: Deserializer<'de>, const N: usize>(
    deserializer: D,
) -> Result<Vec<u8, N>, D::Error> {
    deserializer.deserialize_bytes(BytesVisitor::<N>(PhantomData))
}

struct BytesVisitor<const N: usize>(PhantomData<[u8; N]>);

impl<'de, const N: usize> Visitor<'de> for BytesVisitor<N> {
    type Value = Vec<u8, N>;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "at most {N} bytes")
    }

    fn visit_bytes<E: Error>(self, v: &[u8]) -> Result<Self::Value, E> {
        Vec::from_slice(v).map_err(|_| E::invalid_length(v.len(), &self))
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
        let mut out = Vec::new();
        while let Some(byte) = seq.next_element()? {
            out.push(byte).map_err(|_| A::Error::invalid_length(N + 1, &self))?;
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use heapless::Vec;
    use serde::Serialize;
    use serde::de::value::{BytesDeserializer, Error, SeqDeserializer};

    use crate::link::{
        BridgeToHost, EspNowPayload, GpsBytes, GpsWriteBytes, HostToBridge, SendPayload,
    };
    use crate::mac::Mac;

    // Twins of the link enums with the payloads left to heapless's own sequence encoding.
    // Variants not under test are placeholders, kept so each index stays where it is.
    #[allow(clippy::large_enum_variant)]
    #[derive(Serialize)]
    enum SeqBridgeToHost {
        _Ready,
        Rx { src: Mac, dst: Mac, rssi: i8, rx_us: u32, payload: EspNowPayload },
        _SendResult,
        _Status,
        _Log,
        _Error,
        Gps { bytes: GpsBytes },
    }

    #[allow(clippy::large_enum_variant)]
    #[derive(Serialize)]
    enum SeqHostToBridge {
        _Identify,
        _AddPeer,
        _RemovePeer,
        SendEspNow { id: u16, dst: Mac, ensure_peer: bool, payload: SendPayload },
        _GetStatus,
        _Reset,
        _ShowPanel,
        _SetTxPower,
        GpsWrite { bytes: GpsWriteBytes },
    }

    fn encode<T: Serialize>(msg: &T, buf: &mut [u8; 2048]) -> usize {
        postcard::to_slice(msg, buf).expect("fits").len()
    }

    fn assert_same_wire<A: Serialize, B: Serialize>(ours: &A, seq: &B) {
        let (mut a, mut b) = ([0u8; 2048], [0u8; 2048]);
        let (na, nb) = (encode(ours, &mut a), encode(seq, &mut b));
        assert_eq!(a[..na], b[..nb]);
    }

    fn filled<const N: usize>(len: usize) -> Vec<u8, N> {
        (0..len).map(|i| i as u8).collect()
    }

    #[test]
    fn bytes_encode_matches_sequence_encoding_when_payload_is_rx() {
        // 300 bytes, so the length prefix is a two-byte varint.
        let payload: EspNowPayload = filled(300);
        let ours = BridgeToHost::Rx {
            src: [1; 6],
            dst: [2; 6],
            rssi: -40,
            rx_us: 7,
            payload: payload.clone(),
        };
        let seq = SeqBridgeToHost::Rx { src: [1; 6], dst: [2; 6], rssi: -40, rx_us: 7, payload };
        assert_same_wire(&ours, &seq);
    }

    #[test]
    fn bytes_encode_matches_sequence_encoding_when_payload_is_gps() {
        let bytes: GpsBytes = filled(200);
        assert_same_wire(
            &BridgeToHost::Gps { bytes: bytes.clone() },
            &SeqBridgeToHost::Gps { bytes },
        );
    }

    #[test]
    fn bytes_encode_matches_sequence_encoding_when_payload_is_send() {
        let payload: SendPayload = filled(1470);
        let ours = HostToBridge::SendEspNow {
            id: 9,
            dst: [3; 6],
            ensure_peer: true,
            payload: payload.clone(),
        };
        let seq = SeqHostToBridge::SendEspNow { id: 9, dst: [3; 6], ensure_peer: true, payload };
        assert_same_wire(&ours, &seq);
    }

    #[test]
    fn bytes_encode_matches_sequence_encoding_when_payload_is_gps_write() {
        let bytes: GpsWriteBytes = filled(4);
        assert_same_wire(
            &HostToBridge::GpsWrite { bytes: bytes.clone() },
            &SeqHostToBridge::GpsWrite { bytes },
        );
    }

    #[test]
    fn bytes_encode_matches_sequence_encoding_when_payload_is_empty() {
        assert_same_wire(
            &BridgeToHost::Gps { bytes: GpsBytes::new() },
            &SeqBridgeToHost::Gps { bytes: GpsBytes::new() },
        );
    }

    #[test]
    fn bytes_decode_refuses_payload_when_longer_than_capacity() {
        // A twin one byte wider than `GpsWriteBytes`, filled past what the real type holds.
        #[allow(clippy::large_enum_variant)]
        #[derive(Serialize)]
        enum Wide {
            _0,
            _1,
            _2,
            _3,
            _4,
            _5,
            _6,
            _7,
            GpsWrite { bytes: Vec<u8, { crate::link::GPS_WRITE_MAX + 1 }> },
        }
        let mut buf = [0u8; 2048];
        let n = encode(&Wide::GpsWrite { bytes: filled(crate::link::GPS_WRITE_MAX + 1) }, &mut buf);
        assert!(postcard::from_bytes::<HostToBridge>(&buf[..n]).is_err());
    }

    #[test]
    fn bytes_decode_accepts_sequence_when_format_is_self_describing() {
        let seq = SeqDeserializer::<_, Error>::new([1u8, 2, 3].into_iter());
        let got: Vec<u8, 4> = super::deserialize(seq).expect("fits");
        assert_eq!(got, [1, 2, 3]);
    }

    #[test]
    fn bytes_decode_refuses_sequence_when_longer_than_capacity() {
        let seq = SeqDeserializer::<_, Error>::new([1u8, 2, 3].into_iter());
        assert!(super::deserialize::<_, 2>(seq).is_err());
    }

    #[test]
    fn bytes_decode_refuses_byte_string_when_longer_than_capacity() {
        let de = BytesDeserializer::<Error>::new(&[1, 2, 3]);
        assert!(super::deserialize::<_, 2>(de).is_err());
        let de = BytesDeserializer::<Error>::new(&[1, 2, 3]);
        assert_eq!(super::deserialize::<_, 3>(de).expect("fits"), [1, 2, 3]);
    }
}
