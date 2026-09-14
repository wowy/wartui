//! What the kernel saw the benchmark write, which is the number a slow card cares about.
//!
//! SQLite can say how long a commit took but not what the kernel made of it, and that is
//! the part a microSD card is slow at: many small writes scattered across a file cost it
//! far more than the same bytes in a few long ones. Linux counts both halves without any
//! help — `/proc/self/io` for what this process asked of the kernel, and the block
//! device's own `stat` for what reached the card. Neither exists elsewhere, and the report
//! says so rather than printing zeros that read as a measurement.
//!
//! The device counters are the whole device's, not this process's. Anything else writing
//! to the same card lands in them too, which on a Pi booted from that card is journald at
//! the least.

use std::path::Path;

/// What this process asked the kernel to write, from `/proc/self/io`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ProcessIo {
    /// `write`-family syscalls, `syscw`.
    pub write_calls: u64,
    /// Bytes passed to them, `wchar`, whether or not they reached storage.
    pub written: u64,
    /// Bytes this process caused to be sent to storage, `write_bytes`.
    pub storage_bytes: u64,
}

impl ProcessIo {
    /// The counters as they stand, or `None` where the kernel does not keep them.
    pub fn read() -> Option<Self> {
        parse_process_io(&std::fs::read_to_string("/proc/self/io").ok()?)
    }

    /// How much each counter moved since `earlier`.
    pub const fn since(self, earlier: Self) -> Self {
        Self {
            write_calls: self.write_calls.saturating_sub(earlier.write_calls),
            written: self.written.saturating_sub(earlier.written),
            storage_bytes: self.storage_bytes.saturating_sub(earlier.storage_bytes),
        }
    }
}

/// Parse `/proc/self/io`, which is `key: value` a line.
pub fn parse_process_io(text: &str) -> Option<ProcessIo> {
    let (mut calls, mut written, mut storage) = (None, None, None);
    for line in text.lines() {
        let Some((key, value)) = line.split_once(':') else { continue };
        let Ok(value) = value.trim().parse::<u64>() else { continue };
        match key.trim() {
            "syscw" => calls = Some(value),
            "wchar" => written = Some(value),
            "write_bytes" => storage = Some(value),
            _ => {}
        }
    }
    Some(ProcessIo { write_calls: calls?, written: written?, storage_bytes: storage? })
}

/// What a block device has written, from its `stat` file.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DeviceIo {
    /// Write requests completed. Adjacent writes the kernel merged count once, which is
    /// exactly why this and `sectors` together say how sequential the load was.
    pub writes: u64,
    /// Sectors written. Always 512 bytes in this file, whatever the device's own size.
    pub sectors: u64,
    /// Milliseconds spent on writes, summed across requests.
    pub write_ms: u64,
    /// Flush requests completed, which kernels before 5.5 do not count.
    pub flushes: Option<u64>,
}

impl DeviceIo {
    /// How much each counter moved since `earlier`.
    pub fn since(self, earlier: Self) -> Self {
        Self {
            writes: self.writes.saturating_sub(earlier.writes),
            sectors: self.sectors.saturating_sub(earlier.sectors),
            write_ms: self.write_ms.saturating_sub(earlier.write_ms),
            flushes: self.flushes.zip(earlier.flushes).map(|(now, then)| now.saturating_sub(then)),
        }
    }
}

/// Parse a block device's `stat` file: whitespace-separated counters, of which writes
/// are the fifth to eighth and flushes the sixteenth
/// (`Documentation/block/stat.rst`).
pub fn parse_device_stat(text: &str) -> Option<DeviceIo> {
    let fields: Vec<u64> =
        text.split_whitespace().map(str::parse).collect::<Result<_, _>>().ok()?;
    Some(DeviceIo {
        writes: *fields.get(4)?,
        sectors: *fields.get(6)?,
        write_ms: *fields.get(7)?,
        flushes: fields.get(15).copied(),
    })
}

/// The block device a file lives on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Device {
    /// The kernel's name for it, `mmcblk0p2` or `sda1`.
    pub name: String,
    major: u64,
    minor: u64,
}

impl Device {
    /// The device holding `path`, if the kernel will say. A filesystem with no single
    /// block device behind it — tmpfs, overlay, a network mount — has none.
    #[cfg(unix)]
    pub fn holding(path: &Path) -> Option<Self> {
        use std::os::unix::fs::MetadataExt;
        let (major, minor) = split_dev(std::fs::metadata(path).ok()?.dev());
        let link = std::fs::read_link(format!("/sys/dev/block/{major}:{minor}")).ok()?;
        let name = link.file_name()?.to_string_lossy().into_owned();
        Some(Self { name, major, minor })
    }

    /// Without `st_dev` there is no device to name.
    #[cfg(not(unix))]
    pub fn holding(_path: &Path) -> Option<Self> {
        None
    }

