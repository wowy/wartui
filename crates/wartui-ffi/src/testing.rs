//! A simulated bridge at the byte level, for a test's loopback radio.
//!
//! `wartui_bridge::sim::WireSim` is the simulator's Bluetooth side: Down's writes in, Up's
//! notification values out. A test's [`RadioDriver`](crate::radio::RadioDriver), in Rust, Kotlin
//! or Swift, sends its writes here and pumps [`WireSim::notified`] into
//! [`Radio::notified`](crate::radio::Radio::notified), which puts the whole binding under test
//! with no hardware: the radio adapter, the Bluetooth driver, the capture and the views.
//! [`testing_node_image`] is a node image a simulated fleet takes. Behind the `testing` feature,
//! which no shipped build enables.

use std::sync::Arc;

use tokio::sync::Mutex;
use wartui_app::update::testing::fixture;
use wartui_bridge::sim::{self, SimConfig, WireDown, WireUp};
use wartui_proto::bus::Board;

use crate::runtime::runtime;

/// A simulated bridge at the byte level, for a test's loopback radio.
#[derive(Debug, uniffi::Object)]
pub struct WireSim {
    /// The bridge's octets, which it advertises as its id.
    id: Vec<u8>,
    /// `None` when the simulator did not start: it takes nothing and notifies nothing.
    down: Option<WireDown>,
    up: Mutex<Option<WireUp>>,
}

#[uniffi::export]
impl WireSim {
    /// A simulated bridge with `nodes` nodes, behind a link whose ATT MTU is `mtu`.
    #[uniffi::constructor]
    #[must_use]
    pub fn start(nodes: u8, mtu: u16) -> Arc<Self> {
        let _inside = runtime().enter();
        let config = SimConfig { node_count: nodes, ..SimConfig::default() };
        match sim::WireSim::start(config, mtu) {
            Ok(wire) => {
                let mac = wire.mac();
                let (down, up) = wire.split();
                let id = vec![mac[4], mac[5]];
                Arc::new(Self { id, down: Some(down), up: Mutex::new(Some(up)) })
            }
            Err(error) => {
                tracing::warn!(%error, "the simulator did not start");
                Arc::new(Self { id: Vec::new(), down: None, up: Mutex::new(None) })
            }
        }
    }

    /// The service data the bridge advertises under the id UUID.
    #[must_use]
    pub fn id(&self) -> Vec<u8> {
        self.id.clone()
    }

    /// One write to Down.
    pub fn write(&self, bytes: Vec<u8>) {
        if let Some(down) = &self.down {
            down.write(&bytes);
        }
    }

    /// The next notification's value from Up, or `None` once the bridge is gone.
    pub async fn notified(&self) -> Option<Vec<u8>> {
        self.up.lock().await.as_mut()?.notified().await
    }
}

/// How long [`testing_node_image`] makes an image: one flash sector, so an update is quick.
const TESTING_IMAGE_LEN: usize = 0x1000;

/// A node app image for `board`, carrying `version`, that passes every check `NodeImage.parse`
/// makes. A board no node runs on gets one `parse` refuses.
#[uniffi::export]
#[must_use]
pub fn testing_node_image(board: Board, version: String) -> Vec<u8> {
    fixture(board, &version, TESTING_IMAGE_LEN)
}
