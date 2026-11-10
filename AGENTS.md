# AGENTS.md

Guidance for coding agents working in this repository.

## What this is

A terminal fleet controller for ESP32-C5 wardriving nodes, wired over SPI to an ESP32-P4 bridge
(the node bus), reached over USB through the board's CH343. The board's C6 holds a UART link the P4
updates and reports on. `README.md`
introduces it and says which chip goes where.

Where the prose lives:

- `README.md` — the front door, and stays short. It also carries the full keyboard list.
- `crates/wartui/README.md` — the operator's manual: keys, fleet states, channel pools,
  GPS, troubleshooting. Read it before changing view or CLI behavior, and **keep it true
  when that behavior changes.**
- `firmware/*/README.md` — each firmware's own constraints, including the `esp-*` dependency
  set (`firmware/p4-radio/README.md`).
- `docs/node-bus.md` — the SPI bus between the P4 bridge and its nodes: pins, pin behavior and the
  messages its records carry.
- `docs/*-findings.md` — what each bench actually measured, and why several invariants
  below exist. `phase-N` are the fleet's, and the rest are named for what was measured.

Module-level `//!` docs carry the reasoning behind the invariants below. They are the
record; this file is the index.

## Commands

```sh
cargo test --workspace
cargo clippy --workspace --all-targets
cargo fmt --check

cargo test -p wartui-core --test engine                    # one test binary
cargo test -p wartui-core --test engine engine_ignores_    # tests by name prefix

cargo run --release -p wartui -- bench --db bench.db --fresh  # store I/O, sim-driven; see docs/store-io-findings.md
```

Run it without hardware — the simulator runs a fake fleet on a fake clock:

```sh
cargo run -p wartui -- --sim 3 --lat 37.7749 --lon -122.4194
cargo run -p wartui -- ports | status | sniff        # with a bridge plugged in
cargo run -p wartui -- --log-file wartui.log run     # the only way to see transport logs
```

See what the view draws without a terminal: `tools/render.py --cols 120 --rows 30` runs the
binary on a pty and prints the screen as text, `--keys jb` presses keys first, and `--bridge`
points it at the boards attached rather than the simulator. `README.md` § "Suggested tools" says
when to reach for it over the `TestBackend` tests in `crates/wartui/src/tui/`.

Refer to nodes by their slot label (`N7`), which is printed on the carrier. Use the last two
octets only for bridges, USB-attached boards and nodes with no slot; `wartui ports` names each
attached board by its address, and `crates/wartui/README.md` § "Telling the boards apart" says why
that works.

**Never upload to WDGWars without asking the user directly first, every time.** That covers
`wartui upload`, with or without `--yes`, and any request to its upload endpoints, such as curl.
An upload publishes to the user's profile and cannot be taken back. Approval for one upload is
not approval for the next. Tests use a local listener, never the real site.

Each firmware, and `firmware/common` beside them, is a **separate workspace**
(`exclude = ["firmware"]`): different target, own toolchain pin, own lockfile. `cargo test --workspace` never touches them.

```sh
cd firmware/node
cargo fmt --all -- --check                    # its own workspace, so the root `cargo fmt` misses it
cargo clippy --release

cd firmware/p4-radio                          # the C6 half of the P4 split bridge
cargo fmt --all -- --check
cargo clippy --release

cd firmware/p4-bridge                         # the P4 half, own target
cargo fmt --all -- --check
cargo clippy --release

cd firmware/common                            # a firmware's clippy does not lint it
cargo fmt --all -- --check                    # then each firmware's combination:
cargo clippy --release --features esp32c5         # node
cargo clippy --release --features esp32c6         # P4 bridge's radio
cargo clippy --release --features esp32p4,gps --target riscv32imafc-unknown-none-elf  # P4 bridge
```

CI mirrors the workspace split: host crates on every push and PR; each firmware only
when something it compiles changes, and `firmware/common` when it, `wartui-proto` or a
firmware's `Cargo.toml` does. It runs `fmt` and `clippy` in each firmware directory, so a
`wartui-proto` change that reaches the firmware is checked there too.

## Architecture

Four host crates, strictly layered, plus firmware that shares the bottom one and a crate of its
own.

