//! Finding an NMEA receiver among the host's serial ports.
//!
//! **A receiver is recognised by what it says, not what it is called.** Common pucks sit behind a
//! general-purpose USB-to-UART bridge (a Globalsat BU-353 is a Prolific, a bare module usually a
//! CP210x or CH340), and those vendor IDs are shared with every other adapter. So a vendor table
//! can order the search but never decide it. Reading NMEA off the port decides it.
//!
//! **The probe writes nothing**, unlike the bridge's, which must ask before anything answers.
//! Opening still has consequences: the kernel raises DTR and RTS on any tty it opens, so a board
//! wired for auto-reset on DTR reboots when listened to. An ESP32's USB Serial/JTAG does not
//! (`wartui_bridge::ports` says why). A port another program holds fails to open and is passed
//! over. That cost is why the search is kept to plausible receivers, not every device node.
//!
//! **It never opens an Espressif port.** [`wartui_bridge::ports::could_be_a_receiver`] is the
//! complement of the bridge sweep's predicate, so the fleet's boards are never listed. The bridge
//! sweep writes into whatever it opens, and the two must not meet on one port.

use std::time::Duration;

use wartui_bridge::ports::{self, PortCandidate};

use crate::nmea::Nmea;

/// Line rates to try, in the order they are worth trying.
///
/// 9600 is what most receivers ship at, and u-blox modules are often set to 38400. 4800 is the NMEA
/// 0183 standard and turns up on older pucks, and 115200 is the rest. A USB-CDC receiver ignores
/// the rate, so the ladder costs nothing there and is the whole difference on a UART.
pub const BAUD_LADDER: [u32; 4] = [9_600, 38_400, 4_800, 115_200];

/// How many sentences must pass their checksum before an *unknown* port is believed to be a
/// receiver.
///
/// Two, not one. The checksum is an eight-bit XOR, so text spew passes about once in 256 lines,
/// often enough with four rates tried on every port. Two in one window is not chance. A port the
/// operator named needs only its rate found, and one valid sentence answers that. Two would rule
/// out a receiver set to one sentence a second, a real configuration.
pub const SENTENCES_TO_BELIEVE: usize = 2;

/// How many are enough on a port the operator named. See [`SENTENCES_TO_BELIEVE`].
pub const SENTENCES_ON_A_NAMED_PORT: usize = 1;

/// How long one port at one rate is listened to.
/// How long one port at one rate is listened to: well over one cycle of a 1 Hz receiver, so a whole
/// sentence lands even if the window opens mid-sentence.
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
/// Lines are reassembled as the reader does, so a sample starting or ending mid-sentence is treated
/// as the live stream would treat it: the leading fragment fails.
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
/// `reserved` is whatever the operator named as the bridge. Espressif boards are already absent
/// (`could_be_a_receiver`), but a `--bridge` given as a bare path is opened as given, so it is
/// named here too.
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
    // Sorted, not filtered: the hint only decides order. A bare module behind a CP2102 says nothing
    // about itself and is still usually the receiver.
    found.sort_by_key(|candidate| (rank(candidate), candidate.path.clone()));
    found
}

/// Whether two names reach one device. A port answers to its device node, a `by-id` link and a
/// `by-path` one. The operator names whichever is handy and the enumeration the most stable, so
/// comparing strings misses half the time, and that half opens the bridge's port under the
/// transport.
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

/// Try each port at each rate until one is a receiver. `sample` does the reading, kept out so the
/// choosing is tested against recorded streams, not whatever is plugged in.
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
