//! `Settings` over a phone's `SettingsStore` and `SecretStore`, held in memory here.

mod common;

use std::sync::Arc;

use wartui_app::settings::{ApiKeys, Invalid, SsidRefusal};
use wartui_ffi::{
    Link, Radio, Saved, Session, Settings, SettingsEdit, SettingsError, SteerError,
    check_excluded_ssid, settings_limits,
};

use common::{
    Loopback, Memory, PEER, Secrets, WAIT, capture, host_rows, nowhere, three_nodes, until,
};

#[test]
fn settings_open_fails_when_stored_text_is_malformed() {
    let opened = Settings::open(Memory::holding("excluded-ssids = [unclosed"), None);
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
    let settings = Settings::open(store.clone(), None).expect("an empty store opens");
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

/// How long a test gives the bridge to announce itself, in milliseconds.
const READY_MS: u32 = 15_000;

/// Settings that remember no bridge, over `store`, and a Bluetooth link opened with them over
/// `radio` that has announced its bridge.
async fn announced(store: &Arc<Memory>, radio: Arc<Radio>) -> (Arc<Settings>, Arc<Link>) {
    let settings = Settings::open(store.clone(), None).expect("the store opens");
    let link = Link::bluetooth(radio, Arc::clone(&settings), None).expect("opened");
    link.ready(READY_MS, None).await.expect("announced");
    // The transport connected with remembering off, so it remembered nothing.
    assert_eq!(settings.saved().bridge_mac, None);
    (settings, link)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn settings_save_remembers_connected_bridge_when_remember_turns_on_between_captures() {
    let store = Memory::holding("[bridge]\nremember = false\n");
    let (settings, link) = announced(&store, Loopback::radio()).await;

    let saved = settings.save(host_rows(true, true)).expect("converted");

    assert_eq!(saved, Saved::At { place: "memory".to_owned() });
    let mac = settings.saved().bridge_mac.expect("remembered in the same save");
    let text = store.text().expect("saved");
    assert!(text.contains(&mac) && text.contains("remember = true"), "{mac}: {text}");
    link.close().await.expect("closed");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn settings_save_remembers_no_bridge_when_link_is_closed() {
    let store = Memory::holding("[bridge]\nremember = false\n");
    let (settings, link) = announced(&store, Loopback::radio()).await;
    link.close().await.expect("closed");

    settings.save(host_rows(true, true)).expect("converted");

    assert_eq!(settings.saved().bridge_mac, None);
    assert!(store.text().is_some_and(|text| !text.contains("mac = ")), "{:?}", store.text());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn settings_save_remembers_no_bridge_when_bridge_goes_while_idle() {
    let store = Memory::holding("[bridge]\nremember = false\n");
    let (radio, driver) = Loopback::steered();
    let (settings, link) = announced(&store, radio).await;
    driver.go();
    // The parked link reads the drop at once; this is room for it.
    tokio::time::sleep(std::time::Duration::from_secs(1)).await;

    settings.save(host_rows(true, true)).expect("converted");

    assert_eq!(settings.saved().bridge_mac, None);
    link.close().await.expect("closed");
}

#[test]
fn settings_open_names_the_rule_when_stored_text_is_invalid() {
    let opened =
        Settings::open(Memory::holding("excluded-ssids = [\"HomeNet\", \"HomeNet\"]"), None);
    let Err(SettingsError::Invalid { place, reason }) = opened else { panic!("{opened:?}") };
    assert_eq!(place, "memory");
    assert_eq!(reason, Invalid::ListedSsid("HomeNet".to_owned()));
}

#[test]
fn settings_keep_api_keys_in_the_secret_store_when_one_is_given() {
    let (store, secrets) = (Arc::new(Memory::default()), Arc::new(Secrets::default()));
    let settings = Settings::open(store.clone(), Some(secrets.clone())).expect("opens empty");
    // Obviously fake, and typed with the spaces a paste brings.
    let api_keys = ApiKeys { wdgwars: " AAAAAAAAAA ".to_owned(), ..ApiKeys::default() };
    let edit = SettingsEdit { api_keys, ..host_rows(true, true) };

    assert_eq!(settings.save(edit).expect("converted"), Saved::At { place: "memory".to_owned() });
    let secret = secrets.text().expect("the keys saved");
    assert!(secret.contains("\"AAAAAAAAAA\""), "{secret}");
    let text = store.text().expect("saved");
    assert!(!text.contains("AAAAAAAAAA"), "{text}");

    let reopened = Settings::open(store, Some(secrets)).expect("reopens");
    assert_eq!(reopened.saved().api_keys.wdgwars, "AAAAAAAAAA");
}

#[test]
fn settings_save_host_rows_when_no_capture_runs() {
    let store = Arc::new(Memory::default());
    let settings = Settings::open(store.clone(), None).expect("opens empty");
    let edit = SettingsEdit {
        gps_rate_hz: Some(2),
        excluded_ssids: vec!["HomeNet".to_owned()],
        ..host_rows(false, false)
    };

    assert_eq!(settings.save(edit).expect("converted"), Saved::At { place: "memory".to_owned() });
    let saved = settings.saved();
    assert!(!saved.remember_bridge);
    assert!(!saved.wait_for_fix);
    assert_eq!(saved.gps_rate_hz, Some(2));
    assert_eq!(saved.excluded_ssids, ["HomeNet"]);
    let reopened = Settings::open(store, None).expect("reopens");
    assert_eq!(reopened.saved(), saved);
}

#[test]
fn settings_save_refuses_edit_when_rate_is_out_of_range() {
    let store = Arc::new(Memory::default());
    let settings = Settings::open(store.clone(), None).expect("opens empty");
    let hz = settings_limits().gps_rate_hz_max + 1;
    let edit = SettingsEdit { gps_rate_hz: Some(hz), ..host_rows(true, true) };

    let refused = settings.save(edit);
    assert!(matches!(refused, Err(SteerError::GpsRate { hz: got }) if got == hz), "{refused:?}");
    assert_eq!(store.text(), None, "nothing saved");
}

#[test]
fn settings_save_refuses_edit_when_an_ssid_is_too_long() {
    let store = Arc::new(Memory::default());
    let settings = Settings::open(store.clone(), None).expect("opens empty");
    let long = "x".repeat(usize::try_from(settings_limits().ssid_max_bytes).unwrap() + 1);
    let edit = SettingsEdit { excluded_ssids: vec![long.clone()], ..host_rows(true, true) };

    let refused = settings.save(edit);
    let Err(SteerError::ExcludedSsid { ssid, refusal }) = refused else { panic!("{refused:?}") };
    assert_eq!((ssid, refusal), (long, SsidRefusal::TooLong));
    assert_eq!(store.text(), None, "nothing saved");
}

#[test]
fn settings_save_nowhere_when_settings_are_unsaved() {
    let saved = Settings::unsaved().save(host_rows(true, false)).expect("converted");
    assert_eq!(saved, Saved::Nowhere);
}

#[test]
fn settings_check_refuses_ssid_when_listed_twice() {
    let listed = vec!["HomeNet".to_owned()];
    assert_eq!(
        check_excluded_ssid("HomeNet".to_owned(), listed.clone()),
        Some(SsidRefusal::Listed)
    );
    assert_eq!(check_excluded_ssid("CafeNet".to_owned(), listed), None);
    let long = "x".repeat(usize::try_from(settings_limits().ssid_max_bytes).unwrap() + 1);
    assert_eq!(check_excluded_ssid(long, Vec::new()), Some(SsidRefusal::TooLong));
}
