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

use std::path::{Path, PathBuf};

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
    /// The device holding `path`, if the kernel will say.
    ///
    /// Asked by number first, which is how ext4 and most filesystems answer. btrfs gives
    /// every file an anonymous device number that names no block device, so failing
    /// that the device is the source the filesystem was mounted from. A btrfs spanning
    /// several devices names only the one it was mounted by, and a filesystem with no
    /// block device behind it at all — tmpfs, overlay, a network mount — has none.
    pub fn holding(path: &Path) -> Option<Self> {
        let (major, minor) = dev_of(path)?;
        Self::numbered(major, minor).or_else(|| Self::named(&Mount::holding(path)?.source))
    }

    fn numbered(major: u64, minor: u64) -> Option<Self> {
        let link = std::fs::read_link(format!("/sys/dev/block/{major}:{minor}")).ok()?;
        let name = link.file_name()?.to_string_lossy().into_owned();
        Some(Self { name, major, minor })
    }

    /// The device at a path such as `/dev/nvme0n1p3`. Canonicalised first, because
    /// `/dev/mapper/root` is a link and sysfs knows the device as `dm-0`.
    fn named(source: &str) -> Option<Self> {
        let path = std::fs::canonicalize(source).ok()?;
        let name = path.file_name()?.to_str()?.to_owned();
        let numbers = std::fs::read_to_string(format!("/sys/class/block/{name}/dev")).ok()?;
        let (major, minor) = parse_dev_numbers(&numbers)?;
        Some(Self { name, major, minor })
    }

    /// The device's counters as they stand.
    pub fn stat(&self) -> Option<DeviceIo> {
        let path = format!("/sys/dev/block/{}:{}/stat", self.major, self.minor);
        parse_device_stat(&std::fs::read_to_string(path).ok()?)
    }
}

/// A device number as sysfs and `mountinfo` write it, `259:3`.
fn parse_dev_numbers(text: &str) -> Option<(u64, u64)> {
    let (major, minor) = text.trim().split_once(':')?;
    Some((major.parse().ok()?, minor.parse().ok()?))
}

/// The device number of the filesystem holding `path`.
#[cfg(unix)]
fn dev_of(path: &Path) -> Option<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    Some(split_dev(std::fs::metadata(path).ok()?.dev()))
}

/// Without `st_dev` there is no device to name.
#[cfg(not(unix))]
fn dev_of(_path: &Path) -> Option<(u64, u64)> {
    None
}

/// The filesystem a file lives on, as `/proc/self/mountinfo` describes its mount.
///
/// Reported because the same card behaves differently under another filesystem or other
/// mount options — btrfs compressing and copying on write is not ext4 overwriting in
/// place — and a run that does not say which is not comparable with the next.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mount {
    /// `ext4`, `btrfs`.
    pub fstype: String,
    /// What was mounted, usually a device path.
    pub source: String,
    /// This mount's own options, such as `rw,noatime`.
    pub options: String,
    /// The filesystem's options, such as btrfs's `compress=zstd:1`.
    pub fs_options: String,
    /// Where it is mounted.
    pub point: PathBuf,
}

impl Mount {
    /// The mount holding `path`, where the kernel keeps `mountinfo`.
    ///
    /// By device number first, which is exact. That is not enough for btrfs: each
    /// subvolume gives its files a device number of its own, while `mountinfo` lists the
    /// filesystem's, so a database under a subvolume such as Fedora's `/home` matches no
    /// line. Failing the number, the mount is the one whose mount point is the deepest
    /// directory above the path, which is how `findmnt --target` answers.
    pub fn holding(path: &Path) -> Option<Self> {
        let mountinfo = std::fs::read_to_string("/proc/self/mountinfo").ok()?;
        let (major, minor) = dev_of(path)?;
        find_mount(&mountinfo, major, minor)
            .or_else(|| enclosing_mount(&mountinfo, &std::fs::canonicalize(path).ok()?))
    }
}

/// The mount with this device number. A filesystem mounted more than once — a bind
/// mount — has entries that agree on everything reported, so the last is as good as any.
fn find_mount(mountinfo: &str, major: u64, minor: u64) -> Option<Mount> {
    mountinfo
        .lines()
        .filter_map(parse_mount)
        .filter(|(numbers, _)| *numbers == (major, minor))
        .map(|(_, mount)| mount)
        .next_back()
}

/// The mount whose mount point is the deepest directory above `path`, which must be
/// absolute and canonical. Compared by whole components, so `/homework` is not under
/// `/home`. A later mount over the same point hides an earlier one, so a tie goes to the
/// later.
fn enclosing_mount(mountinfo: &str, path: &Path) -> Option<Mount> {
    mountinfo
        .lines()
        .filter_map(parse_mount)
        .map(|(_, mount)| mount)
        .filter(|mount| path.starts_with(&mount.point))
        .fold(None, |deepest: Option<Mount>, mount| match deepest {
            Some(deeper)
                if deeper.point.components().count() > mount.point.components().count() =>
            {
                Some(deeper)
            }
            _ => Some(mount),
        })
}

