//! The settings' schema, their rules, and the one path that saves them, against a store held in
//! memory.

mod common;

use std::error::Error;
use std::sync::Arc;

use tokio::sync::mpsc;
use wartui_app::session::Controls;
use wartui_app::settings::{
    ApiKeys, Bridge, Config, Gps, Link, SaveError, Settings, SettingsEdit, SettingsStore,
    excluded_ssid_bytes, load, parse, serialize,
};
use wartui_bridge::remember::BridgeMemory;
use wartui_core::engine::{Command, Snapshot};
use wartui_proto::plan::ChannelPool;

use common::{BRIDGE, Fake, OTHER_BRIDGE, snapshot};

/// Settings kept in `store`, last loaded as `saved`, with the bridge `memory` holds.
fn kept(store: &Arc<Fake>, saved: Config, memory: &BridgeMemory) -> Settings {
    Settings::new(Some(store.clone() as Arc<dyn SettingsStore>), saved, memory.clone())
}

/// What a store holding `text` refuses, as anyhow's `{:#}` prints it: each error in the chain,
/// joined by `: `.
fn load_error(text: &str) -> String {
    let error = load(Fake::holding(text).as_ref()).unwrap_err();
    let mut chain = error.to_string();
    let mut source = error.source();
    while let Some(next) = source {
        chain.push_str(&format!(": {next}"));
        source = next.source();
    }
    chain
}

/// Controls sending on `tx` and saving through `settings`.
fn controls(tx: &mpsc::Sender<Command>, settings: &Settings) -> Controls {
    Controls::new(tx.clone(), settings.clone())
}

/// [`Controls::follow`] over `settings`, as a session's follow task calls it.
fn follow(settings: &Settings, snapshot: &Snapshot) {
    let (tx, _rx) = mpsc::channel(4);
    controls(&tx, settings).follow(snapshot);
}

/// The settings modal's rows, unmoved, over `saved` with remembering `on`.
fn unmoved(saved: &Config, on: bool) -> SettingsEdit {
    SettingsEdit {
        remember_bridge: on,
        wait_for_fix: saved.gps.waits_for_fix(),
        api_keys: saved.api_keys.clone(),
        excluded_ssids: saved.excluded_ssids.clone(),
        ..SettingsEdit::default()
    }
}

/// A `Config` holding every key the settings modal saves.
fn full() -> Config {
    Config {
        excluded_ssids: vec!["HomeNet".to_owned(), "Pixel 9".to_owned()],
        bridge: Bridge {
            remember: Some(true),
            mac: Some([0xAA, 0xBB, 0xCC, 1, 2, 3]),
            serial: Some("AAAAAAAAAA".to_owned()),
            bluetooth_address: Some("02:00:5E:10:44:C1".to_owned()),
            link: Some(Link::Bluetooth),
        },
        gps: Gps { bridge_rate_hz: Some(2), wait_for_fix: Some(false) },
        api_keys: ApiKeys {
            wdgwars: "abc123def456".to_owned(),
            wigle_name: "AIDexamplename".to_owned(),
            wigle_token: "exampletoken789".to_owned(),
        },
    }
}

/// A `Config` with only the bridge's GPS rate set.
fn rate_only(rate: u8) -> Config {
    Config { gps: Gps { bridge_rate_hz: Some(rate), wait_for_fix: None }, ..Config::default() }
}

/// `config` written and read back.
fn round_trip(config: &Config) -> Config {
    parse(&serialize(config).expect("valid settings")).expect("text that parses")
}

#[test]
fn settings_load_returns_default_when_store_holds_nothing() {
    assert_eq!(load(Fake::empty().as_ref()).unwrap(), Config::default());
}

#[test]
fn settings_load_points_at_pool_command_when_text_has_pool_key() {
    // The bridge holds the pool, so the settings have no key for it.
    let chain = load_error("pool = \"us\"\n");
    assert!(chain.starts_with("parsing wartui.toml: "), "{chain}");
    assert!(chain.contains("set it with `wartui pool`"), "{chain}");
    assert!(chain.contains("line 1"), "the parse error still names the line: {chain}");
}

#[test]
fn settings_load_points_at_scan_command_when_text_has_bluetooth_table() {
    // The bridge picks the node that scans Bluetooth, so the table is as unknown as any other.
    let chain = load_error("[bluetooth]\nremember = true\n");
    assert!(chain.contains("wartui.toml"), "{chain}");
    assert!(chain.contains("`wartui scan start --bluetooth`"), "{chain}");
}

