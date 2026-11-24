//! The largest `Status` a bridge sends, shared by the test binaries that check what fits.

use heapless::Vec;
use wartui_proto::ChannelPool;
use wartui_proto::bus::{DieTemp, Slot, Vdd};
use wartui_proto::fan::{FanDuty, FanStats};
use wartui_proto::fleet::{Member, ScanMode};
use wartui_proto::link::{
    BluetoothStatus, BridgeHealth, BridgeToHost, Central, HostOwner, PairWindow, QueueId,
    QueueStat, QueueStats, RadioLinkStats, SdLog, SdLogState, SdState, SdStatus,
};
use wartui_proto::radio_link::{BleCounters, RadioHealth};

/// A member in every slot, each with a locally administered MAC.
pub fn full_members() -> Vec<Member, 14> {
    (0..14)
        .map(|byte| Member {
            slot: Slot::from_u8(byte).expect("a slot"),
            mac: [0x02, 0xFF, 0xFF, 0xFF, 0xFF, byte],
        })
        .collect()
}

/// A `Status` with every varint at its longest, every queue reported and a member in every slot.
pub fn largest_status() -> BridgeToHost {
    let mut queues = QueueStats::new();
    for queue in QueueId::ALL.into_iter().cycle().take(8) {
        let stat = QueueStat { queue, capacity: u32::MAX, high_water: u32::MAX, dropped: u32::MAX };
        queues.push(stat).expect("eight fit");
    }
    BridgeToHost::Status {
        rx_count: u32::MAX,
        dropped_tx: u32::MAX,
        uptime_ms: u32::MAX,
        rx_oversize: u32::MAX,
        queues,
        radio_link: RadioLinkStats {
            packets: u32::MAX,
            crc_fail: u32::MAX,
            bad_magic: u32::MAX,
            seq_gap: u32::MAX,
            uart_errors: u32::MAX,
            radio_crc_fail: u32::MAX,
            radio_bad_magic: u32::MAX,
            radio_seq_gap: u32::MAX,
            radio_uart_errors: u32::MAX,
        },
        host_uart_errors: u32::MAX,
        fan: Some(FanStats { duty: FanDuty::FULL, rpm: Some(u16::MAX) }),
        pool: ChannelPool::All,
        scan: ScanMode::WifiBluetooth,
        ble: Slot::from_u8(13),
        present: u32::MAX,
        members: full_members(),
        sd: Some(SdStatus {
            state: SdState::Failed,
            card_mb: u32::MAX,
            free_mb: Some(u32::MAX),
            log: SdLog {
                on: true,
                state: SdLogState::Failed,
                file: Some(u32::MAX),
                written: u64::MAX,
                dropped: u32::MAX,
                errors: u32::MAX,
                busy_max_us: u32::MAX,
            },
        }),
        bluetooth: BluetoothStatus {
            owner: HostOwner::Bluetooth,
            bonds: u8::MAX,
            window: PairWindow::Open { secs: u16::MAX },
            central: Some(Central {
                encrypted: true,
                bonded: true,
                mtu: u16::MAX,
                phy: u8::MAX,
                interval_us: u32::MAX,
            }),
            counters: BleCounters {
                pairings: u32::MAX,
                failed_pairings: u32::MAX,
                unbonded_kicks: u32::MAX,
                last_disconnect: u8::MAX,
                resynced_bytes: u32::MAX,
                dropped_writes: u32::MAX,
                phy: u8::MAX,
                interval_us: u32::MAX,
            },
            wardrive_esp: true,
        },
        health: BridgeHealth {
            p4_die_temp: DieTemp(u8::MAX),
            radio: Some(RadioHealth {
                die_temp: DieTemp(u8::MAX),
                vdd: Vdd(u8::MAX),
                heap_used: u32::MAX,
                heap_free: u32::MAX,
                heap_high_water: u32::MAX,
            }),
        },
    }
}
