# wartui bridge firmware

The dongle. It parks an ESP32 radio on the fleet's ESP-NOW channel and forwards every wartui frame
it hears up a USB link to `wartui` on the host.

It knows COBS framing, `esp-radio` and the magic that marks a frame as wartui's, and nothing else.
What an air frame is, what a heartbeat means, and how channels are assigned all live on the host.
There they are unit-testable, and a fix costs a `cargo run` rather than a reflash.

A board with a screen follows the same rule from the other side. The host sends finished lines, each
with a severity, and this firmware blits them. What the panel says and how it is arranged are a
`cargo run` away too. Only two things are decided here, because nothing else can decide them:

- the three colors a severity means, which belong to the panel rather than the fleet;
- the fallback screen it draws when no host is talking, which is link-local state.

## Transmitting

The bridge answers a send with the **transmit-callback** status, not the enqueue result. The
receiver's own MAC hardware acknowledges unicast ESP-NOW, so `AckOk` means a node really has the
frame.

The callback returns in 2–4 ms, against the node's 100 ms admin window. The pessimistic case is an
_unacknowledged_ send, where it fires only after the radio exhausts its retry chain. That takes
28–35 ms ([`docs/phase-4-findings.md`](../../docs/phase-4-findings.md)). So a dumb bridge has more
than an order of magnitude in hand when a send is acknowledged. When an assignment queues behind an
unacknowledged one, the margin is under three times.

The send does not block the loop. `wartui_firmware::radio` (`firmware/common/src/radio.rs`), the
radio half every bridging firmware shares, runs sends in their own future, joined with the main
loop. The loop hands each `SendEspNow`, `AddPeer`, `RemovePeer` and `SetTxPower` to one queue of
`WARTUI_BRIDGE_RADIO_JOBS` small entries (default 32, `src/config.rs`), so the radio acts on them in
the order the host sent them, and goes on draining `esp-radio`'s ten-frame receive queue while a send
is in flight. A send's payload waits in a pool of `WARTUI_BRIDGE_SEND_JOBS` slots (default 4) beside
it. A send that finds either full is answered `Rejected`, and a peer or power command that finds the
queue full gets an `Error` line. `Status.queues` reports both as `radio-jobs` and `radio-send-jobs`,
with their high-water marks and how many jobs each refused.

Peers are added on demand (`ensure_peer`) and never removed as a side effect of sending. The radio's
table holds twenty entries, and `esp-radio` spends one on the broadcast peer at init. This bridge
only _receives_ broadcasts, and ESP-NOW delivers a received frame whether or not its sender is a
peer. So the bridge gives that slot up to make room for a twentieth node.

The host removes a node's peer (`RemovePeer`) a minute after its last heartbeat.

The two directions carry different frame sizes:

| Direction  | ESP-NOW payload                      | Link frame                    |
| ---------- | ------------------------------------ | ----------------------------- |
| host → air | up to 1470 bytes (`SendPayload`, v2) | up to 1600 (`MAX_HOST_FRAME`) |
| air → host | up to 1024 bytes (`EspNowPayload`)   | up to 1152 (`MAX_FRAME`)      |

Both chips run ESP-NOW v2 and take 1470-byte frames intact
([`docs/espnow-v2-findings.md`](../../docs/espnow-v2-findings.md)). Nothing a wartui node sends is
longer than 1024 bytes, so each outbox slot is 1152. A received frame over 1024 bytes is
somebody else's: the bridge drops it without a log line and counts it in `Status.rx_oversize`,
not in `rx_count`.

## Building and flashing

```sh
cargo run --release --features esp32c6    # or --features esp32c5
```

Exactly one chip feature is required. The toolchain is stable, and `rust-toolchain.toml` pins it.

Three board features layer over a chip feature:

| Feature                 | Board                                                       | What it does                                                                            |
|-------------------------|-------------------------------------------------------------|-----------------------------------------------------------------------------------------|
| `xiao-external-antenna` | Seeed XIAO ESP32-C6 with a U.FL antenna                     | Points the RF switch at the U.FL connector                                              |
| `t-dongle-c5`           | LilyGO T-Dongle-C5 (`esp32c5,t-dongle-c5`)                  | Lights the ST7735 panel, makes the bridge announce it, and turns off the RGB LED        |
| `t-dongle-c5-gps`       | LilyGO T-Dongle-C5 with a u-blox M10 on its Qwiic connector | Forwards the receiver's bytes, makes the bridge announce GPS, and turns off the RGB LED |

[`../node/README.md`](../node/README.md) § "Building and flashing" has the rest on the XIAO antenna.

Every board feature is inert without a chip feature. `xiao-external-antenna` excludes both
T-Dongle-C5 features: it drives GPIO3 and GPIO14, which on a T-Dongle-C5 are `LCD_DC` and `USB_DP`.
Either T-Dongle-C5 feature drives GPIO4 and GPIO5, the RGB LED's clock and data, to turn it off.
The two T-Dongle-C5 features are independent, and either builds without the other. Built without
`t-dongle-c5`, the same source reports no panel and is sent no lines. Built without
`t-dongle-c5-gps`, it reports no GPS port and drops what the host writes to one. One tree covers
every board.

From the host, `wartui flash-bridge --features esp32c6` (or the features for your board) flashes
the one board it can tell is the bridge. In a checkout it builds this firmware. In a release binary
it fetches the same release's image. Each release publishes an image for `esp32c5`,
`esp32c5,t-dongle-c5`, `esp32c5,t-dongle-c5-gps`, `esp32c5,t-dongle-c5,t-dongle-c5-gps`, `esp32c6`,
and `esp32c6,xiao-external-antenna`.
[`crates/wartui/README.md`](../../crates/wartui/README.md) § "Flashing the bridge" has how it picks
the board.

## The panel

The host composes five lines from its own snapshot: GPS, node counts, approximate Wi-Fi and BLE
totals, and the fleet's link RSSI. Each is colored green, amber, or red by how that one thing is
going. [`../../crates/wartui/README.md`](../../crates/wartui/README.md) § "The bridge panel" says
what an operator reads off them.

The pins are the vendor's, from `include/pin_config.h` and `lib/lcd_st7735/`:

| Signal     | GPIO |                                                                                          |
|------------|------|------------------------------------------------------------------------------------------|
| `LCD_MOSI` | 2    | shared with `SD_CMD`                                                                     |
| `LCD_SCK`  | 6    | shared with `SD_CLK`                                                                     |
| `LCD_MISO` | 7    | shared with `SD_DAT0`; the panel never reads, so this is left unclaimed                  |
| `LCD_CS`   | 10   |                                                                                          |
| `LCD_DC`   | 3    | the vendor calls it `LCD_RS`                                                             |
| `LCD_RST`  | 1    |                                                                                          |
| `LCD_BL`   | 0    | **active low** — 0 is lit, and driving it high is the failure that reads as a dead panel |
| `SD_CS`    | 23   | held high, so the card slot stays off a bus it shares with the panel                     |

Know two things about the init before changing it. [`src/panel.rs`](src/panel.rs)'s `//!` covers
both at length.

- The panel is **BGR**, set explicitly rather than inherited. Get it wrong and red renders blue. On
  a screen whose whole job is red against green against yellow, that is not cosmetic.
- mipidsi takes the size and offset in the controller's own portrait framebuffer, then rotates. So
  they are `80x160` at `(26, 1)`. Landscape figures are rejected outright, because 160 is wider than
  an ST7735's 132-column framebuffer.

A panel that will not start is reported and then done without. The bridge carries on bridging and
says `panel: none`, rather than panicking into a reboot nobody can read.

