//! `export_wigle_csv`, a capture written out as a WiGLE CSV.

mod common;

use std::path::Path;
use std::sync::Arc;

use wartui_app::position::Fixed;
use wartui_app::view::NodeRow;
use wartui_ffi::{ExportError, ExportOptions, Link, PositionSources, Session, Settings};

use common::{Memory, capture, until};

/// No option given: each as `wartui export` defaults.
fn defaults() -> ExportOptions {
    ExportOptions { recapture_secs: None }
}

/// A capture into `db` over the simulator, placed by a typed-in position, run until each node has
/// heartbeated, which names its slot, and seen something.
async fn captured(db: &Path) {
    // Synthetic and mid-ocean, so every row has a position.
    let fixed = Fixed { lat: 10.0, lon: -40.0, alt: None };
    let position = PositionSources { bridge_receiver: false, location: None, fixed: Some(fixed) };
    let session =
        Session::start(capture(db), Link::simulated(3), Settings::unsaved(), position, None)
            .await
            .expect("the capture starts");
    let reported = |n: &NodeRow| n.firmware.is_some() && n.sightings > 0;
    until(&session, |view| view.nodes.len() == 3 && view.nodes.iter().all(reported)).await;
    session.stop().await.expect("stopped");
}

/// `db` exported to `out` through `settings`.
async fn export(
    settings: &Arc<Settings>,
    db: &Path,
    out: &Path,
) -> Result<wartui_ffi::Exported, ExportError> {
    let (db_path, out_path) = (db.display().to_string(), out.display().to_string());
    wartui_ffi::export_wigle_csv(
        Arc::clone(settings),
        db_path,
        out_path,
        defaults(),
        "0.1.0".to_owned(),
    )
    .await
}

/// The SSID column of each Wi-Fi row of `csv`.
fn ssids(csv: &str) -> Vec<String> {
    // Past the pre-header and the column names; the simulator's SSIDs need no quoting.
    csv.lines()
        .skip(2)
        .map(|line| line.split(',').collect::<Vec<_>>())
        .filter(|fields| fields.last() == Some(&"WIFI"))
        .map(|fields| fields[1].to_owned())
        .collect()
}

#[tokio::test]
async fn export_writes_csv_and_names_nodes_when_capture_has_rows() {
    let dir = tempfile::tempdir().expect("temp dir");
    let db = dir.path().join("capture.db");
    captured(&db).await;

    let out = dir.path().join("capture.csv");
    let exported = export(&Settings::unsaved(), &db, &out).await.expect("exported");
    assert!(exported.rows > 0, "{exported:?}");
    assert_eq!(exported.placed_by_fixed, exported.wifi_sightings + exported.ble_sightings);
    let names: Vec<&str> = exported.nodes.iter().map(|node| node.name.as_str()).collect();
    assert_eq!(names.len(), 3, "{names:?}");
    assert!(names.iter().all(|name| name.starts_with('N')), "named by slot: {names:?}");
    let csv = std::fs::read_to_string(&out).expect("the CSV");
    assert!(csv.starts_with("WigleWifi-1.6,appRelease=0.1.0,"), "{csv}");
}

#[tokio::test]
async fn export_leaves_out_ssid_when_settings_exclude_it() {
    let dir = tempfile::tempdir().expect("temp dir");
    let db = dir.path().join("capture.db");
    captured(&db).await;
    let all = dir.path().join("all.csv");
    let first = export(&Settings::unsaved(), &db, &all).await.expect("exported");
    assert_eq!(first.excluded_ssid, 0, "{first:?}");
    let csv = std::fs::read_to_string(&all).expect("the CSV");
    let ssid = ssids(&csv).into_iter().find(|ssid| !ssid.is_empty()).expect("a named network");

    let settings =
        Settings::open(Memory::holding(&format!("excluded-ssids = [\"{ssid}\"]\n")), None)
            .expect("the store opens");
    let out = dir.path().join("excluded.csv");
    let exported = export(&settings, &db, &out).await.expect("exported");
    assert!(exported.excluded_ssid > 0, "{exported:?}");
    let csv = std::fs::read_to_string(&out).expect("the CSV");
    assert!(!ssids(&csv).contains(&ssid), "{ssid} written: {csv}");
}

#[tokio::test]
async fn export_refuses_file_when_one_is_at_the_path() {
    let dir = tempfile::tempdir().expect("temp dir");
    let db = dir.path().join("capture.db");
    let session = common::start(&db, &Link::simulated(3)).await;
    session.stop().await.expect("stopped");
    let out = dir.path().join("capture.csv");
    std::fs::write(&out, "the export already uploaded").expect("a file in the way");

    let refused = export(&Settings::unsaved(), &db, &out).await;
    assert!(matches!(refused, Err(ExportError::Exists)), "{refused:?}");
    assert_eq!(std::fs::read_to_string(&out).expect("kept"), "the export already uploaded");
}
