//! Watching a node's own port for the lines every boot prints, so `flash-fleet` knows the new
//! image is running rather than the old one carrying on from RAM.
//!
//! **Either of two lines says the board rebooted.** The bootloader prints `Loaded app from
//! partition` on every boot, and node firmware with the `log` feature prints its banner, `wartui
//! node <MAC> v<version>, control channel N`. Firmware that keeps running prints neither. A banner
//! counts only with this board's address, formatted by the same `mac::full` the firmware uses. The
//! ROM's `ESP-ROM:` does not count: the ROM prints it when it boots into download mode too, where
//! no app runs.
//!
//! **Each open port starts a fresh line, and a line is capped at 1024 bytes.** Bytes from before
//! a reopen would otherwise join the first line after it, and output with no newline would grow
//! without bound.
//!
//! **The port is found by address on every pass and opened without touching DTR or RTS.** The
//! device node can move when the board re-enumerates on reset, and moving either line is the
//! reset sequence (`wartui_bridge::ports`).

use std::io::{ErrorKind, Read};
use std::time::{Duration, Instant};

use wartui_bridge::serial::discover_ports;
use wartui_proto::mac::{self, Mac};

/// How long one read waits before the deadline is checked again.
const READ_TIMEOUT: Duration = Duration::from_millis(100);

/// The longest line kept; a longer run of bytes without a newline is dropped.
const MAX_LINE: usize = 1024;

/// How long to wait before looking for the port again after it vanished or would not open.
const RETRY: Duration = Duration::from_millis(100);

/// Whether the board at `address` is seen to boot within `within`.
pub(super) fn seen_booting(address: Mac, within: Duration) -> bool {
    let deadline = Instant::now() + within;
    let mut buffer = [0u8; 512];
    while Instant::now() < deadline {
        if let Some(mut port) = open(address) {
            let mut watch = BootWatch::new(address);
            loop {
                if Instant::now() >= deadline {
                    return false;
                }
                match port.read(&mut buffer) {
                    // A hung-up tty reads EOF at once and forever; retrying at once would spin.
                    Ok(0) => break,
                    Ok(read) => {
                        if watch.feed(&buffer[..read]) {
                            return true;
                        }
                    }
                    Err(error) if error.kind() == ErrorKind::TimedOut => {}
                    Err(_) => break,
                }
            }
        }
        std::thread::sleep(RETRY);
    }
    false
}

/// Open the board at `address`, wherever it is attached now.
fn open(address: Mac) -> Option<Box<dyn serialport::SerialPort>> {
    let found = discover_ports().ok()?;
    let candidate = found.into_iter().find(|candidate| candidate.mac() == Some(address))?;
    serialport::new(&candidate.path, 115_200)
        .timeout(READ_TIMEOUT)
        // The one setting that keeps the driver from moving RTS by itself.
        .flow_control(serialport::FlowControl::None)
        .open()
        .ok()
}

/// Lines read from one board, joined across reads, until one says it booted.
struct BootWatch {
    address: Mac,
    partial: Vec<u8>,
}

impl BootWatch {
    fn new(address: Mac) -> Self {
        Self { address, partial: Vec::new() }
    }

    /// Take the next bytes read; true once a whole line is a boot marker.
    fn feed(&mut self, bytes: &[u8]) -> bool {
        self.partial.extend_from_slice(bytes);
        let mut seen = false;
        while let Some(end) = self.partial.iter().position(|&byte| byte == b'\n') {
            let line: Vec<u8> = self.partial.drain(..=end).collect();
            let line = String::from_utf8_lossy(&line[..end]);
            seen |= is_boot_line(line.trim_end_matches('\r'), self.address);
        }
        if self.partial.len() >= MAX_LINE {
            self.partial.clear();
        }
        seen
    }
}

fn is_boot_line(line: &str, address: Mac) -> bool {
    line.contains("Loaded app from partition")
        || line.contains(&format!("wartui node {}", mac::full(&address)))
}

#[cfg(test)]
mod tests {
    use super::super::testing::{BRIDGE_MAC, NODE_MAC};
    use super::*;

    fn banner(address: &Mac) -> String {
        format!("wartui node {} v0.9.0, control channel 6\r\n", mac::full(address))
    }

    #[test]
    fn flash_sees_boot_when_banner_carries_this_mac() {
        let mut watch = BootWatch::new(NODE_MAC);
        assert!(watch.feed(banner(&NODE_MAC).as_bytes()));
    }

    #[test]
    fn flash_sees_boot_when_banner_is_split_across_reads() {
        let mut watch = BootWatch::new(NODE_MAC);
        let line = banner(&NODE_MAC);
        let (head, tail) = line.as_bytes().split_at(20);
        assert!(!watch.feed(head));
        assert!(watch.feed(tail));
    }

    #[test]
    fn flash_ignores_banner_when_it_carries_another_mac() {
        let mut watch = BootWatch::new(NODE_MAC);
        assert!(!watch.feed(banner(&BRIDGE_MAC).as_bytes()));
    }

    #[test]
    fn flash_ignores_line_when_it_is_ordinary_log() {
        let mut watch = BootWatch::new(NODE_MAC);
        assert!(!watch.feed(b"bluetooth controller would not start; sniffing Wi-Fi only\r\n"));
    }

    #[test]
    fn flash_sees_boot_when_bootloader_prints() {
        let mut watch = BootWatch::new(NODE_MAC);
        assert!(watch.feed(b"I (45) boot: Loaded app from partition at offset 0x10000\r\n"));
    }

    #[test]
    fn flash_ignores_rom_line_when_chip_may_be_in_download_mode() {
        for line in [
            "ESP-ROM:esp32c6-20220919\r\n",
            "rst:0x15 (USB_UART_HPSYS),boot:0x5 (DOWNLOAD(USB/UART0))\r\n",
        ] {
            let mut watch = BootWatch::new(NODE_MAC);
            assert!(!watch.feed(line.as_bytes()), "{line}");
        }
    }

    #[test]
    fn flash_sees_boot_when_banner_follows_long_run_without_newline() {
        let mut watch = BootWatch::new(NODE_MAC);
        assert!(!watch.feed(&[b'x'; 3000]));
        assert!(watch.feed(format!("\n{}", banner(&NODE_MAC)).as_bytes()));
    }

    #[test]
    fn flash_keeps_partial_under_cap_when_fed_past_it() {
        let mut watch = BootWatch::new(NODE_MAC);
        for _ in 0..10 {
            watch.feed(&[b'x'; 300]);
            assert!(watch.partial.len() <= MAX_LINE, "{}", watch.partial.len());
        }
    }
}
