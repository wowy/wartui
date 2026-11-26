//! `Link`, the link to a bridge held between captures.

mod common;

use wartui_ffi::{Link, LinkError, Settings};

use common::Loopback;

#[tokio::test]
async fn link_refuses_a_path_when_asked_for_a_bluetooth_bridge() {
    let radio = Loopback::radio();
    let opened = Link::bluetooth(radio, Settings::unsaved(), Some("/dev/ttyACM0".to_owned()));
    let Err(LinkError::NotABridgeName { given }) = opened else {
        panic!("a path is no bridge's name: {opened:?}");
    };
    assert_eq!(given, "/dev/ttyACM0");
}
