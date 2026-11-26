//! The simulated bridge reached by bytes, as a central reaches a real one over Bluetooth.
//!
//! [`SimTransport`] hands back events already decoded, so nothing on it drives the Bluetooth
//! driver (`crate::ble`) against a bridge's real traffic. [`WireSim`] is that bridge's Bluetooth
//! side: what a central writes to Down goes in, and Up's notification values come out, framed as
//! the P4 frames its host stream. Behind a radio a test writes, it puts the framing, the gate and
//! `Identify` under test end to end with no hardware.
//!
//! It answers as a real bridge does on this link, and as `ble/fake.rs`'s bridge does:
//!
//! - **It says nothing until asked.** The first `Identify` attaches the central. Until then it
//!   sends no notification, and the messages the simulator emits are dropped, as a P4 with its
//!   host gate closed sends none. Each `Identify` is answered with a `Ready` built from the
//!   simulator's announcement, its uptime counted on from it, so two answers read as one life
//!   (`crate::gate`). One that comes before the announcement is answered when it arrives.
//! - **Every other command goes on the bulk queue** ([`LinkHandle::send_bulk`]). The central
//!   already ordered them on Down, and the P4 takes host frames in the order they arrive: the
//!   urgent queue's bias would reorder them. The bulk queue is also the deeper.
//! - **A frame goes as values of at most the ATT MTU less its 3-byte header**, each value within
//!   one frame. Up is a byte stream, and the driver reassembles frames however values cut them.
//!
//! Dropping both halves drops the simulator's [`LinkHandle`], which stops it.

use tokio::sync::mpsc;
use tokio::time::Instant;
use wartui_proto::link::{
    BridgeToHost, FrameAccumulator, HostToBridge, MAX_FRAME, ShortStr, decode_frame, encode_frame,
};
use wartui_proto::mac::Mac;

use super::{BRIDGE_MAC, SimConfig, SimTransport};
use crate::{BridgeInfo, LinkEvent, LinkHandle, TransportError};

/// The ATT header a notification carries ahead of its value.
const ATT_HEADER: u16 = 3;

/// A simulated bridge and fleet, reached as a central reaches a real one: by bytes.
#[derive(Debug)]
pub struct WireSim {
    down: WireDown,
    up: WireUp,
}

impl WireSim {
    /// Start the fleet `config` describes behind a link whose ATT MTU is `mtu`.
    /// Call it inside a tokio runtime.
    ///
    /// # Errors
    /// What [`SimTransport::start`] returns.
    pub fn start(config: SimConfig, mtu: u16) -> Result<Self, TransportError> {
        let link = SimTransport::new(config).start()?;
        let (down_tx, down_rx) = mpsc::unbounded_channel();
        // Unbounded: only the simulator's own rate fills it, and a central drains it as it reads.
        let (up_tx, up_rx) = mpsc::unbounded_channel();
        let side = Side {
            up: up_tx,
            value_max: usize::from(mtu.saturating_sub(ATT_HEADER)).max(1),
            announced: None,
            asked: false,
            attached: false,
        };
        tokio::spawn(run(link, down_rx, side));
        Ok(Self { down: WireDown(down_tx), up: WireUp(up_rx) })
    }

    /// The address the simulated bridge announces in its `Ready`.
    #[must_use]
    pub const fn mac(&self) -> Mac {
        BRIDGE_MAC
    }

    /// The two halves: what a central writes to, and what it is notified from.
    #[must_use]
    pub fn split(self) -> (WireDown, WireUp) {
        (self.down, self.up)
    }
}

/// The Down characteristic. Cheap to clone.
#[derive(Debug, Clone)]
pub struct WireDown(mpsc::UnboundedSender<Vec<u8>>);

impl WireDown {
    /// One ATT write's value, as a central wrote it. Never blocks.
    pub fn write(&self, bytes: &[u8]) {
        // A simulator that has stopped takes nothing, as a bridge that has gone does.
        let _ = self.0.send(bytes.to_vec());
    }
}

/// The Up characteristic.
#[derive(Debug)]
pub struct WireUp(mpsc::UnboundedReceiver<Vec<u8>>);

impl WireUp {
    /// The next notification's value, at most `mtu - 3` bytes, or `None` once the bridge is gone.
    pub async fn notified(&mut self) -> Option<Vec<u8>> {
        self.0.recv().await
    }
}

