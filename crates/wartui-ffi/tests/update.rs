//! The node and bridge updates over a `Link`, and a release's images, over the simulator, the
//! loopback radio and local listeners only.

mod common;

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use tokio::time::timeout;
use wartui_app::update::image::BRIDGE_PROJECT;
use wartui_app::update::release::{BRIDGE_APP_ASSET, NODE_APP_ASSET, SUMS};
use wartui_app::update::testing::{P4_APP_HEADER, fixture, fixture_with};
use wartui_app::view::SlotShown;
use wartui_ffi::{
    BridgeImage, BridgeUpdateFailed, FetchError, ImageError, Link, NodeAction, NodeImage,
    NodeOutcome, NodeUpdateListener, NodeUpdateProgress, NodesFailed, ReleaseFailure, SelectFailed,
    Settings, SinkError, Stopper, UpdateError, fetch_bridge_image_from, fetch_node_image_from,
    testing_node_image,
};
use wartui_proto::bus::{Board, NODE_VERSION};

use common::{Loopback, Memory, start};

/// How long a listen gives the simulator's nodes, which heartbeat every second while idle.
const LISTEN_MS: u32 = 3_000;

/// The longest an update over the simulator takes before the test fails rather than hangs.
const UPDATE_WAIT: Duration = Duration::from_secs(120);

/// How long a test gives the bridge to announce itself, in milliseconds.
const READY_MS: u32 = 15_000;

/// A node image carrying the version the simulator's nodes run.
fn current_image() -> Arc<NodeImage> {
    let bytes = testing_node_image(Board::Esp32c5, NODE_VERSION.to_string());
    NodeImage::parse(bytes).expect("the testing image passes")
}

/// Counts the progress an update reports, and stops `stopper` on the first node's.
#[derive(Default)]
struct Counted {
    calls: AtomicU32,
    nodes: Mutex<Vec<String>>,
    stopper: Option<Arc<Stopper>>,
}

impl NodeUpdateListener for Counted {
    fn progress(&self, progress: NodeUpdateProgress) -> Result<(), SinkError> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        if let NodeUpdateProgress::Node { node, .. } = progress {
            self.nodes.lock().unwrap_or_else(PoisonError::into_inner).push(node);
            if let Some(stopper) = &self.stopper {
                stopper.stop();
            }
        }
        Ok(())
    }
}

#[test]
fn update_image_refuses_bytes_when_they_are_no_image() {
    let refused = NodeImage::parse(vec![0xE9; 16]);
    assert!(matches!(refused, Err(ImageError::TooShort)), "{refused:?}");
    let mut bytes = fixture(Board::Esp32c5, "0.1.1", 0x1000);
    bytes[0] = 0x7F;
    assert!(matches!(NodeImage::parse(bytes), Err(ImageError::NoHeader)));
    // A node's image is no bridge's.
    let node = fixture(Board::Esp32c5, "0.1.1", 0x1000);
    let refused = BridgeImage::parse(node);
    assert!(matches!(refused, Err(ImageError::OtherChip { .. })), "{refused:?}");
}

