//! A link held between captures, read so nothing piles up in it.
//!
//! **An idle link is still read.** The bridge keeps sending to an attached host, so a link nobody
//! reads fills its event queue, then blocks its transport's pump, and the next capture is fed the
//! backlog as if it were live. The engine reads anything older than its connection as a host that
//! fell behind (`wartui_core::engine::lag`): a connection starts caught up only because nothing
//! older reaches it (`wartui_bridge`'s `gate`). So a parked link's task reads every event and
//! drops it, and [`Parked::resume`] keeps nothing it read: the next reader starts caught up to
//! within what arrives between the stop and its first read.
//!
//! **The announcement stays current.** The transport announces a bridge once per life, and the
//! next capture is handed the last one. The handle keeps it ([`LinkHandle::announced`]), so a link
//! parks with the announcement its last reader left it, and no caller can hand it another. A
//! `Connected` while parked is the announcement, and an `Incompatible` or a `Disconnected`,
//! whatever its cause, clears it: the transport announces again once the bridge is back. A transport that ended is read no
//! more, and comes back with no announcement. The task mirrors the handle's into a watch, so the
//! announcement is readable while parked ([`Parked::announced`]) and a save between captures
//! remembers the bridge connected now.

use tokio::sync::{oneshot, watch};
use tokio::task::JoinHandle;
use wartui_bridge::{BridgeInfo, LinkEvent, LinkHandle};

/// A link nobody captures over, kept read so nothing piles up in it.
#[derive(Debug)]
pub struct Parked {
    /// Dropped, it stops the task: by [`Parked::resume`], or with the `Parked`.
    stop: oneshot::Sender<()>,
    /// Owns the link, and hands it back once stopped.
    task: JoinHandle<LinkHandle>,
    /// The handle's announcement, as the task last read it.
    announced: watch::Receiver<Option<BridgeInfo>>,
}

/// The parking task panicked, and the link went with it.
#[derive(Debug, thiserror::Error)]
#[error("the task holding the idle link panicked")]
pub struct ParkEnded;

/// Park `link`, with the announcement it holds. Call it inside a tokio runtime.
///
/// Dropping the `Parked` stops the task, which drops the link; closing it properly is
/// [`Parked::resume`], then [`LinkHandle::close`].
#[must_use]
pub fn park(link: LinkHandle) -> Parked {
    let (stop, stopped) = oneshot::channel();
    let (publish, announced) = watch::channel(link.announced().cloned());
    Parked { stop, task: tokio::spawn(keep_read(link, publish, stopped)), announced }
}

impl Parked {
    /// The bridge the link has announced now, read without taking the link back.
    #[must_use]
    pub fn announced(&self) -> Option<BridgeInfo> {
        self.announced.borrow().clone()
    }

    /// Take the link back, with the bridge it has announced now: the handle's announcement.
    ///
    /// # Errors
    /// [`ParkEnded`] when the parking task panicked; the link is gone.
    pub async fn resume(self) -> Result<(LinkHandle, Option<BridgeInfo>), ParkEnded> {
        drop(self.stop);
        let link = self.task.await.map_err(|_| ParkEnded)?;
        let announced = link.announced().cloned();
        Ok((link, announced))
    }
}