    /// The device's counters as they stand.
    pub fn stat(&self) -> Option<DeviceIo> {
        let path = format!("/sys/dev/block/{}:{}/stat", self.major, self.minor);
        parse_device_stat(&std::fs::read_to_string(path).ok()?)
    }
}

/// Split a Linux `dev_t` into major and minor, laid out as glibc's `gnu_dev_major` and
/// `gnu_dev_minor` read it. Written out rather than called because the call is `unsafe`
/// and the layout is four shifts.
pub const fn split_dev(dev: u64) -> (u64, u64) {
    let major = ((dev >> 32) & 0xFFFF_F000) | ((dev >> 8) & 0x0FFF);
    let minor = ((dev >> 12) & 0xFFFF_FF00) | (dev & 0xFF);
    (major, minor)
}

/// The kernel release, `6.6.31+rpt-rpi-v8` or similar.
pub fn kernel_release() -> Option<String> {
    Some(std::fs::read_to_string("/proc/sys/kernel/osrelease").ok()?.trim().to_owned())
}

/// This process's peak resident set, in KiB, from `VmHWM` in `/proc/self/status`.
///
/// The whole process's, simulator and engine included, so it bounds what the store's
/// cache costs rather than isolating it.
pub fn peak_rss_kib() -> Option<u64> {
    parse_peak_rss(&std::fs::read_to_string("/proc/self/status").ok()?)
}

fn parse_peak_rss(text: &str) -> Option<u64> {
    let line = text.lines().find_map(|line| line.strip_prefix("VmHWM:"))?;
    line.trim().strip_suffix("kB")?.trim().parse().ok()
}

#[cfg(test)]
mod tests {
    use super::{
        DeviceIo, ProcessIo, parse_device_stat, parse_peak_rss, parse_process_io, split_dev,
    };

    /// glibc's `gnu_dev_makedev`, the inverse of what is under test.
    const fn makedev(major: u64, minor: u64) -> u64 {
        ((major & 0xFFF) << 8) | ((major & !0xFFF) << 32) | (minor & 0xFF) | ((minor & !0xFF) << 12)
    }

    #[test]
    fn a_small_device_number_splits_into_the_pair_ls_shows() {
        // An SD card's second partition.
        assert_eq!(split_dev(makedev(179, 2)), (179, 2));
        assert_eq!(split_dev(45_826), (179, 2));
    }

    #[test]
    fn a_device_number_past_the_old_sixteen_bits_still_splits() {
        assert_eq!(split_dev(makedev(259, 70_000)), (259, 70_000));
        assert_eq!(split_dev(makedev(0x1234, 0x0056_789A)), (0x1234, 0x0056_789A));
    }

    #[test]
    fn the_process_counters_are_read_by_name_not_position() {
        let text = "rchar: 10\nwchar: 2048\nsyscr: 3\nsyscw: 17\nread_bytes: 0\n\
                    write_bytes: 4096\ncancelled_write_bytes: 0\n";
        assert_eq!(
            parse_process_io(text),
            Some(ProcessIo { write_calls: 17, written: 2048, storage_bytes: 4096 })
        );
    }

    #[test]
    fn a_process_file_missing_a_counter_is_not_read_as_zero() {
        assert_eq!(parse_process_io("rchar: 10\nwchar: 2048\n"), None);
    }

    #[test]
    fn a_modern_device_stat_includes_flushes() {
        let text = "    4200     120  336000    1500    9001     700 1234567   88000        0   \
                    51000   90000       0       0        0       0      312     4100\n";
        assert_eq!(
            parse_device_stat(text),
            Some(DeviceIo {
                writes: 9001,
                sectors: 1_234_567,
                write_ms: 88_000,
                flushes: Some(312)
            })
        );
    }

    #[test]
    fn an_old_kernels_device_stat_has_no_flushes_and_still_parses() {
        let text = "4200 120 336000 1500 9001 700 1234567 88000 0 51000 90000";
        let io = parse_device_stat(text).expect("eleven fields are enough");
        assert_eq!(io.flushes, None);
        assert_eq!(io.writes, 9001);
    }

    #[test]
    fn device_counters_subtract_and_an_unknown_flush_count_stays_unknown() {
        let before = DeviceIo { writes: 10, sectors: 80, write_ms: 5, flushes: None };
        let after = DeviceIo { writes: 25, sectors: 200, write_ms: 9, flushes: Some(4) };
        assert_eq!(
            after.since(before),
            DeviceIo { writes: 15, sectors: 120, write_ms: 4, flushes: None }
        );
    }

    #[test]
    fn the_peak_resident_set_is_read_in_kib() {
        let text =
            "Name:\twartui\nVmPeak:\t  900000 kB\nVmHWM:\t   12345 kB\nVmRSS:\t   11000 kB\n";
        assert_eq!(parse_peak_rss(text), Some(12_345));
    }
}
