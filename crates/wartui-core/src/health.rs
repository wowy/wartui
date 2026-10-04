//! The host's own health: firmware throttling, SoC temperature and battery.
//!
//! The capture host is a HackberryPi CM5 (a Raspberry Pi CM5) in a car, on its own battery and
//! usually plugged in too. A battery or charger sagging under load and a SoC cooking in a parked
//! car are what can go wrong with the host, and nothing else in a capture shows them.
//!
//! `throttled` is the Raspberry Pi firmware's `get_throttled` word, as `vcgencmd get_throttled`
//! prints it. It is asked the way `vcgencmd` asks: a `GET_GENCMD_RESULT` property-mailbox message
//! through the `_IOWR(100, 0, char *)` ioctl on `/dev/vcio_gencmd`. That is the only route the CM5
//! kernel gives an unprivileged user:
//!
//! - sysfs has no `get_throttled` attribute on it.
//! - `/dev/vcio` answers the same ioctl but is root-only.
//! - `/dev/vcio_gencmd` is the gencmd-only node, and opens for an unprivileged user.
//!
//! Spawning `vcgencmd` would fork and exec every sample and depend on its install. The ioctl is the
//! one `unsafe` call in the host workspace, kept to [`mailbox_call`]. `analyze` reads these bits:
//!
//! | bit   | meaning                               |
//! |-------|---------------------------------------|
//! | 0     | under-voltage now                     |
//! | 1     | ARM frequency capped now              |
//! | 2     | throttled now                         |
//! | 3     | soft temperature limit now            |
//! | 16–19 | the same four, at any time since boot |
//!
//! `soc_temp_mc` is thermal zone 0, which is the SoC on a Pi and some sensor or other
//! on anything else running Linux, so a laptop records it too.
//!
//! `battery_mv` and `battery_ma` are the HackberryPi's MAX17048 fuel gauge, hwmon device `battery`:
//! `in0_input` in millivolts and `curr1_input` in milliamps, as the driver reports them. hwmon
//! numbers change between boots, so each sample finds it by its `name` file.
//!
//! Anything missing or unreadable is `None` rather than an error: off a Pi, or off Linux,
//! there is nothing to say, and the capture carries on either way.

use std::path::Path;

/// One reading of the host's health. Every field is `None` where it cannot be read.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Health {
    /// The Raspberry Pi firmware's `get_throttled` word (bits in the module docs). `None` off a Pi.
    pub throttled: Option<u32>,
    /// Thermal zone 0, in thousandths of a degree Celsius.
    pub soc_temp_mc: Option<i32>,
    /// Battery voltage in millivolts, from the hwmon device named `battery`.
    pub battery_mv: Option<i32>,
    /// Battery current in milliamps, from the same device, signed as its driver reports it.
    pub battery_ma: Option<i32>,
}

/// Read the running host's health.
#[must_use]
pub fn read() -> Health {
    Health { throttled: throttled(), ..read_from(Path::new("/")) }
}

/// The sysfs half of [`read`] against a tree rooted at `root` rather than `/`. The
/// throttle word comes from a device, not a file, so it is `None` here.
#[must_use]
pub fn read_from(root: &Path) -> Health {
    let battery = hwmon_named(root, "battery");
    let battery_value = |file: &str| battery.as_ref().and_then(|dir| read_int(&dir.join(file)));
    Health {
        throttled: None,
        soc_temp_mc: read_int(&root.join("sys/class/thermal/thermal_zone0/temp")),
        battery_mv: battery_value("in0_input"),
        battery_ma: battery_value("curr1_input"),
    }
}

fn read_int(path: &Path) -> Option<i32> {
    std::fs::read_to_string(path).ok()?.trim().parse().ok()
}

/// The first `<root>/sys/class/hwmon/*` directory whose `name` file reads exactly `name`.
fn hwmon_named(root: &Path, name: &str) -> Option<std::path::PathBuf> {
    let mut dirs: Vec<_> = std::fs::read_dir(root.join("sys/class/hwmon"))
        .ok()?
        .flatten()
        .map(|entry| entry.path())
        .collect();
    // Sorted, so which one is found does not depend on directory order.
    dirs.sort();
    dirs.into_iter()
        .find(|dir| std::fs::read_to_string(dir.join("name")).is_ok_and(|text| text.trim() == name))
}

/// The mailbox's value buffer, in bytes: room for the command going out and the reply
/// coming back. `vcgencmd` uses the same size.
const MAX_STRING: usize = 1024;