#[test]
fn settings_load_adds_no_bridge_hint_when_unknown_table_is_another() {
    let chain = load_error("[not-a-real-table]\nx = 1\n");
    assert!(!chain.contains("the bridge"), "{chain}");
}

#[test]
fn settings_load_names_store_in_errors() {
    let error = load(Fake::holding("[gps]\nbridge-rate-hz = 9\n").as_ref()).unwrap_err();
    assert_eq!(error.to_string(), "in wartui.toml");
}

#[test]
fn settings_parse_accepts_empty_text() {
    assert!(parse("").unwrap().excluded_ssids.is_empty());
}

#[test]
fn settings_parse_rejects_unknown_key() {
    assert!(parse("[bridge]\nremember = true\ntypo = 1\n").is_err());
}

#[test]
fn settings_parse_rejects_unknown_table() {
    assert!(parse("[not-a-real-table]\nx = 1\n").is_err());
}

#[test]
fn settings_parse_reads_bridge_rate_when_gps_table_present() {
    assert_eq!(parse("[gps]\nbridge-rate-hz = 2\n").unwrap().gps.bridge_rate_hz, Some(2));
}

#[test]
fn settings_load_rejects_bridge_rate_when_out_of_range() {
    for rate in [0, 6] {
        let chain = load_error(&format!("[gps]\nbridge-rate-hz = {rate}\n"));
        assert!(chain.contains("gps.bridge-rate-hz"), "{chain}");
    }
}

#[test]
fn settings_parse_rejects_unknown_key_when_under_gps_table() {
    assert!(parse("[gps]\nrate = 2\n").is_err());
}

#[test]
fn settings_parse_reads_wait_for_fix_when_gps_table_present() {
    assert_eq!(parse("[gps]\nwait-for-fix = false\n").unwrap().gps.wait_for_fix, Some(false));
}

#[test]
fn settings_serialize_writes_wait_for_fix_that_reloads_when_set() {
    let skip = Config {
        gps: Gps { bridge_rate_hz: None, wait_for_fix: Some(false) },
        ..Config::default()
    };
    let text = serialize(&skip).unwrap();
    assert!(text.contains("[gps]\nwait-for-fix = false"), "{text}");
    assert_eq!(parse(&text).unwrap().gps.wait_for_fix, Some(false));
}

#[test]
fn settings_serialize_omits_gps_table_when_rate_is_unset() {
    let text = serialize(&Config::default()).unwrap();
    assert!(!text.contains("[gps]"), "{text}");
}

#[test]
fn settings_serialize_writes_gps_table_that_reloads_when_rate_is_set() {
    assert_eq!(round_trip(&full()).gps.bridge_rate_hz, Some(2));
}

#[test]
fn settings_serialize_writes_every_key_that_reloads_when_all_are_set() {
    let saved = round_trip(&full());
    assert_eq!(saved.gps.bridge_rate_hz, Some(2));
    assert_eq!(saved.excluded_ssids, ["HomeNet", "Pixel 9"]);
    assert_eq!(saved.api_keys.wdgwars, "abc123def456");
    assert_eq!(saved.api_keys.wigle_name, "AIDexamplename");
    assert_eq!(saved.api_keys.wigle_token, "exampletoken789");
    assert_eq!(saved.bridge.link, Some(Link::Bluetooth));
    assert_eq!(saved.bridge.bluetooth_address.as_deref(), Some("02:00:5E:10:44:C1"));
}

#[test]
fn settings_parse_reads_link_when_bridge_table_names_one() {
    assert_eq!(
        parse("[bridge]\nlink = \"bluetooth\"\n").unwrap().bridge.link,
        Some(Link::Bluetooth)
    );
    assert_eq!(parse("[bridge]\nlink = \"usb\"\n").unwrap().bridge.link, Some(Link::Usb));
}

#[test]
fn settings_parse_rejects_link_when_it_names_no_link() {
    assert!(parse("[bridge]\nlink = \"wifi\"\n").is_err());
}

#[test]
fn settings_load_rejects_bluetooth_address_when_remember_is_off() {
    let chain =
        load_error("[bridge]\nremember = false\nbluetooth-address = \"02:00:5E:10:44:C1\"\n");
    assert!(chain.contains("bluetooth-address"), "{chain}");
}