- **`crates/wartui-proto`** — `no_std`, allocation-free wire formats and the parsing that goes
  with them: `bus` (the SPI transfers between a P4 bridge and its nodes, their acknowledgment, the
  slots, and the messages a node and the P4 exchange), `carrier` (the carrier board's I/O expander
  as the P4 reads it at boot), `fan` (the carrier's lid fan: its duty, PWM wave and tach reading),
  `settings` (the records the P4 keeps its fan duty, channel pool and Bluetooth slot in), `beacon`, `hci`, `dedup`,
  `link`,
  `radio_link` (the UART packets between the P4 and its C6, and their frozen core), `mac` (the
  `Mac` type and its
  text form), `plan` (channel pools and the partitioning planner), `node` (a node's timings and
  sweep cursor), `pending` (the buffers sightings wait in on a node),
  `ota` (the update receiver a node and both halves of the P4 bridge share, `ota::trial`, the
  record of an image on trial, and `ota::image`, the digest that names an image).
  Compiled into *both* the host and the firmware by path dependency, which is the only thing
  keeping the ends in step — and the reason a node's parsers are testable with `cargo test`
  rather than a reflash.
- **`crates/wartui-bridge`** — host side of the link. Everything above talks to a `LinkHandle`
  and cannot tell a real bridge (`serial`) from the fake fleet (`sim`). It also owns the host's
  serial ports generally (`ports`): what is attached and what the OS says it is, so anything else
  that opens a device shares one enumeration.
- **`crates/wartui-core`** — the headless half. Draws nothing, parses no arguments.
- **`crates/wartui`** — clap CLI (`run`/`export`/`analyze`/`upload`/`sniff`/`status`/`reset`/
  `power-cycle`/`fan`/`pool`/`bluetooth`/`ports`/`flash-fleet`/`flash-bridge`/`ota`) and the ratatui view.
- **`firmware/p4-bridge`** — the bridge, on the P4 of a Waveshare ESP32-P4-WIFI6: COBS framing over
  a DMA UART to the host, through the board's CH343, the node bus's two SPI controllers, and a FIFO
  UART with RTS/CTS to its C6. It checks framing and forwards node records verbatim, with no
  knowledge of what they mean. Fixes there cost a reflash, so logic belongs on the host. Its image
  carries the C6's, and updates it. It caches a node image in PSRAM for `wartui ota`, and writes
  the one record it builds from it, `UpdateChunk`, a layout in the bus's frozen core
  (`src/cache.rs`). On a carrier it drives the lid fan (`src/fan.rs`). It keeps the fan's duty,
  the channel pool and the Bluetooth slot in the `nvs` partition, the settings the bridge rather
  than the host holds (`src/settings.rs`).
- **`firmware/p4-radio`** — the board's C6, behind a DMA UART: its hello, its status and its updates.
  It holds the link for a future Bluetooth host link, and runs no radio today.
- **`firmware/node`** — the nodes. Sniffs rather than scans, so it never transmits while looking,
  and owns the channel (`firmware/node/README.md` § "What it does differently"). Hops from one
  assigned channel to the next, reporting each dwell over the node bus, and idles servicing the
  bus when it holds no assignment. The node given the Bluetooth scan sniffs nothing and runs one
  scan after another, back to back. Same rule as the bridge: the logic lives in `wartui-proto`, and
  this crate is the conversation with the radio and the bus.
- **`firmware/common`** (`wartui-firmware`) — chip-level code the firmwares need and that needs
  `esp-hal`, so it cannot live in `wartui-proto`: the reset funnel `reboot()` and `reset_cause()`,
  the bridge's reset-surviving phase marker (`phase`), the P4's u-blox I2C GPS reader behind the `gps` feature (`gps`), the supply and die readings a node's heartbeat carries (`health`), and
  updates on flash, the slot, the switch and the trial, and the P4's settings sector (`ota`).
  Each firmware depends on it by path and selects its chip feature in that dependency. It is its
  own workspace so its own clippy lints it; a firmware's clippy compiles it without linting it.

### The engine/runtime split is load-bearing

`wartui_core::engine::FleetEngine::handle` is a **pure synchronous state machine**: `Event` in,
`ActionBatch` out. It reads no clock (time arrives as `Now`, carrying both a monotonic `Instant`
and unix millis), touches no socket, opens no file. `wartui_core::runtime::drive` is the only
place that reads a clock and performs actions. This is what makes liveness aging, reboot
detection, assignment timing and auto-partitioning testable in microseconds against a clock the
test invents (`crates/wartui-core/tests/engine.rs`). Do not reach for `Instant::now()`, I/O or
`async` inside `engine`. An over-the-air update is split the same way: `ota::OtaSession` is the
pure state machine and `runtime::drive_fleet` its driver, running every node's session at once
(`crates/wartui-core/tests/ota.rs`).

Operator keypresses are `Event::Command`, not methods — a keypress and a heartbeat have to be
ordered against each other. Only `b`, `c` and `p` get that far; channels are the planner's.

### Store is the system of record

`store` is SQLite behind one owner thread with batched transactions and a bounded queue that
**drops rather than blocks** (a stalled engine misses everything, including the assignment that
answers a heartbeat). A capture file holds one run: `Store::create` refuses a path that exists.
`export` (WiGLE CSV) is a view over the store, re-runnable against a finished or still-running
capture. `SCHEMA` changes shape as freely as the work needs. Each change bumps
`store::SCHEMA_VERSION`, and a capture stamped anything else is refused rather than migrated.

Positions resolve fresh per record through `PositionChain`: a host GPS pinned with `--gps` → the
bridge's GPS (its bytes fed in by `runtime::drive`, never the engine) → a host GPS found by
`discover` (NMEA on its own thread, standing down while the bridge's receiver is sending
sentences) → static `--lat`/`--lon` (test only: it marks the capture as test data, which is never
uploaded) → nothing.
Which tier answered is stored per row.

## Invariants that are easy to break

These bite from a distance — from a file other than the one that owns them — so the reasoning
is here rather than only in a `//!`.

- **The wire is ours, in both directions, and shares nothing with the vendor's.** Every node-bus
  transfer is a fixed 1,044 bytes with a magic, `bus::BUS_VERSION` and a CRC, carrying records of
  a type byte, a length and a body. The type byte is the message, numbered per direction: up,
  `Heartbeat` (27 bytes, its prefix and the node's MAC first), `Sightings` (3 plus records of 12
  plus the SSID and the trailer, up to 1024) and `UpdateStatus` (6); down, `Assign` (8), `Stop`
  (empty), and `UpdateBegin` (41), `UpdateChunk` (6 to 1029), `UpdateCommit` (1), `UpdateAbort` (1)
  and `UpdateConfirm` (empty). The P4 checks the framing and passes records through untouched, to
  the host as `FromNode` and from it as `ToNode`, save the `UpdateChunk`s it builds from its
  node image cache. Encode/decode is written out by hand, never by
  transmuting a packed struct, and pinned byte-for-byte in `crates/wartui-proto/tests/bus.rs` and
  `tests/wire.rs`. Link frames are up to 1152 bytes each way. `docs/node-bus.md` has the table.
- **A wire change means updating every node at once.** `wartui-proto` is a path dependency of every
  firmware. A record whose body is not its type's length is counted as `incompatible` and named in
  the footer (`N frames from an older firmware — update`), never admitted to the node table and
  never half-decoded. A heartbeat whose prefix names another major or minor is the exception: its
  node is shown as `needs update` (older) or `host behind` (newer) and never assigned. Once its
  heartbeat has named that version, its sighting batches are counted `incompatible` and not
  recorded. The bridge checks only the transfer framing, and does not need the update.
  `wartui ota` updates the nodes over the bus.
- **`link::LINK_PROTO_VERSION` does not move before 1.0 either**: there is no older peer for it to
  protect when the policy is to flash both ends from one tree. The cost is that a host and a bridge built from different
  trees may meet as an undecodable frame rather than a named mismatch, which is stated where the
  constant is.
- **The sighting trailer means what the kind says it means, and the wire layer never interprets
  it.** For Wi-Fi it is the roaming consortium element's body verbatim; for BLE it is exactly two
  bytes of company identifier, or nothing. The split is made once, in the engine's
  sightings path, per record; keeping `bus` byte-transparent over the trailer is what lets a change
  to how an identifier is *read* cost a re-export rather than a reflash.
  → `crates/wartui-proto/src/bus/sighting.rs`, `SightingMsg::ext`;
  `crates/wartui-proto/src/beacon.rs`, `rcoi_text`; `crates/wartui-core/src/engine/rx.rs`
- **Nothing here is compatible with an earlier wartui, and that is the policy until 1.0.** No
  migration path is built for a fleet mid-upgrade and no code reads an older wire format to be
  helpful about it. The host decodes and drives nothing from a node on another major or minor: it
  never assigns that node and never records its sightings. It identifies that node by the frozen
  heartbeat identity and updates it with `wartui ota`, so update the fleet together. The node
  firmware's `version` in `firmware/node/Cargo.toml` is bumped on every change: before 1.0 a minor
  bump may break the wire and a patch bump may not.
  `bus::NODE_VERSION` mirrors it, and `crates/wartui-proto/tests/version.rs` fails when they differ.
  The node's `trial` sector layout, which one image writes and the next reads, is frozen with the
  bus's core (`crates/wartui-proto/src/ota/trial.rs`). **No version marker but
  `store::SCHEMA_VERSION` moves until 1.0**, whatever a layout does. `bus::BUS_VERSION` names the
  node bus's frozen core and never moves: the host can update any node in the field over that core,
  so a fleet survives every other change.
  `store::SCHEMA_VERSION` moves with any change to a table, column or index, or to what a stored
  value wartui reads back means. A scan-table change does not move it: `assignment.channels` holds
  the indices the bridge reported, read against the scan table of the build that wrote the row, and
  no wartui code decodes the column against the scan table.
  `.claude/skills/capture-review/scripts/survey.py` decodes it with its own mirror of
  `SCAN_CHANNELS`, so it reads a capture written under another scan table in the mirror's terms.
  `store_keeps_schema_shape_when_schema_version_unchanged` compares the SQL of every table, index,
  trigger and view, comments and whitespace aside, and fails on a difference without a bump. It
  cannot see a change in meaning or a pragma, so the written rule alone enforces a bump for a
  change in meaning. **The store has no migrations before 1.0 either.** `check_version` refuses
  any marker but its own — a lower one
  as firmly as a higher one. A capture with another marker is somebody else's file, and the fix is
  a new `--db` path rather than a `migrate` arm. Bringing one forward would mean deciding what an
  older build meant, which is the compatibility this policy declines to claim: a stored
  `ChannelSet` is the indices the bridge reported, read against the scan table of the build that
  wrote them.
- **The planner is the only author of an assignment.** There is no operator override, no mode and
  no flag: `FleetEngine::replan` decides what every node scans and nothing else writes a node's
  `desired`. That is what lets the fleet table and the store read a node's share as the plan's
  without asking who put it there. The Bluetooth slot is one input to the planner rather than an
  exception to it, and it is the bridge's: kept in NVS beside the pool and reported in `Status`.
  `Command::AssignBle` only asks the bridge to change it. The planner deals `Job::Bluetooth` to the
  node heartbeating from that slot, which takes no channels while its share is handed round, and
  an empty slot means no node scans. So moving the scan re-cuts the whole pool, and `replan` — not
  `reissue` — is what delivers both ends of the move. The pool is the other input, and it is the
  bridge's too: the planner cuts the pool the bridge's `Status` reports, and cuts nothing before
  the first. `Command::SetPool` only asks the bridge to change it. Either way it changes what the
  planner cuts, never who holds which share.
- **Only heartbeating nodes are assignable or in the plan** — an assignment answers a heartbeat,
  and goes to the slot that heartbeat came from. A node whose slot the bridge's `Status` shows
  absent leaves the plan at that `Status`, until its next heartbeat. The 20 s heartbeat timeout is
  the backstop for a node that answers the bus but has stopped heartbeating
  (`crates/wartui-core/src/engine/mod.rs`, `on_bus`). `stale`, `off bus`, `no heartbeat` and
  silence are deliberately distinct states, and `SCAN_CHANNELS` order is load-bearing: never sort
  or deduplicate it, because those indices *are* the wire format.
- **A node is only in the plan once it has said what it is.** `FleetEngine::is_assignable`
  refuses a node whose heartbeats carry no `bus::Capabilities`, or whose version does not speak
  `bus::NODE_VERSION` (another major or minor), alongside one whose bus slot another node's
  heartbeat has taken.
  `None` means only that nothing but a sighting has been heard from that address yet — an ordinary
  few seconds in the life of a node about to be perfectly drivable. A share cut for a node nothing
  has confirmed is a share that may be nobody's.
- **Fourteen nodes maximum** (`plan::MAX_NODES`), the node bus's slots (`bus::MAX_SLOTS`): seven
  per bus on the 14-node carrier, and none without a carrier (`bus::NodeBoard`). A node is known
  by its MAC, which its heartbeat carries, and reached through the slot that heartbeat came from
  (`bus::Slot`, `A1` to `B7`). The operator reads a slot through its board's label, `N1` up on a
  carrier, and the store keeps the bus name.
- **An assignment is believed only on delivery** (`BridgeToHost::Delivered`, sent once the node has
  acknowledged the transfer that carried it), never on a successful enqueue — and delivery is not
  adoption. Every heartbeat carries the epoch the node holds, or 0 for none, so the host can tell
  whether the node actually took the assignment rather than merely receiving it. A node adopts an assignment only when the epoch/version differs
  from the one it holds, so re-sending an identical one is acknowledged and silently discarded. If a
  node's heartbeat reports an epoch other than the one it acked, the frame did not land after all,
  and the host re-sends it on that heartbeat rather than waiting. The host never sends a
  node the epoch its heartbeat says it already holds, because a fresh capture restarts the counter
  while a running node keeps its epoch across it.
  → `crates/wartui-core/src/engine/node.rs`, `NodeState::adopted`;
  `crates/wartui-core/src/engine/snapshot.rs`, `Counters::admin_unadopted`
- **`IndexRun` describes a *pool*, never the shape of an assignment.** An assignment is a
  `ChannelSet` naming any subset of `SCAN_CHANNELS`, so the planner flattens the pool and deals
  round-robin rather than steering around run boundaries. The plan has no phases and no timer; it
  changes when fleet membership changes, when the Bluetooth scan moves, when the bridge reports
  another pool, and at no other time.
- **An updated node image is confirmed only by hearing the bridge.** The first record from the P4 a
  trial image decodes — assign, stop, update confirm or an update record — marks it valid; nothing
  else does, because hearing the bridge is the whole test. Rollback is the node app's
  job, since the prebuilt espflash bootloader has none and reads no image state: the third
  unconfirmed boot returns `otadata` to the other slot. The `trial` flash sector, not `otadata`,
  records which slot is on trial and how many times it has booted, one cleared bit per boot,
  because power cycles are routine on these nodes and RTC memory does not survive them. With no
  watchdog, those power cycles are also what rolls back a hung image. Every step's flash writes
  are ordered so a power cut cannot leave an image running untrialed. The P4 bridge's C6 shares
  the trial: the P4's hello confirms its image, and the P4's resets count its boots. So does the
  P4: the first host frame it decodes confirms its image, and a trial boot arms its RTC watchdog,
  whose resets count its boots, so a hung P4 image or one no host hears rolls back after three.
  → `firmware/common/src/ota.rs` `//!`, `crates/wartui-proto/src/ota/trial.rs` (the sector's fixed
  layout), `firmware/node/README.md` § "Over-the-air updates", `firmware/p4-bridge/src/update.rs`
- **`clippy::all` is denied workspace-wide, in every firmware too, and so is `unsafe_code`.** The
  host allows it on one function, the Raspberry Pi firmware mailbox ioctl
  (`crates/wartui-core/src/health.rs`, `mailbox_call`). `firmware/common` allows two more for the node's heartbeat, both PHY blob reads:
  the rail voltage (`health::vdd_mv`) and the die temperature (`health::die_temp_c`).
  `firmware/p4-bridge` allows one, the slice over its mapped PSRAM (`cache::map`). Avoid `unsafe`
  wherever a safe route exists. Any further `unsafe` — a new
  `#[allow(unsafe_code)]`, or a new `unsafe` block inside a function already allowed — is the
  user's decision, made in review, never a convenience, and committing it needs their approval
  first. An uncommitted experiment, such as a throwaway build that measures what a raw register
  access would buy, may add it freely, through per-function allows and never by relaxing the
  `unsafe_code` lint in a `Cargo.toml`. Check a diff for `unsafe` before committing it.

### Load-bearing, and reasoned where they live

Each of these is enforced in one place and explained there at length. The claim is here so an
edit stops; follow the pointer before changing the rule.

- **A record-carrying transfer is acknowledged, an idle one never**, so an idle transfer cannot
  acknowledge lost data. Toward the P4 each slot is stop-and-wait. Toward a node the P4 keeps two
  numbered flights out and the node takes them strictly in order, dropping any flight past one it
  missed, and any but a `SYNC` flight until it has taken one since boot. So delivery toward a node
  stays in order: the host's update session counts on it. Every message is safe to take twice.
  → `crates/wartui-proto/src/bus/mod.rs` `//!`, `crates/wartui-proto/src/bus/window.rs`
- **Finding the bridge means transmitting into what is opened**, so the sweep opens CH343s and
  nothing else, which means finding a bridge never writes to a node, and `wartui reset` — which
  transmits before anything has identified itself — never sweeps at all. A CH343 reports a serial
  number of its own, so the bridge's address is learned only by asking: any CH343 gets
  `Identify` every 500 ms for up to 3 s (`serial::probe`), for `wartui ports`, `flash-bridge` and
  an address `--bridge` names, and the CH343's serial number is remembered beside the address so
  a run opens that port first.
  The GPS search is the other side: it opens everything but the fleet's boards (any CH343 and any
  Espressif vendor ID), and writes to none of them. `discover_ports` lists those boards for
  `flash-fleet`, `flash-bridge`, boot watching and `wartui ports`, and `discover_bridges` is what the
  sweep, the probe and `reset` read.
  → `crates/wartui-bridge/src/ports.rs` `//!`, `serial::probe` /
  `serial::select` / `serial::unambiguous_bridge`, `crates/wartui-core/src/discover.rs` `//!`
