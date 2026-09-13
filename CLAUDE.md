# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

A terminal fleet controller for ESP32-C5 and ESP32-C6 wardriving nodes, speaking ESP-NOW
through a USB-attached ESP32 dongle. `README.md` introduces it and says which chip goes
where. Both firmwares are ours, and every frame on the air is wartui's own in both
directions.

Where the prose lives:

- `README.md` — the front door, and stays short.
- `crates/wartui/README.md` — the operator's manual: keys, fleet states, channel pools,
  GPS, troubleshooting. Read it before changing view or CLI behaviour, and **keep it true
  when that behaviour changes.**
- `firmware/*/README.md` — each firmware's own constraints, including the esp-hal version
  wall (`firmware/bridge/README.md`).
- `docs/phase-N-findings.md` — what each bench actually measured, and why several
  invariants below exist. Phase 0 is the vendor fleet; 1 is our node firmware; 2 is channel
  masks and Bluetooth; 3 is the USB link and the wedge; 4 is the wire becoming ours, and is
  the one written ahead of the bench rather than after it.

Module-level `//!` docs carry the reasoning behind the invariants below. They are the
record; this file is the index.

## Commands

```sh
cargo test --workspace
cargo clippy --workspace --all-targets
cargo fmt --check

cargo test -p wartui-core --test engine                    # one test binary
cargo test -p wartui-core --test engine a_heartbeat_counter # one test by name substring
```

Run it without hardware — the simulator runs a fake fleet on a fake clock:

```sh
cargo run -p wartui -- --sim 3 --lat 37.7749 --lon -122.4194
cargo run -p wartui -- ports | status | sniff        # with a bridge plugged in
cargo run -p wartui -- --log-file wartui.log run     # the only way to see transport logs
```

`wartui ports` cannot say which attached board is the bridge, and probing is slow, but the USB
serial number *is* the device's MAC — `crates/wartui/README.md` § "Telling the boards apart"
has the one-liners. Refer to boards by their last two octets, per the global rule.

Each firmware is a **separate workspace** (`exclude = ["firmware"]`): different target, own
toolchain pin, own lockfile. `cargo test --workspace` never touches them.

```sh
cd firmware/bridge
cargo clippy --release --features esp32c6     # and --features esp32c5; exactly one is required
cargo run --release --features esp32c6        # runner is `espflash flash --monitor`

# The S3 bridge is Xtensa: espup's `esp` toolchain, its own target, and
# `. ~/export-esp.sh` in the shell so the GCC linker is on PATH.
cargo +esp build --release --features esp32s3 --target xtensa-esp32s3-none-elf

cd firmware/node
cargo clippy --release --features esp32c6     # and esp32c5, and each with ,ble
```

## Architecture

Four host crates, strictly layered, plus firmware that shares the bottom one.

