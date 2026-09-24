# AGENTS.md

Guidance for coding agents working in this repository.

## What this is

A terminal fleet controller for ESP32-C5 and ESP32-C6 wardriving nodes, speaking ESP-NOW
through a USB-attached ESP32 dongle. `README.md` introduces it and says which chip goes
where.

Where the prose lives:

- `README.md` — the front door, and stays short.
- `crates/wartui/README.md` — the operator's manual: keys, fleet states, channel pools,
  GPS, troubleshooting. Read it before changing view or CLI behaviour, and **keep it true
  when that behaviour changes.**
- `firmware/*/README.md` — each firmware's own constraints, including the esp-hal version
  wall (`firmware/bridge/README.md`).
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
when to reach for it over the `TestBackend` tests in `crates/wartui/src/tui.rs`.

Refer to boards by their last two octets; `wartui ports` names each attached board by its
address, and `crates/wartui/README.md` § "Telling the boards apart" says why that works.

Each firmware is a **separate workspace** (`exclude = ["firmware"]`): different target, own
toolchain pin, own lockfile. `cargo test --workspace` never touches them.

```sh
cd firmware/bridge
cargo fmt --all -- --check                    # its own workspace, so the root `cargo fmt` misses it
cargo clippy --release --features esp32c6     # and --features esp32c5; exactly one is required
cargo run --release --features esp32c6        # runner is `espflash flash --monitor`

cd firmware/node
cargo fmt --all -- --check
cargo clippy --release --features esp32c6     # and esp32c5, and each with ,ble
```

CI mirrors the workspace split: host crates on every push and PR; each firmware only
when something it compiles changes. It runs `fmt` and `clippy` in each firmware
directory, so a `wartui-proto` change that reaches the firmware is checked there too.

## Architecture

Four host crates, strictly layered, plus firmware that shares the bottom one.

- **`crates/wartui-proto`** — `no_std`, allocation-free wire formats and the parsing that goes
  with them: `air` (the three ESP-NOW frames, and `air::foreign` for recognising somebody
  else's), `beacon`, `hci`, `dedup`, `link`, `outbox`, `stall`, `plan` (channel pools, timings
  and the partitioning planner). Compiled into *both* the host and the firmware by path
  dependency, which is the only thing keeping the ends in step — and the reason a node's
  parsers are testable with `cargo test` rather than a reflash.
- **`crates/wartui-bridge`** — host side of the USB link. Everything above talks to a `LinkHandle`
  and cannot tell a real dongle (`serial`) from the fake fleet (`sim`). It also owns the host's
  serial ports generally (`ports`): what is attached and what the OS says it is, with no judgement
  about which of them is a bridge, so anything else that opens a device shares one enumeration.
- **`crates/wartui-core`** — the headless half. Draws nothing, parses no arguments.
- **`crates/wartui`** — clap CLI (`run`/`export`/`sniff`/`status`/`reset`/`ports`) and the ratatui
  view.
- **`firmware/bridge`** — dumb radio bridge: COBS framing and `esp-radio` calls, no protocol
  knowledge. Fixes there cost a reflash, so logic belongs on the host. A board with a screen
  draws lines the host composed and hands it, which is the same rule seen from the other side.
- **`firmware/node`** — the nodes. Sniffs rather than scans, so it never transmits while
  looking and can hold `sniffer()` and `esp_now()` at once (both borrow the controller
  immutably; `scan_async` wants `&mut`). Returns to the control channel after every dwell,
  and parks there doing nothing when it holds no assignment. The node given the Bluetooth
  scan never leaves the control channel at all: it sniffs nothing and its whole cycle is
  one scan per `plan::BLE_BEAT_MS`. Same rule as the bridge: the logic lives in
  `wartui-proto`, and this crate is the conversation with the radio.

### The engine/runtime split is load-bearing

`wartui_core::engine::FleetEngine::handle` is a **pure synchronous state machine**: `Event` in,
`ActionBatch` out. It reads no clock (time arrives as `Now`, carrying both a monotonic `Instant`
and unix millis), touches no socket, opens no file. `wartui_core::runtime::drive` is the only
place that reads a clock and performs actions. This is what makes liveness ageing, reboot
detection, assignment timing and auto-partitioning testable in microseconds against a clock the
test invents (`crates/wartui-core/tests/engine.rs`). Do not reach for `Instant::now()`, I/O or
`async` inside `engine`.

Operator keypresses are `Event::Command`, not methods — a keypress and a heartbeat have to be
ordered against each other. Only `b` gets that far; channels are the planner's.

### Store is the system of record

`store` is SQLite behind one owner thread with batched transactions and a bounded queue that
**drops rather than blocks** (a stalled engine misses everything, including an assignment racing
a 100 ms window). `export` (WiGLE CSV) is a view over the store, re-runnable against a finished
or still-running session. `SCHEMA` changes shape as freely as the work needs;
`store::SCHEMA_VERSION` stays at 1 until 1.0, and a capture stamped anything else is refused
rather than migrated.

