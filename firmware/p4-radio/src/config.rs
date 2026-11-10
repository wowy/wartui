//! The C6 firmware's build-time settings, in one place.
//!
//! Each is read from a `WARTUI_RADIO_*` environment variable at build time, with the default
//! here, so a bench run changes it with a rebuild rather than an edit.
//! `wartui_proto::config` parses the sizes; the interval's parser is here. A value that does not
//! parse panics during const evaluation, so a typo is a build error.
//!
//! A bench `wartui status` reports the queue's high-water mark, which is how to right-size it.

use wartui_proto::config::env_usize;

/// Bytes of encoded `Up` records — the C6's lines and its status — waiting for a packet to
/// the P4.
///
/// A full queue evicts its oldest record, counted in `RadioStatus.queues`. These records are
/// small and few, so a backlog this deep means the link has stopped.
pub const UP_QUEUE_BYTES: usize = env_usize(option_env!("WARTUI_RADIO_UP_QUEUE_BYTES"), 8 * 1024);

/// Bytes of the host stream held for a Bluetooth central until they are notified.
///
/// The P4 never has more than its own `BLE_WINDOW_BYTES` uncredited
/// (`firmware/p4-bridge/src/config.rs`), so this matches its default. A smaller one drops what
/// overflows, counted in `BleStats.dropped_overflow`.
pub const BLE_WINDOW_BYTES: usize =
    env_usize(option_env!("WARTUI_RADIO_BLE_WINDOW_BYTES"), 16 * 1024);

/// The connection interval range, in microseconds, the C6 asks a central for once it connects;
/// `None` takes the central's.
///
/// `WARTUI_RADIO_CONN_INTERVAL_MS` is `MIN-MAX` in milliseconds, each a multiple of 1.25 ms
/// from 7.5 ms, such as `7.5-15`. The request asks for peripheral latency 0 and a 4 s
/// supervision timeout, which must exceed twice the interval, so the interval stays under 2 s.
///
/// Apple's Accessory Design Guidelines, from memory: an iOS central takes a range only when
/// the minimum is at least 15 ms, the maximum at least 15 ms above it (or both 15 ms), and the
/// maximum times three under the supervision timeout.
pub const CONN_INTERVAL_US: Option<(u64, u64)> =
    interval_range(option_env!("WARTUI_RADIO_CONN_INTERVAL_MS"));

const INTERVAL_FORMAT: &str =
    "WARTUI_RADIO_CONN_INTERVAL_MS must be MIN-MAX ms, multiples of 1.25 in 7.5..2000, like 7.5-15";

/// `MIN-MAX` in milliseconds, as microseconds.
const fn interval_range(value: Option<&str>) -> Option<(u64, u64)> {
    let Some(text) = value else {
        return None;
    };
    let bytes = text.as_bytes();
    let mut dash = 0;
    while dash < bytes.len() && bytes[dash] != b'-' {
        dash += 1;
    }
    assert!(dash < bytes.len(), "{}", INTERVAL_FORMAT);
    let min = millis_as_micros(bytes, 0, dash);
    let max = millis_as_micros(bytes, dash + 1, bytes.len());
    assert!(min.is_multiple_of(1250) && max.is_multiple_of(1250), "{}", INTERVAL_FORMAT);
    assert!(7_500 <= min && min <= max && max < 2_000_000, "{}", INTERVAL_FORMAT);
    Some((min, max))
}

/// `bytes[start..end]`, a decimal number of milliseconds with at most three decimals, as
/// microseconds.
const fn millis_as_micros(bytes: &[u8], start: usize, end: usize) -> u64 {
    let mut micros = 0u64;
    // Past the point, the unit in microseconds of the last digit read.
    let mut unit: Option<u64> = None;
    let mut digits = false;
    let mut i = start;
    while i < end {
        match (bytes[i], unit) {
            (b'0'..=b'9', None) => micros = micros * 10 + (bytes[i] - b'0') as u64,
            (b'0'..=b'9', Some(scale)) if scale > 1 => {
                micros += (bytes[i] - b'0') as u64 * (scale / 10);
                unit = Some(scale / 10);
            }
            (b'.', None) => {
                micros *= 1000;
                unit = Some(1000);
            }
            _ => panic!("{}", INTERVAL_FORMAT),
        }
        digits |= bytes[i] != b'.';
        i += 1;
    }
    assert!(digits, "{}", INTERVAL_FORMAT);
    if unit.is_none() {
        micros *= 1000;
    }
    micros
}

// The firmware has no host test target, so the parser's cases are checked as it builds.
const _: () = assert!(matches!(interval_range(Some("7.5-15")), Some((7_500, 15_000))));
const _: () = assert!(matches!(interval_range(Some("11.25-11.25")), Some((11_250, 11_250))));
const _: () = assert!(interval_range(None).is_none());