The cargo runner is `espflash flash --monitor` with no `--chip`, so espflash detects the part.
`--monitor` renders the framed link bytes as text, which looks like line noise. Use `wartui sniff`
to read them.

## The GPS

A u-blox M10 receiver (a SparkFun MAX-M10S) plugs into the T-Dongle-C5's Qwiic connector, P3 on the
vendor schematic. It answers at I2C address `0x42`, on I2C0 at 400 kHz:

| P3 pin | GPIO | Signal |
|--------|------|--------|
| 1      | —    | GND    |
| 2      | —    | 3.3 V  |
| 3      | 11   | SDA    |
| 4      | 12   | SCL    |

The dongle has no pull-ups on these lines. The SparkFun board brings its own.

**The bridge is a byte pipe here too.** It reads the receiver's stream and forwards it in `Gps`
frames, and writes the bytes a `GpsWrite` carries. It parses neither. The host splits NMEA from UBX
and composes the receiver's configuration, so a change to the rate or the sentences costs a
`cargo run` rather than a reflash.

**The receiver is read by its own task, over interrupt-driven I2C, and the main loop never waits on
it.** A byte costs about 38 µs, so a full 64-byte read takes 2.5–4 ms
([`docs/t-dongle-c5-gps-findings.md`](../../docs/t-dongle-c5-gps-findings.md)). Read blocking from
the main loop, the CPU spins for all of it while the radio's ten-deep receive queue and the USB
outbox go unserved. With four nodes bursting, a 5 Hz receiver read that way lost 26–38 sighting
batches a four-minute run against 0–17 without it, and the outbox dropped frames. Async I2C lets
the peripheral clock the bytes while the CPU serves the radio and USB.

The task runs on an interrupt executor at the lowest interrupt priority, so it preempts the main
loop for the microseconds it takes to start or finish a transfer. The C5's 32-byte I2C FIFO splits
a 64-byte read into three transfers. On the main loop's executor each would wait for the loop to
give the core back, and a panel repaint holds it for 36 ms. [`src/gps.rs`](src/gps.rs)'s `//!` has
the hand-off between the two.

The task polls no more often than every 25 ms, and again straight away while the receiver has more
waiting, so a backlog drains. Every transaction is bounded at 10 ms.

The reads are batched into `Gps` frames of up to 512 bytes, each handed to the main loop once the
receiver has drained or the next read would not fit. A finished batch ends the loop's idle wait, so
it reaches the outbox within a pass: about one frame per fix at 5 Hz, plus a few for the once-a-second
`GSA`/`GSV` burst. Every `Gps` frame takes a whole slot of the outbox's bulk ring, the slots node
sightings wait in, so a frame per read, 30 to 40 a second, would push sightings out whenever USB
backs up.

A receiver that stops answering is probed once a second and reported as an `Error` at each `Ready`
until it answers again. The `Ready` field `gps` says the board has the port, not that a receiver
answered.

`wartui sniff --verbose` prints each `Gps` frame's bytes as text, which is how to check the wiring.

## Checking it works

```sh
wartui status     # counters, uptime — proves the link runs both ways
wartui sniff      # every frame, decoded
```

Reach for `wartui status` first when frames are not arriving. A bridge that answers is listening,
and `received 0 frames` then points at the nodes rather than the link.

A bridge that does _not_ answer gets a named diagnosis after five seconds rather than a wait. Three
causes produce the identical symptom, and each needs a different action:

- the wrong firmware on the board;
- another program holding the port;
- a bridge that has stopped answering while still enumerating as a USB device.

That last one is not a hung bridge. Its USB _transmit_ endpoint has stopped draining, while its
receive endpoint still decodes and executes everything you send it. One flag decides it:
`SERIAL_IN_EP_DATA_FREE`. `WR_DONE` clears it, and per the TRM it comes back only when the USB host
reads the FIFO. If that read never lands, nothing on the device can clear it.

