//! What the binding's tests share: a loopback radio over a `WireSim`, a settings store held in
//! memory, and waits with deadlines.

#![allow(dead_code)] // Each test binary uses its own part of this.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError, Weak};
use std::time::Duration;

use tokio::runtime::Handle;
use tokio::time::timeout;
use wartui_app::view::View;
use wartui_ffi::{
    CaptureConfig, Completion, DriverError, Heard, Link, PositionSources, Radio, RadioDriver,
    Session, Settings, SettingsStore, StoreFailure, Viewed, WireSim,
};

/// The longest any wait takes before the test fails rather than hangs. The simulator announces
/// within a second, and its nodes heartbeat within a few.
pub const WAIT: Duration = Duration::from_secs(15);

/// The key the loopback radio knows its one bridge by.
pub const PEER: &str = "loopback";

/// A phone's radio over a [`WireSim`]: it hears the one bridge on a scan, and answers every
/// request from a task of the test's runtime, never inside the call, as a phone's stack does.
pub struct Loopback {
    sim: Arc<WireSim>,
    radio: Mutex<Weak<Radio>>,
    tasks: Handle,
    pumping: AtomicBool,
}

impl Loopback {
    /// A radio over a fresh simulated bridge with three nodes. Call it inside a tokio runtime.
    pub fn radio() -> Arc<Radio> {
        let driver = Arc::new(Self {
            sim: WireSim::start(3, 185),
            radio: Mutex::new(Weak::new()),
            tasks: Handle::current(),
            pumping: AtomicBool::new(false),
        });
        let radio = Radio::new(driver.clone());
        *driver.radio.lock().unwrap_or_else(PoisonError::into_inner) = Arc::downgrade(&radio);
        radio
    }

    fn radio_later(&self) -> Weak<Radio> {
        self.radio.lock().unwrap_or_else(PoisonError::into_inner).clone()
    }

    /// Answer `request` with `outcome`, from another task.
    fn answer(&self, request: u64, outcome: Completion) {
        let radio = self.radio_later();
        self.tasks.spawn(async move {
            if let Some(radio) = radio.upgrade() {
                radio.completed(request, outcome);
            }
        });
    }
}

impl RadioDriver for Loopback {
    fn pairs_on_connect(&self) -> Result<bool, DriverError> {
        Ok(true)
    }

    fn recall(&self, request: u64, _key: String) -> Result<(), DriverError> {
        self.answer(request, Completion::Recalled { known: false });
        Ok(())
    }

    fn known(&self, request: u64) -> Result<(), DriverError> {
        self.answer(request, Completion::Known { bridges: Vec::new() });
        Ok(())
    }

    fn start_scan(&self, request: u64) -> Result<(), DriverError> {
        self.answer(request, Completion::Done);
        let (radio, id) = (self.radio_later(), self.sim.id());
        self.tasks.spawn(async move {
            if let Some(radio) = radio.upgrade() {
                radio.heard(Heard { peer: PEER.to_owned(), id: Some(id), name: None, live: true });
            }
        });
        Ok(())
    }

    fn stop_scan(&self) -> Result<(), DriverError> {
        Ok(())
    }

    fn connect(&self, request: u64, _peer: String) -> Result<(), DriverError> {
        self.answer(request, Completion::Connected { made: true });
        Ok(())
    }

    fn abandon(&self, _peer: String) -> Result<(), DriverError> {
        Ok(())
    }

    fn discover(&self, request: u64, _peer: String) -> Result<(), DriverError> {
        self.answer(request, Completion::Done);
        Ok(())
    }

    fn subscribe(&self, request: u64, peer: String) -> Result<(), DriverError> {
        self.answer(request, Completion::Done);
        if !self.pumping.swap(true, Ordering::Relaxed) {
            let (radio, sim) = (self.radio_later(), Arc::clone(&self.sim));
            self.tasks.spawn(async move {
                while let Some(bytes) = sim.notified().await {
                    let Some(radio) = radio.upgrade() else { return };
                    radio.notified(peer.clone(), bytes);
                }
            });
        }
        Ok(())
    }

    fn mtu(&self, _peer: String) -> Result<u16, DriverError> {
        Ok(185)
    }

    fn write(
        &self,
        request: u64,
        _peer: String,
        bytes: Vec<u8>,
        _with_response: bool,
    ) -> Result<(), DriverError> {
        self.sim.write(bytes);
        self.answer(request, Completion::Done);
        Ok(())
    }

    fn is_connected(&self, _peer: String) -> Result<bool, DriverError> {
        Ok(true)
    }

    fn close(&self, _peer: String, _unsubscribe: bool) -> Result<(), DriverError> {
        Ok(())
    }
}

/// A settings store held in memory.
#[derive(Default)]
pub struct Memory {
    text: Mutex<Option<String>>,
}

impl Memory {
    /// A store holding `text`.
    pub fn holding(text: &str) -> Arc<Self> {
        Arc::new(Self { text: Mutex::new(Some(text.to_owned())) })
    }

    /// What it holds now.
    pub fn text(&self) -> Option<String> {
        self.text.lock().unwrap_or_else(PoisonError::into_inner).clone()
    }
}

impl SettingsStore for Memory {
    fn load(&self) -> Result<Option<String>, StoreFailure> {
        Ok(self.text())
    }

    fn save(&self, text: String, _secret: bool) -> Result<(), StoreFailure> {
        *self.text.lock().unwrap_or_else(PoisonError::into_inner) = Some(text);
        Ok(())
    }

    fn place(&self) -> Result<String, StoreFailure> {
        Ok("memory".to_owned())
    }
}

/// A capture into `db`, with nothing else asked for.
pub fn capture(db: &Path) -> CaptureConfig {
    CaptureConfig {
        db_path: db.to_str().expect("a UTF-8 temporary path").to_owned(),
        notes: None,
        record_raw: false,
        build: None,
    }
}

/// No position source.
pub fn nowhere() -> PositionSources {
    PositionSources { bridge_receiver: false, location: None, fixed: None }
}

/// A capture into `db` over `link`, with defaults saved nowhere and no position.
pub fn start(db: &Path, link: &Arc<Link>) -> Arc<Session> {
    Session::start(capture(db), Arc::clone(link), Settings::unsaved(), nowhere(), None)
        .expect("the capture starts")
}

/// The first view `done` holds for, within [`WAIT`].
pub async fn until(session: &Session, mut done: impl FnMut(&View) -> bool) -> Viewed {
    let following = async {
        let mut after = 0;
        loop {
            let viewed = session.next_view(after).await.expect("the capture is running");
            if done(&viewed.view) {
                return viewed;
            }
            after = viewed.seq;
        }
    };
    timeout(WAIT, following).await.expect("a view in time")
}

/// Whether `view` shows the three nodes the simulator runs.
pub fn three_nodes(view: &View) -> bool {
    view.nodes.len() == 3
}