/// Read `link` until `stopped` resolves, mirroring its announcement into `announced`, and hand
/// it back.
async fn keep_read(
    mut link: LinkHandle,
    announced: watch::Sender<Option<BridgeInfo>>,
    mut stopped: oneshot::Receiver<()>,
) -> LinkHandle {
    loop {
        tokio::select! {
            biased;
            // Sent or dropped, either is the stop.
            _ = &mut stopped => return link,
            event = link.recv() => match event {
                Some(
                    LinkEvent::Connected(_)
                    | LinkEvent::Incompatible { .. }
                    | LinkEvent::Disconnected { .. },
                ) => {
                    announced.send_replace(link.announced().cloned());
                }
                Some(LinkEvent::Again(_) | LinkEvent::Message(_) | LinkEvent::Garbled(_)) => {}
                None => break,
            },
        }
    }
    // The transport ended, which cleared the handle's announcement: nothing more comes, so wait
    // for the stop alone.
    announced.send_replace(None);
    let _ = stopped.await;
    link
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use wartui_bridge::{
        BridgeInfo, DisconnectCause, LinkEvent, LinkHandle, LinkPlumbing, link_pair,
    };
    use wartui_proto::bus::NodeBoard;
    use wartui_proto::link::{BridgeToHost, LoopPhase};
    use wartui_proto::reset::ResetCause;

    use super::park;

    /// A bridge announcing itself, `uptime_ms` into its life.
    fn bridge(uptime_ms: u32) -> BridgeInfo {
        BridgeInfo {
            mac: [0x02, 0x00, 0x5E, 0x10, 0x44, 0xC0],
            fw_version: "0.1.0".to_owned(),
            reset_cause: ResetCause::PowerOn,
            last_phase: LoopPhase::Unknown,
            uptime_ms,
            node_board: NodeBoard::Carrier14,
            radio: None,
            image: None,
            version: wartui_proto::link::BRIDGE_VERSION,
            trial: wartui_proto::link::Trial::Settled,
        }
    }

    /// `link`, once it has read `bridge`'s announcement from `plumbing`.
    async fn announcing(
        mut link: LinkHandle,
        plumbing: &LinkPlumbing,
        bridge: BridgeInfo,
    ) -> LinkHandle {
        plumbing.events.send(LinkEvent::Connected(bridge)).await.expect("the link reads");
        link.recv().await.expect("the announcement");
        link
    }

    /// Let the parking task read what is queued.
    async fn settle() {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    /// The next event `link` gives, or `None` when it has none within a moment.
    async fn next(link: &mut LinkHandle) -> Option<Option<LinkEvent>> {
        tokio::time::timeout(Duration::from_millis(100), link.recv()).await.ok()
    }

    fn fault(text: &str) -> LinkEvent {
        LinkEvent::Message(BridgeToHost::Error { message: text.try_into().expect("short") })
    }

    #[tokio::test(start_paused = true)]
    async fn park_drops_messages_when_link_is_idle() {
        let (link, plumbing) = link_pair();
        let parked = park(announcing(link, &plumbing, bridge(1_000)).await);
        plumbing.events.send(fault("while parked")).await.expect("the link reads");
        settle().await;
        let (mut link, announced) = parked.resume().await.expect("resumed");
        assert_eq!(announced, Some(bridge(1_000)));
        plumbing.events.send(fault("after")).await.expect("the link reads");
        assert_eq!(next(&mut link).await, Some(Some(fault("after"))));
    }

    #[tokio::test(start_paused = true)]
    async fn park_keeps_the_latest_announcement_when_bridge_restarts() {
        let (link, plumbing) = link_pair();
        let parked = park(announcing(link, &plumbing, bridge(60_000)).await);
        let gone = LinkEvent::Disconnected {
            reason: "the bridge disconnected".to_owned(),
            cause: DisconnectCause::Other,
        };
        plumbing.events.send(gone).await.expect("the link reads");
        plumbing.events.send(LinkEvent::Connected(bridge(800))).await.expect("the link reads");
        settle().await;
        let (mut link, announced) = parked.resume().await.expect("resumed");
        assert_eq!(announced, Some(bridge(800)));
        assert_eq!(next(&mut link).await, None, "nothing read is kept");
    }

    #[tokio::test(start_paused = true)]
    async fn park_clears_the_announcement_when_link_goes_down() {
        let (link, plumbing) = link_pair();
        let parked = park(announcing(link, &plumbing, bridge(60_000)).await);
        // A cause the transport retries under a held subscription clears it as well.
        let taken = LinkEvent::Disconnected {
            reason: "a USB host took the bridge".to_owned(),
            cause: DisconnectCause::UsbTookBridge,
        };
        plumbing.events.send(taken).await.expect("the link reads");
        settle().await;
        let (_link, announced) = parked.resume().await.expect("resumed");
        assert_eq!(announced, None);
    }

    #[tokio::test(start_paused = true)]
    async fn park_shows_the_announcement_while_parked_when_bridge_connects() {
        let (link, plumbing) = link_pair();
        let parked = park(link);
        assert_eq!(parked.announced(), None);
        plumbing.events.send(LinkEvent::Connected(bridge(800))).await.expect("the link reads");
        settle().await;
        assert_eq!(parked.announced(), Some(bridge(800)));
        let (_link, announced) = parked.resume().await.expect("resumed");
        assert_eq!(announced, Some(bridge(800)));
    }

    #[tokio::test(start_paused = true)]
    async fn park_shows_no_announcement_while_parked_when_link_goes_down() {
        let (link, plumbing) = link_pair();
        let parked = park(announcing(link, &plumbing, bridge(60_000)).await);
        assert_eq!(parked.announced(), Some(bridge(60_000)));
        let gone = LinkEvent::Disconnected {
            reason: "the bridge disconnected".to_owned(),
            cause: DisconnectCause::Other,
        };
        plumbing.events.send(gone).await.expect("the link reads");
        settle().await;
        assert_eq!(parked.announced(), None);
    }

    #[tokio::test(start_paused = true)]
    async fn park_returns_the_link_when_transport_ended() {
        let (link, plumbing) = link_pair();
        let parked = park(announcing(link, &plumbing, bridge(60_000)).await);
        drop(plumbing);
        settle().await;
        let (mut link, announced) = parked.resume().await.expect("resumed");
        assert_eq!(announced, None);
        assert_eq!(link.recv().await, None, "the transport has ended");
    }
}