- **A connection asks a CH343 every 500 ms, and any other port once, until a `Ready` proves the
  board.** A CH343 drains its UART, and a P4 booting behind it loses the first `Identify`. A path
  named with `--bridge` that is not a listed CH343 may be a node, and a node that is not reading
  its USB endpoint absorbs exactly one packet and NACKs the rest, which sit in the tty's output
  queue through a `close` that no signal can interrupt — 30 s measured against a node, during
  which the process cannot exit. Once proven, a connection that has written nothing for 10 s
  sends a `GetStatus`, since the P4 stops writing to a host it has not heard from in 15 s.
  → `crates/wartui-bridge/src/serial.rs`, `connect`'s `identify` arm and `KEEPALIVE`
- **Never set DTR or RTS on a serial port, or ask `serialport` to.** The kernel raises both
  together on open, which a node's USB Serial/JTAG ignores and the P4 board's CH343 carries to a
  real auto-reset circuit on the P4's EN and GPIO35, where both together are inert too. Moving one
  without the other is the reset sequence, so `.dtr_on_open(false)` reboots the board it was being
  polite to. → `crates/wartui-bridge/src/ports.rs` `//!`
- **The planner deals only to a slot that is sniffing**, and what no slot can take comes back in
  `Plan::unreachable` rather than being dealt anyway. A `Job::Bluetooth` slot takes nothing, so a
  fleet of one that holds the scan reports the whole pool.
  → `crates/wartui-proto/src/plan/mod.rs`, `plan_for`; `crates/wartui/README.md` § "Channel pools"