Positions resolve fresh per record through `PositionChain`: GPS (found by `discover`, or pinned with
`--gps`; NMEA on its own thread) → static `--lat`/`--lon` → nothing. Which tier answered is stored
per row.

## Invariants that are easy to break

These bite from a distance — from a file other than the one that owns them — so the reasoning
is here rather than only in a `//!`.

- **The wire is ours, in both directions, and shares nothing with the vendor's.** Every frame is
  `WTUI`, a wire version byte, a type byte and a body: `HeartbeatMsg` (13 bytes), `SightingMsg`
  (18 plus the SSID and a length-prefixed trailer) and `AdminMsg` (17). ESP-NOW has no addressing
  above the MAC layer and a
  node broadcasts, so a shared format is a shared conversation. The magic is checked before
  anything else at both ends. Encode/decode is written out by hand, never by transmuting a
  packed struct, and pinned byte-for-byte in `crates/wartui-proto/tests/wire.rs`.
- **A wire change means reflashing every node at once.** `wartui-proto` is a path dependency of
  both firmwares. A frame carrying our magic and an unknown version byte is counted as
  `incompatible` and named in the footer (`N frames from an older firmware — reflash`), never
  admitted to the node table and never half-decoded. The bridge is format-blind and does not
  need the reflash.
- **The bridge displays what it is handed and never composes it.** A board with a panel is
  sent finished lines with a severity on each, blits them, and owns nothing about them but the
  three colours a `Severity` means and the fallback screen it draws when no host is talking —
  link-local facts, which is why they cost the format-blind rule nothing. It reads no air
  frame to find out what to say, so a change to what the panel says or how it is arranged
  costs a `cargo run` rather than a reflash. The bridge advertising its own geometry in
  `Ready` is what removes the operator flag: a board without a screen reports `None` and is
  sent nothing at all.
  → `crates/wartui-core/src/panel.rs`, `firmware/bridge/src/panel.rs`,
  `crates/wartui-proto/src/link.rs`, `HostToBridge::ShowPanel`
- **`link::LINK_PROTO_VERSION` does not move before 1.0 either**, and for the reason
  `air::WIRE_VERSION` does not: there is no older peer for it to protect when the policy is to
  flash both ends from one tree. The cost is that a host and a bridge built from different
  trees meet as an undecodable frame rather than a named mismatch, which is stated where the
  constant is. Every addition to the link enums therefore goes on the end — postcard writes a
  variant as its index and a struct's fields in order.
- **The sighting trailer means what the kind says it means, and the wire layer never interprets
  it.** For Wi-Fi it is the roaming consortium element's body verbatim; for BLE it is exactly two
  bytes of company identifier, or nothing. The split is made once, in the engine's `Frame::Sighting`
  arm; keeping `air` byte-transparent over the trailer is what lets a change to how an identifier
  is *read* cost a re-export rather than a reflash.
  → `crates/wartui-proto/src/air.rs`, `SightingMsg::ext`; `crates/wartui-proto/src/beacon.rs`,
  `rcoi_text`; `crates/wartui-core/src/engine.rs`
- **Nothing here is compatible with an earlier wartui, and that is the policy until 1.0.** No
  migration path is built for a fleet mid-upgrade and no code reads an older wire format to be
  helpful about it; a node on a previous build is somebody else's traffic as far as this host is
  concerned. Flash the fleet together. **Neither version marker moves until 1.0**, whatever a
  layout does: `air::WIRE_VERSION` is the lever held for the first change a fleet in the field has
  to survive, and `store::SCHEMA_VERSION` the same for the first capture that has to be read in an
  earlier build's terms. Nothing before 1.0 is either, so spending one re-pins every fixture in
  `crates/wartui-proto/tests/wire.rs` to mark a difference the policy has already settled. **The
  store has no migrations before 1.0 either.** `check_version` refuses any marker but its own — a
  lower one as firmly as a higher one — so a capture from another build is somebody else's file,
  and the fix is a new `--db` path rather than a `migrate` arm. Bringing one forward would mean
  deciding what an older build meant, which is the compatibility this policy declines to claim: a
  stored `ChannelSet` is the indices that went out, read against the scan table of the build that
  wrote them.
- **The planner is the only author of an assignment.** There is no operator override, no mode and
  no flag: `FleetEngine::replan` decides what every node scans and nothing else writes a node's
  `desired`. That is what lets the fleet table, the store and the stagger arithmetic read a node's
  share as the plan's without asking who put it there. `Command::AssignBle` is the one operator
  decision, and it is an *input* to the planner rather than an exception to it: it names the node
  whose job is Bluetooth, and the planner is what deals that node nothing and hands its share
  round. So moving the scan re-cuts the whole pool, and `replan` — not `reissue` — is what
  delivers both ends of the move.
