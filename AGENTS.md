# AGENTS.md

Guidance for coding agents working in this repository.

## What this is

A terminal fleet controller for ESP32-C5 wardriving nodes, wired over SPI to an ESP32-P4 bridge
(the node bus), reached over USB through the board's CH343, or over Bluetooth through the board's
C6. The C6 sits on a UART link to the P4, which updates it and reports on it. `README.md`
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
- `docs/wardrive-esp.md` — the WardriveESP service the bridge offers a phone app: what it
  implements and how the fleet maps onto it. `docs/vendor/wardrive-esp/` holds the protocol
  document it is written from, unmodified, with its license and provenance. Never edit that copy.
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
binary on a pty and prints the screen as text, `--keys jc` presses keys first, and `--bridge`
points it at the boards attached rather than the simulator. `README.md` § "Suggested tools" says
when to reach for it over the `TestBackend` tests in `crates/wartui/src/tui/`.

Refer to nodes by their slot label (`N7`), which is printed on the carrier. Use the last two
octets only for bridges, USB-attached boards and nodes with no slot; `wartui ports` names each
attached board by its address, and `crates/wartui/README.md` § "Telling the boards apart" says why
that works.

**Never upload to WDGWars or WiGLE without asking the user directly first, every time.** That
covers `wartui upload`, with or without `--yes` or `--to`, and any request to either site's upload
endpoints (`wdgwars.pl`, `api.wigle.net`), such as curl. An upload publishes to the user's profile
and cannot be taken back. Approval for one upload, or for one site, is not approval for the next.
Tests use a local listener, never the real sites.

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