/// The bridge's side of Up, and what it knows of the central.
struct Side {
    up: mpsc::UnboundedSender<Vec<u8>>,
    /// The most bytes one value carries.
    value_max: usize,
    /// The simulator's announcement, and when it came.
    announced: Option<(BridgeInfo, Instant)>,
    /// An `Identify` came before the announcement, and is answered by it.
    asked: bool,
    /// An `Identify` has been answered, so the simulator's messages reach the central.
    attached: bool,
}

impl Side {
    /// Answer an `Identify` with a `Ready`, or once the simulator has announced itself.
    fn identify(&mut self) {
        let Some((info, at)) = &self.announced else {
            self.asked = true;
            return;
        };
        let ready = ready(info, *at);
        self.attached = true;
        self.notify(&ready);
    }

    /// The simulator announced itself as `info`.
    fn announce(&mut self, info: BridgeInfo) {
        self.announced = Some((info, Instant::now()));
        if std::mem::take(&mut self.asked) {
            self.identify();
        }
    }

    /// Send `msg` to Up as one frame, cut into values.
    fn notify(&self, msg: &BridgeToHost) {
        let mut buf = [0u8; MAX_FRAME];
        match encode_frame(msg, &mut buf) {
            Ok(len) => {
                for value in buf[..len].chunks(self.value_max) {
                    // A central that let Up go reads nothing more.
                    let _ = self.up.send(value.to_vec());
                }
            }
            Err(e) => tracing::debug!(error = %e, "a simulated message did not fit a frame"),
        }
    }
}

/// The `Ready` a bridge that announced itself as `info` at `at` answers an `Identify` with.
fn ready(info: &BridgeInfo, at: Instant) -> BridgeToHost {
    let since = u32::try_from(at.elapsed().as_millis()).unwrap_or(u32::MAX);
    BridgeToHost::Ready {
        mac: info.mac,
        fw_version: ShortStr::try_from(info.fw_version.as_str()).unwrap_or_default(),
        reset_cause: info.reset_cause,
        last_phase: info.last_phase,
        uptime_ms: info.uptime_ms.saturating_add(since),
        node_board: info.node_board,
        radio: info.radio.clone(),
        image: info.image,
    }
}

