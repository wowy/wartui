//! TEMPORARY burst-test build: never merge.
//!
//! Every 10 s, once the node has a core, sends a burst of synthetic sighting batches
//! through the real acked unicast path with the real `seq`, cycling 46, 91 and 132
//! batches. The bench test in `docs/t-dongle-c5-findings.md` § "A redraw on the first
//! frame of a burst costs the batches behind it" reads loss from the host's `batch_gap`.

use esp_hal::time::{Duration, Instant};
use esp_radio::esp_now::EspNowSender;
use wartui_proto::air::{RecordKind, Security, SightingBatchWriter, SightingMsg};

use crate::{Node, radio};

const SIZES: [u16; 3] = [46, 91, 132];
const PERIOD_MS: u64 = 10_000;
const CHANNELS: [u8; 3] = [1, 6, 11];
const SECURITY: [Security; 3] = [Security::Wpa2Psk, Security::Open, Security::Wpa2Wpa3Psk];

pub struct Burst {
    next: Instant,
    size: usize,
    /// Distinguishes this node's fabricated BSSIDs from another node's.
    tag: u8,
    /// Records fabricated since boot; the low bytes of each fabricated BSSID.
    records: u32,
}

impl Burst {
    pub fn new(mac: &[u8; 6]) -> Self {
        // A per-node offset inside the period, so two nodes do not always line up.
        let offset = u64::from(mac[5].wrapping_mul(157)) * PERIOD_MS / 256;
        Self {
            next: Instant::now() + Duration::from_millis(PERIOD_MS + offset),
            size: 0,
            tag: mac[5],
            records: 0,
        }
    }

    /// Send a burst if one is due. Call only on the control channel.
    pub fn poll(&mut self, sender: &mut EspNowSender, node: &mut Node) {
        let now = Instant::now();
        if now < self.next {
            return;
        }
        self.next += Duration::from_millis(PERIOD_MS);
        if self.next <= now {
            self.next = now + Duration::from_millis(PERIOD_MS);
        }
        let size = SIZES[self.size];
        self.size = (self.size + 1) % SIZES.len();
        let Some(core) = node.core else {
            note!("burst {}: skipped, no core", size);
            return;
        };
        let mut writer = SightingBatchWriter::new(node.seq);
        let mut acked = 0u16;
        let mut first = (0, 0);
        for i in 0..size {
            writer.reset(node.seq);
            // Fill to the writer's own byte budget: stop at the first record it refuses,
            // which is not counted, so the next batch starts with it.
            while self.push(&mut writer) {}
            if i == 0 {
                first = (writer.len(), writer.as_bytes().len());
            }
            if radio::unicast(sender, &core, writer.as_bytes()) {
                node.seq = node.seq.wrapping_add(1);
                acked += 1;
            } else {
                node.unacked = node.unacked.wrapping_add(1);
            }
        }
        note!(
            "burst {}: {} sent, {} acked, {} records/batch, {} bytes/frame, seq now {}",
            size,
            size,
            acked,
            first.0,
            first.1,
            node.seq
        );
    }

    /// Append the next fabricated record, advancing the counter only when it fits.
    fn push(&mut self, writer: &mut SightingBatchWriter) -> bool {
        let n = self.records;
        let [b0, b1, b2, b3] = n.to_be_bytes();
        let pick = (n % 3) as usize;
        // 8 to 24 bytes, varying with the counter.
        let len = 8 + (n.wrapping_mul(7) % 17) as usize;
        let mut ssid = [0u8; 24];
        for (k, byte) in ssid.iter_mut().enumerate() {
            *byte = b'a' + ((n as usize + k * 5) % 26) as u8;
        }
        let msg = SightingMsg {
            kind: RecordKind::Wifi,
            bssid: [0x02, self.tag, b0, b1, b2, b3],
            channel: CHANNELS[pick],
            rssi: -50 - (n % 40) as i8,
            security: SECURITY[pick],
            ssid: &ssid[..len],
            ext: &[],
        };
        let fit = writer.push(&msg);
        if fit {
            self.records = n.wrapping_add(1);
        }
        fit
    }
}
