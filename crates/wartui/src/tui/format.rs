use chrono::{DateTime, Local};
use wartui_proto::link::Mac;

/// The last two octets of an address: `57:84`. Boards are told apart by these.
pub(super) fn short_mac(mac: &Mac) -> String {
    format!("{:02X}:{:02X}", mac[4], mac[5])
}

/// A whole address: `02:00:5E:10:57:84`.
pub(super) fn full_mac(mac: &Mac) -> String {
    mac.iter().map(|b| format!("{b:02X}")).collect::<Vec<_>>().join(":")
}

/// Local wall-clock time, `HH:MM:SS`.
pub(super) fn clock(unix_ms: i64) -> String {
    DateTime::from_timestamp_millis(unix_ms).map_or_else(
        || "--:--:--".to_owned(),
        |dt| dt.with_timezone(&Local).format("%H:%M:%S").to_string(),
    )
}

/// A duration as `HH:MM:SS`.
pub(super) fn elapsed(ms: i64) -> String {
    let secs = ms.max(0) / 1000;
    format!("{:02}:{:02}:{:02}", secs / 3600, (secs / 60) % 60, secs % 60)
}

/// A short age: seconds below 100 s, whole minutes above.
pub(super) fn ago(ms: i64) -> String {
    let secs = ms.max(0) / 1000;
    if secs < 100 { format!("{secs}s") } else { format!("{}m", secs / 60) }
}
