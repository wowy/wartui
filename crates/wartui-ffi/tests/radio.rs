//! `Radio`, the phone's Bluetooth stack as the link driver's radio, driven as the driver drives
//! it, against a driver whose every request the test answers by hand.

use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt as _;
use tokio::sync::mpsc;
use tokio::time::timeout;
use wartui_bridge::pipe::{self, Incoming, RadioError, RadioErrorKind, Sighting, WriteKind};
use wartui_ffi::{Completion, DriverError, Heard, Radio, RadioDriver};

/// The longest any wait takes before the test fails rather than hangs.
const WAIT: Duration = Duration::from_secs(5);

/// The bridge's octets, as its id carries them.
const OCTETS: [u8; 2] = [0x44, 0xC0];

/// A request the driver was asked, with its id.
#[derive(Debug, PartialEq, Eq)]
enum Asked {
    Recall(u64),
    Known(u64),
    StartScan(u64),
    Connect(u64),
    Subscribe(u64),
    Write(u64),
}

/// A driver that hands each request to the test, and throws from `known`.
struct Scripted {
    asked: mpsc::UnboundedSender<Asked>,
}

impl Scripted {
    fn ask(&self, asked: Asked) -> Result<(), DriverError> {
        let _ = self.asked.send(asked);
        Ok(())
    }
}

impl RadioDriver for Scripted {
    fn pairs_on_connect(&self) -> Result<bool, DriverError> {
        Ok(true)
    }
    fn recall(&self, request: u64, _key: String) -> Result<(), DriverError> {
        self.ask(Asked::Recall(request))
    }
    fn known(&self, request: u64) -> Result<(), DriverError> {
        self.ask(Asked::Known(request))
    }
    fn start_scan(&self, request: u64) -> Result<(), DriverError> {
        self.ask(Asked::StartScan(request))
    }
    fn stop_scan(&self) -> Result<(), DriverError> {
        Ok(())
    }
    fn connect(&self, request: u64, _peer: String) -> Result<(), DriverError> {
        self.ask(Asked::Connect(request))
    }
    fn abandon(&self, _peer: String) -> Result<(), DriverError> {
        Ok(())
    }
    fn discover(&self, _request: u64, _peer: String) -> Result<(), DriverError> {
        Err(DriverError::Failed { detail: "no GATT client".to_owned() })
    }
    fn subscribe(&self, request: u64, _peer: String) -> Result<(), DriverError> {
        self.ask(Asked::Subscribe(request))
    }
    fn mtu(&self, _peer: String) -> Result<u16, DriverError> {
        Ok(185)
    }
    fn write(&self, request: u64, _: String, _: Vec<u8>, _: bool) -> Result<(), DriverError> {
        self.ask(Asked::Write(request))
    }
    fn is_connected(&self, _peer: String) -> Result<bool, DriverError> {
        Ok(true)
    }
    fn close(&self, _peer: String, _unsubscribe: bool) -> Result<(), DriverError> {
        Ok(())
    }
}

/// A radio over a [`Scripted`] driver, and what the driver is asked.
fn scripted() -> (Arc<Radio>, mpsc::UnboundedReceiver<Asked>) {
    let (asked, requests) = mpsc::unbounded_channel();
    (Radio::new(Arc::new(Scripted { asked })), requests)
}

/// The next request the driver was asked.
async fn next(requests: &mut mpsc::UnboundedReceiver<Asked>) -> Asked {
    timeout(WAIT, requests.recv()).await.expect("a request in time").expect("the driver lives")
}

/// A bridge heard advertising its id.
fn advertising(peer: &str) -> Heard {
    Heard { peer: peer.to_owned(), id: Some(OCTETS.to_vec()), name: None, live: true }
}

/// A scan under way: its stream, once the driver has answered.
async fn scanning(
    radio: &Arc<Radio>,
    requests: &mut mpsc::UnboundedReceiver<Asked>,
) -> futures_util::stream::BoxStream<'static, Sighting> {
    let scanner = Arc::clone(radio);
    let scan = tokio::spawn(async move { pipe::Radio::scan(&*scanner).await });
    let Asked::StartScan(id) = next(requests).await else { panic!("not a scan") };
    radio.completed(id, Completion::Done);
    scan.await.expect("no panic").expect("scanning")
}

/// A subscription to `peer`'s Up: its stream, once the driver has answered.
async fn subscribed(
    radio: &Arc<Radio>,
    requests: &mut mpsc::UnboundedReceiver<Asked>,
) -> futures_util::stream::BoxStream<'static, Incoming> {
    let connector = Arc::clone(radio);
    let link = tokio::spawn(async move {
        let mut connected = pipe::Radio::connect(&*connector, "p").await?;
        connected.connection.subscribe().await
    });
    let Asked::Connect(id) = next(requests).await else { panic!("not a connect") };
    radio.completed(id, Completion::Connected { made: true });
    let Asked::Subscribe(id) = next(requests).await else { panic!("not a subscription") };
    radio.completed(id, Completion::Done);
    link.await.expect("no panic").expect("subscribed")
}

/// What the peer's link dropping looks like.
fn gone() -> RadioError {
    RadioError::new(RadioErrorKind::Disconnected, "the peer went away")
}

