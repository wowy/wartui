//! Pairing this PC with a bridge, and forgetting it, through BlueZ's D-Bus API on Linux.
//!
//! **Why `wartui bluetooth pair` exists.** The bridge serves only a central that holds a bond it
//! keeps, and BlueZ never starts a first pairing on its own: a connect that meets the bridge's
//! Insufficient Authentication answer fails rather than pairs (`docs/c6-ble-pairing-findings.md`).
//! So the PC pairs once through `org.bluez.Device1.Pair`, the call `bluetoothctl pair` makes. It
//! is LE Secure Connections Just Works: nothing to type and nothing to show.
//!
//! **A stale bond is removed, never repaired in place.** BlueZ never pairs again on its own
//! either: a PC still holding a bond the bridge has dropped fails every connect, and was
//! recovered on the bench only by forgetting the bridge and pairing afresh. So `pair` removes
//! such a device (`org.bluez.Adapter1.RemoveDevice`) before pairing, and `wartui bluetooth
//! forget` removes it after clearing the bridge's bonds.
//!
//! **An agent for the call.** `Pair` uses an agent its caller registered, else BlueZ's default
//! agent, and a Just Works pairing may still ask the agent to confirm. A desktop session
//! registers a default agent and a headless PC has none, so [`pair`] registers its own, with
//! capability `NoInputNoOutput`, on the connection it calls from. That agent says yes to every
//! confirmation and no to any request for a PIN or passkey, and is unregistered after. A
//! registration BlueZ refuses leaves the call to the default agent. Which of the two BlueZ 5.88
//! needs for this bridge is for the bench to show.
//!
//! **The calls block**, on libdbus, so callers run them off the async runtime.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use btleplug::api::bleuuid::uuid_from_u16;
use dbus::arg::{PropMap, RefArg, Variant, cast, prop_cast};
use dbus::blocking::Connection;
use dbus::blocking::stdintf::org_freedesktop_dbus::ObjectManager;
use dbus::message::MessageType;
use dbus::strings::ErrorName;
use dbus::{Message, Path};
use thiserror::Error;
use wartui_proto::ble::{ID_UUID, advertised_octets};

const BLUEZ: &str = "org.bluez";
const DEVICE: &str = "org.bluez.Device1";
const ADAPTER: &str = "org.bluez.Adapter1";
const AGENT_MANAGER: &str = "org.bluez.AgentManager1";
const AGENT: &str = "org.bluez.Agent1";

/// Where the pairing agent is exported, on the connection that pairs.
const AGENT_PATH: &str = "/org/wartui/agent";

/// How long an ordinary call waits for BlueZ.
const CALL_WITHIN: Duration = Duration::from_secs(10);

/// How long a pairing may take. The spikes measured 254–439 ms, and BlueZ ends a pairing that
/// fails well inside this.
pub const PAIR_WITHIN: Duration = Duration::from_secs(40);

/// How often the pairing call looks for its reply between agent requests.
const POP_EVERY: Duration = Duration::from_millis(250);

/// Why a call to BlueZ failed.
#[derive(Debug, Clone, Error, PartialEq, Eq)]
pub enum BluezError {
    /// The system bus or BlueZ on it did not answer.
    #[error("BlueZ did not answer over D-Bus ({0}); check the bluetooth service is running")]
    Unreachable(String),
    /// The bridge ended the pairing.
    #[error("the bridge refused the pairing ({0})")]
    Rejected(String),
    /// BlueZ never reached the device, so the pairing did not start.
    #[error("BlueZ: {0}")]
    NotReached(String),
    /// Anything else, in BlueZ's words.
    #[error("BlueZ: {0}")]
    Failed(String),
}

/// A device BlueZ lists.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Device {
    /// Its object path, `/org/bluez/hci0/dev_…`.
    pub path: String,
    /// Its adapter's object path.
    pub adapter: String,
    /// Its address, as BlueZ writes it.
    pub address: String,
    /// The name it advertised, if BlueZ heard one: a label, never matched.
    pub name: Option<String>,
    /// The bridge's last two octets, when the service data BlueZ holds carries its id. BlueZ
    /// holds none for a device it has not heard since it started.
    pub octets: Option<[u8; 2]>,
    /// Whether this PC holds a pairing with it.
    pub paired: bool,
}