- **Channel 14 is in no pool and is never dealt**, and stays in the scan table because that table's
  indices are the wire format. → `crates/wartui-proto/src/plan/channels.rs`, `UNSUPPORTED_INDEX`
- **At most one node scans Bluetooth, by default none does, and it is that node's whole job.**
  `ASSIGN_FLAG_BLE` is an operator's decision, not a property of the flashed firmware — made with
  `b`, which asks the bridge to hold the slot whose node scans; the bridge keeps it in NVS. The node
  holding it is dealt no channels and is counted in `Plan::node_count` anyway, because that reports
  the fleet size the plan was cut for rather than a census of who is sniffing. An empty `ChannelSet`
  is sent only with the flag beside it; without it, a node told to scan nothing parks
  while the host believes it is sweeping, and `Plan::assign_for` refuses to build that frame.
  → `crates/wartui-proto/src/plan/mod.rs`, `Job` / `Plan::channels_for` / `assign_for`;
  `crates/wartui-core/src/engine/event.rs`, `Command::AssignBle`;
  `crates/wartui-core/src/engine/replan.rs`, `on_bridge_ble` / `replan`;
  `docs/phase-2-findings.md`
- **A heartbeat read behind the bridge is history.** A host that falls behind a busy fleet drains
  the bridge's queue late; the bridge's clock stamps run ahead of the host's, and the gap is the
  lag. Such a heartbeat's epoch is not believed, and no assignment goes out on one: the slot it
  names may be another node's now, and `deal` cannot skip an epoch it has not seen. Delete either
  half of the check and the symptom is a plausible-looking lie rather than an error.
  A connection starts caught up, which is safe only because nothing older than it reaches the
  engine: a bridge clears its queue when a host connects, and the transport drops what decodes
  ahead of a connection's first `Ready`.
  → `crates/wartui-core/src/engine/lag.rs`, `note_arrival` / `reading_live` / `BEHIND_US`;
  `crates/wartui-core/src/engine/admin.rs`, `send_admin`;
  `crates/wartui-bridge/src/serial.rs`, `read_loop`