#[test]
fn settings_serialize_writes_excluded_ssids_that_reload() {
    let text = serialize(&full()).unwrap();
    assert_eq!(parse(&text).unwrap().excluded_ssids, ["HomeNet", "Pixel 9"]);
    assert!(text.contains("excluded-ssids = [\"HomeNet\", \"Pixel 9\"]"), "{text}");
}

#[test]
fn settings_excluded_ssid_bytes_yields_each_entry_as_bytes_when_list_is_set() {
    let config = full();
    let bytes: Vec<Vec<u8>> = excluded_ssid_bytes(&config.excluded_ssids).collect();
    assert_eq!(bytes, [b"HomeNet".to_vec(), b"Pixel 9".to_vec()]);
}

#[test]
fn settings_serialize_omits_excluded_ssids_when_list_is_empty() {
    let text = serialize(&rate_only(3)).unwrap();
    assert!(!text.contains("excluded-ssids"), "{text}");
}

#[test]
fn settings_load_rejects_excluded_ssid_when_entry_is_empty() {
    let chain = load_error("excluded-ssids = [\"HomeNet\", \"\"]\n");
    assert!(chain.contains("excluded-ssids: an entry is empty"), "{chain}");
}

#[test]
fn settings_load_rejects_excluded_ssid_when_entry_is_over_32_bytes() {
    let chain = load_error(&format!("excluded-ssids = [\"{}\"]\n", "x".repeat(33)));
    assert!(chain.contains("longer than 32 bytes"), "{chain}");
}

#[test]
fn settings_load_rejects_excluded_ssid_when_entry_is_listed_twice() {
    let chain = load_error("excluded-ssids = [\"HomeNet\", \"Pixel 9\", \"HomeNet\"]\n");
    assert!(chain.contains("\"HomeNet\" is listed twice"), "{chain}");
}

#[test]
fn settings_serialize_omits_bridge_table_when_remember_is_unset() {
    let text = serialize(&rate_only(3)).unwrap();
    assert!(!text.contains("[bridge]"), "{text}");
}

#[test]
fn settings_serialize_writes_bridge_table_that_reloads_when_remember_is_off() {
    let config = Config {
        bridge: Bridge { remember: Some(false), ..Bridge::default() },
        ..Config::default()
    };
    let text = serialize(&config).unwrap();
    assert!(text.contains("[bridge]\nremember = false"), "{text}");
    assert_eq!(parse(&text).unwrap().bridge.remember, Some(false));
}

#[test]
fn settings_parse_rejects_unknown_key_when_under_bridge_table() {
    assert!(parse("[bridge]\nremember = true\nport = \"/dev/ttyACM0\"\n").is_err());
}

#[test]
fn settings_parse_reads_bridge_table_when_mac_is_set() {
    let config = parse("[bridge]\nmac = \"aa:BB:cc:01:02:03\"\n").unwrap();
    assert_eq!(config.bridge.remember, None);
    assert_eq!(config.bridge.mac, Some([0xAA, 0xBB, 0xCC, 1, 2, 3]));
}

#[test]
fn settings_serialize_writes_bridge_table_that_reloads_when_mac_is_set() {
    let text = serialize(&full()).unwrap();
    let saved = parse(&text).unwrap();
    assert_eq!(saved.bridge.remember, Some(true));
    assert_eq!(saved.bridge.mac, Some([0xAA, 0xBB, 0xCC, 1, 2, 3]));
    assert!(
        text.contains("[bridge]\nremember = true\nmac = \"AA:BB:CC:01:02:03\""),
        "as `wartui ports` prints it: {text}"
    );
    assert_eq!(saved.bridge.serial.as_deref(), Some("AAAAAAAAAA"));
}

#[test]
fn settings_load_rejects_bridge_mac_when_malformed() {
    let chain = load_error("[bridge]\nmac = \"AA:BB:CC\"\n");
    assert!(chain.contains("bridge.mac"), "{chain}");
}

#[test]
fn settings_refuses_bridge_mac_when_remember_is_off() {
    let chain = load_error("[bridge]\nremember = false\nmac = \"AA:BB:CC:01:02:03\"\n");
    assert!(chain.contains("bridge.mac"), "{chain}");
    let forgetting = Config {
        bridge: Bridge { remember: Some(false), mac: Some(BRIDGE), ..Bridge::default() },
        ..Config::default()
    };
    assert!(serialize(&forgetting).is_err(), "a save is refused the same way");
}