/// One line of `mountinfo`: an ID, the parent's, the device number, the root, the mount
/// point, the mount's options and any number of optional fields, then ` - `, the
/// filesystem type, the source and the filesystem's options (`proc_pid_mountinfo(5)`).
fn parse_mount(line: &str) -> Option<((u64, u64), Mount)> {
    let (mount, filesystem) = line.split_once(" - ")?;
    let mount: Vec<&str> = mount.split(' ').collect();
    let mut filesystem = filesystem.split(' ');
    Some((
        parse_dev_numbers(mount.get(2)?)?,
        Mount {
            fstype: filesystem.next()?.to_owned(),
            source: unescape(filesystem.next()?),
            options: (*mount.get(5)?).to_owned(),
            fs_options: filesystem.next().unwrap_or_default().to_owned(),
            point: PathBuf::from(unescape(mount.get(4)?)),
        },
    ))
}

/// Undo `mountinfo`'s octal escapes, `\040` for a space, which is how a field with a
/// space in it survives a format split on spaces.
fn unescape(field: &str) -> String {
    let bytes = field.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let code = bytes
            .get(i + 1..i + 4)
            .filter(|digits| bytes[i] == b'\\' && digits.iter().all(u8::is_ascii_digit))
            .and_then(|digits| u8::from_str_radix(std::str::from_utf8(digits).ok()?, 8).ok());
        if let Some(code) = code {
            out.push(code);
            i += 4;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Whether the machine is running on its battery: `Some(false)` on mains, `None` with no
/// battery to ask, as on a Pi.
///
/// Reported because a laptop on battery holds its CPU to a lower performance level, and
/// the store's writer is CPU-bound, so the same laptop plugged in is a different machine.
/// Read once, as the run starts.
pub fn on_battery() -> Option<bool> {
    let read = |dir: &Path, name: &str| {
        std::fs::read_to_string(dir.join(name)).ok().map(|s| s.trim().to_owned())
    };
    let supplies: Vec<Supply> = std::fs::read_dir("/sys/class/power_supply")
        .ok()?
        .flatten()
        .map(|entry| {
            let dir = entry.path();
            Supply {
                kind: read(&dir, "type"),
                status: read(&dir, "status"),
                scope: read(&dir, "scope"),
            }
        })
        .collect();
    judge_power(&supplies)
}

/// One entry of `/sys/class/power_supply`, as far as [`judge_power`] cares.
#[derive(Debug, Default)]
struct Supply {
    kind: Option<String>,
    status: Option<String>,
    scope: Option<String>,
}

/// On battery when a system battery is discharging. A wireless mouse's battery is a
/// `Battery` too, and is told apart by its `Device` scope.
fn judge_power(supplies: &[Supply]) -> Option<bool> {
    let mut batteries = supplies
        .iter()
        .filter(|s| s.kind.as_deref() == Some("Battery") && s.scope.as_deref() != Some("Device"))
        .peekable();
    batteries.peek()?;
    Some(batteries.any(|b| b.status.as_deref() == Some("Discharging")))
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
    use std::path::Path;

    use super::{
        DeviceIo, ProcessIo, Supply, enclosing_mount, find_mount, judge_power, parse_dev_numbers,
        parse_device_stat, parse_peak_rss, parse_process_io, split_dev, unescape,
    };

    /// Fedora's layout: a root and a home subvolume of one btrfs, a tmpfs, and sysfs. The
    /// device number a file under `/home` reports is the subvolume's, which appears on no
    /// line here.
    const LAPTOP: &str = "\
24 1 0:25 / /sys rw,nosuid shared:2 - sysfs sysfs rw
72 1 0:35 /root / rw,relatime shared:1 - btrfs /dev/nvme0n1p3 rw,compress=zstd:1,ssd,discard=async,space_cache=v2,subvolid=257,subvol=/root
73 72 0:35 /home /home rw,relatime shared:3 - btrfs /dev/nvme0n1p3 rw,compress=zstd:1,ssd,discard=async,space_cache=v2,subvolid=256,subvol=/home
80 72 0:40 / /tmp rw,nosuid,nodev shared:4 - tmpfs tmpfs rw
";

    #[test]
    fn a_path_under_a_btrfs_subvolume_is_found_by_its_deepest_mount_point() {
        let mount = enclosing_mount(LAPTOP, Path::new("/home/someone/bench")).expect("home");
        assert_eq!(mount.point, Path::new("/home"));
        assert_eq!(mount.fstype, "btrfs");
        assert_eq!(mount.source, "/dev/nvme0n1p3");
        assert!(mount.fs_options.contains("subvol=/home"), "{mount:?}");
    }

    #[test]
    fn mount_points_are_compared_by_whole_components() {
        let mount = enclosing_mount(LAPTOP, Path::new("/homework/bench")).expect("root");
        assert_eq!(mount.point, Path::new("/"));
        let mount = enclosing_mount(LAPTOP, Path::new("/tmp/bench")).expect("tmp");
        assert_eq!(mount.fstype, "tmpfs");
    }

    #[test]
    fn a_later_mount_over_the_same_point_is_the_one_in_force() {
        let shadowed = format!("{LAPTOP}90 80 179:1 / /tmp rw,noatime - ext4 /dev/mmcblk0p1 rw\n");
        let mount = enclosing_mount(&shadowed, Path::new("/tmp/bench")).expect("tmp");
        assert_eq!(mount.source, "/dev/mmcblk0p1");
    }

    /// A Pi on ext4, a laptop's btrfs subvolume with its anonymous device number, and a
    /// card whose mount point and source both have spaces in them.
    const MOUNTINFO: &str = "\
23 1 259:2 / / rw,noatime shared:1 - ext4 /dev/nvme0n1p2 rw
29 23 179:1 / /mnt/card rw,noatime shared:12 - ext4 /dev/mmcblk0p1 rw
72 1 0:35 /root / rw,relatime shared:1 - btrfs /dev/nvme0n1p3 rw,compress=zstd:1,ssd,discard=async,space_cache=v2,subvolid=257,subvol=/root
40 23 8:1 / /media/my\\040card rw,nosuid - vfat /dev/disk/by-label/MY\\040CARD rw,fmask=0022
";

    #[test]
    fn an_ext4_mount_is_found_by_its_device_number() {
        let mount = find_mount(MOUNTINFO, 179, 1).expect("the card");
        assert_eq!(mount.fstype, "ext4");
        assert_eq!(mount.source, "/dev/mmcblk0p1");
        assert_eq!(mount.options, "rw,noatime");
        assert_eq!(mount.fs_options, "rw");
    }

    #[test]
    fn a_btrfs_mount_is_found_by_its_anonymous_number_and_names_its_real_device() {
        // A btrfs device number names no block device, so the source is the device. The
        // number only matches like this outside a subvolume; see the enclosing-mount tests.
        let mount = find_mount(MOUNTINFO, 0, 35).expect("the subvolume");
        assert_eq!(mount.fstype, "btrfs");
        assert_eq!(mount.source, "/dev/nvme0n1p3");
        assert_eq!(mount.options, "rw,relatime");
        assert!(mount.fs_options.contains("compress=zstd:1"), "{mount:?}");
    }

    #[test]
    fn a_mount_with_spaces_and_no_optional_fields_still_parses() {
        let mount = find_mount(MOUNTINFO, 8, 1).expect("the vfat card");
        assert_eq!(mount.source, "/dev/disk/by-label/MY CARD");
        assert_eq!(mount.options, "rw,nosuid");
    }

    #[test]
    fn a_device_number_nothing_is_mounted_from_finds_nothing() {
        assert_eq!(find_mount(MOUNTINFO, 8, 2), None);
    }

    #[test]
    fn only_complete_octal_escapes_are_undone() {
        assert_eq!(unescape(r"a\040b"), "a b");
        assert_eq!(unescape(r"tab\011end"), "tab\tend");
        assert_eq!(unescape(r"not\+12"), r"not\+12");
        assert_eq!(unescape(r"short\04"), r"short\04");
    }

    #[test]
    fn device_numbers_are_read_as_sysfs_writes_them() {
        assert_eq!(parse_dev_numbers("259:3\n"), Some((259, 3)));
        assert_eq!(parse_dev_numbers("259"), None);
    }

    fn supply(kind: &str, status: &str, scope: Option<&str>) -> Supply {
        Supply {
            kind: Some(kind.to_owned()),
            status: Some(status.to_owned()),
            scope: scope.map(str::to_owned),
        }
    }

    #[test]
    fn a_discharging_system_battery_is_on_battery_and_a_charging_one_is_not() {
        let adapter = Supply { kind: Some("Mains".to_owned()), ..Supply::default() };
        let on = [adapter, supply("Battery", "Discharging", Some("System"))];
        assert_eq!(judge_power(&on), Some(true));
        let charging = [supply("Battery", "Charging", None)];
        assert_eq!(judge_power(&charging), Some(false));
    }

    #[test]
    fn a_machine_whose_only_battery_is_in_its_mouse_has_no_battery() {
        assert_eq!(judge_power(&[supply("Battery", "Discharging", Some("Device"))]), None);
        assert_eq!(judge_power(&[]), None);
    }

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
