use super::{DecodeError, MsgType, OFF_BODY, Security, header, write_header};
use crate::link::MAX_ESPNOW_PAYLOAD;

/// Longest SSID 802.11 allows, and so the most a record carries.
pub const SSID_MAX: usize = 32;

/// Most a record's trailer carries, past its length byte.
///
/// Sized for the widest roaming consortium element real access points beacon: a count
/// byte, a lengths byte and three five-byte identifiers, the
/// `5A03BA0000 BAA2D00000 BAA2D02000` OpenRoaming triple. A trailer that does not fit is
/// dropped whole where it is parsed, not truncated on the wire.
pub const EXT_MAX: usize = 17;

/// Length of a [`SightingMsg`] record with no SSID and no trailer: the floor a decoder
/// needs before it can read `ssid_len`.
pub const SIGHTING_RECORD_MIN: usize = 12;

/// Length of a [`SightingMsg`] record with the longest SSID and trailer, and so a
/// buffer [`SightingMsg::encode_record_into`] can always finish in.
pub const SIGHTING_RECORD_MAX: usize = SIGHTING_RECORD_MIN + SSID_MAX + EXT_MAX;

/// Length of a [`SightingBatch`] header: magic, version, type, `seq` and `count`.
pub const SIGHTING_BATCH_HEADER: usize = OFF_BODY + 3;

/// The most a [`SightingBatch`] may be on the wire: ESP-NOW's own payload ceiling, so a
/// batch fills exactly one frame.
pub const SIGHTING_BATCH_MAX: usize = 250;

const _: () = assert!(SIGHTING_BATCH_MAX == MAX_ESPNOW_PAYLOAD);

/// The most records a batch can carry: the header taken off the ceiling, divided by
/// the smallest record.
///
/// [`SightingBatchWriter`] packs to a byte budget, not to this count. Records vary from
/// [`SIGHTING_RECORD_MIN`] to [`SIGHTING_RECORD_MAX`], so a count cap would either
/// overflow the frame or waste it.
pub const SIGHTINGS_PER_BATCH_MAX: usize =
    (SIGHTING_BATCH_MAX - SIGHTING_BATCH_HEADER) / SIGHTING_RECORD_MIN;

/// What a [`SightingMsg`] records.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum RecordKind {
    /// A Wi-Fi access point.
    Wifi = 0,
    /// A BLE advertiser.
    Ble = 1,
}

impl RecordKind {
    /// The discriminant as it appears on the wire.
    #[must_use]
    pub const fn as_u8(self) -> u8 {
        self as u8
    }
}

impl TryFrom<u8> for RecordKind {
    type Error = DecodeError;

    fn try_from(v: u8) -> Result<Self, Self::Error> {
        match v {
            0 => Ok(Self::Wifi),
            1 => Ok(Self::Ble),
            other => Err(DecodeError::UnknownType(other)),
        }
    }
}

/// One record of a [`SightingBatch`]: one sighting of an access point or advertiser.
///
/// Twelve bytes plus the SSID and the trailer, with no padding, because every byte is
/// paid on the control channel every node shares. A record has no header of its own;
/// [`SightingBatch`] carries one for the whole frame.
///
/// The SSID is length-prefixed, so a comma or any other byte inside it arrives intact.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SightingMsg<'a> {
    /// Wi-Fi or BLE.
    pub kind: RecordKind,
    /// The observed BSSID or advertiser address, six raw bytes.
    pub bssid: [u8; 6],
    /// Wi-Fi channel number, or 0 for BLE.
    pub channel: u8,
    /// Signal strength in dBm, as the node measured it.
    pub rssi: i8,
    /// What its information elements amounted to.
    pub security: Security,
    /// Raw SSID bytes, at most [`SSID_MAX`]. Empty for a hidden network and always
    /// empty for BLE. Not necessarily UTF-8: an SSID is whatever the access point
    /// beaconed.
    ///
    /// This layer is byte-transparent and trims nothing. A cloaked access point's zero
    /// padding is stripped where the beacon is parsed
    /// ([`crate::beacon::visible_ssid`]), so it never reaches the wire.
    pub ssid: &'a [u8],
    /// The kind-dependent trailer, at most [`EXT_MAX`] bytes, length-prefixed on the
    /// wire after the SSID.
    ///
    /// - **Wi-Fi:** the roaming consortium element's body verbatim, every byte after its
    ///   `Length` octet.
    /// - **BLE:** the Bluetooth SIG company identifier, two bytes little-endian, or
    ///   nothing when the advertiser carried none.
    ///
    /// One trailer rather than a field per kind, because the two never compete for the
    /// same record. This layer carries the bytes and never interprets them; what they
    /// mean is decided where they are parsed and where they are written down.
    pub ext: &'a [u8],
}

