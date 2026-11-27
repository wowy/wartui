//! A capture written out as a WiGLE CSV: `wartui_app::export::wigle_csv`, for the phone.
//!
//! **Off the phone's thread.** The export reads the whole capture and writes the file, which is
//! disk I/O for as long as the capture is large, so it runs on the runtime's blocking pool and the
//! phone only awaits it.
//!
//! **The phone's path is the app's own choice** (`OutName::Chosen`): a file already there is
//! refused rather than written over, since it may be the export already uploaded, and a failure
//! part way through removes the partial file, so a retry is not refused by it.
//!
//! **[`ExportOptions`] are `wartui export`'s flags**, each defaulting as the CLI does. The
//! excluded list is the settings', as the CLI's is. The summary crosses flattened: every count as
//! the export counted it, and each node by the name a report gives it
//! (`wartui_app::export::NodeNames`).

use std::path::Path;
use std::sync::Arc;

use wartui_app::export::{self as app, ExportError as AppError, OutName};
use wartui_core::export::{
    Bands, DEFAULT_RECAPTURE_SECS, ExportError as CoreError, ExportFilter, ExportSummary,
    KindStats, NodeStats, Positions,
};
use wartui_core::store::StoreError;
use wartui_proto::mac;

use crate::runtime::runtime;
use crate::settings::{Settings, chain};
use crate::temp;

/// Which rows an export writes.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct ExportOptions {
    /// How long a network's sightings fold into one row, in seconds. `None` for an hour, as
    /// `wartui export` defaults; `0` writes one row per network.
    #[uniffi(default)]
    pub recapture_secs: Option<u32>,
}

/// One node's share of an export.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct ExportedNode {
    /// Its whole address.
    pub mac: String,
    /// What a report calls it: its slot's label, or its address's tail.
    pub name: String,
    /// Wi-Fi sightings it reported.
    pub wifi: u64,
    /// Bluetooth sightings it reported.
    pub ble: u64,
}

/// What an export wrote and what the capture held.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct Exported {
    /// Rows written: one per network per window with a positioned sighting.
    pub rows: u64,
    /// Rows left out because no sighting in their window had a position.
    pub unpositioned: u64,
    /// Sightings left out because their stored kind is neither Wi-Fi nor Bluetooth.
    pub unknown_kind: u64,
    /// Wi-Fi sightings left out for a zero, broadcast or multicast address.
    pub invalid_address: u64,
    /// Wi-Fi sightings left out because their SSID is excluded.
    pub excluded_ssid: u64,
    /// Wi-Fi sightings with channel zero, those left out for an invalid address included.
    pub wifi_channel_zero: u64,
    /// Distinct Wi-Fi addresses.
    pub wifi_networks: u64,
    /// Wi-Fi sightings.
    pub wifi_sightings: u64,
    /// Wi-Fi rows written.
    pub wifi_rows: u64,
    /// Distinct Bluetooth addresses.
    pub ble_devices: u64,
    /// Bluetooth sightings.
    pub ble_sightings: u64,
    /// Bluetooth rows written.
    pub ble_rows: u64,
    /// Wi-Fi networks heard on 2.4 GHz.
    pub wifi_networks_2_4_ghz: u64,
    /// Wi-Fi networks heard on 5 GHz.
    pub wifi_networks_5_ghz: u64,
    /// Wi-Fi networks heard on a channel outside the scan table.
    pub wifi_networks_other_band: u64,
    /// Sightings placed by a GPS receiver.
    pub placed_by_gps: u64,
    /// Sightings placed by a phone's location services.
    pub placed_by_platform: u64,
    /// Sightings placed by a typed-in position.
    pub placed_by_fixed: u64,
    /// Sightings with no position.
    pub placed_nowhere: u64,
    /// Each node's sightings, sorted by address.
    pub nodes: Vec<ExportedNode>,
    /// The earliest sighting's receive time, in Unix milliseconds.
    pub first_rx_ms: Option<i64>,
    /// The latest sighting's receive time, in Unix milliseconds.
    pub last_rx_ms: Option<i64>,
    /// The highest observation id read, which an upload records as its cutoff.
    pub last_id: Option<i64>,
}