/// Six header words, the value buffer, and the end tag.
const WORDS: usize = 6 + MAX_STRING / 4 + 1;

/// The property tag that runs a gencmd command and returns its text.
const GET_GENCMD_RESULT: u32 = 0x0003_0080;

/// Set in word 1 of a reply the firmware processed.
const RESPONSE_SUCCESS: u32 = 0x8000_0000;

/// `_IOWR(100, 0, char *)`: direction read|write (3) in bits 30–31, the argument size
/// (a pointer) in bits 16–29, type 100 (`0x64`) in bits 8–15, number 0 in bits 0–7.
const MAILBOX_PROPERTY: u32 = (3 << 30) | ((size_of::<*mut u8>() as u32) << 16) | (100 << 8);

#[cfg(target_pointer_width = "64")]
const _: () = assert!(MAILBOX_PROPERTY == 0xC008_6400);

/// A property-mailbox message asking the firmware to run `command`, as `vcgencmd` builds
/// it. A command longer than the buffer allows is cut short to keep its terminating NUL.
fn gencmd_message(command: &str) -> [u32; WORDS] {
    let mut message = [0u32; WORDS];
    message[0] = (WORDS * 4) as u32;
    message[1] = 0; // process request
    message[2] = GET_GENCMD_RESULT;
    message[3] = MAX_STRING as u32;
    message[4] = 0; // request length
    message[5] = 0; // error response
    let mut text = [0u8; MAX_STRING];
    let len = command.len().min(MAX_STRING - 1);
    text[..len].copy_from_slice(&command.as_bytes()[..len]);
    let (chunks, _) = text.as_chunks::<4>();
    for (word, bytes) in message[6..6 + MAX_STRING / 4].iter_mut().zip(chunks) {
        *word = u32::from_ne_bytes(*bytes);
    }
    // The last word is the end tag, already 0.
    message
}

/// The word in a `get_throttled` reply: `throttled=0x…` as hex, or `None` when the
/// firmware did not process the message, reported an error, or said something else.
fn parse_throttled(reply: &[u32]) -> Option<u32> {
    if reply.get(1)? & RESPONSE_SUCCESS == 0 || *reply.get(5)? != 0 {
        return None;
    }
    let bytes: Vec<u8> = reply.get(6..)?.iter().flat_map(|word| word.to_ne_bytes()).collect();
    let text = &bytes[..bytes.iter().position(|&b| b == 0)?];
    let hex = std::str::from_utf8(text).ok()?.trim().strip_prefix("throttled=0x")?;
    u32::from_str_radix(hex, 16).ok()
}

/// The firmware's throttle word via `/dev/vcio_gencmd`, opened afresh each sample: cheap next to
/// five seconds, and a node that appears or gains permissions mid-capture is picked up.
#[cfg(target_os = "linux")]
fn throttled() -> Option<u32> {
    let device = std::fs::File::open("/dev/vcio_gencmd").ok()?;
    let mut message = gencmd_message("get_throttled");
    mailbox_call(&device, &mut message).then(|| parse_throttled(&message))?
}

#[cfg(not(target_os = "linux"))]
fn throttled() -> Option<u32> {
    None
}