/// Carry Down's bytes to the simulator and its messages to Up, until both halves are gone or the
/// simulator stops.
async fn run(mut link: LinkHandle, mut down: mpsc::UnboundedReceiver<Vec<u8>>, mut side: Side) {
    let mut acc = FrameAccumulator::<MAX_FRAME>::new();
    let mut writable = true;
    loop {
        tokio::select! {
            bytes = down.recv(), if writable => {
                let Some(bytes) = bytes else {
                    writable = false;
                    continue;
                };
                for &byte in &bytes {
                    let Some(frame) = acc.push(byte) else { continue };
                    match decode_frame::<HostToBridge>(frame) {
                        Ok(HostToBridge::Identify) => side.identify(),
                        Ok(cmd) => {
                            if let Err(e) = link.send_bulk(cmd) {
                                tracing::debug!(error = %e, "the simulator did not take a command");
                            }
                        }
                        Err(e) => tracing::debug!(error = %e, "undecodable host frame on Down"),
                    }
                }
            }
            event = link.recv() => match event {
                Some(LinkEvent::Connected(info)) => side.announce(info),
                Some(LinkEvent::Message(msg)) if side.attached => side.notify(&msg),
                Some(_) => {}
                None => return,
            },
            () = side.up.closed(), if !writable => break,
        }
    }
    link.close().await;
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex, PoisonError};
    use std::time::Duration;

    use futures_util::future::BoxFuture;
    use futures_util::stream::{self, BoxStream};
    use futures_util::{FutureExt as _, StreamExt as _};
    use wartui_proto::ble::ATT_MTU;
    use wartui_proto::bus::UpType;
    use wartui_proto::link::{
        BridgeToHost, FrameAccumulator, HostToBridge, MAX_FRAME, MAX_HOST_FRAME, decode_frame,
    };

    use super::{WireDown, WireSim, WireUp};
    use crate::ble::BleTransport;
    use crate::pipe::{
        Connected, Connection, Incoming, Radio, RadioError, RadioErrorKind, RadioTraits, Recalled,
        Sighting, WriteKind,
    };
    use crate::sim::SimConfig;
    use crate::wire::encode_for_wire;
    use crate::{LinkEvent, LinkHandle};

    /// The longest any test waits on one thing, on the paused clock.
    const WITHIN: Duration = Duration::from_secs(60);

    /// A small fleet, quickly.
    fn fleet() -> SimConfig {
        SimConfig { node_count: 2, speed: 10.0, ..SimConfig::default() }
    }

    /// The bytes a central writes to ask the bridge to announce itself, as a connection's first.
    fn identify() -> Vec<u8> {
        let mut buf = [0u8; MAX_HOST_FRAME + 1];
        let len = encode_for_wire(&HostToBridge::Identify, true, &mut buf).expect("fits");
        buf[..len].to_vec()
    }

    /// Up's values, read as the driver reads them.
    struct Reader {
        up: WireUp,
        acc: FrameAccumulator<MAX_FRAME>,
        /// The length of every value read.
        lens: Vec<usize>,
    }

    impl Reader {
        fn new(up: WireUp) -> Self {
            Self { up, acc: FrameAccumulator::new(), lens: Vec::new() }
        }

        /// The next message, across as many values as it takes.
        async fn next(&mut self) -> BridgeToHost {
            loop {
                let value = tokio::time::timeout(WITHIN, self.up.notified()).await;
                let value = value.expect("a value in time").expect("the bridge is up");
                self.lens.push(value.len());
                let mut decoded = None;
                for byte in value {
                    if let Some(frame) = self.acc.push(byte) {
                        decoded = Some(decode_frame(frame).expect("a bridge frame"));
                    }
                }
                // Each value is within one frame, so it ends at most one.
                if let Some(msg) = decoded {
                    return msg;
                }
            }
        }

        /// Read until a node's heartbeat.
        async fn heartbeat(&mut self) {
            while !is_heartbeat(&self.next().await) {}
        }
    }

    fn is_heartbeat(msg: &BridgeToHost) -> bool {
        matches!(msg, BridgeToHost::FromNode { kind, .. } if *kind == UpType::Heartbeat.as_u8())
    }

    /// A started sim's halves, its address and a reader on Up.
    fn started(mtu: u16) -> (WireDown, Reader, wartui_proto::mac::Mac) {
        let sim = WireSim::start(fleet(), mtu).expect("the simulator starts");
        let mac = sim.mac();
        let (down, up) = sim.split();
        (down, Reader::new(up), mac)
    }

    #[tokio::test(start_paused = true)]
    async fn wire_sim_sends_nothing_when_no_identify_was_written() {
        let (_down, mut reader, _) = started(ATT_MTU);
        let heard = tokio::time::timeout(Duration::from_secs(30), reader.up.notified()).await;
        assert!(heard.is_err(), "{heard:?}");
    }

    #[tokio::test(start_paused = true)]
    async fn wire_sim_answers_ready_when_identify_is_written() {
        let (down, mut reader, mac) = started(ATT_MTU);
        down.write(&identify());
        let BridgeToHost::Ready { mac: announced, .. } = reader.next().await else {
            panic!("not a Ready first");
        };
        assert_eq!(announced, mac);
    }

    #[tokio::test(start_paused = true)]
    async fn wire_sim_splits_frames_to_the_mtu_when_notifying() {
        for mtu in [23, ATT_MTU] {
            let (down, mut reader, _) = started(mtu);
            down.write(&identify());
            for _ in 0..3 {
                reader.heartbeat().await;
            }
            let max = usize::from(mtu - 3);
            assert!(reader.lens.iter().all(|&len| len <= max), "{mtu}: {:?}", reader.lens);
            if mtu == 23 {
                // A heartbeat's frame is longer than one value, so it was split.
                assert!(reader.lens.contains(&max), "{:?}", reader.lens);
            }
        }
    }

    #[tokio::test(start_paused = true)]
    async fn wire_sim_forwards_node_records_when_identified() {
        let (down, mut reader, _) = started(ATT_MTU);
        down.write(&identify());
        assert!(matches!(reader.next().await, BridgeToHost::Ready { .. }));
        reader.heartbeat().await;
    }

    /// A radio that hears one bridge, a [`WireSim`], and reaches it at once.
    #[derive(Debug)]
    struct WireRadio {
        octets: [u8; 2],
        mtu: u16,
        wire: Mutex<Option<(WireDown, WireUp)>>,
    }

    impl WireRadio {
        fn new(sim: WireSim, mtu: u16) -> Self {
            let mac = sim.mac();
            Self { octets: [mac[4], mac[5]], mtu, wire: Mutex::new(Some(sim.split())) }
        }
    }

    const PEER: &str = "wire-sim";

    impl Radio for WireRadio {
        fn traits(&self) -> RadioTraits {
            RadioTraits { pairs_on_connect: false, macos: false }
        }

        fn recall<'a>(
            &'a self,
            _key: &'a str,
        ) -> BoxFuture<'a, Result<Option<Recalled>, RadioError>> {
            async { Ok(None) }.boxed()
        }

        fn scan(&self) -> BoxFuture<'_, Result<BoxStream<'static, Sighting>, RadioError>> {
            let sighting =
                Sighting { peer: PEER.to_owned(), octets: self.octets, name: None, live: true };
            // Heard once, and the scan goes on reporting nothing: it never ends on its own.
            let sightings = stream::once(async { sighting }).chain(stream::pending()).boxed();
            async { Ok(sightings) }.boxed()
        }

        fn stop_scan(&self) -> BoxFuture<'_, ()> {
            async {}.boxed()
        }

        fn known(&self) -> BoxFuture<'_, Result<Vec<Sighting>, RadioError>> {
            async { Ok(Vec::new()) }.boxed()
        }

        fn connect<'a>(&'a self, _peer: &'a str) -> BoxFuture<'a, Result<Connected, RadioError>> {
            async move {
                let wire = self.wire.lock().unwrap_or_else(PoisonError::into_inner).take();
                let (down, up) =
                    wire.ok_or_else(|| RadioError::new(RadioErrorKind::Other, "connected once"))?;
                let connection = WireConnection { down, up: Some(up), mtu: self.mtu };
                Ok(Connected { connection: Box::new(connection), made: true })
            }
            .boxed()
        }

        fn abandon<'a>(&'a self, _peer: &'a str) -> BoxFuture<'a, ()> {
            async {}.boxed()
        }
    }

    /// The one link [`WireRadio::connect`] makes.
    struct WireConnection {
        down: WireDown,
        up: Option<WireUp>,
        mtu: u16,
    }

    impl Connection for WireConnection {
        fn discover(&mut self) -> BoxFuture<'_, Result<(), RadioError>> {
            async { Ok(()) }.boxed()
        }

        fn subscribe(&mut self) -> BoxFuture<'_, Result<BoxStream<'static, Incoming>, RadioError>> {
            let up = self.up.take();
            async move {
                let up = up.ok_or_else(|| RadioError::new(RadioErrorKind::Other, "subscribed"))?;
                let values = stream::unfold(up, |mut up| async move {
                    let value = up.notified().await?;
                    Some((Incoming::Notified(value), up))
                });
                Ok(values.boxed())
            }
            .boxed()
        }

        fn mtu(&self) -> u16 {
            self.mtu
        }

        fn write<'a>(
            &'a mut self,
            bytes: &'a [u8],
            _kind: WriteKind,
        ) -> BoxFuture<'a, Result<(), RadioError>> {
            self.down.write(bytes);
            async { Ok(()) }.boxed()
        }

        fn is_connected(&self) -> BoxFuture<'_, bool> {
            async { true }.boxed()
        }

        fn close(self: Box<Self>, _unsubscribe: bool) -> BoxFuture<'static, ()> {
            async {}.boxed()
        }
    }

    /// The next event on `link`, in time.
    async fn next(link: &mut LinkHandle) -> LinkEvent {
        let event = tokio::time::timeout(WITHIN, link.recv()).await.expect("an event in time");
        event.expect("the link is up")
    }

    #[tokio::test(start_paused = true)]
    async fn ble_link_announces_and_carries_the_fleet_when_radio_reaches_a_wire_sim() {
        // COBS framing, the gate and `Identify`, end to end through the Bluetooth driver.
        let sim = WireSim::start(fleet(), ATT_MTU).expect("the simulator starts");
        let mac = sim.mac();
        let radio = WireRadio::new(sim, ATT_MTU);
        let mut link = BleTransport::new(Arc::new(radio)).start().expect("the driver starts");

        let info = loop {
            match next(&mut link).await {
                LinkEvent::Connected(info) => break info,
                LinkEvent::Disconnected { reason, .. } => panic!("link down: {reason}"),
                _ => {}
            }
        };
        assert_eq!(info.mac, mac);
        while !matches!(next(&mut link).await, LinkEvent::Message(msg) if is_heartbeat(&msg)) {}

        link.send_bulk(HostToBridge::GetStatus).expect("queued");
        while !matches!(next(&mut link).await, LinkEvent::Message(BridgeToHost::Status { .. })) {}
        link.close().await;
    }
}
