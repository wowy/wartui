//! Positive control for `ext_scan_bench`: an advertiser that uses extended
//! advertising only, so a legacy scan can never hear it.
//!
//! Two sets, each non-connectable, non-scannable and not legacy, carrying
//! manufacturer data under the test company identifier `0xFFFF`:
//!
//! - set 0 at [`common::BEACON_1M`], primary 1M, secondary 2M;
//! - set 1 at [`common::BEACON_CODED`], primary Coded, secondary Coded.
//!
//! Every command's status is printed as `beacon:`. A set whose setup is refused
//! is not enabled and the other carries on, so a controller that will not
//! advertise on Coded, or will not hold a second set, still yields the 1M control.
//!
//! The builders stay here rather than in `wartui_proto::hci`: the product never
//! advertises. Layouts are Core v5.3 Vol 4 Part E — §7.8.52 to §7.8.56.
#![no_std]
#![no_main]

#[macro_use]
mod common;

use esp_hal::time::{Duration, Instant};
use esp_radio::ble::Config;
use wartui_proto::hci::{PHY_1M, PHY_2M, PHY_CODED, RESET};

use common::{BEACON_1M, BEACON_CODED, Hci, MacFmt, Status, delay_ms};

esp_bootloader_esp_idf::esp_app_desc!();

extern crate alloc;

/// Primary advertising interval, 100 ms in 625 µs units, as three bytes.
const INTERVAL: [u8; 3] = [0xA0, 0x00, 0x00];

/// How often the idle loop says it is still advertising.
const ALIVE_MS: u64 = 30_000;

struct Set {
    handle: u8,
    address: [u8; 6],
    primary: u8,
    secondary: u8,
}

const SETS: [Set; 2] = [
    Set { handle: 0, address: BEACON_1M, primary: PHY_1M, secondary: PHY_2M },
    Set { handle: 1, address: BEACON_CODED, primary: PHY_CODED, secondary: PHY_CODED },
];

/// `HCI_LE_Set_Extended_Advertising_Parameters` (§7.8.53).
///
/// Properties zero: non-connectable, non-scannable, undirected and not legacy.
/// Own address type random, which is the set's own address from
/// [`set_random_address`]. No peer, no filter, no Tx power preference, all three
/// primary channels, the handle as the SID and scan request notifications off.
const fn ext_adv_parameters(set: &Set) -> [u8; 29] {
    let mut cmd = [0u8; 29];
    cmd[0] = 0x01;
    cmd[1] = 0x36; // opcode 0x2036
    cmd[2] = 0x20;
    cmd[3] = 25;
    cmd[4] = set.handle;
    // 5..7: properties, zero.
    cmd[7] = INTERVAL[0]; // interval min
    cmd[8] = INTERVAL[1];
    cmd[9] = INTERVAL[2];
    cmd[10] = INTERVAL[0]; // interval max
    cmd[11] = INTERVAL[1];
    cmd[12] = INTERVAL[2];
    cmd[13] = 0x07; // channels 37, 38 and 39
    cmd[14] = 0x01; // own address type: random
    // 15..23: peer address type, peer address and filter policy, zero.
    cmd[23] = 0x7F; // Tx power: no preference
    cmd[24] = set.primary;
    // 25: secondary max skip, zero.
    cmd[26] = set.secondary;
    cmd[27] = set.handle; // SID
    // 28: scan request notifications off.
    cmd
}

/// `HCI_LE_Set_Advertising_Set_Random_Address` (§7.8.52), address least
/// significant byte first.
const fn set_random_address(set: &Set) -> [u8; 11] {
    let a = set.address;
    [0x01, 0x35, 0x20, 7, set.handle, a[5], a[4], a[3], a[2], a[1], a[0]]
}

/// `HCI_LE_Set_Extended_Advertising_Data` (§7.8.54): complete data, which the
/// controller is asked not to fragment, holding one manufacturer-specific
/// structure — company `0xFFFF`, then `WT` and the set's handle.
#[rustfmt::skip]
const fn ext_adv_data(set: &Set) -> [u8; 15] {
    [
        0x01, 0x37, 0x20, 11, // opcode 0x2037
        set.handle,
        0x03, // operation: complete data
        0x01, // fragment preference: do not fragment
        7,    // data length
        0x06, 0xFF, 0xFF, 0xFF, b'W', b'T', set.handle,
    ]
}

/// `HCI_LE_Set_Extended_Advertising_Enable` (§7.8.56) for one set, with no
/// duration and no event limit.
const fn ext_adv_enable(set: &Set) -> [u8; 10] {
    [0x01, 0x39, 0x20, 6, 0x01, 0x01, set.handle, 0x00, 0x00, 0x00]
}

#[esp_hal::main]
fn main() -> ! {
    let bt = common::boot();
    // esp-radio's default holds one extended advertising set.
    let Some(mut hci) = Hci::new(bt, Config::default().with_multi_adv_instances(2)) else {
        say!("beacon: bluetooth controller would not start");
        loop {
            delay_ms(1_000);
        }
    };

    say!("beacon: reset={}", Status(hci.command(&RESET, |_| {})));

    let mut ready = [false; SETS.len()];
    for (set, ready) in SETS.iter().zip(&mut ready) {
        let parameters = hci.command(&ext_adv_parameters(set), |_| {});
        let address = hci.command(&set_random_address(set), |_| {});
        let data = hci.command(&ext_adv_data(set), |_| {});
        say!(
            "beacon: set={} address={} phy={}/{} parameters={} random_address={} data={}",
            set.handle,
            MacFmt(set.address),
            set.primary,
            set.secondary,
            Status(parameters),
            Status(address),
            Status(data)
        );
        *ready = [parameters, address, data].iter().all(|s| *s == Some(0));
    }

    for (set, ready) in SETS.iter().zip(ready) {
        if ready {
            let enable = hci.command(&ext_adv_enable(set), |_| {});
            say!("beacon: set={} enable={}", set.handle, Status(enable));
        } else {
            say!("beacon: set={} not enabled: setup refused", set.handle);
        }
    }

    // Nothing more to say to the controller; drain whatever it says so its queue
    // never fills.
    let mut alive = Instant::now() + Duration::from_millis(ALIVE_MS);
    loop {
        while hci.next().is_some() {}
        if Instant::now() >= alive {
            say!("beacon: advertising");
            alive = Instant::now() + Duration::from_millis(ALIVE_MS);
        }
        delay_ms(100);
    }
}