So the firmware notices and reboots itself within about three seconds.
`wartui_proto::stall::StallWatch` times how long the endpoint has refused bytes _while somebody was
waiting for them_. The firmware resets only when all four hold:

- something is queued and this pass moved none of it;
- a host frame that asks for a reply has been decoded since the last byte moved;
- a host frame arrived within the last ten seconds, so that host is still present;
- the endpoint has refused bytes for three seconds since both the first such frame and the first
  refusal.

A panel push or a peer change asks for nothing, so it proves a host is present but cannot start the
clock. A panel push is often the last thing the TUI sends before it quits, and counting it would
reset the bridge after an ordinary quit.

The unanswered host frame is what keeps a bridge on a bench with no host attached quiet forever. It
also keeps the bridge from resetting every time an operator closes a window: a host that read what
it asked for leaves no unanswered frame behind. Presence alone can't do that, because a host that
has just quit still counts as present for longer than the three-second timeout.

The single `Identify` that `wartui` sends on connecting is enough to clear a bridge that wedged
beforehand. The rule lives in `wartui-proto` rather than here, so it is tested in microseconds
instead of on a bench.

Measured on a C6 left unread for two minutes: the reboot was heard 3.47 s after that `Identify`. The
host sweep gives the remembered board 4.5 s when other boards are attached, so it outlasts the reset
(`docs/phase-3-findings.md`).

The reset _is_ the message, since any explanation would go out through the broken path. The `Ready`
after it says `TxStalled`.

That `Ready` follows ~1.5 KB of ROM banner with no `0x00` in it. The banner overflows the host's
frame buffer and would take the first frame with it. So the bridge queues a lone `0x00` at boot,
ahead of everything (`Outbox::delimit`). That is an empty frame every receiver already skips, so it
is not a wire change.

**A bridge holds all transmit until a host speaks**, unless a host was present when the previous
life ended (`ResetCause::speaks_first`). Each pass of the main loop records in RTC memory whether
a host frame decoded in the last ten seconds, and the next life reads it back, except after a
power-on. Writing to the endpoint before any host has opened the port is what wedges it: after a replug and two minutes unread, 3 of 3 first opens found it dead, against 3 of 3
healthy with transmit held. Only the `0x00` waits behind the gate, and goes out first when it
opens. The first host frame opens it. If that frame is not an `Identify`, the bridge announces
before handling it, so a host that was already running still sees a new life. A frame that decodes
but carries another link version counts as a host and opens the gate too. A life that follows a
present host speaks first because that host is still attached and reading, and it relies on that
`Ready`: it sends one `Identify` per connection (`docs/phase-3-findings.md`).

**Nothing queues for an absent host, and a connecting host starts from an empty queue.** A host
counts as reading while one of its frames decoded in the last ten seconds, or for the first ten
seconds of a life that speaks first. Every frame goes through `Bridge::send`, which drops it
otherwise. An `Identify`, or any frame from a host not yet reading, clears the outbox before the
host is served (`note_host_frame`); a frame part-way out is abandoned behind a `0x00`. A backlog
replayed at attach arrived late, carried the host's current position onto old sightings and read as
missed admin windows. Faults worth keeping, the panel's and the GPS receiver's, are repeated by
every `Ready`. Neither discard counts toward `Status.dropped_tx`, which keeps meaning congestion
while a host reads.

Reach for `wartui reset --bridge <board>` before `espflash`. The receive path is alive in this
state, so the bridge reboots on being asked. A software reset also keeps the device path. An
`espflash` reset re-enumerates the board and can move `ttyACM0` to `ttyACM1` underneath a script.
`espflash reset --port <path>` is the fallback for when even that goes unanswered, which means the
firmware really is hung. [`docs/phase-3-findings.md`](../../docs/phase-3-findings.md) has why no
watchdog covers that case.

## The two things that are easy to get wrong

