//! A bench build's own WardriveESP session: for trying the service against a phone app with the
//! C6 alone, before the P4 runs one.
//!
//! Built with `WARTUI_RADIO_WARDRIVE_ESP_BENCH` set ([`WARDRIVE_ESP_BENCH`]), the C6 admits a phone
//! whatever the P4's `Down::Bonds` says (`src/ble.rs`, `take_bonds`), and answers it from a
//! [`Session`] of its own. Its lines go straight into the notify ring, framed as the P4's would be.
//! A subscription gets the scan's header, each command its reply, and the scan a `WD:STATUS`
//! every [`PERIOD`]. No `WD:AP` is ever sent: the C6 hears no access point, and the bench sends no
//! invented one. Each command is logged, and still goes up as `Up::WardriveEspCommand`.
//!
//! Without the variable every hook returns at once and [`run`] never wakes.

use core::cell::{Cell, RefCell};
use core::fmt;

use embassy_time::{Duration, Instant, Timer};
use wartui_proto::mac::Mac;
use wartui_proto::plan::ChannelPool;
use wartui_proto::radio_link::Stream;
use wartui_proto::wardrive_esp::session::{Class, Ctx, Session, Sink};

use crate::config::WARDRIVE_ESP_BENCH;
use crate::link::Shared;

/// How often the scan sends a `WD:STATUS`.
const PERIOD: Duration = Duration::from_secs(5);

/// The session, and the C6's last two address octets its `info` reply names.
pub struct Bench {
    session: RefCell<Session>,
    bridge: Cell<[u8; 2]>,
}

impl Bench {
    pub const fn new() -> Self {
        Self { session: RefCell::new(Session::new()), bridge: Cell::new([0; 2]) }
    }
}

/// The session's lines, into the notify ring.
struct Ring<'a>(&'a Shared);

impl Sink for Ring<'_> {
    fn line(&mut self, _class: Class, line: &[u8]) {
        crate::ble::push_bench_line(self.0, line);
    }
}

/// The C6's state, as the session's replies read it.
fn ctx(shared: &Shared) -> Ctx<'static> {
    Ctx {
        pool: ChannelPool::All,
        version: env!("CARGO_PKG_VERSION"),
        bridge: shared.ble.bench.bridge.get(),
        uptime_ms: Instant::now().as_millis() as u32,
        nodes: 0,
        mtu: shared.ble.state().mtu,
        dropped: 0,
    }
}

/// The stream changed from `was` to `now`: a phone that let go ends its session, and one that
/// subscribed starts a fresh one. Called once the ring is cleared, so the header leads it.
pub fn on_stream(shared: &Shared, was: Stream, now: Stream) {
    if !WARDRIVE_ESP_BENCH {
        return;
    }
    let bench = &shared.ble.bench;
    if was == Stream::Phone {
        let _ = bench.session.borrow_mut().release();
    }
    if now == Stream::Phone {
        let _ = bench.session.borrow_mut().on_subscribe(&ctx(shared), &mut Ring(shared));
    }
}

/// The phone wrote `text`, framed or not: log it and answer it.
pub fn on_command(shared: &Shared, text: &[u8], framed: bool) {
    if !WARDRIVE_ESP_BENCH {
        return;
    }
    let how = if framed { "framed" } else { "unframed" };
    shared.log(format_args!("WardriveESP command: {} ({how})", Printable(text)));
    let bench = &shared.ble.bench;
    let _ = bench.session.borrow_mut().on_command(text, &ctx(shared), &mut Ring(shared));
}

/// Send the scan's `WD:STATUS` every [`PERIOD`] while a phone reads, for a C6 whose address is
/// `mac`.
pub async fn run(shared: &Shared, mac: &Mac) -> ! {
    if !WARDRIVE_ESP_BENCH {
        core::future::pending().await
    }
    // Set on the first poll, before any central can connect.
    shared.ble.bench.bridge.set([mac[4], mac[5]]);
    loop {
        Timer::after(PERIOD).await;
        if shared.ble.stream() == Stream::Phone {
            shared.ble.bench.session.borrow_mut().on_period(&mut Ring(shared));
        }
    }
}

/// Bytes as text, each one outside printable ASCII as `?`.
struct Printable<'a>(&'a [u8]);

impl fmt::Display for Printable<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for &byte in self.0 {
            let shown = if (0x20..0x7F).contains(&byte) { char::from(byte) } else { '?' };
            fmt::Write::write_char(f, shown)?;
        }
        Ok(())
    }
}