impl From<app::Exported> for Exported {
    fn from(exported: app::Exported) -> Self {
        let app::Exported { summary, names } = exported;
        // Whole, so a count added to the summary fails to compile here.
        let ExportSummary {
            rows,
            unpositioned,
            unknown_kind,
            invalid_address,
            excluded_ssid,
            wifi_channel_zero,
            wifi,
            ble,
            wifi_bands,
            positions,
            nodes,
            first_rx,
            last_rx,
            last_id,
        } = summary;
        let KindStats { networks: wifi_networks, sightings: wifi_sightings, rows: wifi_rows } =
            wifi;
        let KindStats { networks: ble_devices, sightings: ble_sightings, rows: ble_rows } = ble;
        let Bands { ghz2_4, ghz5, other } = wifi_bands;
        let Positions { gps, platform, fixed, none } = positions;
        let nodes = nodes
            .into_iter()
            .map(|NodeStats { mac, wifi, ble }| ExportedNode {
                mac: mac::full(&mac).to_string(),
                name: names.name(&mac),
                wifi,
                ble,
            })
            .collect();
        Self {
            rows,
            unpositioned,
            unknown_kind,
            invalid_address,
            excluded_ssid,
            wifi_channel_zero,
            wifi_networks,
            wifi_sightings,
            wifi_rows,
            ble_devices,
            ble_sightings,
            ble_rows,
            wifi_networks_2_4_ghz: ghz2_4,
            wifi_networks_5_ghz: ghz5,
            wifi_networks_other_band: other,
            placed_by_gps: gps,
            placed_by_platform: platform,
            placed_by_fixed: fixed,
            placed_nowhere: none,
            nodes,
            first_rx_ms: first_rx,
            last_rx_ms: last_rx,
            last_id,
        }
    }
}

/// Why an export wrote nothing, or not all of it. `detail` is SQLite's or the OS's own text.
#[derive(Debug, thiserror::Error, uniffi::Error)]
pub enum ExportError {
    /// The capture could not be opened.
    #[error("opening the capture: {detail}")]
    Open { detail: String },
    /// The capture is another build's: its schema is `found`, and this build reads `ours`.
    #[error("the capture is schema v{found}, not v{ours}")]
    SchemaMismatch { found: i32, ours: i32 },
    /// The file is no wartui capture.
    #[error("the file is not a capture")]
    NotACapture,
    /// A file is already at the export's path.
    #[error("the export's file exists")]
    Exists,
    /// The CSV could not be created.
    #[error("creating the export's file: {detail}")]
    Create { detail: String },
    /// Reading the capture failed.
    #[error("reading the capture: {detail}")]
    Read { detail: String },
    /// Writing a row failed.
    #[error("writing the export: {detail}")]
    Write { detail: String },
    /// The last rows could not be written out.
    #[error("flushing the export: {detail}")]
    Flush { detail: String },
    /// On Android, neither `SQLITE_TMPDIR` nor `TMPDIR` names a directory SQLite can sort in.
    /// An Android app sets `SQLITE_TMPDIR` to a directory of its own, such as its cache
    /// directory, before its first export or upload. Nothing was written.
    #[error("no temporary directory: set SQLITE_TMPDIR")]
    NoTemporaryDirectory,
    /// The export's task panicked.
    #[error("the export panicked")]
    Panicked,
}

impl From<AppError> for ExportError {
    fn from(error: AppError) -> Self {
        match error {
            AppError::Open(StoreError::SchemaMismatch { found, ours }) => {
                Self::SchemaMismatch { found, ours }
            }
            AppError::Open(StoreError::NotACapture) => Self::NotACapture,
            AppError::Open(error) => Self::Open { detail: chain(&error) },
            AppError::Exists => Self::Exists,
            AppError::Create(error) => Self::Create { detail: chain(&error) },
            AppError::Export(CoreError::Sqlite(error)) => Self::Read { detail: chain(&error) },
            AppError::Export(CoreError::Io(error)) => Self::Write { detail: chain(&error) },
            AppError::Flush(error) => Self::Flush { detail: chain(&error) },
        }
    }
}

/// Write `db_path`'s capture as a WiGLE CSV at `out_path`, which must not exist, stamped with
/// `app_version`. The Wi-Fi networks `settings` exclude are left out.
///
/// # Errors
/// The step that failed.
#[uniffi::export]
pub async fn export_wigle_csv(
    settings: Arc<Settings>,
    db_path: String,
    out_path: String,
    options: ExportOptions,
    app_version: String,
) -> Result<Exported, ExportError> {
    let excluded = settings.app().saved().excluded_ssids;
    let filter = ExportFilter {
        recapture_secs: options.recapture_secs.map_or(DEFAULT_RECAPTURE_SECS, u64::from),
        after_uploads: None,
        excluded_ssids: wartui_app::settings::excluded_ssid_bytes(&excluded).collect(),
    };
    let writing = runtime().spawn_blocking(move || {
        if cfg!(target_os = "android") && !temp::found() {
            return Err(ExportError::NoTemporaryDirectory);
        }
        let (db, out) = (Path::new(&db_path), Path::new(&out_path));
        Ok(app::wigle_csv(db, out, OutName::Chosen, &filter, &app_version)?.into())
    });
    match writing.await {
        Ok(written) => written,
        Err(_) => Err(ExportError::Panicked),
    }
}
