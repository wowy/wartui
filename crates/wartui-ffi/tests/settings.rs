//! `Settings` over a phone's `SettingsStore`, held in memory here.

mod common;

use std::sync::Arc;

use wartui_ffi::{Link, Session, Settings, SettingsError};

use common::{Loopback, Memory, PEER, WAIT, capture, nowhere, three_nodes, until};

#[test]
fn settings_open_fails_when_stored_text_is_malformed() {
    let opened = Settings::open(Memory::holding("excluded-ssids = [unclosed"));
    let Err(SettingsError::Malformed { place, detail }) = opened else {
        panic!("{opened:?}");
    };
    assert_eq!(place, "memory");
    assert!(!detail.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn settings_save_remembered_bridge_when_bluetooth_link_announces() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = Arc::new(Memory::default());
    let settings = Settings::open(store.clone()).expect("an empty store opens");
    assert_eq!(settings.place().as_deref(), Some("memory"));
    let link = Link::bluetooth(Loopback::radio(), Arc::clone(&settings), None).expect("opened");
    let session = Session::start(
        capture(&dir.path().join("capture.db")),
        Arc::clone(&link),
        settings,
        nowhere(),
        None,
    )
    .await
    .expect("the capture starts");
    until(&session, |view| three_nodes(view) && view.bridge.is_some()).await;
    // The follow task saves the bridge the transport remembered, without the operator.
    let saved = tokio::time::timeout(WAIT, async {
        loop {
            if let Some(text) = store.text().filter(|text| text.contains(PEER)) {
                return text;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("saved in time");
    assert!(saved.contains("bluetooth-address = \"loopback\""), "{saved}");
    session.stop().await.expect("stopped");
    link.close().await.expect("closed");
}