**Never block on the USB endpoint.** `UsbSerialJtag` stops accepting bytes as soon as its FIFO
fills. Nothing drains that FIFO unless a host is reading. So a blocking write from the receive path
would stall the radio for as long as the TUI is wedged or the cable is out.

Everything outbound goes through the rings in `wartui_proto::outbox`, drained by whatever the FIFO
will take. The only wait on the endpoint is the main loop's idle one. The FIFO emptying, a host
command, or a received frame ends it; otherwise `IDLE_TICK` bounds it. So a host that stops reading
costs no busy passes, and ten seconds after its last frame the bridge stops queueing for it. Under
pressure both rings evict oldest-first and count it into `Status.dropped_tx`. Priority frames (`Ready`, `SendResult`, `Status`, `Error`) are served ahead of
`Rx`, `Log` and `Gps` rather than given an unbounded queue. That module is in `wartui-proto` so
those rules are unit-testable on the host.

The wait for a transmit callback, which is what makes `AckOk` mean anything, is not on the loop
either: `send_async` awaits it in the send future, so the loop drains the radio and the endpoint
meanwhile.

**Never link `esp-println` with `jtag-serial`.** It writes to the same USB endpoint and would
interleave into the COBS stream. Diagnostics go out as `Log` frames instead. For the same reason the
panic handler resets rather than printing. The message is lost, but a bridge that panics repeatedly
says so by re-announcing itself, which the host already listens for.

## Dependency versions

`esp-radio 1.0.0-beta.1` requires `esp-hal = "~1.2.0"`, and that one requirement fixes the whole
family. It holds the family in two ways:

- **Walled:** `esp-hal` and `esp-rtos`. Cargo enforces this one.
- **Resolves, but doubles:** `esp-alloc`, the `esp-wifi-sys-*` bindings, `esp-sync`, and
  `esp-storage`.

The same dependency holds both firmwares at the same set. Keep that true: they share `wartui-proto`
and `firmware/common`, which states the same `esp-hal` requirement.

The panel crates (`embedded-graphics`, `embedded-hal-bus`, and `mipidsi`) are free to move. Only
their shared `embedded-hal` 1.0 must stay a single copy.

`embassy-executor`, `embassy-sync`, `embassy-time`, and `embedded-io-async` are held the same way,
through `esp-rtos` and `esp-hal`.

Each chip's PAC resolves twice, and that is known: `esp-hal 1.2.2` builds against `esp32c5 0.3` and
`esp32c6 0.24`, `esp-radio 1.0.0-beta.1` against `esp32c5 0.4` and `esp32c6 0.25`. Both build and
link. A later `esp-hal` release on the newer PACs lines them up.

### Crates that double

The other half of the family is the dangerous half, because it resolves. `esp-alloc` is a
dependency of `esp-radio` *and* a direct dependency of both firmwares, and the `esp-wifi-sys-*`
bindings of `esp-radio` and the node. A version above what `esp-radio` asks for is
semver-incompatible with it, not in conflict with it. So Cargo adds a second copy instead of
refusing, and links both:

- Two `esp-wifi-sys-*` each export `__esp_radio_printf` as `no_mangle`, and each carries its own
  copy of the IDF Wi-Fi blobs. `lto = "fat"` refuses the build. That is the cheap version.
- Two `esp-alloc` are two heaps. Only the direct copy takes `#[global_allocator]` and the `malloc`
  shims. Only that one is handed regions by `heap_allocator!` in `src/main.rs`. But `esp-rtos` and
  `esp-radio` allocate every task stack and every Wi-Fi driver queue through *their* copy's
  `esp_alloc::InternalMemory`. It reads its own `HEAP` static rather than going through those shims.
  So the firmware builds, links, and passes CI, then panics at `esp_rtos::start`, before the radio
  is up. The panic handler resets, so the board does it again: 88 boots in 20 seconds, never
  reaching `Ready`. No host build can see any of that.

