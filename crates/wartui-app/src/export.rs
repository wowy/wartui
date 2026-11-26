//! A capture written out as a WiGLE CSV, and what a report calls its nodes.
//!
//! **The capture is opened read-only**, so an export runs against a finished capture or one still
//! being written.
//!
//! **A name the front end chose is never written over** ([`OutName::Chosen`]): the operator did
//! not pick it, and the file it would replace may be the one already uploaded. A failure part way
//! through removes the partial file under that name, so the retry is not refused by what the fault
//! left behind. A name the operator gave ([`OutName::Named`]) is replaced, and a partial file is
//! left there for them to look at.
//!
//! [`NodeNames`] are data: a slot's label, or an address's tail.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{File, OpenOptions};
use std::io::{self, BufWriter, Write};
use std::path::Path;

use wartui_core::analyze::node_slots;
use wartui_core::export::{ExportFilter, ExportSummary};
use wartui_core::store::{Connection, StoreError, open_readonly};
use wartui_proto::bus::{NodeBoard, Slot};
use wartui_proto::mac::{self, Mac};

/// Whose name the CSV is written under.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutName {
    /// The operator's: a file there is replaced, and a partial export is left in place.
    Named,
    /// The front end's own: a file there is refused, and a partial export is removed.
    Chosen,
}

/// What an export wrote, and what its report calls the capture's nodes.
#[derive(Debug, Clone)]
pub struct Exported {
    /// What the export wrote and what the capture held.
    pub summary: ExportSummary,
    /// The capture's nodes, named.
    pub names: NodeNames,
}

