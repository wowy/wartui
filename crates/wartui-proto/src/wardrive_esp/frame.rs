//! The WardriveESP GATT identity, and the 12-byte frames a phone and the board exchange.
//!
//! A phone writes a command to [`RX_UUID`] or [`CTRL_UUID`], framed as a CMD or as bare text, and
//! reads output from [`TX_UUID`] notifications, in DATA frames. The board acknowledges each CMD
//! with an ACK of its id ([`Inbound::on_write`]).
//!
//! **Every frame the board sends carries the latest command's id**, DATA frames included, so
//! stream lines carry it too: the protocol's frames echo it, and an app may match output to the
//! command it last sent. [`Inbound`] keeps that id, and any framed write updates it, whatever its
//! type. Bare text names no id, so it leaves it alone.
//!
//! A write is framed only when its magic matches and its length field fits inside it
//! ([`parse_write`]). Anything else is a command in bare text, so a write a phone cut short is
//! still read rather than refused. The version byte, status and flags are not read: the first
//! two bytes and the length are all the framing needs.

/// The WardriveESP GATT service.
pub const SERVICE_UUID: u128 = 0x4768_6F73_7445_5350_4272_6964_6765_5376;
/// RX, phone to board: commands, written with or without response.
pub const RX_UUID: u128 = 0x0147_686F_7374_4553_5042_7269_6467_5852;
/// TX, board to phone, notify: output in DATA frames.
pub const TX_UUID: u128 = 0x0247_686F_7374_4553_5042_7269_6467_5854;
/// CTRL, phone to board: commands, read as RX's are.
pub const CTRL_UUID: u128 = 0x0347_686F_7374_4553_5042_7269_4C52_5443;
/// The status beacon's UUID, advertised in place of [`SERVICE_UUID`] while a central is connected,
/// so an app looking for the service does not try a board that is taken.
pub const STATUS_UUID: u128 = 0x0547_686F_7374_4553_5042_7269_6467_5354;

/// Bytes in a frame's header.
pub const HEADER_LEN: usize = 12;
/// A frame's type: a command.
pub const TYPE_CMD: u8 = 1;
/// A frame's type: the acknowledgment of a command.
pub const TYPE_ACK: u8 = 2;
/// A frame's type: output text.
pub const TYPE_DATA: u8 = 3;

/// The two bytes a frame starts with.
const MAGIC: [u8; 2] = [0x47, 0x42];
/// The version byte the board sends.
const VERSION: u8 = 0x01;
/// The ATT MTU every link has at least.
const MTU_MIN: u16 = 23;
/// ATT bytes ahead of a notification's value: the opcode and the handle.
const ATT_OVERHEAD: usize = 3;

/// What one write to RX or CTRL holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Write<'a> {
    /// A frame, its payload exactly the length its header gives.
    Framed {
        /// The frame's type byte.
        kind: u8,
        /// The command id.
        id: u32,
        /// The payload.
        payload: &'a [u8],
    },
    /// Unframed text: the whole write.
    Text(&'a [u8]),
}

/// What `write` holds. Bytes past a frame's payload are ignored.
#[must_use]
pub fn parse_write(write: &[u8]) -> Write<'_> {
    if write.len() >= HEADER_LEN && write[..2] == MAGIC {
        let len = usize::from(u16::from_le_bytes([write[10], write[11]]));
        if let Some(payload) = write.get(HEADER_LEN..HEADER_LEN + len) {
            let id = u32::from_le_bytes([write[6], write[7], write[8], write[9]]);
            return Write::Framed { kind: write[3], id, payload };
        }
    }
    Write::Text(write)
}

/// A header of `kind`, carrying `id` and a payload of `len`.
const fn header(kind: u8, id: u32, len: u16) -> [u8; HEADER_LEN] {
    let id = id.to_le_bytes();
    let len = len.to_le_bytes();
    [MAGIC[0], MAGIC[1], VERSION, kind, 0, 0, id[0], id[1], id[2], id[3], len[0], len[1]]
}

/// The ACK of the command `id`. It has no payload.
#[must_use]
pub const fn ack(id: u32) -> [u8; HEADER_LEN] {
    header(TYPE_ACK, id, 0)
}

/// The header of a DATA frame carrying `len` bytes, under the command `id`.
#[must_use]
pub const fn data_header(id: u32, len: u16) -> [u8; HEADER_LEN] {
    header(TYPE_DATA, id, len)
}

/// The most output bytes one DATA notification carries at an ATT MTU of `att_mtu`.
#[must_use]
pub const fn data_room(att_mtu: u16) -> usize {
    let mtu = if att_mtu < MTU_MIN { MTU_MIN } else { att_mtu };
    mtu as usize - ATT_OVERHEAD - HEADER_LEN
}

/// What a phone's writes leave: the id the board's frames carry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Inbound {
    reply_id: u32,
}

/// What one write asks of the board.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Taken<'a> {
    /// The ACK to notify, for a CMD frame.
    pub ack: Option<[u8; HEADER_LEN]>,
    /// The command text to run.
    pub command: Option<&'a [u8]>,
}

impl Inbound {
    /// Nothing written yet: frames carry id 1.
    #[must_use]
    pub const fn new() -> Self {
        Self { reply_id: 1 }
    }

    /// The id the board's frames carry.
    #[must_use]
    pub const fn reply_id(&self) -> u32 {
        self.reply_id
    }

    /// Read one write to RX or CTRL.
    ///
    /// A framed write of any type sets the reply id. A CMD is acknowledged and run; another type
    /// does neither. Bare text is run, unacknowledged, and keeps the reply id.
    pub fn on_write<'a>(&mut self, write: &'a [u8]) -> Taken<'a> {
        match parse_write(write) {
            Write::Framed { kind, id, payload } => {
                self.reply_id = id;
                if kind == TYPE_CMD {
                    Taken { ack: Some(ack(id)), command: Some(payload) }
                } else {
                    Taken { ack: None, command: None }
                }
            }
            Write::Text(text) => Taken { ack: None, command: Some(text) },
        }
    }
}

impl Default for Inbound {
    fn default() -> Self {
        Self::new()
    }
}
