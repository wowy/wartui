//! The wait for the bridge's announcement, the `Ready` a connection starts at.
//!
//! Over USB the first disconnect ends it. Over Bluetooth the transport reports every failed try
//! as a disconnect and tries again a second later, so a [`DisconnectCause::Other`] is kept and
//! the wait goes on, as is each cause the transport names and retries alike ([`retried`]). The
//! bridge's refusal of this host and a USB takeover still end it at once. The first
//! [`DisconnectCause::AwaitingPairing`] is the operator's notice, and the wait stretches to the
//! transport's pairing window; a later one returns no notice. A
//! [`DisconnectCause::AwaitingReady`] says the subscription is through and the wait goes on, for
//! the bridge's `Ready`; the first is the operator's notice that the click landed, and a later one
//! returns no notice. A wait that runs out names a failure that is retried, else the bridge's
//! silence once the subscription went through, else the pairing wait once it was announced.
//!
//! **An incompatible bridge ends the wait at once** ([`NotReady::Incompatible`]): it answered, with
//! a `Ready` whose rest this host cannot read, and it announces nothing. It shares the frozen core
//! alone with this host, so every command behind this wait refuses it the same way; only an update,
//! which needs no announcement, goes on, from the core the handle keeps (`LinkHandle::reached`). A
//! link whose last `Ready` read was such a one answers so at once, though the link dropped since
//! (`LinkHandle::heard_incompatible`).

use std::time::Duration;

use wartui_bridge::{BridgeCore, BridgeInfo, DisconnectCause, LinkEvent, LinkHandle};

use crate::settings::Link;

/// A notice the wait hands its caller as it comes, carrying the transport's own text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadyNotice {
    /// The transport waits on the OS's pairing dialog.
    Pairing(String),
    /// The subscription that waited on the pairing is through.
    Through(String),
}

/// Why the wait ended without the bridge's announcement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NotReady {
    /// The link went down, with the transport's reason.
    Down(String),
    /// The transport ended.
    Closed,
    /// The wait ran out while the transport retried this failure.
    Retried(String),
    /// The wait ran out after the subscription that waited on the pairing went through: the bridge
    /// did not answer.
    SilentAfterPairing,
    /// The wait ran out after the OS's pairing dialog was announced.
    PairingRanOut,
    /// The bridge answered with a `Ready` whose rest this host cannot read: another major, or
    /// another build of this one. `core` says which, and its version.
    Incompatible(BridgeCore),
}

/// What [`ready`] came to.
#[derive(Debug)]
pub struct ReadyWaited {
    /// The announcement, `None` when the wait ran out with no failure to name, or why the link
    /// went down first.
    pub ready: Result<Option<BridgeInfo>, NotReady>,
    /// A frame arrived that did not decode: a bridge running firmware from another tree.
    pub garbled: bool,
}

/// The bridge's announcement over `over` within `within`, handing each [`ReadyNotice`] to `notice`
/// as it comes. `usual` is the wait the caller gives a `Ready` once a link is up, which the
/// deadline keeps behind the transport's pairing window. `notice` is `Send`, so a phone's binding
/// runs the wait as a task of its own runtime.
pub async fn ready(
    link: &mut LinkHandle,
    over: Link,
    within: Duration,
    usual: Duration,
    notice: &mut (dyn FnMut(ReadyNotice) + Send),
) -> ReadyWaited {
    // Its `Ready` has gone by, and the transport sends one only as asked.
    if let Some(&core) = link.heard_incompatible() {
        return ReadyWaited { ready: Err(NotReady::Incompatible(core)), garbled: false };
    }
    let mut until = tokio::time::Instant::now() + within;
    let mut wait = ReadyWait::new(over);
    let mut garbled = false;
    loop {
        let Ok(event) = tokio::time::timeout_at(until, link.recv()).await else {
            let ready = wait.timed_out().map_or(Ok(None), Err);
            return ReadyWaited { ready, garbled };
        };
        garbled |= matches!(event, Some(LinkEvent::Garbled(_)));
        match wait.take(event) {
            Taken::Waiting => {}
            Taken::Notice(said @ ReadyNotice::Pairing(_)) => {
                notice(said);
                until = pairing_deadline(until, tokio::time::Instant::now(), usual);
            }
            // The pairing already stretched the deadline.
            Taken::Notice(said) => notice(said),
            Taken::Ended(ready) => return ReadyWaited { ready: ready.map(Some), garbled },
        }
    }
}