- **Every reset goes through `reboot()`.** On a C5, a bare `software_reset()` before
  `esp_hal::init` has cleared `reset_event_bypass` does not reboot the board, it stops it.
  `esp-hal`'s C5 `pre_init` clears it; both firmwares share the one funnel so every reset takes
  the benched path. → `firmware/common/src/lib.rs`, `reboot`
- **The radio link's core is frozen around the UART, even before 1.0**, so any P4 build can
  identify and update any C6 build and a link change never costs a wired flash.
  → `crates/wartui-proto/src/radio_link/mod.rs` `//!`
- **The node bus's core is frozen, even before 1.0**, so any host and P4 can identify and update
  any node build, and a node never needs a wired flash.
  → `crates/wartui-proto/src/bus/mod.rs` `//!`
- **The P4's I2C0 is shared** by the GPS reader and the carrier's I/O expander, behind one mutex.
  Lock it per transaction, never across an await on anything but the bus, or one stalls the
  other. → `firmware/common/src/gps.rs` `//!`, `firmware/p4-bridge/src/carrier.rs` `//!`
- **A restart of either chip is invisible unless the host compares uptimes**, because the CH343
  stays on the bus through it, so the connection holds, and `Ready.uptime_ms` is the C6's, which the
  P4 resets at its own boot. → `crates/wartui-bridge/src/serial.rs`, the `Ready` arm
