//! `prepare_upload` and `PreparedUpload::send`, against local listeners only: no test reaches
//! WDGWars or WiGLE.

mod common;

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use wartui_app::upload::testing::{
    EPOCH_MS, capture, capture_simulated, serve_once, serve_sequence, sighting,
};
use wartui_core::record::{BridgeSeen, Heartbeat, Record};
use wartui_core::store::UploadSite;
use wartui_ffi::{
    PreparedUpload, Settings, SinkError, SiteOutcome, SitePrepared, UploadEndpoints, UploadError,
    UploadListener, UploadOptions, UploadProgress, UploadRefusal, UploadSendFailure,
    prepare_upload_at,
};
use wartui_proto::bus::{NodeBoard, NodeBus, Slot};
use wartui_proto::reset::ResetCause;

use common::Memory;

/// A local port nothing listens on, for a site a test never sends to.
const NOWHERE: &str = "http://127.0.0.1:9";

/// WiGLE's answer queuing an upload.
const WIGLE_QUEUED: &str =
    r#"{"success":true,"results":{"transids":[{"transId":"20261122-00017"}]}}"#;

/// Settings holding `keys`, the `[api-keys]` table's lines. Every key is obviously fake.
fn keyed(keys: &str) -> Arc<Settings> {
    let text = format!("[api-keys]\n{keys}\n");
    Settings::open(Memory::holding(&text), None).expect("the settings load")
}

fn wdgwars_key() -> Arc<Settings> {
    keyed(r#"wdgwars = "AAAAAAAAAA""#)
}

fn wigle_keys() -> Arc<Settings> {
    keyed("wigle-name = \"AAAAAAAAAA\"\nwigle-token = \"AAAAAAAAAA\"")
}

/// Every site with credentials, each over its own window.
fn options() -> UploadOptions {
    UploadOptions {
        to: None,
        recapture_secs: None,
        resend: false,
        excluded_ssids: Vec::new(),
        app_version: "0.1.0".to_owned(),
    }
}

fn at(wdgwars: &str, wigle: &str) -> UploadEndpoints {
    UploadEndpoints { wdgwars: wdgwars.to_owned(), wigle: wigle.to_owned() }
}

fn path(db: &Path) -> String {
    db.display().to_string()
}

/// One positioned sighting, in a capture at `dir`.
fn one_row(dir: &Path) -> PathBuf {
    capture(dir, vec![sighting([0x02, 0, 0, 0, 0, 1], EPOCH_MS, Some(37.0))])
}

/// The one ready upload `prepared` holds.
fn ready(prepared: Vec<SitePrepared>) -> Arc<PreparedUpload> {
    let [SitePrepared::Ready { upload }] = <[SitePrepared; 1]>::try_from(prepared)
        .unwrap_or_else(|prepared| panic!("not one upload: {prepared:?}"))
    else {
        panic!("nothing ready")
    };
    upload
}

/// Every step an upload reports.
#[derive(Default)]
struct Steps(Mutex<Vec<UploadProgress>>);

impl Steps {
    fn heard(&self) -> Vec<UploadProgress> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner).clone()
    }
}

impl UploadListener for Steps {
    fn progress(&self, progress: UploadProgress) -> Result<(), SinkError> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner).push(progress);
        Ok(())
    }
}

#[tokio::test]
async fn upload_prepare_refuses_capture_when_it_holds_test_data() {
    let dir = tempfile::tempdir().expect("temp dir");
    let records = vec![sighting([0x02, 0, 0, 0, 0, 1], EPOCH_MS, Some(37.0))];
    let db = capture_simulated(dir.path(), records, true);
    let refused = prepare_upload_at(wdgwars_key(), path(&db), options(), at(NOWHERE, NOWHERE));
    assert!(matches!(refused.await, Err(UploadRefusal::TestData)));
}

#[tokio::test]
async fn upload_prepare_refuses_when_no_site_has_credentials() {
    let refused = prepare_upload_at(
        Settings::unsaved(),
        "/nonexistent/wartui.db".to_owned(),
        options(),
        at(NOWHERE, NOWHERE),
    )
    .await;
    assert!(matches!(refused, Err(UploadRefusal::NoCredentials)), "{refused:?}");
}

#[tokio::test]
async fn upload_prepare_refuses_site_when_wigle_token_is_missing() {
    let dir = tempfile::tempdir().expect("temp dir");
    let db = one_row(dir.path());
    let settings = keyed("wdgwars = \"AAAAAAAAAA\"\nwigle-name = \"AAAAAAAAAA\"");
    let prepared = prepare_upload_at(settings, path(&db), options(), at(NOWHERE, NOWHERE))
        .await
        .expect("prepared");
    let [wdgwars, wigle] = prepared.as_slice() else { panic!("not two sites: {prepared:?}") };
    let SitePrepared::Ready { upload } = wdgwars else { panic!("WDGWars not ready: {wdgwars:?}") };
    assert_eq!(upload.shown().site, UploadSite::Wdgwars);
    assert!(
        matches!(
            wigle,
            SitePrepared::Refused {
                site: UploadSite::Wigle,
                refusal: wartui_app::upload::SiteRefusal::Missing(
                    wartui_app::upload::Missing::WigleToken
                ),
            }
        ),
        "{wigle:?}"
    );
}

