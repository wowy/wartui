//! Fixtures shared by the upload tests of the CLI and the fleet view: captures to send, and
//! a local HTTP server to send them to, so no test reaches the real site.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::time::Duration;

use wartui_core::position::{Fix, PositionSource};
use wartui_core::record::{Observation, Record};
use wartui_core::store::{CaptureInfo, Store, StoreConfig};
use wartui_proto::air::RecordKind;
use wartui_proto::plan::ChannelPool;

use crate::upload::{Prepare, Prepared};

pub(crate) const EPOCH_MS: i64 = 1_777_642_477_000;

/// One Wi-Fi sighting of `bssid`, positioned when `lat` is given.
pub(crate) fn sighting(bssid: [u8; 6], at_ms: i64, lat: Option<f64>) -> Record {
    Record::Observation(Observation {
        node_mac: [0x02, 0x00, 0x5E, 0x10, 0x57, 0x84],
        rx_at_ms: at_ms,
        link_rssi: Some(-41),
        bssid,
        ssid: b"example".to_vec(),
        security: "[WPA2_PSK]".to_owned(),
        channel: 6,
        rssi: -60,
        kind: RecordKind::Wifi,
        rcoi: None,
        mfgr_id: None,
        fix: Fix {
            lat,
            lon: lat.map(|_| -122.0),
            alt: None,
            accuracy: None,
            source: PositionSource::Static,
            at_ms: None,
        },
        raw_body: Vec::new(),
    })
}

/// A capture at `dir/wartui.db` holding `records`.
pub(crate) fn capture(dir: &tempfile::TempDir, records: Vec<Record>) -> PathBuf {
    let path = dir.path().join("wartui.db");
    let mut config = StoreConfig::new(&path);
    config.batch_interval = Duration::from_millis(10);
    let info = CaptureInfo { pool: ChannelPool::Us, notes: None };
    let store = Store::create(&config, &info, EPOCH_MS).expect("creating the store");
    assert_eq!(store.submit(records), 0, "nothing should have been dropped");
    store.close();
    path
}

impl Prepare {
    /// The upload, for a test that built a capture with something new in it.
    pub(crate) fn ready(self) -> Prepared {
        match self {
            Prepare::Ready(prepared) => prepared,
            _ => panic!("nothing to send"),
        }
    }
}

/// A server on a free local port that answers one request with `status` and `body`,
/// and hands back the request it read.
pub(crate) fn serve_once(status: &str, body: &str) -> (String, std::thread::JoinHandle<Vec<u8>>) {
    serve_once_with(status, "", body.as_bytes())
}

/// [`serve_once`], with `headers` (each ending `\r\n`) added to the response.
pub(crate) fn serve_once_with(
    status: &str,
    headers: &str,
    body: &[u8],
) -> (String, std::thread::JoinHandle<Vec<u8>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let response = response(status, headers, body);
    let handle = std::thread::spawn(move || {
        let request = answer(&listener, &response);
        refuse_more(&listener);
        request
    });
    (base, handle)
}

/// A server that answers one request per `(status, body)` in turn, then refuses any more,
/// and hands back the requests it read.
pub(crate) fn serve_sequence(
    answers: &[(&str, &str)],
) -> (String, std::thread::JoinHandle<Vec<Vec<u8>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let responses: Vec<Vec<u8>> =
        answers.iter().map(|(status, body)| response(status, "", body.as_bytes())).collect();
    let handle = std::thread::spawn(move || {
        let requests = responses.iter().map(|response| answer(&listener, response)).collect();
        refuse_more(&listener);
        requests
    });
    (base, handle)
}

fn response(status: &str, headers: &str, body: &[u8]) -> Vec<u8> {
    let mut response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\n{headers}\
         Content-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )
    .into_bytes();
    response.extend_from_slice(body);
    response
}

/// Accept one connection, read its request whole, and write `response`.
fn answer(listener: &TcpListener, response: &[u8]) -> Vec<u8> {
    let (mut stream, _) = listener.accept().unwrap();
    let mut request = Vec::new();
    let mut buf = [0u8; 8192];
    let header_end = loop {
        let n = stream.read(&mut buf).unwrap();
        assert!(n > 0, "the client hung up mid-request");
        request.extend_from_slice(&buf[..n]);
        if let Some(at) = request.windows(4).position(|w| w == b"\r\n\r\n") {
            break at + 4;
        }
    };
    let headers = String::from_utf8_lossy(&request[..header_end]).to_ascii_lowercase();
    let length = headers
        .lines()
        .find_map(|line| line.strip_prefix("content-length:"))
        .map_or(0, |n| n.trim().parse::<usize>().unwrap());
    while request.len() < header_end + length {
        let n = stream.read(&mut buf).unwrap();
        assert!(n > 0, "the client hung up mid-body");
        request.extend_from_slice(&buf[..n]);
    }
    stream.write_all(response).unwrap();
    request
}

/// Panic if another request arrives: a followed redirect would come back for one.
fn refuse_more(listener: &TcpListener) {
    listener.set_nonblocking(true).unwrap();
    std::thread::sleep(Duration::from_millis(200));
    if let Ok((mut stream, _)) = listener.accept() {
        let mut buf = [0u8; 8192];
        stream.set_nonblocking(false).unwrap();
        stream.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
        let n = stream.read(&mut buf).unwrap_or(0);
        panic!("a second request arrived: {}", String::from_utf8_lossy(&buf[..n]));
    }
}