- **The P4 writes nothing to the host until a host frame arrives, and queues nothing until then.**
  It cannot see the CH343's DTR, so a decoded frame is the sign that someone is reading, and the
  gate closes 15 s after the last one: three of the engine's 5 s status polls. Frames produced while
  it is closed are dropped, and opening clears whatever is queued before it is served, so a host
  never reads a backlog from before it attached. Bytes already past the queue still reach the port,
  and the host drops every frame that decodes ahead of the connection's first `Ready`.
  → `firmware/p4-bridge/src/host.rs` `//!`, and `src/main.rs`, `Shared::push_host`
- **The node passes `US` for the regulatory domain, and that is not a preference**: `esp-radio`
  defaults to China, which silently costs a C5 channels 100–144 on every sweep.
  → `firmware/node/src/main.rs`, `firmware/node/README.md`
- **Every crate `esp-radio` also depends on is held at the version `esp-radio` resolves.**
  `esp-radio 1.0.0-beta.1` requires `esp-hal ~1.2.0`, which walls off `esp-hal` and `esp-rtos`
  outright. The others — `esp-alloc`, `esp-sync`, `esp-storage`, `esp-wifi-sys-*` — go higher
  perfectly happily, and a higher one is a *second copy* rather than an upgrade: a duplicate
  `__esp_radio_printf` that fat LTO refuses, or a second `esp-alloc` whose `HEAP` nothing fills,
  which builds and then panics on the first task the radio spawns. The node is the firmware that
  links `esp-radio`; Dependabot ignores them and each firmware's CI job counts the versions; a new `esp-radio` moves all of them at once, by hand.
  → `firmware/p4-radio/README.md` § "Dependency versions"