- **Only heartbeating nodes are assignable or in the plan** — a node that is merely being heard
  never opens an admin window. `stale`, `no heartbeat` and silence are deliberately distinct
  states, and `SCAN_CHANNELS` order is load-bearing: never sort or deduplicate it, because those
  indices *are* the wire format.
- **A node is only in the plan once it has said what its radio is.** `FleetEngine::is_assignable`
  refuses a node whose heartbeats carry no `air::Capabilities`, alongside one the bridge has no
  peer slot for. `None` means only that nothing but a sighting has been heard from that address
  yet — an ordinary few seconds in the life of a node about to be perfectly drivable. A share cut
  for a node whose band nothing has confirmed is a share that may be nobody's.
- **Twenty nodes is the hard maximum** (`plan::MAX_NODES`) — an ESP-NOW radio's peer table. Above
  it, capture continues and the planner refuses to re-cut rather than partitioning among nodes the
  bridge cannot address.
- **An assignment is believed only on a MAC-layer ack** (`SendStatus::AckOk` from the transmit
  callback), never on a successful enqueue.
- **A node adopts an assignment only when the epoch/version differs** from the one it holds, so
  re-sending an identical one is acknowledged and silently discarded. `node_index`/`node_count`
  travel in every assignment and drive each node's transmit stagger, so every fleet change re-cuts
  the whole plan, not just the affected node.
- **`IndexRun` describes a *pool*, never the shape of an assignment.** An assignment is a
  `ChannelSet` naming any subset of `SCAN_CHANNELS`, so the planner flattens the pool and deals
  round-robin rather than steering around run boundaries. The plan has no phases and no timer; it
  changes when fleet membership changes, when the Bluetooth scan moves, and at no other time.
- **`clippy::all` is denied workspace-wide, in both firmwares too, and so is `unsafe_code`.** The
  host workspace forbids it outright. Each firmware allows the two IDF calls `esp-radio` cannot
  express after its long-lived handles borrow the controller: the ESP-NOW peer-rate call and the
  runtime transmit-power call. Any further `#[allow(unsafe_code)]` is a decision to make in
  review, not a convenience.

### Load-bearing, and reasoned where they live

Each of these is enforced in one place and explained there at length. The claim is here so an
edit stops; follow the pointer before changing the rule.

- **Somebody else's fleet is recognised in order to be reported, never to be accommodated.**
  → `crates/wartui-proto/src/air.rs` `//!`, `air::foreign`
- **Finding the bridge means transmitting into what is opened**, so the sweep opens Espressif
  vendor IDs and nothing else, and `wartui reset` — which transmits before anything has identified
  itself — never sweeps at all. The GPS search is the complement: it opens everything *but* those,
  and writes to none of them. → `crates/wartui-bridge/src/ports.rs` `//!`,
  `serial::select` / `serial::unambiguous_bridge`, `crates/wartui-core/src/discover.rs` `//!`
- **A connection asks once and then only waits.** A board that is not reading its USB endpoint
  absorbs exactly one packet and NACKs the rest, and they sit in the tty's output queue through a
  `close` that no signal can interrupt — 30 s measured against a node, during which the process
  cannot exit. One `Identify` is also sufficient, because a board that is merely still booting
  reads it out of that same FIFO when its loop starts. → `crates/wartui-bridge/src/serial.rs`,
  `connect`'s `identify` arm
- **Never set DTR or RTS on a serial port, or ask `serialport` to.** The kernel raises both
  together on open, which an ESP32's USB Serial/JTAG ignores; moving one without the other is its
  reset sequence, so `.dtr_on_open(false)` reboots the board it was being polite to.
  → `crates/wartui-bridge/src/ports.rs` `//!`
- **The planner deals only channels a node's own radio can tune**, and what no radio present can
  reach comes back in `Plan::unreachable` rather than being dealt anyway. A `Job::Bluetooth` slot
  can tune nothing, so a fleet whose only 5 GHz radio holds the scan reports 5 GHz unreachable, and
  a fleet of one that holds it reports the whole pool.
  → `crates/wartui-proto/src/plan.rs`, `plan_for`; `crates/wartui/README.md` § "Channel pools"
- **Channel 14 is in no pool and is never dealt**, and stays in the scan table because that table's
  indices are the wire format. → `crates/wartui-proto/src/plan.rs`, `UNSUPPORTED_INDEX`