/// Every device BlueZ lists, on every adapter.
///
/// # Errors
/// [`BluezError::Unreachable`] when the system bus or BlueZ does not answer.
pub fn devices() -> Result<Vec<Device>, BluezError> {
    let conn = Connection::new_system().map_err(unreachable)?;
    let proxy = conn.with_proxy(BLUEZ, "/", CALL_WITHIN);
    let objects = proxy.get_managed_objects().map_err(unreachable)?;
    Ok(objects
        .into_iter()
        .filter_map(|(path, interfaces)| device_of(&path, interfaces.get(DEVICE)?))
        .collect())
}

/// The device BlueZ lists at `address`, if any.
///
/// # Errors
/// As [`devices`].
pub fn device_at(address: &str) -> Result<Option<Device>, BluezError> {
    Ok(devices()?.into_iter().find(|device| device.address.eq_ignore_ascii_case(address)))
}

/// A `Device1`'s properties as a [`Device`], or `None` without an address or adapter.
fn device_of(path: &Path<'_>, props: &PropMap) -> Option<Device> {
    let address = prop_cast::<String>(props, "Address")?.clone();
    let adapter = prop_cast::<Path<'static>>(props, "Adapter")?.to_string();
    let name = prop_cast::<String>(props, "Name").cloned();
    let paired = prop_cast::<bool>(props, "Paired").copied().unwrap_or(false);
    Some(Device {
        path: path.to_string(),
        adapter,
        address,
        name,
        octets: octets_of(props),
        paired,
    })
}

/// The bridge's octets from a `Device1`'s `ServiceData` (`a{sv}`, keyed by UUID), through
/// [`advertised_octets`].
fn octets_of(props: &PropMap) -> Option<[u8; 2]> {
    let key = uuid_from_u16(ID_UUID).to_string();
    let services = prop_cast::<HashMap<String, Variant<Box<dyn RefArg>>>>(props, "ServiceData")?;
    let id = services.iter().find(|(uuid, _)| uuid.eq_ignore_ascii_case(&key)).map(|(_, v)| v);
    let id = cast::<Vec<u8>>(&id?.0)?;
    advertised_octets(Some(id))
}

/// Forget `device` on this PC: its pairing and everything BlueZ knows of it.
///
/// # Errors
/// [`BluezError`] when BlueZ refuses or does not answer.
pub fn remove(device: &Device) -> Result<(), BluezError> {
    let conn = Connection::new_system().map_err(unreachable)?;
    let proxy = conn.with_proxy(BLUEZ, device.adapter.as_str(), CALL_WITHIN);
    let path = Path::new(device.path.as_str()).map_err(BluezError::Failed)?;
    proxy.method_call::<(), _, _, _>(ADAPTER, "RemoveDevice", (path,)).map_err(failed)
}

/// Pair with `device`, which BlueZ connects to for the call, under an agent of this call's own
/// when BlueZ takes one. A device already paired is a success.
///
/// # Errors
/// [`BluezError::Rejected`] when the bridge ended the pairing, [`BluezError::NotReached`] when
/// BlueZ never reached it, else [`BluezError`].
pub fn pair(device: &Device) -> Result<(), BluezError> {
    let conn = Connection::new_system().map_err(unreachable)?;
    let agent = register_agent(&conn);
    let paired = call_pair(&conn, device);
    if agent {
        let manager = conn.with_proxy(BLUEZ, "/org/bluez", CALL_WITHIN);
        let path = Path::from(AGENT_PATH);
        if let Err(e) =
            manager.method_call::<(), _, _, _>(AGENT_MANAGER, "UnregisterAgent", (path,))
        {
            tracing::debug!(error = %e, "BlueZ did not unregister the pairing agent");
        }
    }
    paired
}

/// Register this connection's agent; `false` when BlueZ refuses it, which leaves the pairing to
/// the default agent.
fn register_agent(conn: &Connection) -> bool {
    let manager = conn.with_proxy(BLUEZ, "/org/bluez", CALL_WITHIN);
    let args = (Path::from(AGENT_PATH), "NoInputNoOutput");
    match manager.method_call::<(), _, _, _>(AGENT_MANAGER, "RegisterAgent", args) {
        Ok(()) => true,
        Err(e) => {
            tracing::info!(error = %e, "BlueZ refused the pairing agent; pairing under its default");
            false
        }
    }
}