- **Setting a node's channel is not `set_channel` alone** — without promiscuous mode on across the
  change it does not stick, and a node whose channel silently did not change reads as a dead node
  rather than a bug. → `firmware/node/src/radio.rs`, `park`
- **A node's promiscuous callback runs in the Wi-Fi task, on a buffer that dies when it returns.**
  It cannot capture state and must not block. → `firmware/node/src/sniff.rs` `//!`

## Conventions

- Module-level `//!` docs explain *why* the design is the way it is, not what the code does; the
  reasoning in them is often the only record of a hardware constraint. Match that register, and
  update the reasoning when the decision changes.
- Documentation should be clear and concise. Link documents when necessary, do not repeat.
  Exception: `README.md` carries the full keyboard list, settings modal included, and surfaces
  any other key usage an operator needs at a glance. Keep its tables identical to
  `crates/wartui/README.md` § "Keyboard commands", and change both together;
  `crates/wartui/tests/readme.rs` fails when they differ.
- **The wartui-carrier's design stays private.** Its PCB designs and layouts are proprietary.
  The P4's and the C5s' pinouts are public, so others can write code for the boards with this
  repo as the example. Docs, comments, commit messages and PRs may give:
  - pins and pin behavior: polarity, pulls, which lines are shared;
  - bus speeds;
  - the I/O expander's I2C address and its port→node map.

  They never give part numbers, component values, the power circuit, the layout or dimensions.
  The lid fan's model, a Noctua NF-A6x15 5V PWM, may be named: it is the operator's fan, plugged
  into the header, not a part of the board.