A change to the node's radio start-up, `src/ble.rs` or an `esp-*` version also needs the fleet
check in `firmware/node/README.md` § "Bench check", which no CI job can run.

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
  `settings` (the records the P4 keeps its fan duty and channel pool in), `bonds` (the Bluetooth
  bonds the P4 keeps for its C6, and their records), `host_owner` (who holds the P4's host queue:
  USB, a Bluetooth central, a phone or no one), `crc`, `beacon`, `hci`, `dedup`, `link`,
  `radio_link` (the UART packets between the P4 and its C6, and their frozen core), `ble` (the
  GATT identity of the C6's Bluetooth host link, the id a host finds a bridge by, the address its
  set advertises from, and the rest of the phones' set after a silent central),
  `wardrive_esp` (the WardriveESP protocol a phone app reads the bridge in: its frames, commands,
  lines and session), `mac` (the `Mac` type and its text form), `plan` (channel pools and the
  round-robin deal that cuts a set of channels), `fleet` (the P4's fleet planner: membership, the
  cut, the Bluetooth pick and the records that tell each node), `node` (a node's timings and
  sweep cursor), `pending` (the buffers sightings wait in on a node), `sd` (the FAT32 layout the
  P4 formats its microSD card with, and the reading of a card it did not format),
  `ota` (the update receiver a node and both halves of the P4 bridge share, `ota::trial`, the
  record of an image on trial, and `ota::image`, the digest that names an image).
  Compiled into *both* the host and the firmware by path dependency, which is the only thing
  keeping the ends in step — and the reason a node's parsers are testable with `cargo test`
  rather than a reflash, and the P4's planner testable without one.
- **`crates/wartui-bridge`** — host side of the link. Everything above talks to a `LinkHandle`
  and cannot tell a real bridge, over USB (`serial`) or Bluetooth (`ble`, btleplug), from the fake
  fleet (`sim`). The two transports differ only where their links do; what a connection admits
  from the bridge's stream is one rule both read through (`gate`), and `bluez` is the Linux
  pairing `wartui bluetooth pair` drives (`src/lib.rs` `//!`). `wartui`'s `main.rs::open` is the
  one place a link is picked. It also owns the host's serial ports generally (`ports`): what is
  attached and what the OS says it is, so anything else that opens a device shares one
  enumeration.
- **`crates/wartui-core`** — the headless half. Draws nothing, parses no arguments.
- **`crates/wartui`** — clap CLI (`run`/`export`/`analyze`/`upload`/`sniff`/`status`/`reset`/
  `power-cycle`/`fan`/`pool`/`scan`/`ports`/`flash-fleet`/`flash-bridge`/`ota`/`bluetooth`) and the
  ratatui view.
- **`firmware/p4-bridge`** — the bridge, on the P4 of a Waveshare ESP32-P4-WIFI6: COBS framing over
  a DMA UART to the host, through the board's CH343, the node bus's two SPI controllers, and a FIFO
  UART with RTS/CTS to its C6. It plans the fleet, so a board scans with no host attached: it
  reads heartbeats, decides what every node scans and sends every `Assign` and `Stop`
  (`src/fleet.rs`). The planner itself is `wartui_proto::fleet`, because a fix in firmware costs
  a reflash and a fix there is tested with `cargo test` first. It forwards every node record to
  the host, heartbeats included. It reads heartbeats, and sighting batches while a phone is
  linked, and no other kind. Its image carries the C6's, and
  updates it. It caches a node image in PSRAM for `wartui ota`, and builds `UpdateChunk`s from it,
  a layout in the bus's frozen core (`src/cache.rs`). On a carrier it drives the lid fan
  (`src/fan.rs`). It keeps the fan's duty, the channel pool and the `wardrive esp` setting in the `nvs`
  partition, the settings the bridge rather than the host holds (`src/settings.rs`); the scan mode is kept
  nowhere, so it boots stopped. A bonded central over Bluetooth, through the C6, can be its host
  instead of USB (`src/ble.rs`): it keeps the C6's bonds in `nvs` too (`src/bonds.rs`), and the host
  bulk ring in PSRAM, past the node image cache. With `wardrive esp` on, a phone on the C6's
  WardriveESP service can hold the queue instead: the P4 runs its session, scans for it and
  sends it text lines (`src/wardrive_esp.rs`). It reports the board's microSD card in `Status` and
  formats it FAT32 on a host's `FormatSd` (`src/sd.rs`), the layout `wartui_proto::sd`'s.
- **`firmware/p4-radio`** — the board's C6, behind a DMA UART: its hello, its status and its updates.
  It runs the Bluetooth host link, a TrouBLE GATT peripheral that only a bonded central reaches
  (`src/ble.rs`), and beside it the open WardriveESP service, which it frames for a phone while
  the P4 admits one. Each service advertises as its own set.
- **`firmware/node`** — the nodes. Sniffs rather than scans, so it never transmits while looking,
  and owns the channel (`firmware/node/README.md` § "What it does differently"). Hops from one
  assigned channel to the next, reporting each dwell over the node bus, and idles servicing the
  bus when it holds no assignment: from boot, and after a `Stop`. The P4 sends both. The node
  given the Bluetooth scan sniffs nothing and runs one scan after another, back to back. Same rule
  as the bridge: the logic lives in `wartui-proto`, and this crate is the conversation with the
  radio and the bus.
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
detection, adoption against the P4's reported plan and the run's scan start and restart rules
testable in microseconds against a clock the test invents (`crates/wartui-core/tests/engine.rs`).
Do not reach for `Instant::now()`, I/O or `async` inside `engine`. An over-the-air update is split
the same way: `ota::OtaSession` is the pure state machine and `runtime::drive_fleet` its driver,
running every node's session at once (`crates/wartui-core/tests/ota.rs`). So is the P4's planner:
`wartui_proto::fleet::Fleet` is pure, `firmware/p4-bridge/src/fleet.rs` drives it, and
`crates/wartui-proto/tests/fleet.rs` tests it.

Operator keypresses are `Event::Command`, not methods — a keypress and a heartbeat have to be
ordered against each other. Only `c`, `p` and the waiting-for-GPS pop-up's `Enter` get that
far, beside the `StopScan` a quit sends; channels are the P4's.

### Store is the system of record

`store` is SQLite behind one owner thread with batched transactions and a bounded queue that
**drops rather than blocks** (a stalled engine misses everything, the bridge's reports and every
node's heartbeats included). A capture file holds one run: `Store::create` refuses a path that
exists. `export` (WiGLE CSV) is a view over the store, re-runnable against a finished or
still-running capture. Each bridge status is a `bridge_status` row, with a `bridge_queue` row per
queue and a `bridge_bluetooth` row for its Bluetooth host link: owner, bonds, pairing window,
central and the C6's counters, kept over either link. `capture.link` is the link the host used,
written once at creation (`store.rs` `//!`). `SCHEMA` changes shape as freely as the work needs.
Each change bumps `store::SCHEMA_VERSION`, and a capture stamped anything else is refused rather
than migrated.

Positions resolve fresh per record through `PositionChain`: a host GPS pinned with `--gps` → the
bridge's GPS (its bytes fed in by `runtime::drive`, never the engine) → a host GPS found by
`discover` (NMEA on its own thread, standing down while the bridge's receiver is sending
sentences) → static `--lat`/`--lon` (test only: it marks the capture as test data, which is never
uploaded) → nothing.
Which tier answered is stored per row.

## Invariants that are easy to break

These bite from a distance — from a file other than the one that owns them — so the reasoning
is here rather than only in a `//!`.

- **One wire is not ours: the WardriveESP service.** The C6 offers it to a phone while
  `wardrive esp` is on. It carries text lines in the vendored protocol's frames, written from
  `docs/vendor/wardrive-esp/PROTOCOL.md`, and shares no path with host frames or the node bus: a
  phone's command arrives as its own radio-link record and never reaches the host frame decoder.
  → `crates/wartui-proto/src/wardrive_esp/mod.rs` `//!`, `docs/wardrive-esp.md`
- **The wire is ours, in both directions, and shares nothing with the vendor's.** Every node-bus
  transfer is a fixed 1,044 bytes with a magic, `bus::BUS_VERSION` and a CRC, carrying records of
  a type byte, a length and a body. The type byte is the message, numbered per direction: up,
  `Heartbeat` (27 bytes, its prefix and the node's MAC first), `Sightings` (3 plus records of 12
  plus the SSID and the trailer, up to 1024) and `UpdateStatus` (6); down, `Assign` (8), `Stop`
  (empty), and `UpdateBegin` (41), `UpdateChunk` (6 to 1029), `UpdateCommit` (1), `UpdateAbort` (1)
  and `UpdateConfirm` (empty). The P4 checks the framing and passes every node record to the host
  as `FromNode`, heartbeats included, and every host `ToNode` to its node. It reads heartbeats for
  its planner, authors every `Assign` and `Stop`, and builds the `UpdateChunk`s from its node image
  cache; the host sends a node only an update's other records. Encode/decode is written out by
  hand, never by
  transmuting a packed struct, and pinned byte-for-byte in `crates/wartui-proto/tests/bus.rs` and
  `tests/wire.rs`. Link frames are up to 1152 bytes each way. `docs/node-bus.md` has the table.
- **A wire change means updating every node at once.** `wartui-proto` is a path dependency of every
  firmware. A record whose body is not its type's length is counted as `incompatible` and named in
  the footer (`N frames from an older firmware — update`), never admitted to the node table and
  never half-decoded. A heartbeat whose prefix names another major or minor is the exception: its
  node is shown as `needs update` (older) or `host behind` (newer), and the P4 leaves it out of
  its plan and sends it nothing. Once its heartbeat has named that version, its sighting batches
  are counted `incompatible` and not recorded. The P4 decodes heartbeats and sighting batches and encodes `Assign` and
  `Stop`, so a change to those four layouts costs a P4 reflash too (`wartui flash-bridge`, from
  the same tree); any other node message passes through it unread and does not.
  `wartui ota` updates the nodes over the bus.
- **`link::LINK_PROTO_VERSION` does not move before 1.0 either**: there is no older peer for it to
  protect when the policy is to flash both ends from one tree. The cost is that a host and a bridge built from different
  trees may meet as an undecodable frame rather than a named mismatch, which is stated where the
  constant is.
- **The sighting trailer means what the kind says it means, and the wire layer never interprets
  it.** For Wi-Fi it is the roaming consortium element's body verbatim; for BLE it is a flags
  byte (random address, connectable), then two bytes of company identifier when the advertiser
  sent one. The split is made once, in the engine's
  sightings path, per record; keeping `bus` byte-transparent over the trailer is what lets a change
  to how an identifier is *read* cost a re-export rather than a reflash.
  → `crates/wartui-proto/src/bus/sighting.rs`, `SightingMsg::ext`;
  `crates/wartui-proto/src/beacon.rs`, `rcoi_text`; `crates/wartui-proto/src/hci.rs`,
  `BleTrailer`; `crates/wartui-core/src/engine/rx.rs`
- **Nothing here is compatible with an earlier wartui, and that is the policy until 1.0.** No
  migration path is built for a fleet mid-upgrade and no code reads an older wire format to be
  helpful about it. Nothing decodes or drives a node on another major or minor: the P4 never
  assigns it and the host never records its sightings. Both identify it by the frozen heartbeat
  identity, and `wartui ota` updates it, so update the fleet together. The node
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
- **The P4's planner is the only author of an assignment.** There is no operator override, no
  per-node mode and no flag: `wartui_proto::fleet::Fleet` decides what every node scans and sends
  every `Assign` and `Stop`, and the host authors no node record but an update's. That is what
  lets the fleet table and the store read a node's share as the plan's without asking who put it
  there. It has three inputs, all the bridge's: the pool, kept in NVS and set by `SetPool`; the
  scan mode, set by `StartScan { bluetooth }` and `StopScan`, or by a linked phone's session,
  in Wi-Fi or Bluetooth mode, and kept nowhere; and a linked phone's channel list, which outranks
  the pool while the phone holds it and is kept nowhere. Which node scans
  Bluetooth is its pick, not an input. The host's `Command::SetPool`, `SetBluetooth` and
  `StopScan` only ask, and the engine believes what the next `Status` or `Fleet` report says.
  → `crates/wartui-proto/src/fleet.rs` `//!`; `crates/wartui-core/src/engine/scan.rs` `//!`
- **Only heartbeating nodes are assignable or in the plan.** The P4 plans by slot: a member is a
  present slot that has heartbeated within `fleet::HEARTBEAT_TIMEOUT_MS` (20 s), and its
  assignment goes to that slot. A slot the P4 counts absent leaves the plan at once, until its
  next heartbeat. The timeout is the backstop for a node that answers the bus but has stopped
  heartbeating. The host mirrors the rule for the view with the same 20 s, dropping a node on the
  first `Status` that shows its slot absent (`crates/wartui-core/src/engine/mod.rs`, `on_bus`,
  `is_assignable`). `stale`, `off bus`, `no heartbeat` and
  silence are deliberately distinct states, and `SCAN_CHANNELS` order is load-bearing: never sort
  or deduplicate it, because those indices *are* the wire format.
- **A node is only in the plan once it has said what it is.** The P4 makes a slot a member only
  on a heartbeat that decodes and speaks `bus::NODE_VERSION` (its major and minor). A body that
  does not decode is read for its MAC alone and leaves the slot out at once, and a heartbeat from
  another MAC than the slot's last starts the slot afresh. `FleetEngine::is_assignable` mirrors
  this for the view, and also refuses a node whose heartbeats carry no `bus::Capabilities` yet:
  `None` means only that nothing but a sighting has been heard from that address — an ordinary
  few seconds in the life of a node about to be perfectly drivable. A share cut for a node that
  has not said what it is is a share that may be nobody's.
- **Fourteen nodes maximum** (`plan::MAX_NODES`), the node bus's slots (`bus::MAX_SLOTS`): seven
  per bus on the 14-node carrier, and none without a carrier (`bus::NodeBoard`). The P4 plans by
  slot (`bus::Slot`, `A1` to `B7`). The host knows a node by its MAC, which its heartbeat carries,
  and finds its share at the slot that heartbeat came from. A `Status` names each member slot's
  MAC too, and fills only a slot nothing has named (`link::BridgeToHost::Status`, `members`).
  The operator reads a slot through its board's label, `N1` up on a carrier, and the store keeps
  the bus name.
- **Delivery is the P4's, and delivery is not adoption.** The planner takes a send as reaching
  its node only on the bus ack of the transfer that carried it, never on a successful enqueue.
  Every heartbeat carries the epoch the node holds, or 0 for none. A node adopts an assignment
  only when its epoch differs from the one it holds, so re-sending an identical one is
  acknowledged and silently discarded. If a node's heartbeat reports an epoch other than its want
  while nothing is in flight, the record did not land after all, and the planner re-sends it
  under the same epoch on that heartbeat rather than waiting. The planner never deals a node the
  epoch its heartbeat says it already holds (`fleet::next_epoch`), because a P4 that reboots under
  running nodes restarts its counter while they keep theirs. The host sees no delivery of the P4's
  sends: it reads a node as holding its share when its latest live heartbeat carries the epoch
  the last `Fleet` report gives that slot.
  → `crates/wartui-proto/src/fleet.rs` `//!`, `next_epoch`;
  `crates/wartui-core/src/engine/snapshot.rs`, `NodeView::adopted`
- **`IndexRun` describes a *pool*, never the shape of an assignment.** An assignment is a
  `ChannelSet` naming any subset of `SCAN_CHANNELS`, so `plan_for` deals a `ChannelSet`, the
  pool's channels or a phone's list, round-robin from one order (1, 6 and 11 first) rather than
  steering around run boundaries. The plan has no phases and no timer; it changes when
  membership, the scan mode, the Bluetooth pick, the pool or a phone's list changes, and at no
  other time (`Fleet::settle`).
- **An updated node image is confirmed only by hearing the bridge.** The first record from the P4 a
  trial image decodes — assign, stop, update confirm or an update record — marks it valid; nothing
  else does, because hearing the bridge is the whole test. A stopped P4 sends an idle node
  nothing, so on a stopped fleet `wartui ota`'s `UpdateConfirm` or the next scan's `Assign` is what
  confirms a trial image. Rollback is the node app's
  job, since the prebuilt espflash bootloader has none and reads no image state: the third
  unconfirmed boot returns `otadata` to the other slot. The `trial` flash sector, not `otadata`,
  records which slot is on trial and how many times it has booted, one cleared bit per boot,
  because power cycles are routine on these nodes and RTC memory does not survive them. With no
  watchdog, those power cycles are also what rolls back a hung image. Every step's flash writes
  are ordered so a power cut cannot leave an image running untrialed. The P4 bridge's C6 shares
  the trial: its image is confirmed once it has heard the P4's hello and its Bluetooth host has
  started advertising, one that has not advertised 30 s after boot restarts itself, and that
  restart and the P4's resets count its boots. So does the P4: the first host frame it decodes confirms its image, which a phone's command is not, and a trial boot arms its RTC watchdog,
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
- **A connection starts at its first `Ready`, on both links, and asks with `Identify` until one
  answers.** What decodes ahead of it was written before the connection and is dropped
  (`gate`). Over USB a connection asks a CH343 every 500 ms, and any other port once. A CH343
  drains its UART, and a P4 booting behind it loses the first `Identify`. A path named with
  `--bridge` that is not a listed CH343 may be a node, and a node that is not reading its USB
  endpoint absorbs exactly one packet and NACKs the rest, which sit in the tty's output queue
  through a `close` that no signal can interrupt — 30 s measured against a node, during which the
  process cannot exit. The transport leaves a thread held in `write` or `close` behind after 2 s,
  and `main` names the port it holds. Once proven, a USB connection that has written nothing for
  10 s sends a `GetStatus`, since the P4 stops writing to a host it has not heard from in 15 s.
  Over Bluetooth
  a connection asks every second once its notifications are open, because a bonded central's
  subscription outlives its connection and the bridge's `Ready` can go by before the transport
  listens, and it sends no keepalive, since that gate never closes on silence.
  → `crates/wartui-bridge/src/lib.rs` `//!`, `src/gate.rs` `//!`; `src/serial.rs`, `connect`'s
  `identify` arm, `KEEPALIVE` and `PORT_CLOSE_WITHIN`; `src/ble.rs` `//!`, `IDENTIFY_EVERY`
- **Never set DTR or RTS on a serial port, or ask `serialport` to.** The kernel raises both
  together on open, which a node's USB Serial/JTAG ignores and the P4 board's CH343 carries to a
  real auto-reset circuit on the P4's EN and GPIO35, where both together are inert too. Moving one
  without the other is the reset sequence, so `.dtr_on_open(false)` reboots the board it was being
  polite to. → `crates/wartui-bridge/src/ports.rs` `//!`
- **The planner deals only to a slot that is sniffing**, and what no slot can take comes back in
  `Plan::unreachable` rather than being dealt anyway. A `Job::Bluetooth` slot takes nothing, so a
  fleet whose one member held the scan would report the whole pool, and the P4 gives Bluetooth to
  no member below two.
  → `crates/wartui-proto/src/plan/mod.rs`, `plan_for`; `crates/wartui/README.md` § "Channel pools"
- **Channel 14 is in no pool and is never dealt**, and stays in the scan table because that table's
  indices are the wire format. → `crates/wartui-proto/src/plan/channels.rs`, `UNSUPPORTED_INDEX`
- **At most one node scans Bluetooth, by default none does, and it is that node's whole job.**
  `ASSIGN_FLAG_BLE` is never a property of the flashed firmware. Whether a node scans Bluetooth is
  the operator's choice of scan mode (the settings' `bluetooth` row,
  `wartui scan start --bluetooth`); which node is the P4's pick: none below two members, so a lone
  node keeps Wi-Fi, the current pick while it stays a member, and else the highest member slot.
  The node holding it is dealt no channels and is counted in `Plan::node_count` anyway, because
  that reports the fleet size the plan was cut for rather than a census of who is sniffing. An
  empty `ChannelSet` is sent only with the flag beside it; without it, a node told to scan nothing
  parks while the fleet believes it is sweeping, and `Plan::assign_for` refuses to build that
  frame.
  → `crates/wartui-proto/src/plan/mod.rs`, `Job` / `Plan::channels_for` / `assign_for`;
  `crates/wartui-proto/src/fleet.rs`, `pick_bluetooth`; `docs/phase-2-findings.md`
- **A heartbeat read behind the bridge is history.** A host that falls behind a busy fleet drains
  the bridge's queue late; the bridge's clock stamps run ahead of the host's, and the gap is the
  lag. How much lag counts as behind depends on the link: Bluetooth clumps delivery by its
  connection interval, so its line is wider than USB's. Such a heartbeat's epoch is not believed:
  it does not set what the node holds, so the view never shows a node adopting from one, and its
  epoch 0 never reads as a restart. A falling counter in it still does. Delete the check and the
  symptom is a plausible-looking lie rather than an error.
  A connection starts caught up, which is safe only because nothing older than it reaches the
  engine: a bridge clears its queue when a host connects, and the transport drops what decodes
  ahead of a connection's first `Ready`.
  → `crates/wartui-core/src/engine/lag.rs`, `note_arrival` / `reading_live` / `behind_us`;
  `crates/wartui-core/src/engine/rx.rs`, `on_heartbeat`; `crates/wartui-bridge/src/gate.rs`,
  which both transports read through
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
- **The P4's core 1 stays off.** Every future, the microSD card's included, is joined on core 0,
  and nothing calls `start_second_core`. esp-storage stalls the other core for every flash write,
  wherever it is, so a core 1 frozen holding a lock leaves core 0 spinning on it: on the bench,
  `flash-bridge` updates restarted or hung the P4. → `firmware/p4-bridge/src/sd.rs` `//!`,
  `docs/p4-sd-findings.md`
- **The P4's I2C0 is shared** by the GPS reader and the carrier's I/O expander, behind one mutex.
  Lock it per transaction, never across an await on anything but the bus, or one stalls the
  other. → `firmware/common/src/gps.rs` `//!`, `firmware/p4-bridge/src/carrier.rs` `//!`
- **Over USB, a restart of either chip is invisible unless the host compares uptimes**, because
  the CH343 stays on the bus through it, so the connection holds, and `Ready.uptime_ms` is the
  C6's, which the P4 resets at its own boot. Both transports read each `Ready` through the same
  gate, so a new life under a held connection is reported on either; over Bluetooth a restart of
  either chip also drops the link. → `crates/wartui-bridge/src/gate.rs` `//!`, `is_a_new_life`
- **The P4 writes nothing to a host until one attaches, and queues nothing until then.** A USB
  host attaches with a decoded frame, since the P4 cannot see the CH343's DTR, and its gate closes
  15 s after the last one: three of the engine's 5 s status polls. A Bluetooth central attaches by
  subscribing on an encrypted, bonded link, and lets go when it unsubscribes, disconnects or the C6
  restarts, never on silence. A phone attaches by subscribing to the WardriveESP stream while
  `wardrive esp` is on and USB does not hold the queue. It needs no bond, is sent lines and never
  frames, and its release stops the scan. Frames produced while no host is attached are dropped, and attaching
  clears whatever is queued before it is served, so a host never reads a backlog from before it
  attached. Bytes already past the queue still reach the port, and the host drops every frame that
  decodes ahead of the connection's first `Ready`. The fleet planner runs regardless: a P4 with no
  host keeps scanning, its reports are dropped, and the announcement that attaches a host carries
  the current plan.
  → `firmware/p4-bridge/src/host.rs` `//!`, `src/ble.rs` `//!`, and `src/main.rs`,
  `Shared::push_host`
- **One host owns the P4's host queue, and an `Identify` never takes it from a Bluetooth host or a
  phone.** There is one stream and one credit window, so the queue goes to USB, to a central or to
  a phone, never two. A UART0 frame takes it from a phone with no notice, and the C6 admits no
  phone until UART0 has been silent 15 s.
  Any other UART0 frame takes it, so `flash-bridge` over USB always gets in, and the central is
  told why. An `Identify` while a central owns it gets a `Ready` written straight to UART0: `wartui
  ports`, `--bridge` lookups and a probe from another PC all send one, and taking the queue for
  each dropped what was queued for the central. Once UART0 falls silent for 15 s, a central still
  subscribed takes the queue back.
  → `crates/wartui-proto/src/host_owner.rs` `//!`, `firmware/p4-bridge/src/ble.rs`
- **Only an encrypted central holding a stored bond subscribes to or writes the wartui link.** The
  attribute table requires encryption, but Just Works gives an unbonded link the same level, so
  the C6 checks the bond itself before a write is forwarded or a subscription counts, and drops a
  central that cannot bond, whose bond the P4 dropped while it was connected, or that reads no
  stream for 30 s. With `wardrive esp` on, any central subscribes to the WardriveESP stream and
  writes its commands, which reach only the phone's session. A central reads one stream, the
  first it subscribes to, and one central connects at a time, so a linked phone keeps a Bluetooth
  host out and USB always gets in. The C6 sends no Security Request, which a phone would answer
  with a pairing prompt: a host starts its own pairing. A central that, 3 s after connecting, has
  asked nothing of either service, has neither paired nor encrypted, and inside the pairing window
  has exchanged no MTU is silent and dropped, and while phones are admitted the phones' set rests
  after it, up to `ble::REST_MAX_MS`.
  → `firmware/p4-radio/src/ble.rs` `//!`, `authorized`, `revoke`; `crates/wartui-proto/src/ble.rs`,
  `Rest`
- **The C6's host stream credit is an absolute stream position, not a sum.** It is the offset, in
  the P4's own offsets, up to which the C6 is done, so every `HostStart` and `HostBytes`
  resynchronizes it and a restart of either chip cannot skew it. A sum kept from the C6's boot
  would leave the P4 holding more credit than it sent after a restart, and it would never send
  again. A lost packet is credited as though sent, and costs the central its frame, or a phone
  its line (`resynced_bytes`).
  → `crates/wartui-proto/src/radio_link/inflow.rs` `//!`
- **A central's write group ends with a write request**, and its earlier pieces are writes without
  response. TrouBLE queues 8 writes per connection and drops the rest, and the C6 answers the
  request only once it has forwarded that piece, so a central has one group outstanding at most,
  a write short of that queue. A group carries the frames queued together, since Down is a byte
  stream. A request whose write the C6 dropped is refused, and the transport ends the connection.
  → `crates/wartui-bridge/src/ble.rs` `//!` § "Writes", `firmware/p4-radio/src/ble.rs` `//!`
- **The C6 serves an ATT MTU of `ble::ATT_MTU` (182)**, so a write toward the bridge carries at
  most `ble::DOWN_VALUE_MAX` (179) value bytes. esp-radio truncates a packet its controller hands
  over as a chain, a longer write can arrive as one, and the truncation stops the C6's Bluetooth
  host. The size is TrouBLE's packet pool, set in `firmware/p4-radio/.cargo/config.toml`, so the C6
  builds from that directory, and `src/ble.rs` fails a build whose pool disagrees. wartui's
  transport caps its writes at 179 whatever the MTU, so it updates a C6 that serves more.
  → `crates/wartui-proto/src/ble.rs`, `ATT_MTU`
- **A host matches the bridge's advertised id, never its name or address, and the wartui set
  advertises from its own static random address.** A phone that has linked reconnects by address,
  so on a shared address it lands on the wartui set and holds the C6's one connection. The set
  that pairs is the one on TrouBLE's stack address, so the phones' set and the status beacon name
  the public address in their parameters. → `crates/wartui-proto/src/ble.rs` `//!`,
  `link_address`; `firmware/p4-radio/src/ble.rs` `//!`
- **Every C6 advertisement goes through the extended advertising commands, and the C6 stops every
  set itself when a central connects.** A controller that has taken an extended command refuses
  the legacy ones. TrouBLE stops no set on a connection, and a set left enabled ends its runner at
  the next resolving-list load, which restarts the C6. → `firmware/p4-radio/src/ble.rs` `//!`,
  `stop_sets`
- **The C6's `trouble-host` is the vendored 0.8.0 with one upstream commit, the move to bt-hci
  0.11, and takes no other change.** The release builds on bt-hci 0.10, whose encoder and
  parameter-request replies are wrong. The C6 names `bt-hci` itself, held at the version
  `trouble-host` resolves, and the copy goes when a release builds on 0.11.
  → `firmware/p4-radio/vendor/trouble-host/VENDORED.md`, `firmware/p4-radio/README.md`
  § "Dependency versions"
- **Where the Bluetooth code picks a protocol, it matches every `Stream`, `Holder` or
  `HostOwner`.** An `if phone … else host` gives a third stream the host's arm with no compile
  error. → `crates/wartui-proto/src/radio_link/message.rs`, `Stream::terminator`;
  `crates/wartui-proto/src/host_owner.rs`, `Holder::through_c6`
- **A Bluetooth link is closed before the process exits.** The bridge serves a central for as
  long as it stays subscribed, and a process that exits with the link up leaves BlueZ connected
  to it. `close_all` waits for every Bluetooth transport before `main` returns, so no command path
  has to remember to. → `crates/wartui-bridge/src/lib.rs` `//!`, `close_all`
- **The C6 restarts itself when its Bluetooth host stops, and the P4 keeps its last panic line.**
  A stopped TrouBLE host cannot be started again in place: that resets the controller with no
  disconnection event, and the C6 never advertises again. The C6 panics, and its panic handler
  keeps the reason for the next boot to report. That report reaches a host that has usually gone,
  so the P4 keeps the line and repeats it to every host that attaches.
  → `firmware/p4-radio/src/ble.rs`, `run_host`; `firmware/p4-radio/src/last_panic.rs`;
  `firmware/p4-bridge/src/link.rs`, `relay`, and `src/main.rs`, `Shared::announce`
- **Updates follow the link, and a failed one over Bluetooth is recovered over USB.**
  `flash-bridge` and `ota` pick theirs as every command does (`link_for`). Over Bluetooth the P4's
  reboot and its C6's update each drop the link, so `flash-bridge` reads its outcome across the
  reconnects, and nothing resumes a partly sent image.
  → `crates/wartui/src/flash/p4.rs` `//!`
- **The P4 pumps its planner after every input.** Each heartbeat, presence change, host command
  and tick is followed at once by pulling every record the core owes into its downlink. A
  `StopScan` pumped only after the `StartScan` behind it loses its `Stop` to the new `Assign`, and
  the nodes keep their dedup rings across the restart. A delivery outcome, answered inside a
  borrow of the downlinks, only wakes the driver to pump once the borrow is released.
  → `firmware/p4-bridge/src/fleet.rs` `//!`
- **Send ids split at `fleet::BRIDGE_ID` (`0x8000`).** The P4's own sends carry that bit and their
  outcomes go to its planner; the host's stay below it and their outcomes go to the host. A host
  id with the bit set would be answered to the planner, and the host's update would never hear.
  → `firmware/p4-bridge/src/fleet.rs`, `answer`; `crates/wartui-core/src/ota.rs`, `Ids::take`
- **A stop drains no downlink.** Draining would discard an update's queued records with the
  planner's, and delivery toward a node is strictly in order, so a `Stop` lands after any `Assign`
  queued ahead of it. → `firmware/p4-bridge/src/fleet.rs` `//!`
- **A run stops a P4 found scanning before it starts the scan**, and starts only once the P4
  reports stopped, so each capture begins with every node's dedup ring empty: a start that reached
  the P4 first would replace the `Stop`. A run that waits for a GPS fix still stops at once, and
  holds only the start, until a receiver has held a fix for `FIX_WAIT` or the operator presses
  `Enter`. After that the run restarts a P4 found stopped at once, which is how a P4 that resets
  mid-run comes back. → `crates/wartui-core/src/engine/scan.rs` `//!`, `Startup`
- **The node passes `US` for the regulatory domain, and that is not a preference**: `esp-radio`
  defaults to China, which silently costs a C5 channels 100–144 on every sweep.
  → `firmware/node/src/main.rs`, `firmware/node/README.md`
- **Every crate `esp-radio` also depends on is held at the version `esp-radio` resolves.**
  `esp-radio 1.0.0-beta.1` requires `esp-hal ~1.2.0`, which walls off `esp-hal` and `esp-rtos`
  outright. The others — `esp-alloc`, `esp-sync`, `esp-storage`, `esp-wifi-sys-*` — go higher
  perfectly happily, and a higher one is a *second copy* rather than an upgrade: a duplicate
  `__esp_radio_printf` that fat LTO refuses, or a second `esp-alloc` whose `HEAP` nothing fills,
  which builds and then panics on the first task the radio spawns. The node and the C6 are the firmwares
  that link `esp-radio`; Dependabot ignores them and each firmware's CI job counts the versions; a new `esp-radio` moves all of them at once, by hand.
  → `firmware/p4-radio/README.md` § "Dependency versions"
- **`colorgrad` is held at the version `tui-bar-graph` resolves.** `BarGraph::with_gradient` takes
  that copy's `Gradient` trait, so a higher `colorgrad` is a second copy whose gradients do not
  satisfy it, and `chart.rs` stops compiling (E0277). Dependabot ignores its minor and major
  releases; a new `tui-bar-graph` moves both at once, by hand. → `.github/dependabot.yml`
- **Setting a node's channel is not `set_channel` alone** — without promiscuous mode on across the
  change it does not stick, and a node whose channel silently did not change reads as a dead node
  rather than a bug. → `firmware/node/src/radio.rs`, `park`
- **A node turns off Wi-Fi's disconnected-station power management**, because the radio Bluetooth
  shares sleeps without it, and a Bluetooth scan succeeds at every command and hears nothing.
  → `firmware/node/src/main.rs`, `firmware/node/src/ble.rs` `//!`, `docs/bluetooth-scan-findings.md`
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
- **No full real MAC address** in docs, comments, tests, commit messages or PRs, quoted output
  included. Name a board by its last two octets (`96:C4`), the form `--bridge` takes, and shorten
  a quoted line to them (`…:6A:4C on Esp32S3`). Where a whole address is needed, as in a test, use
  a locally administered one (`02:00:5E:10:44:C0`). Broadcast is fine.
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