impl<'a> SightingMsg<'a> {
    /// Decode one record from the front of `buf`, borrowing the SSID and
    /// trailer from it and returning how many bytes it took.
    ///
    /// `buf` may hold more records after this one. [`SightingBatch::decode`] walks them,
    /// using the returned length to find the next. The batch header has already
    /// answered for magic and version.
    ///
    /// # Errors
    /// See [`DecodeError`].
    pub fn decode_record(buf: &'a [u8]) -> Result<(Self, usize), DecodeError> {
        if buf.len() < SIGHTING_RECORD_MIN {
            return Err(DecodeError::TooShort { need: SIGHTING_RECORD_MIN, got: buf.len() });
        }
        let ssid_len = buf[10];
        if usize::from(ssid_len) > SSID_MAX {
            return Err(DecodeError::SsidTooLong(ssid_len));
        }
        // `SIGHTING_RECORD_MIN` already counts the trailer's length byte, so
        // this is the first byte past whatever SSID there is.
        let need = SIGHTING_RECORD_MIN + usize::from(ssid_len);
        if buf.len() < need {
            return Err(DecodeError::TooShort { need, got: buf.len() });
        }
        let ext_len = buf[need - 1];
        if usize::from(ext_len) > EXT_MAX {
            return Err(DecodeError::ExtTooLong(ext_len));
        }
        let need = need + usize::from(ext_len);
        if buf.len() < need {
            return Err(DecodeError::TooShort { need, got: buf.len() });
        }
        let mut bssid = [0u8; 6];
        bssid.copy_from_slice(&buf[1..7]);
        let msg = Self {
            kind: RecordKind::try_from(buf[0])?,
            bssid,
            channel: buf[7],
            // Two's complement, so the cast is the reinterpretation we want.
            #[allow(clippy::cast_possible_wrap)]
            rssi: buf[8] as i8,
            security: Security::from_u8(buf[9]),
            ssid: &buf[11..SIGHTING_RECORD_MIN + usize::from(ssid_len) - 1],
            ext: &buf[SIGHTING_RECORD_MIN + usize::from(ssid_len)..need],
        };
        Ok((msg, need))
    }

    /// Write the record into `out`, returning how many bytes it took.
    ///
    /// `None` if `out` is too small, the SSID is longer than [`SSID_MAX`] or the
    /// trailer is longer than [`EXT_MAX`]; [`SIGHTING_RECORD_MAX`] is always enough.
    /// Nothing is written when it fails, so a caller cannot send a half-formed record.
    /// [`SightingBatchWriter::push`] relies on that.
    #[must_use]
    pub fn encode_record_into(&self, out: &mut [u8]) -> Option<usize> {
        if self.ssid.len() > SSID_MAX || self.ext.len() > EXT_MAX {
            return None;
        }
        let len = SIGHTING_RECORD_MIN + self.ssid.len() + self.ext.len();
        let out = out.get_mut(..len)?;
        out[0] = self.kind.as_u8();
        out[1..7].copy_from_slice(&self.bssid);
        out[7] = self.channel;
        // Two's complement again; `to_le_bytes` on an `i8` is the same byte.
        out[8] = self.rssi.to_le_bytes()[0];
        out[9] = self.security.as_u8();
        // The casts cannot lose data: bounded by SSID_MAX and EXT_MAX.
        #[allow(clippy::cast_possible_truncation)]
        {
            out[10] = self.ssid.len() as u8;
            out[11 + self.ssid.len()] = self.ext.len() as u8;
        }
        out[11..11 + self.ssid.len()].copy_from_slice(self.ssid);
        out[SIGHTING_RECORD_MIN + self.ssid.len()..len].copy_from_slice(self.ext);
        Some(len)
    }
}

/// Node → host: sightings from one dwell or Bluetooth scan, unicast to the bridge.
///
/// A dwell or scan sends as many batches as its sightings fill, and none outlives it. The
/// [`crate::air`] docs say why.
///
/// `"WTUI" | ver | 0x02 | seq:u16le | count:u8 | records…`, each record a
/// [`SightingMsg`].
///
/// [`Self::decode`] validates the whole frame before anything is read out of it.
/// `count` is at least 1 and accounts for every byte, and every record is within
/// [`SIGHTING_RECORD_MIN`]/[`SIGHTING_RECORD_MAX`]. A fault anywhere rejects the whole
/// frame, the same "never half-decoded" rule every frame here follows. [`Self::iter`] is
/// infallible because of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SightingBatch<'a> {
    /// Counts up once per batch the bridge's radio acknowledged, wrapping and
    /// restarting at boot. A batch that went unacknowledged keeps its `seq`. A gap
    /// between two arrivals is batches lost after the bridge's radio took them.
    pub seq: u16,
    /// How many records follow. Always at least 1: a node with nothing to report sends
    /// no batch.
    pub count: u8,
    /// The records, back to back with no padding between them.
    records: &'a [u8],
}