- **`crates/wartui-proto`** — `no_std`, allocation-free wire formats and the parsing that goes
  with them. `air` (the three ESP-NOW frames, and `air::foreign` for recognising somebody
  else's), `beacon` (802.11 management frames and RSN/WPA elements to a `Sighting`), `hci` (the four
  Bluetooth commands and one event a scan needs), `dedup` (the node's oldest-out MAC ring),
  `link` (our own COBS/postcard/CRC USB protocol), `outbox` (the bridge's bounded TX rings),
  `stall` (when that endpoint has stopped draining), `plan` (channel pools, timings and the
  partitioning planner). Compiled into *both* the host
  and the firmware by path dependency, which is the only thing keeping the ends in step — and
  the reason a node's parsers are testable with `cargo test` rather than a reflash.
- **`crates/wartui-bridge`** — host side of the USB link. Everything above talks to a `LinkHandle`
  and cannot tell a real dongle (`serial`) from the fake fleet (`sim`).
- **`crates/wartui-core`** — the headless half. Draws nothing, parses no arguments.
- **`crates/wartui`** — clap CLI (`run`/`export`/`sniff`/`status`/`reset`/`ports`) and the ratatui
  view.
- **`firmware/bridge`** — dumb radio bridge: COBS framing and `esp-radio` calls, no protocol
  knowledge. Fixes there cost a reflash, so logic belongs on the host.
- **`firmware/node`** — the nodes. Sniffs rather than scans, so it never transmits while
  looking and can hold `sniffer()` and `esp_now()` at once (both borrow the controller
  immutably; `scan_async` wants `&mut`). Returns to the control channel after every dwell,
  and parks there doing nothing when it holds no assignment. Same rule as the bridge: the
  logic lives in `wartui-proto`, and this crate is the conversation with the radio.

### The engine/runtime split is load-bearing

`wartui_core::engine::FleetEngine::handle` is a **pure synchronous state machine**: `Event` in,
`ActionBatch` out. It reads no clock (time arrives as `Now`, carrying both a monotonic `Instant`
and unix millis), touches no socket, opens no file. `wartui_core::runtime::drive` is the only
place that reads a clock and performs actions. This is what makes liveness ageing, reboot
detection, assignment timing and auto-partitioning testable in microseconds against a clock the
test invents (`crates/wartui-core/tests/engine.rs`). Do not reach for `Instant::now()`, I/O or
`async` inside `engine`.

Operator keypresses are `Event::Command`, not methods — a keypress and a heartbeat have to be
ordered against each other.

### Store is the system of record

`store` is SQLite behind one owner thread with batched transactions and a bounded queue that
**drops rather than blocks** (a stalled engine misses everything, including an assignment racing
a 300 ms window). `export` (WiGLE CSV) is a view over the store, re-runnable against a finished
or still-running session. Bump `store::SCHEMA_VERSION` when the schema changes shape.

Positions resolve fresh per record through `PositionChain`: GPS (`--gps`, NMEA on its own thread)
→ static `--lat`/`--lon` → nothing. Which tier answered is stored per row.

## Invariants that are easy to break

These bite from a distance — from a file other than the one that owns them — so the reasoning
is here rather than only in a `//!`.

- **The wire is ours, in both directions, and shares nothing with the vendor's.** Every frame is
  `WTUI`, a wire version byte, a type byte and a body: `HeartbeatMsg` (13 bytes), `SightingMsg`
  (17 plus the SSID) and `AdminMsg` (15). ESP-NOW has no addressing above the MAC layer and a
  node broadcasts, so a shared format is a shared conversation. The magic is checked before
  anything else at both ends. Encode/decode is written out by hand, never by transmuting a
  packed struct, and pinned byte-for-byte in `crates/wartui-proto/tests/wire.rs` — hand-written
  vectors, because there is no second implementation of either end left to check against.
- **A wire change means reflashing every node at once.** `wartui-proto` is a path dependency of
  both firmwares. A frame carrying our magic and an unknown version byte is counted as
  `incompatible` and named in the footer (`N frames from an older firmware — reflash`), never
  admitted to the node table and never half-decoded. The bridge is format-blind and does not
  need the reflash.
- **Nothing here is compatible with an earlier wartui, and that is the policy until 1.0.** No
  migration path is built for a fleet mid-upgrade and no code reads an older wire format to be
  helpful about it; a node on a previous build is somebody else's traffic as far as this host is
  concerned. Flash the fleet together. The one thing that does survive a shape change is the
  store, which migrates (`store::SCHEMA_VERSION`) because a capture is data rather than a
  deployment.
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
  changes when fleet membership changes and at no other time.
- **`clippy::all` is denied workspace-wide, in both firmwares too, and so is `unsafe_code`.** The
  host workspace forbids it outright. Each firmware denies it and allows it on exactly one
  function, the ESP-NOW rate call `esp-radio` does not wrap; a second `#[allow(unsafe_code)]`
  is a decision to make in review, not a convenience.

### Load-bearing, and reasoned where they live

Each of these is enforced in one place and explained there at length. The claim is here so an
edit stops; follow the pointer before changing the rule.

- **Somebody else's fleet is recognised in order to be reported, never to be accommodated.**
  Nothing keeps a mixed fleet working; the vendor node is what is being replaced.
  → `crates/wartui-proto/src/air.rs` `//!`, `air::foreign`
- **The planner deals only channels a node's own radio can tune**, and what no radio present can
  reach comes back in `Plan::unreachable` rather than being dealt anyway.
  → `crates/wartui-proto/src/plan.rs`, `plan_for`; `crates/wartui/README.md` § "Channel pools"
- **Channel 14 is in no pool and is never dealt**, and stays in the scan table because that table's
  indices are the wire format. → `crates/wartui-proto/src/plan.rs`, `UNSUPPORTED_INDEX`
- **At most one node scans Bluetooth, and by default none does.** `ADMIN_FLAG_BLE` is an operator's
  decision, not a property of the flashed firmware, and withdrawing it means an assignment
  **re-issued** without the flag — so `reissue` is the one funnel.
  → `crates/wartui-core/src/engine.rs`, `Command::AssignBle` / `on_assign_ble` / `reissue`;
  `docs/phase-2-findings.md`
- **A heartbeat replayed out of the bridge's backlog is not an admin window.** Such a heartbeat
  still admits its node; the window it names shut long ago. Delete either half of the check and the
  symptom is a plausible-looking lie rather than an error.
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
- **`esp-radio 1.0.0-beta.0` requires `esp-hal ~1.1.0`, and that pins the whole family.** Do not try
  to force it; `cargo update` lists newer versions and moves nothing. Issue #16 is the real upgrade.
  → `firmware/bridge/README.md` § "Dependency versions"
- **Every bridge and node transmits at 2 dBm**, the lowest `set_max_tx_power` accepts. It is the
  operator's policy for the whole fleet, not a default to raise for one board; range is the cost.
  → `crates/wartui-proto/src/plan.rs`, `TX_POWER_QUARTER_DBM`
- **ESP-NOW goes out at 802.11g 24 Mbps, set per peer through IDF directly** — `esp-radio`'s
  `set_rate` is refused on the C5 and C6, and misnumbered besides. The rate belongs to the peer
  entry, so every peer the bridge adds goes through `add_peer`, and a node sets it once on the
  broadcast peer. → `firmware/bridge/src/main.rs`, `set_peer_rate`; `firmware/node/src/radio.rs`
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
- Rustfmt is configured with `max_width = 100` and `use_small_heuristics = "Max"`.
- Tests are mostly integration tests under `crates/*/tests/` with full-sentence names
  (`observations_keep_a_node_visible_but_only_heartbeats_keep_it_assignable`); `#[cfg(test)]`
  modules are used for parsing/formatting units (`nmea`, `position`, `export`, `outbox`).
- All work happens on a branch and lands through a pull request; nothing is committed directly to
  `main`.
- Commit messages are clear and concise: an imperative one-line subject, then a paragraph explaining
  what was wrong and why the fix is shaped as it is.