/// The wait's deadline once the transport, at `now`, waits on the OS's pairing dialog: `until`,
/// or when that is sooner, the transport's own deadline for the subscription and the `usual` wait
/// for a `Ready` behind it. The notice comes a few seconds into the subscription, so its
/// deadline alone leaves the bridge little time to announce itself.
fn pairing_deadline(
    until: tokio::time::Instant,
    now: tokio::time::Instant,
    usual: Duration,
) -> tokio::time::Instant {
    until.max(now + wartui_bridge::ble::PAIR_WITHIN + usual)
}

/// Whether the Bluetooth transport retries a try that ended on `cause` as it retries
/// [`DisconnectCause::Other`], so the wait keeps it and goes on.
const fn retried(cause: DisconnectCause) -> bool {
    match cause {
        DisconnectCause::Other
        | DisconnectCause::RadioOff
        | DisconnectCause::NotAuthorized
        | DisconnectCause::StaleBond
        | DisconnectCause::NotAdvertising
        | DisconnectCause::SeveralAdvertising => true,
        DisconnectCause::UsbTookBridge
        | DisconnectCause::AwaitingPairing
        | DisconnectCause::AwaitingReady
        | DisconnectCause::NotBonded
        | DisconnectCause::MaybeNotBonded => false,
    }
}

/// A wait for the bridge's announcement, fed one link event at a time (the module's `//!`).
#[derive(Debug)]
struct ReadyWait {
    over: Link,
    /// The last failure the transport is retrying.
    retried: Option<String>,
    /// Whether the transport's wait on the OS's pairing has been announced.
    pairing: bool,
    /// Whether the subscription went through since the pairing wait last began.
    through: bool,
    /// Whether the subscription going through has been announced. `through` resets when a
    /// pairing wait begins again, so it cannot say this.
    through_said: bool,
}

/// What one link event does to a [`ReadyWait`].
#[derive(Debug, PartialEq, Eq)]
enum Taken {
    /// The wait goes on.
    Waiting,
    /// A notice for the operator, and the wait goes on.
    Notice(ReadyNotice),
    /// The wait is over, with the announcement or the reason.
    Ended(Result<BridgeInfo, NotReady>),
}

impl ReadyWait {
    const fn new(over: Link) -> Self {
        Self { over, retried: None, pairing: false, through: false, through_said: false }
    }

    /// What `event` does to the wait.
    fn take(&mut self, event: Option<LinkEvent>) -> Taken {
        match event {
            Some(LinkEvent::Connected(info)) => Taken::Ended(Ok(info)),
            Some(LinkEvent::Incompatible { core, .. }) => {
                Taken::Ended(Err(NotReady::Incompatible(core)))
            }
            Some(LinkEvent::Disconnected { reason, cause: DisconnectCause::AwaitingPairing }) => {
                self.through = false;
                if std::mem::replace(&mut self.pairing, true) {
                    Taken::Waiting
                } else {
                    Taken::Notice(ReadyNotice::Pairing(reason))
                }
            }
            Some(LinkEvent::Disconnected { reason, cause })
                if retried(cause) && self.over == Link::Bluetooth =>
            {
                self.retried = Some(reason);
                Taken::Waiting
            }
            Some(LinkEvent::Disconnected { reason, cause: DisconnectCause::AwaitingReady }) => {
                // The failures before it came before a subscription that went through.
                self.through = true;
                self.retried = None;
                if std::mem::replace(&mut self.through_said, true) {
                    Taken::Waiting
                } else {
                    Taken::Notice(ReadyNotice::Through(reason))
                }
            }
            Some(LinkEvent::Disconnected { reason, .. }) => {
                Taken::Ended(Err(NotReady::Down(reason)))
            }
            Some(_) => Taken::Waiting,
            None => Taken::Ended(Err(NotReady::Closed)),
        }
    }