- **At most one node scans Bluetooth, by default none does, and it is that node's whole job.**
  `ADMIN_FLAG_BLE` is an operator's decision, not a property of the flashed firmware. The node
  holding it is dealt no channels and is counted in `node_count` anyway, because `node_index` and
  `node_count` are the stagger's arithmetic rather than a census of who is sniffing. An empty
  `ChannelSet` is sent only with the flag beside it; without it, a node told to scan nothing parks
  while the host believes it is sweeping, and `Plan::admin_for` refuses to build that frame.
  → `crates/wartui-proto/src/plan.rs`, `Job` / `Plan::channels_for` / `admin_for`;
  `crates/wartui-core/src/engine.rs`, `Command::AssignBle` / `on_assign_ble` / `replan`;
  `docs/phase-2-findings.md`
- **A heartbeat replayed out of the bridge's backlog is not an admin window.** Delete either
  half of the check and the symptom is a plausible-looking lie rather than an error.
  → `crates/wartui-core/src/engine.rs`, `note_arrival` / `air_is_live` / `BEHIND_THE_AIR`
- **The bridge's USB transmit endpoint can die on its own, and the bridge reboots when it does.**
  `StallWatch` times the *contradiction*, and **every one of its four clauses is load-bearing**:
  delete any and a bridge reboots for ever, or never. There is no watchdog behind the hang case.
  → `crates/wartui-proto/src/stall.rs`, `StallWatch`; `docs/phase-3-findings.md`
- **Every reset goes through `reboot()`, and on a C5 that funnel *is* the reset** — a bare
  `software_reset()` there does not reboot the board, it stops it. Each firmware has its own copy,
  so the esp-hal 1.2 cleanup (issue #16) has to remove both.
  → `firmware/bridge/src/main.rs`, `fn reboot`, which `firmware/node/src/main.rs` points at
- **A bridge reboot is invisible unless the host compares uptimes**, because a software reset does
  not re-enumerate the USB device. → `crates/wartui-bridge/src/serial.rs`, the `Ready` arm
- **Neither firmware may block on the USB endpoint.** The bridge's rings evict oldest-first and
  resynchronise COBS behind a truncated frame; a node loses a line rather than a sweep.
  → `crates/wartui-proto/src/outbox.rs` `//!`, `firmware/node/src/main.rs`, `note!`
- **Never link `esp-println` with `jtag-serial` in the *bridge*** — its link protocol shares that
  endpoint, and diagnostics go out as `Log` frames instead. → `firmware/bridge/README.md`
- **Both firmwares pass `US` for the regulatory domain, and that is not a preference**: `esp-radio`
  defaults to China, which silently costs a C5 channels 100–144 on every sweep.
  → `firmware/bridge/src/main.rs`, `firmware/node/README.md`
- **Every crate `esp-radio` also depends on is held at the version `esp-radio` resolves.**
  `esp-radio 1.0.0-beta.0` requires `esp-hal ~1.1.0`, which walls off `esp-hal` and `esp-rtos`
  outright. The others — `esp-alloc`, `esp-sync`, `esp-wifi-sys-*` — go higher perfectly happily,
  and a higher one is a *second copy* rather than an upgrade: a duplicate `__esp_radio_printf` that
  fat LTO refuses, or a second `esp-alloc` whose `HEAP` nothing fills, which builds and then panics
  on the first task the radio spawns. Dependabot ignores them and each firmware's CI job counts the
  versions; issue #16 is the real upgrade, and moves all of them at once.
  → `firmware/bridge/README.md` § "Dependency versions"
- **Every bridge and node defaults to 2 dBm**, the lowest `set_max_tx_power` accepts, and the host
  is what decides otherwise: `--tx-power` sets the fleet, `--bridge-tx-power` overrides it for the
  bridge alone, and both take whole dBm (2 to 20) that the CLI converts to quarter-dBm. The engine
  clamps the two settings once, at construction, then carries the bridge's with **every status
  poll** and the nodes' in their assignments. The poll carries it because `bulk` drops rather than
  blocks and a lost `SetTxPower` has nothing behind it to notice; range is the cost of changing
  the default. The ceiling is 20 dBm rather than the 21 the IDF accepts — whether anything above
  20 dBm works correctly is unverified, so the clamp keeps it unreachable.
  → `crates/wartui-proto/src/plan.rs`, `clamp_tx_power` / `DEFAULT_TX_POWER_QUARTER_DBM`;
  `crates/wartui-core/src/engine.rs`, `poll_bridge`
- **ESP-NOW goes out at 802.11g 24 Mbps, set per peer through IDF directly** — `esp-radio`'s
  `set_rate` is refused on the C5 and C6, and misnumbered besides.
  → `firmware/bridge/src/main.rs`, `set_peer_rate`; `firmware/node/src/radio.rs`
- **The air is plaintext ESP-NOW in both directions** — no pairing handshake and no key.
  → `firmware/bridge/src/main.rs`, peer registration
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
  `crates/*/tests/`. Most of the suite is the latter; `nmea`, `position`, `export` and `outbox`
  carry unit tests of their own.
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