impl<'a> SightingBatch<'a> {
    /// Decode from a received frame, validating every record before returning.
    ///
    /// # Errors
    /// See [`DecodeError`].
    pub fn decode(buf: &'a [u8]) -> Result<Self, DecodeError> {
        match header(buf)? {
            MsgType::SightingBatch => {}
            other => return Err(DecodeError::UnknownType(other.as_u8())),
        }
        if buf.len() < SIGHTING_BATCH_HEADER {
            return Err(DecodeError::TooShort { need: SIGHTING_BATCH_HEADER, got: buf.len() });
        }
        let seq = u16::from_le_bytes([buf[OFF_BODY], buf[OFF_BODY + 1]]);
        let count = buf[OFF_BODY + 2];
        let records = &buf[SIGHTING_BATCH_HEADER..];
        // An empty batch is the same fault as one claiming records it does not
        // carry: a frame this layout cannot produce.
        if count == 0 {
            return Err(DecodeError::TooShort {
                need: SIGHTING_BATCH_HEADER + SIGHTING_RECORD_MIN,
                got: buf.len(),
            });
        }
        let mut consumed = 0usize;
        for _ in 0..count {
            let (_, len) = SightingMsg::decode_record(&records[consumed..])?;
            consumed += len;
        }
        // Bytes past the last record are the batch form of a fixed-length frame
        // that outgrew its layout. Reading them as records would go past what
        // `count` promised.
        if consumed != records.len() {
            return Err(DecodeError::BadLength {
                need: SIGHTING_BATCH_HEADER + consumed,
                got: buf.len(),
            });
        }
        Ok(Self { seq, count, records })
    }

    /// Every record in the batch, paired with its own raw bytes.
    ///
    /// Infallible: [`Self::decode`] already validated every record.
    pub fn iter(&self) -> impl Iterator<Item = (SightingMsg<'a>, &'a [u8])> {
        let mut rest = self.records;
        let mut remaining = self.count;
        core::iter::from_fn(move || {
            if remaining == 0 {
                return None;
            }
            remaining -= 1;
            let (msg, len) =
                SightingMsg::decode_record(rest).expect("validated whole by Self::decode");
            let (raw, tail) = rest.split_at(len);
            rest = tail;
            Some((msg, raw))
        })
    }
}

/// Packs [`SightingMsg`] records into one [`SightingBatch`] frame, up to
/// [`SIGHTING_BATCH_MAX`] bytes.
///
/// [`Self::push`] writes nothing when a record does not fit. The caller then sends what is
/// packed and starts a new batch with the record that did not fit. Whatever is packed
/// goes out when the dwell or Bluetooth scan ends; the [`crate::air`] docs say why
/// nothing is held past it.
#[derive(Debug, Clone)]
pub struct SightingBatchWriter {
    buf: [u8; SIGHTING_BATCH_MAX],
    len: usize,
}

impl SightingBatchWriter {
    /// An empty batch carrying `seq`.
    #[must_use]
    pub fn new(seq: u16) -> Self {
        let mut writer = Self { buf: [0u8; SIGHTING_BATCH_MAX], len: SIGHTING_BATCH_HEADER };
        writer.reset(seq);
        writer
    }

    /// Append one record, saying whether it fit.
    ///
    /// Writes nothing when it does not fit, so the caller can send what it has and
    /// retry the same record in a fresh batch.
    #[must_use]
    pub fn push(&mut self, msg: &SightingMsg<'_>) -> bool {
        let Some(written) = msg.encode_record_into(&mut self.buf[self.len..]) else {
            return false;
        };
        self.len += written;
        self.buf[OFF_BODY + 2] += 1;
        true
    }

    /// How many records have been packed in.
    #[must_use]
    pub fn len(&self) -> usize {
        usize::from(self.buf[OFF_BODY + 2])
    }

    /// Whether nothing has been packed in yet.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The frame as it stands, ready to send.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.buf[..self.len]
    }

    /// Empty the writer and start a new batch carrying `seq`.
    pub fn reset(&mut self, seq: u16) {
        write_header(&mut self.buf, MsgType::SightingBatch);
        self.buf[OFF_BODY..OFF_BODY + 2].copy_from_slice(&seq.to_le_bytes());
        self.buf[OFF_BODY + 2] = 0;
        self.len = SIGHTING_BATCH_HEADER;
    }
}