#[test]
fn settings_load_rejects_bridge_serial_when_remember_is_off() {
    let chain = load_error("[bridge]\nremember = false\nserial = \"AAAAAAAAAA\"\n");
    assert!(chain.contains("bridge.serial"), "{chain}");
}

#[test]
fn settings_serialize_writes_empty_api_keys_when_config_is_default() {
    let text = serialize(&Config::default()).unwrap();
    assert_eq!(text, "[api-keys]\nwdgwars = \"\"\nwigle-name = \"\"\nwigle-token = \"\"\n");
}

#[test]
fn settings_parse_reads_api_keys_table() {
    assert_eq!(parse("[api-keys]\nwdgwars = \"abc\"\n").unwrap().api_keys.wdgwars, "abc");
}

#[test]
fn settings_parse_reads_wigle_keys_when_api_keys_table_holds_them() {
    let text = "[api-keys]\nwigle-name = \"AIDexamplename\"\nwigle-token = \"tok\"\n";
    let keys = parse(text).unwrap().api_keys;
    assert_eq!(keys.wigle_name, "AIDexamplename");
    assert_eq!(keys.wigle_token, "tok");
    assert_eq!(keys.wdgwars, "", "an absent key reads as not set");
}

#[test]
fn settings_parse_returns_empty_api_key_when_table_absent() {
    assert_eq!(parse("[gps]\nbridge-rate-hz = 2\n").unwrap().api_keys.wdgwars, "");
}

#[test]
fn settings_parse_rejects_unknown_key_when_under_api_keys_table() {
    assert!(parse("[api-keys]\nbeacondb = \"abc\"\n").is_err());
}

#[test]
fn settings_serialize_writes_api_key_that_reloads_when_set() {
    assert_eq!(round_trip(&full()).api_keys.wdgwars, "abc123def456");
}

#[test]
fn settings_serialize_writes_wigle_keys_that_reload_when_set() {
    let config = Config {
        api_keys: ApiKeys {
            wigle_name: "AIDexamplename".to_owned(),
            wigle_token: "exampletoken789".to_owned(),
            ..ApiKeys::default()
        },
        ..Config::default()
    };
    let keys = round_trip(&config).api_keys;
    assert_eq!(keys.wigle_name, "AIDexamplename");
    assert_eq!(keys.wigle_token, "exampletoken789");
    assert_eq!(keys.wdgwars, "");
}

#[test]
fn settings_save_reports_nowhere_when_there_is_no_store() {
    let settings = Settings::new(None, Config::default(), BridgeMemory::default());
    assert!(settings.place().is_none());

    assert!(matches!(settings.save_wait_for_fix(false), Err(SaveError::Nowhere)));
    let (tx, mut rx) = mpsc::channel(4);
    let applied = controls(&tx, &settings)
        .apply(&unmoved(&Config::default(), true), &snapshot(None))
        .unwrap();
    assert!(matches!(applied.saved, Err(SaveError::Nowhere)), "{applied:?}");
    let excluded = controls(&tx, &settings).apply_excluded(&[]);
    assert!(matches!(excluded, Ok(Err(SaveError::Nowhere))));
    assert!(matches!(rx.try_recv(), Ok(Command::SetExcludedSsids { .. })), "applied anyway");
    assert_eq!(settings.saved(), Config::default(), "nothing saved, nothing changed");
}

#[test]
fn settings_save_keeps_link_as_store_had_it_when_bridge_has_not_announced_itself() {
    let store = Fake::holding("[bridge]\nlink = \"usb\"\n");
    let saved = load(store.as_ref()).unwrap();
    let settings = kept(&store, saved, &BridgeMemory::default()).over(Link::Bluetooth);

    settings.save_wait_for_fix(false).unwrap();
    assert_eq!(store.config().bridge.link, Some(Link::Usb));

    let store = Fake::holding("");
    let settings = kept(&store, Config::default(), &BridgeMemory::default()).over(Link::Bluetooth);
    settings.save_wait_for_fix(true).unwrap();
    assert_eq!(store.config().bridge.link, None, "an absent key stays absent");
}