    /// Why the wait ran out: the last failure the transport was retrying, else the bridge's
    /// silence once the subscription went through, else the pairing wait once it was announced,
    /// if any.
    fn timed_out(self) -> Option<NotReady> {
        if let Some(retried) = self.retried {
            Some(NotReady::Retried(retried))
        } else if self.through {
            Some(NotReady::SilentAfterPairing)
        } else {
            self.pairing.then_some(NotReady::PairingRanOut)
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use wartui_bridge::{BridgeCore, DisconnectCause, LinkEvent, link_pair};
    use wartui_proto::bus::Version;
    use wartui_proto::link::{BRIDGE_VERSION, Standing, Trial};

    use super::{NotReady, ReadyNotice, ReadyWait, Taken, pairing_deadline};
    use crate::settings::Link;

    /// The wait a one-shot command gives a `Ready` once its link is up.
    const USUAL: Duration = Duration::from_secs(5);

    fn down(reason: &str, cause: DisconnectCause) -> Option<LinkEvent> {
        Some(LinkEvent::Disconnected { reason: reason.to_owned(), cause })
    }

    #[test]
    fn ready_wait_ends_on_first_disconnect_when_over_usb() {
        let mut wait = ReadyWait::new(Link::Usb);
        let ended = wait.take(down("could not open", DisconnectCause::Other));
        assert_eq!(ended, Taken::Ended(Err(NotReady::Down("could not open".to_owned()))));
    }

    #[test]
    fn ready_wait_keeps_waiting_and_names_last_failure_when_bluetooth_retries() {
        let mut wait = ReadyWait::new(Link::Bluetooth);
        assert_eq!(wait.take(down("timed out", DisconnectCause::Other)), Taken::Waiting);
        assert_eq!(wait.take(down("not found", DisconnectCause::Other)), Taken::Waiting);
        assert_eq!(wait.timed_out(), Some(NotReady::Retried("not found".to_owned())));
    }

    #[test]
    fn ready_wait_keeps_waiting_and_names_last_failure_when_bluetooth_retries_a_named_cause() {
        for cause in [
            DisconnectCause::RadioOff,
            DisconnectCause::NotAuthorized,
            DisconnectCause::StaleBond,
            DisconnectCause::NotAdvertising,
            DisconnectCause::SeveralAdvertising,
        ] {
            let mut wait = ReadyWait::new(Link::Bluetooth);
            assert_eq!(wait.take(down("timed out", DisconnectCause::Other)), Taken::Waiting);
            assert_eq!(wait.take(down("named", cause)), Taken::Waiting, "{cause:?}");
            assert_eq!(wait.timed_out(), Some(NotReady::Retried("named".to_owned())), "{cause:?}");
        }
    }

    #[test]
    fn ready_wait_ends_at_once_when_bridge_refuses_or_usb_takes_it_over_bluetooth() {
        for cause in [
            DisconnectCause::NotBonded,
            DisconnectCause::MaybeNotBonded,
            DisconnectCause::UsbTookBridge,
        ] {
            let mut wait = ReadyWait::new(Link::Bluetooth);
            let refused = Taken::Ended(Err(NotReady::Down("refused".to_owned())));
            assert_eq!(wait.take(down("refused", cause)), refused);
        }
    }

    #[test]
    fn ready_wait_ends_when_link_closes() {
        let mut wait = ReadyWait::new(Link::Usb);
        assert_eq!(wait.take(None), Taken::Ended(Err(NotReady::Closed)));
    }

    #[test]
    fn ready_wait_returns_pairing_notice_once_when_transport_awaits_pairing() {
        let mut wait = ReadyWait::new(Link::Bluetooth);
        let notice = "pairing: click Connect if macOS shows a dialog for 44:C0";
        let pairing = DisconnectCause::AwaitingPairing;
        let said = Taken::Notice(ReadyNotice::Pairing(notice.to_owned()));
        assert_eq!(wait.take(down(notice, pairing)), said);
        assert_eq!(wait.take(down(notice, pairing)), Taken::Waiting, "said once");
    }

    #[test]
    fn ready_wait_names_pairing_wait_when_it_runs_out_after_notice() {
        let mut wait = ReadyWait::new(Link::Bluetooth);
        let pairing = DisconnectCause::AwaitingPairing;
        assert!(matches!(wait.take(down("pairing", pairing)), Taken::Notice(_)));
        assert_eq!(wait.timed_out(), Some(NotReady::PairingRanOut));
    }

    #[test]
    fn ready_wait_names_retried_failure_when_it_runs_out_after_notice() {
        let mut wait = ReadyWait::new(Link::Bluetooth);
        assert_eq!(wait.take(down("timed out", DisconnectCause::Other)), Taken::Waiting);
        let pairing = DisconnectCause::AwaitingPairing;
        assert!(matches!(wait.take(down("pairing", pairing)), Taken::Notice(_)));
        assert_eq!(wait.timed_out(), Some(NotReady::Retried("timed out".to_owned())));
    }

    #[test]
    fn ready_wait_returns_ready_notice_once_when_subscription_is_through() {
        // Without its own arm, the first pairing of a one-shot command would fail at the click.
        let mut wait = ReadyWait::new(Link::Bluetooth);
        let pairing = DisconnectCause::AwaitingPairing;
        assert!(matches!(wait.take(down("pairing", pairing)), Taken::Notice(_)));
        let ready = "waiting for 44:C0 to answer";
        let through = DisconnectCause::AwaitingReady;
        let said = Taken::Notice(ReadyNotice::Through(ready.to_owned()));
        assert_eq!(wait.take(down(ready, through)), said);
        assert_eq!(wait.take(down(ready, through)), Taken::Waiting, "said once");
    }

    #[test]
    fn ready_wait_names_bridge_wait_when_it_runs_out_after_subscription_is_through() {
        // The failure before the subscription went through is not why the wait ran out.
        let mut wait = ReadyWait::new(Link::Bluetooth);
        assert_eq!(wait.take(down("timed out", DisconnectCause::Other)), Taken::Waiting);
        let pairing = DisconnectCause::AwaitingPairing;
        assert!(matches!(wait.take(down("pairing", pairing)), Taken::Notice(_)));
        let ready = "waiting for 44:C0 to answer";
        let through = wait.take(down(ready, DisconnectCause::AwaitingReady));
        assert!(matches!(through, Taken::Notice(ReadyNotice::Through(_))));
        assert_eq!(wait.timed_out(), Some(NotReady::SilentAfterPairing));
    }

    #[test]
    fn ready_wait_names_retried_failure_when_it_runs_out_after_subscription_is_through() {
        let mut wait = ReadyWait::new(Link::Bluetooth);
        let pairing = DisconnectCause::AwaitingPairing;
        assert!(matches!(wait.take(down("pairing", pairing)), Taken::Notice(_)));
        let ready = "waiting for 44:C0 to answer";
        let through = wait.take(down(ready, DisconnectCause::AwaitingReady));
        assert!(matches!(through, Taken::Notice(ReadyNotice::Through(_))));
        let failed = "the bridge disconnected";
        assert_eq!(wait.take(down(failed, DisconnectCause::Other)), Taken::Waiting);
        assert_eq!(wait.timed_out(), Some(NotReady::Retried(failed.to_owned())));
    }

    #[test]
    fn ready_wait_names_nothing_when_it_runs_out_with_no_failure() {
        let mut wait = ReadyWait::new(Link::Bluetooth);
        assert_eq!(
            wait.take(Some(LinkEvent::Garbled(wartui_proto::link::LinkError::Corrupt))),
            Taken::Waiting
        );
        assert_eq!(wait.timed_out(), None);
    }

    /// The core of a bridge one major past this host's.
    fn other_major() -> BridgeCore {
        let version = Version::new(BRIDGE_VERSION.major + 1, 0, 0);
        BridgeCore {
            mac: [0x02, 0x00, 0x5E, 0x10, 0x44, 0xC0],
            image: None,
            version,
            trial: Trial::Settled,
            uptime_ms: 400,
            standing: Standing::of(version, BRIDGE_VERSION, false),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn ready_wait_ends_incompatible_when_bridge_is_another_major() {
        let mut wait = ReadyWait::new(Link::Bluetooth);
        let event = Some(LinkEvent::Incompatible { core: other_major(), fresh: true });
        assert_eq!(wait.take(event), Taken::Ended(Err(NotReady::Incompatible(other_major()))));

        // At once, not at the timeout, and again for a link that already read it.
        let (mut link, plumbing) = link_pair();
        let event = LinkEvent::Incompatible { core: other_major(), fresh: true };
        plumbing.events.send(event).await.expect("the link reads");
        let start = tokio::time::Instant::now();
        let within = Duration::from_secs(30);
        let waited = super::ready(&mut link, Link::Usb, within, USUAL, &mut |_| {}).await;
        assert_eq!(waited.ready, Err(NotReady::Incompatible(other_major())));
        assert_eq!(start.elapsed(), Duration::ZERO);
        let again = super::ready(&mut link, Link::Usb, within, USUAL, &mut |_| {}).await;
        assert_eq!(again.ready, Err(NotReady::Incompatible(other_major())));
    }

    #[tokio::test(start_paused = true)]
    async fn ready_wait_ends_incompatible_when_link_dropped_after_incompatible_ready() {
        let (mut link, plumbing) = link_pair();
        let event = LinkEvent::Incompatible { core: other_major(), fresh: true };
        plumbing.events.send(event).await.expect("the link reads");
        let reason = "the bridge disconnected".to_owned();
        let down = LinkEvent::Disconnected { reason, cause: DisconnectCause::Other };
        plumbing.events.send(down).await.expect("the link reads");
        link.recv().await.expect("the unread `Ready`");
        link.recv().await.expect("the drop");
        let start = tokio::time::Instant::now();
        let within = Duration::from_secs(30);
        let waited = super::ready(&mut link, Link::Bluetooth, within, USUAL, &mut |_| {}).await;
        assert_eq!(waited.ready, Err(NotReady::Incompatible(other_major())));
        assert_eq!(start.elapsed(), Duration::ZERO, "the bridge is known already");
    }

    #[tokio::test(start_paused = true)]
    async fn ready_wait_stretches_to_pairing_window_when_notice_comes_before_deadline() {
        // A one-shot command's 17 s wait, and the transport's notice 3 s in.
        let start = tokio::time::Instant::now();
        let until = start + Duration::from_secs(17);
        let now = start + Duration::from_secs(3);
        let stretched = pairing_deadline(until, now, USUAL);
        // The subscription's deadline, and room for the `Ready` behind it.
        assert_eq!(stretched, now + wartui_bridge::ble::PAIR_WITHIN + USUAL);
        assert!(stretched > until);
    }

    #[tokio::test(start_paused = true)]
    async fn ready_wait_keeps_deadline_when_it_is_past_pairing_window() {
        let now = tokio::time::Instant::now();
        let window = wartui_bridge::ble::PAIR_WITHIN + USUAL;
        let until = now + window + Duration::from_secs(1);
        assert_eq!(pairing_deadline(until, now, USUAL), until);
    }
}
