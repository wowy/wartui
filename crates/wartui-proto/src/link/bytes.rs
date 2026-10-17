//! Serde for a link message's byte payloads, through `#[serde(with = "bytes")]`.
//!
//! A heapless `Vec<u8, N>` serializes as a sequence of `u8`. Postcard writes a byte string
//! with the same varint length and the same bytes, so the two are interchangeable on the
//! wire, and this writes it in one call rather than an element at a time. The bridge
//! encodes every frame it forwards and pumps no USB while it does, so that time counts.
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
    use serde::de::value::{BytesDeserializer, Error, SeqDeserializer};
    use serde::{Deserialize, Serialize};

    use crate::link::MAX_NODE_PAYLOAD;

    /// The largest payload under test: a node's, which a `BridgeToHost::Rx` carries.
    const N: usize = MAX_NODE_PAYLOAD;

    /// A payload through this module.
    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    struct Bytes<const M: usize> {
        #[serde(with = "super")]
        payload: Vec<u8, M>,
    }

    /// The same payload left to heapless's own sequence encoding.
    #[derive(Serialize)]
    struct Seq<const M: usize> {
        payload: Vec<u8, M>,
    }

    fn filled<const M: usize>(len: usize) -> Vec<u8, M> {
        (0..len).map(|i| i as u8).collect()
    }

    fn encode<T: Serialize>(msg: &T) -> Vec<u8, 2048> {
        let mut buf = [0u8; 2048];
        Vec::from_slice(postcard::to_slice(msg, &mut buf).expect("fits")).expect("fits")
    }

    #[test]
    fn bytes_encode_matches_sequence_encoding_when_given_any_length() {
        // 127 and 128 straddle the one-byte varint length; 300 and N take two bytes.
        for len in [0, 1, 127, 128, 300, N] {
            let payload: Vec<u8, N> = filled(len);
            let ours = encode(&Bytes { payload: payload.clone() });
            assert_eq!(ours, encode(&Seq { payload }), "length {len}");
        }
    }

    #[test]
    fn bytes_decode_round_trips_payload_when_given_any_length() {
        for len in [0, 1, 127, 128, 300, N] {
            let sent = Bytes::<N> { payload: filled(len) };
            let got: Bytes<N> = postcard::from_bytes(&encode(&sent)).expect("decodes");
            assert_eq!(got, sent, "length {len}");
        }
    }

    #[test]
    fn bytes_decode_reads_sequence_encoding_when_given_any_length() {
        for len in [0, 1, 127, 128, 300, N] {
            let payload: Vec<u8, N> = filled(len);
            let got: Bytes<N> =
                postcard::from_bytes(&encode(&Seq { payload: payload.clone() })).expect("decodes");
            assert_eq!(got.payload, payload, "length {len}");
        }
    }

    #[test]
    fn bytes_decode_refuses_payload_when_one_byte_past_capacity() {
        // Exactly N decodes, so the refusal below is the extra byte's.
        let full = encode(&Bytes::<{ N + 1 }> { payload: filled(N) });
        let got: Bytes<N> = postcard::from_bytes(&full).expect("N bytes fit");
        assert_eq!(got.payload, filled::<N>(N));

        let over = encode(&Bytes::<{ N + 1 }> { payload: filled(N + 1) });
        assert!(postcard::from_bytes::<Bytes<N>>(&over).is_err());
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