#[test]
fn settings_follow_saves_link_when_bridge_announces_itself() {
    let store = Fake::empty();
    let settings = kept(&store, Config::default(), &BridgeMemory::default()).over(Link::Bluetooth);
    follow(&settings, &snapshot(None));
    assert_eq!(store.saves(), 0, "not before it connects");

    follow(&settings, &snapshot(Some(BRIDGE)));

    assert_eq!(store.config().bridge.link, Some(Link::Bluetooth));
    assert_eq!(settings.saved().bridge.link, Some(Link::Bluetooth));
}

#[test]
fn settings_follow_writes_nothing_when_run_is_over_usb_and_store_names_no_link() {
    // Absent means USB, so there is nothing to change.
    let store = Fake::empty();
    let settings = kept(&store, Config::default(), &BridgeMemory::default()).over(Link::Usb);
    follow(&settings, &snapshot(Some(BRIDGE)));
    assert_eq!(store.saves(), 0);
}

#[test]
fn settings_follow_saves_usb_when_store_names_bluetooth_and_run_is_over_usb() {
    let store = Fake::holding("[bridge]\nlink = \"bluetooth\"\n");
    let saved = load(store.as_ref()).unwrap();
    let settings = kept(&store, saved, &BridgeMemory::default()).over(Link::Usb);

    follow(&settings, &snapshot(Some(BRIDGE)));

    assert_eq!(store.config().bridge.link, Some(Link::Usb));
}

#[test]
fn settings_follow_saves_no_link_when_link_is_the_simulator() {
    let store = Fake::empty();
    let settings = kept(&store, Config::default(), &BridgeMemory::default()).simulated();
    follow(&settings, &snapshot(Some(BRIDGE)));
    assert_eq!(store.saves(), 0);
}

#[test]
fn settings_follow_saves_bluetooth_address_when_transport_remembers_one() {
    let store = Fake::empty();
    let memory = BridgeMemory::none();
    let settings = kept(&store, Config::default(), &memory);
    memory.remember_bluetooth(OTHER_BRIDGE, "02:00:5E:10:44:C1".to_owned());

    follow(&settings, &snapshot(None));

    let saved = store.config().bridge;
    assert_eq!(saved.mac, Some(OTHER_BRIDGE));
    assert_eq!(saved.bluetooth_address.as_deref(), Some("02:00:5E:10:44:C1"));
}

#[test]
fn settings_follow_saves_bridge_once_when_transport_remembers_it() {
    let store = Fake::empty();
    let memory = BridgeMemory::none();
    let settings = kept(&store, Config::default(), &memory);
    follow(&settings, &snapshot(None));
    assert_eq!(store.saves(), 0, "nothing changed, nothing written");

    memory.remember_on(OTHER_BRIDGE, None); // As the transport does on connecting.
    follow(&settings, &snapshot(None));
    follow(&settings, &snapshot(None));

    assert_eq!(store.saves(), 1, "saved once, not every frame");
    assert_eq!(store.config().bridge.mac, Some(OTHER_BRIDGE));
    assert_eq!(settings.saved().bridge.mac, Some(OTHER_BRIDGE), "the saved copy knows it too");
}

#[test]
fn settings_follow_saves_removal_when_transport_forgets_bridge() {
    let store = Fake::empty();
    let memory = BridgeMemory::none();
    let settings = kept(&store, Config::default(), &memory);
    memory.remember_on(OTHER_BRIDGE, None);
    follow(&settings, &snapshot(None));

    memory.forget(); // As the transport does when the remembered board goes quiet.
    follow(&settings, &snapshot(None));

    assert_eq!(store.config().bridge.mac, None);
}

#[test]
fn settings_follow_saves_remember_on_with_bridge_when_saved_copy_still_says_off() {
    // As after turning remember on in the modal with a save that failed.
    let store = Fake::empty();
    let memory = BridgeMemory::none();
    let saved = Config {
        bridge: Bridge { remember: Some(false), ..Bridge::default() },
        ..Config::default()
    };
    let settings = kept(&store, saved, &memory);
    memory.remember_on(OTHER_BRIDGE, None);

    follow(&settings, &snapshot(None));

    let bridge = store.config().bridge;
    assert_eq!(bridge.remember, Some(true));
    assert_eq!(bridge.mac, Some(OTHER_BRIDGE));
}