/// A link to `p`, once the driver has answered the connect.
async fn connected(
    radio: &Arc<Radio>,
    requests: &mut mpsc::UnboundedReceiver<Asked>,
) -> Box<dyn pipe::Connection> {
    let connector = Arc::clone(radio);
    let connecting = tokio::spawn(async move { pipe::Radio::connect(&*connector, "p").await });
    let Asked::Connect(id) = next(requests).await else { panic!("not a connect") };
    radio.completed(id, Completion::Connected { made: true });
    connecting.await.expect("no panic").expect("connected").connection
}

#[tokio::test]
async fn radio_fails_request_when_driver_method_errs() {
    let (radio, mut requests) = scripted();
    let mut connection = connected(&radio, &mut requests).await;
    // The scripted driver throws from `discover`.
    let error = timeout(WAIT, connection.discover()).await.expect("at once").unwrap_err();
    assert_eq!(error.kind, RadioErrorKind::Other);
    assert!(error.text.contains("no GATT client"), "{error}");
}

#[tokio::test]
async fn radio_ignores_completion_when_request_was_given_up() {
    let (radio, mut requests) = scripted();
    let asker = Arc::clone(&radio);
    let given_up = tokio::spawn(async move {
        timeout(Duration::from_millis(50), pipe::Radio::known(&*asker)).await
    });
    let Asked::Known(old) = next(&mut requests).await else { panic!("not a listing") };
    assert!(given_up.await.expect("no panic").is_err(), "the driver gave up");
    radio.completed(old, Completion::Known { bridges: vec![advertising("late")] });

    // The next request waits on its own answer, not the late one.
    let asker = Arc::clone(&radio);
    let recalling = tokio::spawn(async move { pipe::Radio::recall(&*asker, "p").await });
    let Asked::Recall(id) = next(&mut requests).await else { panic!("not a recall") };
    assert_ne!(id, old);
    radio.completed(old, Completion::Recalled { known: true });
    radio.completed(id, Completion::Recalled { known: false });
    let recalled = timeout(WAIT, recalling).await.expect("in time").expect("no panic");
    assert_eq!(recalled, Ok(None));
}

#[tokio::test]
async fn radio_fails_request_when_completion_has_the_wrong_shape() {
    let (radio, mut requests) = scripted();
    let mut connection = connected(&radio, &mut requests).await;
    let bytes = [0u8; 4];
    let writing = connection.write(&bytes, WriteKind::Request);
    let answering = async {
        let Asked::Write(id) = next(&mut requests).await else { panic!("not a write") };
        radio.completed(id, Completion::Connected { made: true });
    };
    let (written, ()) = tokio::join!(writing, answering);
    let error = written.unwrap_err();
    assert_eq!(error.kind, RadioErrorKind::Other);
    assert!(error.text.contains("write") && error.text.contains("Connected"), "{error}");
}

#[tokio::test]
async fn radio_drops_heard_device_when_it_carries_no_id_and_was_never_identified() {
    let (radio, mut requests) = scripted();
    let mut sightings = scanning(&radio, &mut requests).await;
    radio.heard(Heard { peer: "stranger".to_owned(), id: None, name: None, live: true });
    radio.heard(advertising("bridge"));
    let first = timeout(WAIT, sightings.next()).await.expect("in time").expect("scanning");
    assert_eq!(first.peer, "bridge");
    assert_eq!(first.octets, OCTETS);
}

#[tokio::test]
async fn radio_keeps_octets_when_identified_device_is_listed_without_an_id() {
    let (radio, mut requests) = scripted();
    let mut sightings = scanning(&radio, &mut requests).await;
    radio.heard(advertising("bridge"));
    timeout(WAIT, sightings.next()).await.expect("in time").expect("scanning");

    let asker = Arc::clone(&radio);
    let listing = tokio::spawn(async move { pipe::Radio::known(&*asker).await });
    let Asked::Known(id) = next(&mut requests).await else { panic!("not a listing") };
    let listed = Heard { peer: "bridge".to_owned(), id: None, name: None, live: false };
    let unknown = Heard { peer: "stranger".to_owned(), id: None, name: None, live: false };
    radio.completed(id, Completion::Known { bridges: vec![listed, unknown] });
    let known = listing.await.expect("no panic").expect("listed");
    let expected = Sighting { peer: "bridge".to_owned(), octets: OCTETS, name: None, live: false };
    assert_eq!(known, vec![expected]);
}

#[tokio::test]
async fn radio_yields_notification_before_drop_when_both_are_queued() {
    let (radio, mut requests) = scripted();
    let mut incoming = subscribed(&radio, &mut requests).await;
    radio.notified("p".to_owned(), vec![1, 2, 3]);
    radio.dropped("p".to_owned(), gone());
    let read = timeout(WAIT, incoming.by_ref().collect::<Vec<_>>()).await.expect("ended in time");
    assert_eq!(read, vec![Incoming::Notified(vec![1, 2, 3]), Incoming::Dropped(gone())]);
}

#[tokio::test]
async fn radio_yields_drop_once_when_dropped_twice() {
    let (radio, mut requests) = scripted();
    let incoming = subscribed(&radio, &mut requests).await;
    radio.dropped("p".to_owned(), gone());
    radio.dropped("p".to_owned(), gone());
    let read = timeout(WAIT, incoming.collect::<Vec<_>>()).await.expect("ended in time");
    assert_eq!(read, vec![Incoming::Dropped(gone())]);
}