#[tokio::test]
async fn upload_prepare_names_nodes_in_the_summary() {
    let dir = tempfile::tempdir().expect("temp dir");
    // The node `upload::testing`'s sightings come from, heartbeating from N2 of a carrier.
    let node = [0x02, 0x00, 0x5E, 0x10, 0x57, 0x84];
    let bridge = BridgeSeen {
        mac: [0x02, 0x00, 0x5E, 0x10, 0x9D, 0x24],
        fw_version: "0.1.0".to_owned(),
        node_board: NodeBoard::Carrier14,
    };
    let beat = Heartbeat {
        node_mac: node,
        rx_at_ms: EPOCH_MS,
        counter: 1,
        epoch: 1,
        slot: Slot::new(0, 1).expect("A2"),
        wifi_refused: 0,
        ble_refused: 0,
        reset_cause: ResetCause::PowerOn,
        vdd_mv: None,
        die_temp_c: None,
        heap_free: 40_000,
        heap_used: 0,
        heap_high_water: 0,
        bus: NodeBus::default(),
        live: true,
    };
    let records = vec![
        Record::Bridge(bridge),
        Record::Heartbeat(beat),
        sighting([0x02, 0, 0, 0, 0, 1], EPOCH_MS, Some(37.0)),
    ];
    let db = capture(dir.path(), records);
    let prepared = prepare_upload_at(wdgwars_key(), path(&db), options(), at(NOWHERE, NOWHERE));
    let shown = ready(prepared.await.expect("prepared")).shown();
    assert_eq!(shown.summary.rows, 1);
    let names: Vec<&str> = shown.summary.nodes.iter().map(|node| node.name.as_str()).collect();
    assert_eq!(names, ["N2"]);
    assert!(shown.file_bytes > 0 && shown.csv_bytes > 0, "{shown:?}");
}

#[tokio::test]
async fn upload_send_queues_at_wigle_and_records_it_when_site_answers() {
    let dir = tempfile::tempdir().expect("temp dir");
    let db = one_row(dir.path());
    let (base, server) = serve_once("200 OK", WIGLE_QUEUED);
    let prepared = prepare_upload_at(wigle_keys(), path(&db), options(), at(NOWHERE, &base));
    let upload = ready(prepared.await.expect("prepared"));
    let sent = upload.send(None).await.expect("sent");
    server.join().expect("the listener answered once");

    assert!(!sent.failed, "{sent:?}");
    let SiteOutcome::Wigle { queued, unrecorded: None } = &sent.outcome else {
        panic!("not a recorded WiGLE transaction: {sent:?}")
    };
    assert_eq!(queued.trans_id, "20261122-00017");
    // Recorded, so the capture has nothing new for WiGLE.
    let again = prepare_upload_at(wigle_keys(), path(&db), options(), at(NOWHERE, NOWHERE));
    let again = again.await.expect("prepared");
    let [SitePrepared::NothingNew { site: UploadSite::Wigle, previous, .. }] = again.as_slice()
    else {
        panic!("something new: {again:?}")
    };
    assert_eq!(previous.job, "20261122-00017");
}

#[tokio::test]
async fn upload_send_follows_the_wdgwars_job_and_reports_progress() {
    let dir = tempfile::tempdir().expect("temp dir");
    let db = one_row(dir.path());
    let (base, server) = serve_sequence(&[
        ("202 Accepted", r#"{"ok":true,"job_id":42}"#),
        ("200 OK", r#"{"ok":true,"job_id":42,"status":"processing"}"#),
        ("200 OK", r#"{"ok":true,"job_id":42,"status":"done","result":{"imported":1}}"#),
    ]);
    let prepared = prepare_upload_at(wdgwars_key(), path(&db), options(), at(&base, NOWHERE));
    let upload = ready(prepared.await.expect("prepared"));
    let steps = Arc::new(Steps::default());
    let sent = upload.send(Some(steps.clone())).await.expect("sent");
    server.join().expect("the listener answered each request");

    use wartui_app::upload::wdgwars::{Count, CountValue, JobState, Waited};
    assert_eq!(
        steps.heard(),
        [
            UploadProgress::Queued { job: 42 },
            UploadProgress::State { job: 42, state: JobState::Processing },
        ]
    );
    let SiteOutcome::Wdgwars { job: 42, waited, unrecorded: None, .. } = &sent.outcome else {
        panic!("not a followed WDGWars job: {sent:?}")
    };
    let imported = Count { name: "imported".to_owned(), value: CountValue::Number(1) };
    assert_eq!(waited, &Waited::Done(vec![imported]));
    assert!(!sent.failed);
}

#[tokio::test]
async fn upload_send_fails_and_asks_for_a_retry_when_site_refuses() {
    let dir = tempfile::tempdir().expect("temp dir");
    let db = one_row(dir.path());
    let (base, server) = serve_once("401 Unauthorized", r#"{"success":false,"message":"no"}"#);
    let prepared = prepare_upload_at(wigle_keys(), path(&db), options(), at(NOWHERE, &base));
    let sent = ready(prepared.await.expect("prepared")).send(None).await.expect("sent");
    server.join().expect("the listener answered once");

    assert!(sent.failed, "{sent:?}");
    let SiteOutcome::SendFailed { failure: UploadSendFailure::Site { error } } = &sent.outcome
    else {
        panic!("not refused by the site: {sent:?}")
    };
    let rejected = wartui_app::upload::SiteError::CredentialsRejected { reason: Some("no".into()) };
    assert_eq!(error, &rejected);
}

#[tokio::test]
async fn upload_send_refuses_a_second_send() {
    let dir = tempfile::tempdir().expect("temp dir");
    let db = one_row(dir.path());
    let (base, server) = serve_once("200 OK", WIGLE_QUEUED);
    let prepared = prepare_upload_at(wigle_keys(), path(&db), options(), at(NOWHERE, &base));
    let upload = ready(prepared.await.expect("prepared"));
    upload.send(None).await.expect("sent");
    // The listener panics, failing the join, if a second request arrives.
    let again = upload.send(None).await;
    assert!(matches!(again, Err(UploadError::AlreadySent)), "{again:?}");
    server.join().expect("one request alone");
}
