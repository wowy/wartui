//! The desktop radio's reading of the OS: which peripheral is the remembered one, and which
//! errors are the C6's refusal for want of a bond.

use wartui_desktop::{is_remembered, refused_for_bonding};

#[test]
fn ble_failure_reads_as_not_bonded_when_os_words_att_authentication_or_encryption_refusal() {
    for error in [
        // BlueZ, for 0x05, 0x0c and 0x0f, as btleplug passes it on.
        "Not paired",
        // CoreBluetooth's CBATTError texts.
        "Authentication is insufficient.",
        "Encryption is insufficient.",
    ] {
        assert!(refused_for_bonding(error), "{error}");
    }
}

#[test]
fn ble_failure_reads_as_other_when_os_error_is_no_bonding_refusal() {
    for error in [
        "Write not permitted",
        "Operation failed with ATT error: 0x11",
        "Operation Not Authorized",
        "le-connection-abort-by-local",
        "Not connected",
        "Timed out after 10s",
    ] {
        assert!(!refused_for_bonding(error), "{error}");
    }
}

#[test]
fn ble_remembered_matches_address_or_identifier_when_case_differs() {
    assert!(is_remembered("02:00:5e:10:44:c1", "02:00:5E:10:44:C1", "hci0/dev_02_00_5E_10_44_C1"));
    let mac_id = "5c3e1a2b-0000-4000-8000-00805f9b34fb";
    assert!(is_remembered(mac_id, "00:00:00:00:00:00", mac_id), "macOS hides the address");
    assert!(!is_remembered("02:00:5E:10:44:C1", "02:00:5E:10:44:C2", "hci0/dev_02_00_5E_10_44_C2"));
}