/// Send `Pair` and wait for its reply, answering the agent's requests meanwhile. A blocking call
/// would hold every request BlueZ sends the agent until the call timed out.
fn call_pair(conn: &Connection, device: &Device) -> Result<(), BluezError> {
    let call = Message::new_method_call(BLUEZ, device.path.as_str(), DEVICE, "Pair")
        .map_err(BluezError::Failed)?;
    let channel = conn.channel();
    let serial = channel
        .send(call)
        .map_err(|()| BluezError::Unreachable("the pairing request was not sent".to_owned()))?;
    let deadline = Instant::now() + PAIR_WITHIN;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            let proxy = conn.with_proxy(BLUEZ, device.path.as_str(), CALL_WITHIN);
            let _ = proxy.method_call::<(), _, _, _>(DEVICE, "CancelPairing", ());
            return Err(BluezError::Failed(format!(
                "the pairing did not finish in {} s",
                PAIR_WITHIN.as_secs()
            )));
        }
        let Some(mut msg) =
            channel.blocking_pop_message(left.min(POP_EVERY)).map_err(unreachable)?
        else {
            continue;
        };
        if msg.get_reply_serial() == Some(serial) {
            return match msg.as_result() {
                Ok(_) => Ok(()),
                Err(e) => {
                    pair_outcome(e.name().unwrap_or_default(), e.message().unwrap_or_default())
                }
            };
        }
        if let Some(reply) = agent_reply(&msg) {
            let _ = channel.send(reply);
            channel.flush();
        }
    }
}

/// What a `Pair` that BlueZ answered with the error `name` and `text` comes to.
///
/// A bridge whose pairing window is closed answers `AuthenticationCanceled`.
/// `ConnectionAttemptFailed` is BlueZ never reaching the bridge, so it is a failure to retry,
/// not a refusal.
///
/// # Errors
/// [`BluezError::Rejected`] when the bridge ended the pairing, [`BluezError::NotReached`] when
/// BlueZ never reached it, else [`BluezError::Failed`].
pub fn pair_outcome(name: &str, text: &str) -> Result<(), BluezError> {
    let short = name.strip_prefix("org.bluez.Error.").unwrap_or(name);
    let said = if text.is_empty() { short.to_owned() } else { format!("{short}: {text}") };
    match short {
        "AlreadyExists" => Ok(()),
        "AuthenticationFailed"
        | "AuthenticationRejected"
        | "AuthenticationCanceled"
        | "AuthenticationTimeout" => Err(BluezError::Rejected(said)),
        "ConnectionAttemptFailed" => Err(BluezError::NotReached(said)),
        _ => Err(BluezError::Failed(said)),
    }
}

/// How the agent answers a request, by its `org.bluez.Agent1` method.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentAnswer {
    /// Accept: a confirmation, an authorization, a display or the agent's release.
    Yes,
    /// Refuse: a PIN or passkey to type, which a Just Works pairing never needs.
    No,
}

/// The agent's answer to `method`.
#[must_use]
pub fn agent_answer(method: &str) -> AgentAnswer {
    match method {
        "RequestPinCode" | "RequestPasskey" => AgentAnswer::No,
        _ => AgentAnswer::Yes,
    }
}

/// The reply to `msg` when it is a request to this call's agent.
fn agent_reply(msg: &Message) -> Option<Message> {
    if msg.msg_type() != MessageType::MethodCall
        || msg.path().as_deref() != Some(AGENT_PATH)
        || msg.interface().as_deref() != Some(AGENT)
    {
        return None;
    }
    let member = msg.member()?;
    tracing::debug!(request = %member, "BlueZ asked the pairing agent");
    Some(match agent_answer(&member) {
        AgentAnswer::Yes => msg.method_return(),
        AgentAnswer::No => msg.error(
            &ErrorName::from("org.bluez.Error.Rejected"),
            c"wartui pairs with nothing to type",
        ),
    })
}

fn unreachable(error: dbus::Error) -> BluezError {
    BluezError::Unreachable(error.message().unwrap_or("no reason given").to_owned())
}

fn failed(error: dbus::Error) -> BluezError {
    let name = error.name().unwrap_or_default();
    let name = name.strip_prefix("org.bluez.Error.").unwrap_or(name);
    BluezError::Failed(format!("{name}: {}", error.message().unwrap_or_default()))
}

#[cfg(test)]
mod tests {
    use wartui_proto::wardrive_esp::beacon::{self, Beacon, COMPANY, STATUS_MAX};

    use super::*;

    fn bytes(value: Vec<u8>) -> Variant<Box<dyn RefArg>> {
        Variant(Box::new(value))
    }

