//! A transport written outside the crate, through the public link plumbing alone.

use wartui_bridge::{DisconnectCause, LinkEvent, link_pair};
use wartui_proto::link::HostToBridge;

#[tokio::test]
async fn link_pair_delivers_event_when_transport_sends_one() {
    let (mut handle, plumbing) = link_pair();
    let event =
        LinkEvent::Disconnected { reason: "cable out".to_owned(), cause: DisconnectCause::Other };
    plumbing.events.send(event.clone()).await.expect("the handle is listening");
    assert_eq!(handle.recv().await, Some(event));
}

#[tokio::test]
async fn link_handle_backlog_falls_when_transport_reports_command_done() {
    let (handle, mut plumbing) = link_pair();
    handle.send_bulk(HostToBridge::GetStatus).expect("queued");
    handle.send_bulk(HostToBridge::GetStatus).expect("queued");
    assert_eq!(handle.backlog(), 2);

    assert_eq!(plumbing.commands.recv().await, Some(HostToBridge::GetStatus));
    plumbing.commands.done(1);
    assert_eq!(handle.backlog(), 1);
    assert_eq!(handle.written_count(), 1);
}

#[tokio::test]
async fn link_handle_backlog_empties_when_transport_discards_queue() {
    let (handle, mut plumbing) = link_pair();
    for _ in 0..3 {
        handle.send_bulk(HostToBridge::GetStatus).expect("queued");
    }
    assert_eq!(plumbing.commands.discard(), 3);
    assert_eq!(handle.backlog(), 0);
}

#[tokio::test]
async fn link_handle_recv_returns_none_when_plumbing_is_dropped() {
    let (mut handle, plumbing) = link_pair();
    drop(plumbing);
    assert_eq!(handle.recv().await, None);
}