A second copy does not need a manifest to arrive, either. `esp-sync` is a direct dependency of the
node alone, and `esp-storage` reaches the bridge only through `esp-bootloader-esp-idf`, yet either
doubles in both firmwares if anything above it moves out of step.

### Panel crates

The panel's three crates — `embedded-graphics`, `embedded-hal-bus`, and `mipidsi` — are outside the
wall. None is in `esp-radio`'s dependency closure, so all three are free to move. They are not
entirely outside it, though. All three sit on `embedded-hal` 1.0, which `esp-hal` also brings. A
second copy of *that* would be the same class of problem, so the check below names it too. It prints
two lines for it, which is correct. `embedded-hal` 0.2 is a different crate that has always
coexisted here; two copies of 1.x would not be.

### Holding the set

Every crate `esp-radio` also depends on is held at the version `esp-radio` resolves, and they all
move when it does. Two things hold that without a board to test on:

- `.github/dependabot.yml` ignores the incompatible bumps in both firmware directories;
- each firmware's CI job counts the versions before it builds.

By hand, from either firmware directory:

```sh
cargo tree --locked --features esp32c6 -e normal --prefix none --format '{p}' \
  | awk '{print $1, $2}' | sort -u \
  | grep -E '^(esp-(alloc|hal|radio|rtos|storage|sync|wifi-sys-esp32c[56])|embedded-hal) '
```

They are direct dependencies for reasons that do not go away. `esp-wifi-sys-*` is a direct
dependency of the node alone, for `esp_wifi_get_max_tx_power` (`tx_power` in
`firmware/node/src/radio.rs`), which `esp-radio` does not wrap. The bridge sets its transmit power
through `WifiController::set_max_tx_power` and reaches the bindings only through `esp-radio`.

The bridge's one `unsafe` is not an IDF call: the GPS build takes its interrupt executor's spawner
through `Spawner::for_current_executor` (`current_spawner` in `src/gps.rs`). `esp-hal`'s async I2C
driver is not `Send`, and `InterruptExecutor::start` gives out only a `SendSpawner`, so nothing safe
can start the task that owns the bus.

`esp-alloc` is here because the firmware owns the heap and declares its regions.

`esp-storage` is a direct dependency of the node alone, for over-the-air updates, and reaches the
bridge through `esp-bootloader-esp-idf`. It is held at the version `esp-bootloader-esp-idf`
resolves, and brings no crate the tree lacks: its `esp-sync` and `esp-rom-sys` are the copies
`esp-hal` uses, and `esp-rom-sys` is `links`-keyed besides. Both firmwares' CI jobs count it with
the rest, and Dependabot ignores its and `esp-bootloader-esp-idf`'s minor and major releases.

### Embassy and `embedded-io-async`

`embassy-executor`, `embassy-sync`, `embassy-time`, and `embedded-io-async` are held at the
versions `esp-rtos` and `esp-hal` build against, and move with them. `embedded-io-async` stays at
0.7, the newer of the two versions `esp-hal` 1.2 implements its async traits for. None can double
quietly:

- the executor's `Spawner` would not type-check against `esp-rtos`'s;
- `embassy-time-driver` is `links`-keyed;
- a second `embedded-io-async` would have no implementation on the USB half.

So they are not in the check, and Dependabot ignores their minor and major releases.

`embassy-sync` is the GPS build's alone, for the task's channels, and held at 0.8, the copy
`esp-hal` and `esp-rtos` use. 0.6 and 0.7 are in the lockfile too, both `esp-sync`'s, which depends
on all three under aliases. A second 0.x would double rather than conflict, and harmlessly: the
channels never pass through an `esp-hal` or `esp-rtos` API, and the crate keeps no global state. It
is held so the tree adds no copy, not because one would break the build.

`esp-generate` is a version behind this set. Its scaffolding (`build.rs`, `.cargo/config.toml`) is
what was taken from it, not its dependency list.
