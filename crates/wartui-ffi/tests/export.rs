//! `export_wigle_csv`, a capture written out as a WiGLE CSV.

mod common;

use wartui_app::position::Fixed;
use wartui_app::view::NodeRow;
use wartui_ffi::{ExportError, ExportOptions, Link, PositionSources, Session, Settings};

use common::{capture, until};

/// No option given: each as `wartui export` defaults.
fn defaults() -> ExportOptions {
    ExportOptions { recapture_secs: None, excluded_ssids: Vec::new() }
}

#[tokio::test]
async fn export_writes_csv_and_names_nodes_when_capture_has_rows() {
    let dir = tempfile::tempdir().expect("temp dir");
    let db = dir.path().join("capture.db");
    // Synthetic and mid-ocean, so every row has a position.
    let fixed = Fixed { lat: 10.0, lon: -40.0, alt: None };
    let position = PositionSources { bridge_receiver: false, location: None, fixed: Some(fixed) };
    let session =
        Session::start(capture(&db), Link::simulated(3), Settings::unsaved(), position, None)
            .await
            .expect("the capture starts");
    // Each node heartbeating, which names its slot, and seen something.
    let reported = |n: &NodeRow| n.firmware.is_some() && n.sightings > 0;
    until(&session, |view| view.nodes.len() == 3 && view.nodes.iter().all(reported)).await;
    session.stop().await.expect("stopped");

    let out = dir.path().join("capture.csv");
    let (db_path, out_path) = (db.display().to_string(), out.display().to_string());
    let exported = wartui_ffi::export_wigle_csv(db_path, out_path, defaults(), "0.1.0".to_owned())
        .await
        .expect("exported");
    assert!(exported.rows > 0, "{exported:?}");
    assert_eq!(exported.placed_by_fixed, exported.wifi_sightings + exported.ble_sightings);
    let names: Vec<&str> = exported.nodes.iter().map(|node| node.name.as_str()).collect();
    assert_eq!(names.len(), 3, "{names:?}");
    assert!(names.iter().all(|name| name.starts_with('N')), "named by slot: {names:?}");
    let csv = std::fs::read_to_string(&out).expect("the CSV");
    assert!(csv.starts_with("WigleWifi-1.6,appRelease=0.1.0,"), "{csv}");
}

#[tokio::test]
async fn export_refuses_file_when_one_is_at_the_path() {
    let dir = tempfile::tempdir().expect("temp dir");
    let db = dir.path().join("capture.db");
    let session = common::start(&db, &Link::simulated(3)).await;
    session.stop().await.expect("stopped");
    let out = dir.path().join("capture.csv");
    std::fs::write(&out, "the export already uploaded").expect("a file in the way");

    let (db_path, out_path) = (db.display().to_string(), out.display().to_string());
    let refused =
        wartui_ffi::export_wigle_csv(db_path, out_path, defaults(), "0.1.0".to_owned()).await;
    assert!(matches!(refused, Err(ExportError::Exists)), "{refused:?}");
    assert_eq!(std::fs::read_to_string(&out).expect("kept"), "the export already uploaded");
}