/// Hand `message` to the firmware's property mailbox, which overwrites it with the reply.
/// `false` when the ioctl fails.
#[cfg(target_os = "linux")]
#[allow(unsafe_code, reason = "the mailbox is reachable only through an ioctl")]
fn mailbox_call(device: &std::fs::File, message: &mut [u32; WORDS]) -> bool {
    use std::os::fd::AsRawFd;
    // The driver copies as many bytes as word 0 says, in and back out.
    message[0] = size_of_val(message) as u32;
    // SAFETY: `device` is an open file, so its descriptor is valid for the call. The
    // argument is a pointer to `message` itself, which the driver reads and writes for
    // the byte count in its word 0, set just above to exactly the array's size. The
    // exclusive borrow keeps the array alive and unaliased until the ioctl returns.
    let status = unsafe {
        libc::ioctl(device.as_raw_fd(), MAILBOX_PROPERTY as libc::Ioctl, message.as_mut_ptr())
    };
    status >= 0
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{
        GET_GENCMD_RESULT, Health, MAX_STRING, RESPONSE_SUCCESS, WORDS, gencmd_message,
        parse_throttled, read_from,
    };

    fn put(root: &Path, path: &str, contents: &str) {
        let path = root.join(path);
        std::fs::create_dir_all(path.parent().expect("a parent")).expect("creating dirs");
        std::fs::write(path, contents).expect("writing");
    }

    fn bytes(words: &[u32]) -> Vec<u8> {
        words.iter().flat_map(|word| word.to_ne_bytes()).collect()
    }

    /// A reply the firmware processed, carrying `text` in the value buffer.
    fn reply(text: &str) -> [u32; WORDS] {
        let mut reply = gencmd_message(text);
        reply[1] = RESPONSE_SUCCESS;
        reply
    }

    #[test]
    fn gencmd_message_lays_out_header_command_and_end_tag() {
        let message = gencmd_message("get_throttled");
        assert_eq!(message[0] as usize, size_of_val(&message));
        assert_eq!(message[..6], [message[0], 0, GET_GENCMD_RESULT, MAX_STRING as u32, 0, 0]);
        let raw = bytes(&message);
        assert_eq!(&raw[24..24 + 14], b"get_throttled\0");
        assert!(raw[24 + 14..24 + MAX_STRING].iter().all(|&b| b == 0));
        assert_eq!(message[WORDS - 1], 0);
    }

    #[test]
    fn gencmd_message_keeps_terminating_nul_when_command_overflows() {
        let message = gencmd_message(&"x".repeat(MAX_STRING + 10));
        let raw = bytes(&message);
        assert_eq!(raw[24 + MAX_STRING - 1], 0);
        assert_eq!(message[WORDS - 1], 0);
    }

    #[test]
    fn parse_throttled_reads_word_when_reply_succeeds() {
        assert_eq!(parse_throttled(&reply("throttled=0x50005")), Some(0x50005));
        assert_eq!(parse_throttled(&reply("throttled=0x0")), Some(0));
    }

    #[test]
    fn parse_throttled_returns_none_when_error_word_set() {
        let mut reply = reply("throttled=0x0");
        reply[5] = 1;
        assert_eq!(parse_throttled(&reply), None);
    }

    #[test]
    fn parse_throttled_returns_none_when_response_flag_missing() {
        let mut reply = reply("throttled=0x0");
        reply[1] = 0;
        assert_eq!(parse_throttled(&reply), None);
    }

    #[test]
    fn parse_throttled_returns_none_when_text_is_garbage() {
        assert_eq!(parse_throttled(&reply("error=1 error_msg=\"no\"")), None);
        assert_eq!(parse_throttled(&reply("throttled=0xzz")), None);
        assert_eq!(parse_throttled(&[RESPONSE_SUCCESS; 3]), None);
    }

    #[test]
    fn read_from_finds_battery_by_name_when_hwmon_number_differs() {
        let dir = tempfile::tempdir().expect("temp dir");
        put(dir.path(), "sys/class/hwmon/hwmon0/name", "nvme\n");
        put(dir.path(), "sys/class/hwmon/hwmon0/in0_input", "1\n");
        put(dir.path(), "sys/class/hwmon/hwmon4/name", "battery\n");
        put(dir.path(), "sys/class/hwmon/hwmon4/in0_input", "4185\n");
        put(dir.path(), "sys/class/hwmon/hwmon4/curr1_input", "-21\n");
        put(dir.path(), "sys/class/thermal/thermal_zone0/temp", "61234\n");
        assert_eq!(
            read_from(dir.path()),
            Health {
                throttled: None,
                soc_temp_mc: Some(61234),
                battery_mv: Some(4185),
                battery_ma: Some(-21),
            }
        );
    }

    #[test]
    fn read_from_reads_no_battery_when_none_named_battery() {
        let dir = tempfile::tempdir().expect("temp dir");
        put(dir.path(), "sys/class/hwmon/hwmon5/name", "max17048_mains\n");
        put(dir.path(), "sys/class/hwmon/hwmon5/in0_input", "5000\n");
        put(dir.path(), "sys/class/thermal/thermal_zone0/temp", "48000\n");
        assert_eq!(read_from(dir.path()), Health { soc_temp_mc: Some(48000), ..Health::default() });
    }

    #[test]
    fn read_from_reads_none_when_files_missing() {
        let dir = tempfile::tempdir().expect("temp dir");
        assert_eq!(read_from(dir.path()), Health::default());
    }

    #[test]
    fn read_from_reads_none_when_contents_garbage() {
        let dir = tempfile::tempdir().expect("temp dir");
        put(dir.path(), "sys/class/hwmon/hwmon4/name", "battery\n");
        put(dir.path(), "sys/class/hwmon/hwmon4/in0_input", "full\n");
        put(dir.path(), "sys/class/thermal/thermal_zone0/temp", "warm\n");
        assert_eq!(read_from(dir.path()), Health::default());
    }
}