#[test]
fn settings_follow_saves_remember_off_without_bridge_when_saved_copy_still_says_on() {
    // As after turning remember off in the modal with a save that failed.
    let store = Fake::empty();
    let memory = BridgeMemory::holding(Some(OTHER_BRIDGE));
    let saved = Config {
        bridge: Bridge { remember: Some(true), mac: Some(OTHER_BRIDGE), ..Bridge::default() },
        ..Config::default()
    };
    let settings = kept(&store, saved, &memory);
    memory.set_enabled(false);

    follow(&settings, &snapshot(None));

    let bridge = store.config().bridge;
    assert_eq!(bridge.remember, Some(false));
    assert_eq!(bridge.mac, None);
}

#[test]
fn settings_follow_leaves_store_alone_when_remembered_bridge_unchanged() {
    let store = Fake::empty();
    let memory = BridgeMemory::none();
    let settings = kept(&store, Config::default(), &memory);
    memory.remember_on(OTHER_BRIDGE, None);
    follow(&settings, &snapshot(None));
    // A hand edit after the save. Only a change to the bridge rewrites the store.
    *store.text.lock().unwrap() = Some("# mine\n".to_owned());

    follow(&settings, &snapshot(None));
    memory.remember_on(OTHER_BRIDGE, None);
    follow(&settings, &snapshot(None));

    assert_eq!(store.text().as_deref(), Some("# mine\n"));
}

#[test]
fn settings_follow_tries_failed_bridge_save_again_when_bridge_changes() {
    let store = Fake::empty();
    store.fail(true);
    let memory = BridgeMemory::none();
    let settings = kept(&store, Config::default(), &memory);
    memory.remember_on(OTHER_BRIDGE, None);
    follow(&settings, &snapshot(None));
    assert_eq!(settings.saved().bridge.mac, None, "the save failed");

    // Now the save would succeed, but the bridge has not changed since it failed.
    store.fail(false);
    follow(&settings, &snapshot(None));
    assert_eq!(store.saves(), 0, "not retried every frame");

    memory.remember_on(BRIDGE, None);
    follow(&settings, &snapshot(None));
    assert_eq!(store.config().bridge.mac, Some(BRIDGE), "a new bridge is tried again");
}

#[test]
fn settings_follow_saves_bridge_when_it_returns_after_a_failed_save() {
    let store = Fake::empty();
    store.fail(true);
    let memory = BridgeMemory::none();
    let settings = kept(&store, Config::default(), &memory);
    memory.remember_on(OTHER_BRIDGE, None);
    follow(&settings, &snapshot(None));
    assert_eq!(settings.saved().bridge.mac, None, "the save failed");

    // Back to what the store holds, which clears the failed attempt.
    memory.forget();
    follow(&settings, &snapshot(None));
    store.fail(false);
    memory.remember_on(OTHER_BRIDGE, None);
    follow(&settings, &snapshot(None));

    assert_eq!(store.config().bridge.mac, Some(OTHER_BRIDGE));
}

#[test]
fn settings_apply_saves_remember_on_without_bridge_when_link_is_simulated() {
    let store = Fake::empty();
    let memory = BridgeMemory::none();
    memory.set_enabled(false);
    let settings = kept(&store, Config::default(), &memory).simulated();
    let (tx, _rx) = mpsc::channel(4);

    let applied = controls(&tx, &settings)
        .apply(&unmoved(&Config::default(), true), &snapshot(Some(BRIDGE)))
        .unwrap();

    assert_eq!(applied.remember, Some(true));
    assert!(memory.is_enabled());
    assert_eq!(memory.recall(), None, "the simulator's bridge is not a board");
    let bridge = store.config().bridge;
    assert_eq!(bridge.remember, Some(true));
    assert_eq!(bridge.mac, None);
}

#[test]
fn settings_apply_sends_nothing_when_command_queue_is_full() {
    let store = Fake::empty();
    let memory = BridgeMemory::none();
    let settings = kept(&store, Config::default(), &memory);
    // Room for one of the edit's two commands.
    let (tx, mut rx) = mpsc::channel(2);
    tx.try_send(Command::StopScan).unwrap();
    let edit = SettingsEdit {
        pool: Some(ChannelPool::Eu),
        gps_rate: Some(3),
        ..unmoved(&Config::default(), false)
    };

    assert!(controls(&tx, &settings).apply(&edit, &snapshot(Some(BRIDGE))).is_err());

    assert_eq!(rx.try_recv().unwrap(), Command::StopScan);
    assert!(rx.try_recv().is_err(), "nothing else was sent");
    assert!(memory.is_enabled(), "the remember switch did not move");
    assert_eq!(store.saves(), 0, "nothing was saved");
}