- Use American spellings at all times: in prose, doc comments, identifiers, test names and
  commit messages (`meters`, `behavior`, `color`, `recognize`).
- Write short sentences with one idea each. Lead with what the reader does or sees, then the
  reason. Use a table or list when prose would enumerate flags, states, or messages.
- **Prose says what the project does, not where it was.** Nothing before 1.0 is compatible with
  an earlier wartui, so no reader has one to reconcile this build against: "used to", "no
  longer", "previously", and notes that a thing was renamed, moved or replaced cost a reader the
  present tense and buy them nothing. The reasoning survives the chronology — state the
  constraint that rules an alternative out, not the order the two were tried in. Git history
  holds the change and the commit message explains it. Two exemptions: `docs/*-findings.md` are
  dated bench records and say what a bench measured; `README.md` § "History" is provenance and
  attribution, not a changelog.
- Rustfmt is configured with `max_width = 100` and `use_small_heuristics = "Max"`.
- Tests follow the Rust convention: unit tests live in a `#[cfg(test)]` module beside the code
  they test, and tests that exercise a crate through its public API live under
  `crates/*/tests/`. Most of the suite is the latter; `nmea`, `position` and `export` carry unit
  tests of their own.
- Test names follow the `component_action_when_condition` pattern
  (`engine_increments_reboot_counter_when_heartbeat_counter_decreases`):
  - `component` is the unit under test, named the same way across a file (`engine`, `store`,
    `planner`, `beacon_parser`, `fault_lines`), so a prefix filters to it.
  - `action` is what it does, in the present tense (`increments_reboot_counter`, `rejects_frame`).
  - `when_condition` is the input or state that triggers it
    (`when_heartbeat_counter_decreases`).

  The name says what failed without opening the body; the *why* belongs in a comment inside
  the test, not in the name.
- All work happens on a branch and lands through a pull request; nothing is committed directly to
  `main`.
- Commit messages are clear and concise: an imperative one-line subject, then a paragraph explaining
  what was wrong and why the fix is shaped as it is.