#[test]
fn update_image_reads_what_it_carries_when_bytes_pass() {
    let image = NodeImage::parse(fixture(Board::Esp32c5, "0.1.1", 0x1000)).expect("passes");
    assert_eq!((image.version(), image.board()), ("0.1.1".to_owned(), Board::Esp32c5));
    let digest = image.sha256();
    assert!(digest.len() == 64 && digest.chars().all(|c| matches!(c, '0'..='9' | 'a'..='f')));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn update_nodes_plan_skips_a_node_when_it_already_runs_the_image() {
    let link = Link::simulated(3);
    let heard = link.hear_nodes(LISTEN_MS).await.expect("heard");
    let plan = heard.plan(Vec::new(), vec![current_image()], false).expect("a plan");
    let rows = plan.rows();
    assert_eq!(rows.len(), 3, "{rows:?}");
    let version = NODE_VERSION.to_string();
    for row in &rows {
        assert_eq!(row.action, NodeAction::Same { version: version.clone() }, "{row:?}");
        assert_eq!(row.image.as_deref(), Some(version.as_str()));
        // Labeled on the simulator's carrier.
        let slot = row.target.slot.as_ref().expect("heard in a slot");
        assert!(slot.label.starts_with('N'), "{slot:?}");
    }
    assert_eq!(plan.chosen(), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn update_nodes_plan_refuses_two_images_for_one_board() {
    let link = Link::simulated(3);
    let heard = link.hear_nodes(LISTEN_MS).await.expect("heard");
    let refused = heard.plan(Vec::new(), vec![current_image(), current_image()], true);
    assert!(
        matches!(refused, Err(SelectFailed::TwoImages { board: Board::Esp32c5 })),
        "{refused:?}"
    );
    let no_slot = SlotShown { index: 0xFF, label: String::new() };
    let refused = heard.targets(vec![no_slot]);
    assert!(matches!(refused, Err(SelectFailed::NoSuchSlot { slot: 0xFF })), "{refused:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn update_nodes_updates_the_fleet_when_forced_over_the_simulator() {
    let link = Link::simulated(3);
    let heard = link.hear_nodes(LISTEN_MS).await.expect("heard");
    let plan = heard.plan(Vec::new(), vec![current_image()], true).expect("a plan");
    assert_eq!(plan.chosen(), 3);
    let listener = Arc::new(Counted::default());
    let updating = link.update_nodes(plan, None, Some(listener.clone()));
    let updated = timeout(UPDATE_WAIT, updating).await.expect("in time").expect("updated");

    assert_eq!(updated.outcomes.len(), 3, "{updated:?}");
    let version = NODE_VERSION.to_string();
    for ended in &updated.outcomes {
        assert_eq!(ended.outcome, NodeOutcome::Done { version: version.clone() }, "{ended:?}");
    }
    assert_eq!((updated.cut_short, updated.failed), (None, None));
    // Each node's progress was heard.
    let mut nodes = listener.nodes.lock().unwrap_or_else(PoisonError::into_inner).clone();
    nodes.sort();
    nodes.dedup();
    assert_eq!(nodes.len(), 3, "{nodes:?}");
    // Given back idle.
    link.hear_nodes(100).await.expect("the link is idle again");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn update_nodes_tells_each_node_when_stopped() {
    let link = Link::simulated(3);
    let heard = link.hear_nodes(LISTEN_MS).await.expect("heard");
    let plan = heard.plan(Vec::new(), vec![current_image()], true).expect("a plan");
    let stopper = Stopper::new();
    let listener = Arc::new(Counted { stopper: Some(stopper.clone()), ..Counted::default() });
    let updating = link.update_nodes(plan, Some(stopper), Some(listener));
    let updated = timeout(UPDATE_WAIT, updating).await.expect("in time").expect("ran");

    // Every session under way was canceled, which tells its node.
    assert_eq!(updated.outcomes.len(), 3, "{updated:?}");
    let canceled = NodeOutcome::Failed { failure: wartui_ffi::NodeFailure::Canceled };
    assert!(updated.outcomes.iter().all(|ended| ended.outcome == canceled), "{updated:?}");
    assert_eq!(updated.failed, Some(NodesFailed::Canceled));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn update_nodes_refuse_link_when_it_is_in_use() {
    let dir = tempfile::tempdir().expect("temp dir");
    let link = Link::simulated(3);
    let session = start(&dir.path().join("capture.db"), &link).await;
    let refused = link.hear_nodes(LISTEN_MS).await;
    assert!(matches!(refused, Err(UpdateError::InUse)), "{refused:?}");
    let image = BridgeImage::parse(fixture_with(P4_APP_HEADER, BRIDGE_PROJECT, "0.1.0", 0x1000))
        .expect("a bridge image");
    let refused = link.update_bridge(image, None, None).await;
    assert!(matches!(refused, Err(BridgeUpdateFailed::InUse)), "{refused:?}");
    session.stop().await.expect("stopped");
}

/// Settings remembering the loopback's bridge over Bluetooth, as a capture over it saves them.
fn remembering(driver: &Loopback) -> Arc<Settings> {
    let tail = driver.tail();
    let text = format!(
        "[bridge]\nmac = \"02:00:00:00:{tail}\"\nbluetooth-address = \"{}\"\n",
        common::PEER
    );
    Settings::open(Memory::holding(&text), None).expect("the settings load")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn update_refuses_bluetooth_link_when_no_bridge_was_named() {
    let (radio, driver) = Loopback::steered();
    // Remembered is not named: a transport opened unnamed may take another bridge.
    let link = Link::bluetooth(radio, remembering(&driver), None).expect("opened");
    let refused = link.hear_nodes(LISTEN_MS).await;
    assert!(matches!(refused, Err(UpdateError::BridgeNotNamed)), "{refused:?}");
    let image = BridgeImage::parse(fixture_with(P4_APP_HEADER, BRIDGE_PROJECT, "0.1.0", 0x1000))
        .expect("a bridge image");
    let refused = link.update_bridge(image, None, None).await;
    assert!(matches!(refused, Err(BridgeUpdateFailed::BridgeNotNamed)), "{refused:?}");
    link.close().await.expect("closed");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn update_takes_bluetooth_link_when_bridge_was_named() {
    let (radio, driver) = Loopback::steered();
    let link = Link::bluetooth(radio, Settings::unsaved(), Some(driver.tail())).expect("opened");
    link.ready(READY_MS, None).await.expect("announced");
    let heard = link.hear_nodes(LISTEN_MS).await.expect("heard over the named bridge");
    assert_eq!(heard.targets(Vec::new()).expect("targets").len(), 3);
    link.close().await.expect("closed");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn update_bridge_refuses_when_no_bridge_is_announced() {
    let (radio, driver) = Loopback::steered();
    // The bridge is gone before the link opens, so it never announces itself.
    driver.go();
    let link = Link::bluetooth(radio, Settings::unsaved(), Some(driver.tail())).expect("opened");
    let image = BridgeImage::parse(fixture_with(P4_APP_HEADER, BRIDGE_PROJECT, "0.1.0", 0x1000))
        .expect("a bridge image");
    let refused = link.update_bridge(image, None, None).await;
    assert!(matches!(refused, Err(BridgeUpdateFailed::NotAnnounced)), "{refused:?}");
    link.close().await.expect("closed");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn update_bridge_abandons_the_update_when_stopped() {
    let link = Link::simulated(3);
    link.ready(READY_MS, None).await.expect("announced");
    let image = BridgeImage::parse(fixture_with(P4_APP_HEADER, BRIDGE_PROJECT, "0.1.0", 0x1000))
        .expect("a bridge image");
    let stopper = Stopper::new();
    stopper.stop();
    let stopping = link.update_bridge(image, Some(stopper), None);
    let stopped = timeout(Duration::from_secs(5), stopping).await.expect("at once");
    assert!(matches!(stopped, Err(BridgeUpdateFailed::Stopped)), "{stopped:?}");
    // Given back idle.
    link.hear_nodes(100).await.expect("the link is idle again");
}

/// A local listener serving each `(path, body)` it is asked for, for `requests` requests, and
/// a 404 for any other path.
fn serve(files: Vec<(String, Vec<u8>)>, requests: usize) -> (String, std::thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("a local port");
    let base = format!("http://{}", listener.local_addr().expect("its address"));
    let serving = std::thread::spawn(move || {
        for _ in 0..requests {
            let (mut stream, _) = listener.accept().expect("a request");
            let mut request = Vec::new();
            let mut buf = [0u8; 4096];
            while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                let n = stream.read(&mut buf).expect("the request");
                assert!(n > 0, "the client hung up mid-request");
                request.extend_from_slice(&buf[..n]);
            }
            let line = String::from_utf8_lossy(&request).lines().next().unwrap_or("").to_owned();
            let path = line.split(' ').nth(1).unwrap_or("").to_owned();
            let found = files.iter().find(|(served, _)| *served == path);
            let (status, body) = match found {
                Some((_, body)) => ("200 OK", body.clone()),
                None => ("404 Not Found", Vec::new()),
            };
            let head = format!(
                "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            stream.write_all(head.as_bytes()).expect("the answer");
            stream.write_all(&body).expect("the answer's body");
        }
    });
    (base, serving)
}

/// A node image's SHA-256, as `sha256sum` writes it.
fn node_sum(bytes: &[u8]) -> String {
    NodeImage::parse(bytes.to_vec()).expect("a node image").sha256()
}

/// A bridge image's SHA-256, as `sha256sum` writes it.
fn bridge_sum(bytes: &[u8]) -> String {
    BridgeImage::parse(bytes.to_vec()).expect("a bridge image").sha256()
}

/// A digest no image here has.
const OTHER_SUM: &str = "0000000000000000000000000000000000000000000000000000000000000000";

#[tokio::test]
async fn update_fetch_checks_the_release_sums_when_asset_is_listed() {
    let cache = tempfile::tempdir().expect("temp dir");
    let node = fixture(Board::Esp32c5, "0.1.1", 0x1000);
    let bridge = fixture_with(P4_APP_HEADER, BRIDGE_PROJECT, "0.1.0", 0x1000);
    let sums = format!(
        "{}  {NODE_APP_ASSET}\n{}  {BRIDGE_APP_ASSET}\n",
        node_sum(&node),
        bridge_sum(&bridge)
    );
    let files = vec![
        (format!("/v0.0.0-test/{SUMS}"), sums.into_bytes()),
        (format!("/v0.0.0-test/{NODE_APP_ASSET}"), node.clone()),
        (format!("/v0.0.0-test/{BRIDGE_APP_ASSET}"), bridge.clone()),
    ];
    let (base, serving) = serve(files, 4);
    let dir = cache.path().display().to_string();

    let image = fetch_node_image_from(base.clone(), "v0.0.0-test".into(), Board::Esp32c5, dir)
        .await
        .expect("fetched");
    assert_eq!((image.version(), image.sha256()), ("0.1.1".to_owned(), node_sum(&node)));
    // The sums are cached under the tag, so the bridge's image is checked against the same ones.
    let dir = cache.path().display().to_string();
    let image = fetch_bridge_image_from(base, "v0.0.0-test".into(), dir).await.expect("fetched");
    assert_eq!(image.sha256(), bridge_sum(&bridge));
    serving.join().expect("served");
    assert!(cache.path().join("v0.0.0-test").join(NODE_APP_ASSET).exists());

    // No node image is built for a C6, and nothing is asked for.
    let dir = cache.path().display().to_string();
    let refused =
        fetch_node_image_from("http://127.0.0.1:9".into(), "v".into(), Board::Esp32c6, dir);
    assert!(matches!(refused.await, Err(FetchError::NoImage { board: Board::Esp32c6 })));
}

#[tokio::test]
async fn update_fetch_fails_when_digest_does_not_match() {
    let cache = tempfile::tempdir().expect("temp dir");
    let node = fixture(Board::Esp32c5, "0.1.1", 0x1000);
    let sums = format!("{OTHER_SUM}  {NODE_APP_ASSET}\n");
    let files = vec![
        (format!("/v0.0.0-test/{SUMS}"), sums.into_bytes()),
        (format!("/v0.0.0-test/{NODE_APP_ASSET}"), node),
    ];
    let (base, serving) = serve(files, 2);
    let dir = cache.path().display().to_string();
    let refused = fetch_node_image_from(base, "v0.0.0-test".into(), Board::Esp32c5, dir).await;
    serving.join().expect("served");
    let Err(FetchError::Release { error: ReleaseFailure::Mismatch { asset, listed, .. } }) =
        refused
    else {
        panic!("a mismatched image was taken: {refused:?}")
    };
    assert_eq!((asset.as_str(), listed), (NODE_APP_ASSET, OTHER_SUM.to_owned()));
    assert!(!cache.path().join("v0.0.0-test").join(NODE_APP_ASSET).exists(), "not cached");
}
