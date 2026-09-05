//! Parsing the `MSG_TEXT` payload.

use wartui_proto::air::{LineError, RecordKind, Security, WardriveLine};

#[test]
fn parses_a_wifi_line() {
    let line = WardriveLine::parse(b"AA:BB:CC:DD:EE:FF,My Net,[WPA2_PSK],11,-42,W").expect("valid");
    assert_eq!(line.bssid, [0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF]);
    assert_eq!(line.ssid, b"My Net");
    assert_eq!(line.security, Security::Wpa2Psk);
    assert_eq!(line.channel, 11);
    assert_eq!(line.rssi, -42);
    assert_eq!(line.kind, RecordKind::Wifi);
}

#[test]
fn parses_a_ble_line() {
    // The BLE path emits an empty SSID and channel 0 (`src/WiFiOps.cpp:144`).
    let line = WardriveLine::parse(b"aa:bb:cc:dd:ee:ff,,[BLE],0,-70,B").expect("valid");
    assert_eq!(line.bssid, [0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF]);
    assert!(line.ssid.is_empty());
    assert_eq!(line.security, Security::Ble);
    assert_eq!(line.channel, 0);
    assert_eq!(line.kind, RecordKind::Ble);
}

#[test]
fn mac_case_is_normalised_across_the_wifi_and_ble_paths() {
    // `WiFi.BSSIDstr()` is uppercase and NimBLE's `toString()` is lowercase.
    // Treating those as different keys would double-count the same device.
    let upper = WardriveLine::parse(b"AA:BB:CC:DD:EE:FF,x,[OPEN],1,-1,W").expect("valid");
    let lower = WardriveLine::parse(b"aa:bb:cc:dd:ee:ff,,[BLE],0,-1,B").expect("valid");
    assert_eq!(upper.bssid, lower.bssid);
}

#[test]
fn commas_in_ssids_arrive_as_underscores() {
    // The firmware rewrites them before transmitting (`ssid.replace(",","_")`),
    // which is the only reason splitting on `,` is safe.
    let line = WardriveLine::parse(b"AA:BB:CC:DD:EE:FF,cafe_bar,[OPEN],6,-55,W").expect("valid");
    assert_eq!(line.ssid, b"cafe_bar");
}

#[test]
fn ssids_need_not_be_utf8() {
    let raw = b"AA:BB:CC:DD:EE:FF,\xff\xfe\x80,[OPEN],6,-55,W";
    let line = WardriveLine::parse(raw).expect("valid");
    assert_eq!(line.ssid, b"\xff\xfe\x80");
    assert!(std::str::from_utf8(line.ssid).is_err(), "test data should be invalid UTF-8");
}

#[test]
fn unknown_security_tokens_survive_instead_of_failing() {
    // A future firmware could add auth modes; dropping the record would be worse
    // than carrying the token through.
    let line = WardriveLine::parse(b"AA:BB:CC:DD:EE:FF,x,[WPA3_ENT_192],36,-60,W").expect("valid");
    assert_eq!(line.security, Security::Other(b"[WPA3_ENT_192]"));
    assert_eq!(line.security.as_bytes(), b"[WPA3_ENT_192]");
}

#[test]
fn every_known_security_token_round_trips() {
    for token in [
        &b"[OPEN]"[..],
        b"[WEP]",
        b"[WPA_PSK]",
        b"[WPA2_PSK]",
        b"[WPA_WPA2_PSK]",
        b"[WPA2]",
        b"[WPA3_PSK]",
        b"[WPA2_WPA3_PSK]",
        b"[WAPI_PSK]",
        b"[UNDEFINED]",
        b"[BLE]",
    ] {
        let mut raw = b"AA:BB:CC:DD:EE:FF,x,".to_vec();
        raw.extend_from_slice(token);
        raw.extend_from_slice(b",6,-55,W");
        let line = WardriveLine::parse(&raw).expect("valid");
        assert_eq!(line.security.as_bytes(), token);
    }
}

#[test]
fn five_ghz_channels_and_weak_signals_parse() {
    let line = WardriveLine::parse(b"00:11:22:33:44:55,ssid,[WPA3_PSK],177,-99,W").expect("valid");
    assert_eq!(line.channel, 177);
    assert_eq!(line.rssi, -99);
}

#[test]
fn wrong_field_count_is_rejected() {
    // `parseWardriveLine` requires exactly six fields.
    assert_eq!(
        WardriveLine::parse(b"AA:BB:CC:DD:EE:FF,x,[OPEN],6,-55"),
        Err(LineError::FieldCount(5))
    );
    assert_eq!(
        WardriveLine::parse(b"AA:BB:CC:DD:EE:FF,x,[OPEN],6,-55,W,extra"),
        Err(LineError::FieldCount(7))
    );
}

#[test]
fn malformed_fields_are_rejected() {
    assert_eq!(WardriveLine::parse(b"nope,x,[OPEN],6,-55,W"), Err(LineError::BadBssid));
    assert_eq!(
        WardriveLine::parse(b"AA-BB-CC-DD-EE-FF,x,[OPEN],6,-55,W"),
        Err(LineError::BadBssid)
    );
    assert_eq!(
        WardriveLine::parse(b"AA:BB:CC:DD:EE:FF,x,[OPEN],six,-55,W"),
        Err(LineError::BadChannel)
    );
    assert_eq!(
        WardriveLine::parse(b"AA:BB:CC:DD:EE:FF,x,[OPEN],6,strong,W"),
        Err(LineError::BadRssi)
    );
    assert_eq!(WardriveLine::parse(b"AA:BB:CC:DD:EE:FF,x,[OPEN],6,-55,Z"), Err(LineError::BadKind));
}

#[test]
fn empty_numeric_fields_are_rejected_rather_than_defaulted() {
    assert_eq!(
        WardriveLine::parse(b"AA:BB:CC:DD:EE:FF,x,[OPEN],,-55,W"),
        Err(LineError::BadChannel)
    );
    assert_eq!(WardriveLine::parse(b"AA:BB:CC:DD:EE:FF,x,[OPEN],6,,W"), Err(LineError::BadRssi));
    assert_eq!(WardriveLine::parse(b"AA:BB:CC:DD:EE:FF,x,[OPEN],6,-,W"), Err(LineError::BadRssi));
}
