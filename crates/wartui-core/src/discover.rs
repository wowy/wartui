//! Finding an NMEA receiver among the host's serial ports.
//!
//! **A receiver is recognised by what it says, not by what it is called.** The
//! common pucks sit behind a general-purpose USB-to-UART bridge — a Globalsat
//! BU-353 is a Prolific, and a bare module is usually a CP210x or a CH340 — and
//! those vendor IDs are shared with every other adapter on the bus. So a vendor
//! table can order the search and can never decide it. What decides it is reading
//! the port and finding NMEA.
//!
//! **The probe writes nothing**, which is the difference from the bridge's — that
//! one has to ask before anything answers. Opening is not free of consequence even
//! so: the kernel raises DTR and RTS on any tty it opens, so a board wired for
//! auto-reset on DTR reboots when this listens to it. An ESP32's USB Serial/JTAG
//! does not, and `wartui_bridge::ports` has why. A port another program holds fails
//! to open and is passed over. That cost is the reason the search is kept to ports
//! that could plausibly be a receiver rather than pointed at every device node.
//!
//! **It never opens an Espressif port.** [`wartui_bridge::ports::could_be_a_receiver`]
//! is the complement of the predicate the bridge sweeps by, so the fleet's own
//! boards are not in this list at all — a node is the one device that must never be
//! opened by something looking for a GPS, because the sweep looking for a bridge is
//! writing into whatever it opens and the two must not meet on one port.

use std::time::Duration;

use wartui_bridge::ports::{self, PortCandidate};

use crate::nmea::Nmea;

/// Line rates to try, in the order they are worth trying.
///
/// 9600 is what most receivers ship at and 38400 is what u-blox modules are often
/// configured to; 4800 is the NMEA 0183 standard rate and still turns up on older
/// pucks; 115200 is the rest. A USB-CDC receiver ignores the rate altogether, so
/// the ladder costs nothing on one and is the whole difference on a UART.
pub const BAUD_LADDER: [u32; 4] = [9_600, 38_400, 4_800, 115_200];

/// How many sentences have to pass their checksum before an *unknown* port is
/// believed to be a receiver.
///
/// Two, not one. The checksum is an eight-bit XOR over the body, so a device
/// spewing text agrees with one by chance about once in 256 lines — often enough to
/// matter when four rates are tried against every port attached. Two in one window
/// is not chance.
///
/// A port the operator named is a different question. They have already said it is
/// a receiver; all that is left to find out is the rate, and one sentence that
/// passes its checksum answers that. Asking for two would rule out a receiver
/// emitting a single sentence a second — a real configuration, and one the reader
/// handled perfectly well before it was ever asked to search.
pub const SENTENCES_TO_BELIEVE: usize = 2;

/// How many are enough on a port the operator named. See [`SENTENCES_TO_BELIEVE`].
pub const SENTENCES_ON_A_NAMED_PORT: usize = 1;

/// How long one port at one rate is listened to.
///
/// Comfortably more than one cycle of a 1 Hz receiver, so a whole sentence lands
/// inside it even when the window opens in the middle of one.
pub const PROBE_WINDOW: Duration = Duration::from_millis(1_200);

/// Words a device puts in its own USB strings when it wants to be found.
const HINTS: [&str; 6] = ["gps", "gnss", "glonass", "u-blox", "ublox", "navi"];

/// Vendors whose devices are only ever receivers.
const RECEIVER_VIDS: [u16; 3] = [
    0x1546, // u-blox
    0x091E, // Garmin
    0x1BD2, // STMicroelectronics-based SiRF pucks
];

/// How many sentences in this sample passed their checksum.
///
/// Lines are reassembled the way the reader does it, so a sample that begins or
/// ends mid-sentence is treated exactly as the live stream would treat it — the
/// leading fragment fails, as it must.
#[must_use]
pub fn sentences_in(sample: &[u8]) -> usize {
    let mut lines = crate::gps::Lines::default();
    let mut nmea = Nmea::new();
    let mut believed = 0;
    lines.push(sample, |line| {
        if nmea.parse(line).is_ok() {
            believed += 1;
        }
    });
    believed
}

/// Whether this sample is an unknown port turning out to be a receiver.
#[must_use]
pub fn looks_like_nmea(sample: &[u8]) -> bool {
    sentences_in(sample) >= SENTENCES_TO_BELIEVE
}

/// The ports worth listening to, best first.
///
/// `reserved` is whatever the operator named as the bridge. Espressif boards are
/// already absent — that is `could_be_a_receiver` — but a `--bridge` given as a bare
/// path is opened as given, whatever it is, so it has to be named here too.
#[must_use]
pub fn candidates(attached: Vec<PortCandidate>, reserved: &[String]) -> Vec<PortCandidate> {
    let mut found: Vec<PortCandidate> = attached
        .into_iter()
        .filter(ports::could_be_a_receiver)
        .filter(|candidate| {
            !reserved.iter().any(|held| {
                same_device(held, &candidate.path) || same_device(held, &candidate.device)
            })
        })
        .collect();
    // Sorted rather than filtered: the hint decides what is tried first and nothing
    // else. A bare module behind a CP2102 says nothing about itself and is still the
    // receiver more often than not.
    found.sort_by_key(|candidate| (rank(candidate), candidate.path.clone()));
    found
}

/// Whether two names reach one device.
///
/// A port answers to several: the device node, a `by-id` link and a `by-path` one.
/// The operator names whichever they have to hand and the enumeration names
/// whichever is most stable, so comparing the strings alone reserves nothing half
/// the time — and the half it misses is the half where this opens the bridge's port
/// while the transport is using it.
fn same_device(one: &str, other: &str) -> bool {
    if one == other {
        return true;
    }
    match (std::fs::canonicalize(one), std::fs::canonicalize(other)) {
        (Ok(one), Ok(other)) => one == other,
        // A name that resolves to nothing is not the same device as anything, and
        // two that both resolve to nothing are not each other.
        _ => false,
    }
}

/// How promising a port looks before anything has been read from it.
fn rank(candidate: &PortCandidate) -> u8 {
    let says_so = |text: &Option<String>| {
        text.as_deref().is_some_and(|text| {
            let text = text.to_ascii_lowercase();
            HINTS.iter().any(|hint| text.contains(hint))
        })
    };
    if says_so(&candidate.product) || says_so(&candidate.manufacturer) {
        return 0;
    }
    if candidate.vid.is_some_and(|vid| RECEIVER_VIDS.contains(&vid)) {
        return 1;
    }
    2
}

/// Try each port at each rate until one of them is a receiver.
///
/// `sample` is the reading, kept out of here so the choosing can be tested against
/// recorded streams rather than against whatever is plugged into the machine
/// running the suite.
pub fn settle<'a>(
    ports: &'a [PortCandidate],
    ladder: &[u32],
    needed: usize,
    mut sample: impl FnMut(&str, u32) -> Vec<u8>,
) -> Option<(&'a str, u32)> {
    for candidate in ports {
        for &baud in ladder {
            if sentences_in(&sample(&candidate.path, baud)) >= needed {
                return Some((&candidate.path, baud));
            }
        }
    }
    None
}