    /// `Device1` properties with `service_data` under the id's UUID, as BlueZ spells it, and
    /// `status` under the WardriveESP company id.
    fn props(service_data: Option<Vec<u8>>, status: Option<Vec<u8>>) -> PropMap {
        let mut props = PropMap::new();
        if let Some(data) = service_data {
            let key = "00005754-0000-1000-8000-00805F9B34FB".to_owned();
            let services: HashMap<String, Variant<Box<dyn RefArg>>> =
                HashMap::from([(key, bytes(data))]);
            props.insert("ServiceData".to_owned(), Variant(Box::new(services)));
        }
        if let Some(status) = status {
            let makers: HashMap<u16, Variant<Box<dyn RefArg>>> =
                HashMap::from([(COMPANY, bytes(status))]);
            props.insert("ManufacturerData".to_owned(), Variant(Box::new(makers)));
        }
        props
    }

    #[test]
    fn bluez_reads_octets_when_service_data_carries_the_id() {
        assert_eq!(octets_of(&props(Some(vec![0x44, 0xC0]), None)), Some([0x44, 0xC0]));
    }

    #[test]
    fn bluez_reads_no_octets_when_device_carries_a_status_and_no_id() {
        // The phones' pair and the status beacon carry the WardriveESP status and no id.
        let mut status = [0; STATUS_MAX];
        let len = beacon::status(&mut status, &Beacon::default(), [0x44, 0xC0], false);
        assert_eq!(octets_of(&props(None, Some(status[..len].to_vec()))), None);
    }

    #[test]
    fn bluez_reads_no_octets_when_device_holds_no_id() {
        // A BlueZ that restarted lists a paired bridge with no service data until it hears it.
        assert_eq!(octets_of(&props(None, None)), None);
        assert_eq!(octets_of(&props(Some(Vec::new()), None)), None);
    }

    #[test]
    fn bluez_reads_pairing_as_rejected_when_bridge_ends_it() {
        for name in ["AuthenticationFailed", "AuthenticationCanceled"] {
            let outcome = pair_outcome(&format!("org.bluez.Error.{name}"), "");
            assert_eq!(outcome, Err(BluezError::Rejected(name.to_owned())));
        }
    }

    #[test]
    fn bluez_reads_pairing_as_not_reached_when_bluez_never_reached_the_bridge() {
        let outcome = pair_outcome("org.bluez.Error.ConnectionAttemptFailed", "Page Timeout");
        let said = "ConnectionAttemptFailed: Page Timeout".to_owned();
        assert_eq!(outcome, Err(BluezError::NotReached(said)));
    }

    #[test]
    fn bluez_reads_pairing_as_done_when_device_is_already_paired() {
        assert_eq!(pair_outcome("org.bluez.Error.AlreadyExists", "Already Exists"), Ok(()));
    }

    #[test]
    fn bluez_reads_pairing_as_failed_when_error_is_not_the_bridges() {
        let outcome = pair_outcome("org.bluez.Error.InProgress", "In Progress");
        assert_eq!(outcome, Err(BluezError::Failed("InProgress: In Progress".to_owned())));
    }

    #[test]
    fn bluez_agent_refuses_typed_secrets_when_asked_and_accepts_the_rest() {
        assert_eq!(agent_answer("RequestPinCode"), AgentAnswer::No);
        assert_eq!(agent_answer("RequestPasskey"), AgentAnswer::No);
        for method in ["RequestConfirmation", "RequestAuthorization", "AuthorizeService", "Cancel"]
        {
            assert_eq!(agent_answer(method), AgentAnswer::Yes, "{method}");
        }
    }

    #[test]
    fn bluez_agent_answers_call_only_when_it_is_addressed_to_its_own_path() {
        let call = |path: &str, member: &str| {
            let mut call = Message::new_method_call(":1.7", path, AGENT, member).expect("valid");
            // As the bus numbers a call it delivers; a reply names it.
            call.set_serial(7);
            call
        };
        let confirm = agent_reply(&call(AGENT_PATH, "RequestConfirmation")).expect("answered");
        assert_eq!(confirm.msg_type(), MessageType::MethodReturn);
        let passkey = agent_reply(&call(AGENT_PATH, "RequestPasskey")).expect("answered");
        assert_eq!(passkey.msg_type(), MessageType::Error);
        assert!(agent_reply(&call("/org/other/agent", "RequestConfirmation")).is_none());
    }
}