/// Why an export wrote nothing, or not all of it.
#[derive(Debug, thiserror::Error)]
pub enum ExportError {
    /// The capture could not be opened.
    #[error("opening the capture")]
    Open(#[source] StoreError),
    /// A file is already at a name the front end chose.
    #[error("the export's file exists")]
    Exists,
    /// The CSV could not be created.
    #[error("creating the export's file")]
    Create(#[source] io::Error),
    /// The export's own failure: reading the capture or writing a row.
    #[error(transparent)]
    Export(wartui_core::export::ExportError),
    /// The last rows could not be written out.
    #[error("flushing the export")]
    Flush(#[source] io::Error),
}

/// Write the capture at `db` to `out` as a WiGLE CSV of the rows `filter` asks for, stamped with
/// `app_version`, under the rules [`OutName`] gives `name`.
///
/// # Errors
/// The step that failed.
pub fn wigle_csv(
    db: &Path,
    out: &Path,
    name: OutName,
    filter: &ExportFilter,
    app_version: &str,
) -> Result<Exported, ExportError> {
    let conn = open_readonly(db).map_err(ExportError::Open)?;
    let names = NodeNames::of(&conn);
    let file = create(out, name)?;
    let mut writer = BufWriter::new(file);
    let written = wartui_core::export::wigle_csv(&conn, filter, &mut writer, app_version)
        .map_err(ExportError::Export)
        .and_then(|summary| writer.flush().map(|()| summary).map_err(ExportError::Flush));
    if written.is_err() && name == OutName::Chosen {
        let _ = std::fs::remove_file(out);
    }
    Ok(Exported { summary: written?, names })
}

/// Open the CSV at `path`. `create_new` for a name the front end chose, rather than asking whether
/// the path is there and then creating it, so nothing can appear in the gap between the two.
fn create(path: &Path, name: OutName) -> Result<File, ExportError> {
    match name {
        OutName::Named => File::create(path).map_err(ExportError::Create),
        OutName::Chosen => match OpenOptions::new().write(true).create_new(true).open(path) {
            Ok(file) => Ok(file),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => Err(ExportError::Exists),
            Err(error) => Err(ExportError::Create(error)),
        },
    }
}

/// What the operator calls a node: its slot's label, which is on the carrier for them to see, or
/// the tail of its address for a node with no slot, which has nothing else to go by.
#[must_use]
pub fn node_name(board: NodeBoard, slot: Option<Slot>, mac: &Mac) -> String {
    slot.map_or_else(|| mac::short(mac).to_string(), |slot| board.label(slot).to_string())
}

/// What a capture's reports call its nodes, and the order they list them in: [`node_name`] for
/// each, with the address tail after the label when two nodes' latest heartbeats name one slot, so
/// every section of a report names a node alike.
#[derive(Debug, Clone)]
pub struct NodeNames {
    board: NodeBoard,
    /// Each node's slot (`wartui_core::analyze::node_slots`).
    slots: BTreeMap<Mac, Slot>,
    /// Slots more than one node's latest heartbeat names.
    shared: BTreeSet<Slot>,
}

impl NodeNames {
    /// Names for the nodes on `board` whose slots `slots` gives. A node missing from it has none.
    #[must_use]
    pub fn new(board: NodeBoard, slots: &BTreeMap<Mac, Slot>) -> Self {
        let mut seen = BTreeSet::new();
        let shared = slots.values().copied().filter(|slot| !seen.insert(*slot)).collect();
        Self { board, slots: slots.clone(), shared }
    }

    /// Names for a capture whose slots are unknown: every node goes by its address tail.
    #[must_use]
    pub fn by_address() -> Self {
        Self::new(NodeBoard::NoCarrier, &BTreeMap::new())
    }

    /// Names for the nodes of the capture `conn` holds. A capture whose slots cannot be read
    /// names every node by its address tail: the names only label a summary, so they never fail
    /// the export or upload it describes.
    #[must_use]
    pub fn of(conn: &Connection) -> Self {
        match node_slots(conn) {
            Ok((board, slots)) => Self::new(board, &slots),
            Err(error) => {
                tracing::warn!(%error, "cannot read the capture's node slots; naming nodes by address");
                Self::by_address()
            }
        }
    }

    /// What the reports call `mac`.
    #[must_use]
    pub fn name(&self, mac: &Mac) -> String {
        let slot = self.slots.get(mac).copied();
        let name = node_name(self.board, slot, mac);
        if slot.is_some_and(|slot| self.shared.contains(&slot)) {
            format!("{name} {}", mac::short(mac))
        } else {
            name
        }
    }

    /// Where `mac` sorts in a list of nodes: by slot, then slotless nodes, ties by address.
    #[must_use]
    pub fn order(&self, mac: &Mac) -> (bool, Option<Slot>, Mac) {
        let slot = self.slots.get(mac).copied();
        (slot.is_none(), slot, *mac)
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use wartui_core::export::ExportFilter;
    use wartui_core::record::{BridgeSeen, Heartbeat, Record};
    use wartui_proto::bus::{NodeBoard, NodeBus, Slot};
    use wartui_proto::mac::Mac;
    use wartui_proto::reset::ResetCause;

    use super::{ExportError, NodeNames, OutName, node_name, wigle_csv};
    use crate::upload::testing::{EPOCH_MS, capture, sighting};

    /// The node `upload::testing`'s sightings come from.
    const MAC: Mac = [0x02, 0x00, 0x5E, 0x10, 0x57, 0x84];

    /// Every sighting, one row per network.
    fn every_row() -> ExportFilter {
        ExportFilter { recapture_secs: 0, ..ExportFilter::default() }
    }

    /// A capture on a 14-node carrier whose node heartbeats from N2 and saw one network.
    fn slotted(dir: &Path) -> PathBuf {
        let bridge = BridgeSeen {
            mac: [0x02, 0x00, 0x5E, 0x10, 0x9D, 0x24],
            fw_version: "0.1.0".to_owned(),
            node_board: NodeBoard::Carrier14,
        };
        let beat = Heartbeat {
            node_mac: MAC,
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
        capture(dir, records)
    }

    /// A file whose export fails once the CSV is created: an empty database, which opens and
    /// holds no capture's tables.
    fn hollow(dir: &Path) -> PathBuf {
        let db = dir.join("hollow.db");
        std::fs::write(&db, "").unwrap();
        db
    }

    #[test]
    fn export_writes_the_csv_and_names_nodes_when_capture_has_slots() {
        let dir = tempfile::tempdir().unwrap();
        let db = slotted(dir.path());
        let out = dir.path().join("tonight.csv");

        let exported = wigle_csv(&db, &out, OutName::Chosen, &every_row(), "0.1.0").unwrap();

        assert_eq!(exported.summary.rows, 1);
        assert_eq!(exported.names.name(&MAC), "N2");
        let csv = std::fs::read_to_string(&out).unwrap();
        assert!(csv.starts_with("WigleWifi-1.6,appRelease=0.1.0,"), "{csv}");
        assert!(csv.contains("02:00:00:00:00:01"), "{csv}");
    }

    #[test]
    fn export_refuses_file_and_leaves_it_when_chosen_name_exists() {
        let dir = tempfile::tempdir().unwrap();
        let db = capture(dir.path(), Vec::new());
        let out = dir.path().join("wartui-2026-09-18-14-30-07.csv");
        std::fs::write(&out, "the export that was uploaded").unwrap();

        let error = wigle_csv(&db, &out, OutName::Chosen, &every_row(), "0.1.0").unwrap_err();

        assert!(matches!(error, ExportError::Exists), "{error:?}");
        assert_eq!(std::fs::read_to_string(&out).unwrap(), "the export that was uploaded");
    }

    #[test]
    fn export_replaces_file_when_name_is_the_operators() {
        let dir = tempfile::tempdir().unwrap();
        let db = capture(dir.path(), Vec::new());
        let out = dir.path().join("tonight.csv");
        std::fs::write(&out, "an export of the capture before").unwrap();

        wigle_csv(&db, &out, OutName::Named, &every_row(), "0.1.0").unwrap();

        assert!(std::fs::read_to_string(&out).unwrap().starts_with("WigleWifi-1.6,"));
    }

    #[test]
    fn export_removes_partial_file_when_export_under_chosen_name_fails() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("wartui-2026-09-18-14-30-07.csv");

        let error = wigle_csv(&hollow(dir.path()), &out, OutName::Chosen, &every_row(), "0.1.0")
            .unwrap_err();

        assert!(matches!(error, ExportError::Export(_)), "{error:?}");
        // Nothing left under the name, so the retry that fixes the fault is not refused by what
        // the fault left behind.
        assert!(!out.exists(), "an empty export was left at {}", out.display());
    }

    #[test]
    fn export_keeps_partial_file_when_export_under_operators_name_fails() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("tonight.csv");

        let error = wigle_csv(&hollow(dir.path()), &out, OutName::Named, &every_row(), "0.1.0")
            .unwrap_err();

        assert!(matches!(error, ExportError::Export(_)), "{error:?}");
        // They named the file, so what happened to it is theirs to look at.
        assert!(out.exists());
    }

    #[test]
    fn export_refuses_capture_when_it_cannot_be_opened() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("tonight.csv");
        let missing = dir.path().join("missing.db");

        let error = wigle_csv(&missing, &out, OutName::Chosen, &every_row(), "0.1.0").unwrap_err();

        assert!(matches!(error, ExportError::Open(_)), "{error:?}");
        assert!(!out.exists(), "nothing created before the capture opened");
    }

    #[test]
    fn node_names_of_names_nodes_by_address_tail_when_slots_cannot_be_read() {
        // No tables at all, so reading the slots fails.
        let conn = wartui_core::store::Connection::open_in_memory().unwrap();
        let names = NodeNames::of(&conn);
        assert_eq!(names.name(&MAC), "57:84");
    }

    #[test]
    fn node_name_gives_slot_label_when_node_has_slot() {
        let slot = Slot::new(1, 0).expect("B1");
        assert_eq!(node_name(NodeBoard::Carrier14, Some(slot), &MAC), "N8");
    }

    #[test]
    fn node_name_gives_address_tail_when_node_has_no_slot() {
        assert_eq!(node_name(NodeBoard::Carrier14, None, &MAC), "57:84");
    }
}
