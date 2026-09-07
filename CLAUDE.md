# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

A terminal fleet controller for ESP32-C5 and ESP32-C6 wardriving nodes. wartui does not
assist a vendor CORE node — it **replaces** one: it owns the node table, issues channel
assignments and collects every observation, speaking ESP-NOW through a USB-attached ESP32
dongle running our own Rust bridge firmware. The nodes run our own firmware too
(`firmware/node`), which speaks the wire format of the vendor
[ESP32DualBandWardriver](https://github.com/justcallmekoko/ESP32DualBandWardriver) —
still the reference for everything on the air, checked out at
`/Users/wowy/code/ESP32DualBandWardriver` on `feat/node-interference-mitigation`.

`README.md` is the operator's manual and is unusually complete — read it before changing
behaviour, and keep it true when behaviour changes. `docs/phase-0-findings.md` records what was
measured on the vendor fleet and is why several of the invariants below exist;
`docs/phase-1-findings.md` records what our own node firmware then did on the
same bench, including which of those invariants it has actually been checked
against and which are still only reasoning; `docs/phase-2-findings.md` does the
same for channel masks and Bluetooth-by-assignment, and is where the measured
cost of the Bluetooth scan comes from.

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

Each firmware is a **separate workspace** (`exclude = ["firmware"]`): different target, own
toolchain pin, own lockfile. `cargo test --workspace` never touches them.

```sh
cd firmware/bridge
cargo clippy --release --features esp32c6     # and --features esp32c5; exactly one is required
cargo run --release --features esp32c6        # runner is `espflash flash --monitor`

cd firmware/node
cargo clippy --release --features esp32c6     # and esp32c5, and each with ,ble
```

Regenerating the wire golden vectors (only when the C++ typedefs in `tools/golden/gen_golden.cpp`
are re-copied from the vendor firmware):

```sh
c++ -std=c++17 -Wall -Wextra -o /tmp/gen_golden tools/golden/gen_golden.cpp
/tmp/gen_golden > crates/wartui-proto/tests/golden_vectors.txt
```

## Architecture

Four host crates, strictly layered, plus firmware that shares the bottom one.

- **`crates/wartui-proto`** — `no_std`, allocation-free wire formats and the parsing that goes
  with them. `air` (the vendor's packed ESP-NOW structs, and the wardrive-line reader/writer),
  `beacon` (802.11 management frames and RSN/WPA elements to a `Sighting`), `hci` (the four
  Bluetooth commands and one event a scan needs), `dedup` (the node's oldest-out MAC ring),
  `link` (our own COBS/postcard/CRC USB protocol), `outbox` (the bridge's bounded TX rings),
  `plan` (channel pools, timings and the partitioning planner). Compiled into *both* the host
  and the firmware by path dependency, which is the only thing keeping the ends in step — and
  the reason a node's parsers are testable with `cargo test` rather than a reflash.
- **`crates/wartui-bridge`** — host side of the USB link. Everything above talks to a `LinkHandle`
  and cannot tell a real dongle (`serial`) from the fake fleet (`sim`).
- **`crates/wartui-core`** — the headless half. Draws nothing, parses no arguments.
- **`crates/wartui`** — clap CLI (`run`/`export`/`sniff`/`status`/`ports`) and the ratatui view.
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

- **Node → core is a contract with a separately compiled C++ program.** Heartbeats and
  observations are byte-identical to a stock node's, which is what lets golden vectors captured off
  a vendor fleet keep testing this code. Encode/decode is written out by hand, never by transmuting
  a packed struct, and is checked byte-for-byte against `tests/golden_vectors.txt`. Frames captured
  by `tools/espnow-sniffer` can be pasted into that file (`name len hex`) to become regression
  tests. Doc comments cite `file:line` into the vendor firmware repo; keep those references when
  touching the code they annotate.
- **Core → node is ours, and deliberately incompatible.** From Phase 2 `MSG_ADMIN` is wartui's own
  fourteen-byte frame: version, index, count, a flags byte and a forty-bit `ChannelSet`. The
  vendor's ten-byte one does not decode; `air::is_legacy_admin` recognises it without obeying it,
  so a stock core powered up nearby is reported rather than counted as line noise. Nothing is done
  to keep a mixed fleet working — the vendor node is the thing being replaced.
- **A node is only in the plan if it says it is one of ours.** Node → core is byte-identical, so
  a stock node heartbeats like ours and its radio acknowledges an assignment its application
  cannot decode — it then scans all forty channels while the share cut for it goes uncovered.
  Measured: two nodes and a stranger covered less of the pool than the two alone
  (`docs/phase-2-findings.md`). So every heartbeat carries `air::Capabilities`, an ASCII token
  in the text field a stock node leaves empty, and `FleetEngine::is_assignable` refuses anything
  without one — the same rule that already excluded encrypted and peer-refused nodes, for the
  same reason. The consequence to keep in mind when flashing: **a node running firmware older
  than the token is ignored**, and the fleet table says `not wartui` rather than pretending.
- **Twenty nodes is the hard maximum** (`plan::MAX_NODES`) — an ESP-NOW radio's peer table. Above
  it, capture continues and the planner refuses to re-cut rather than partitioning among nodes the
  bridge cannot address.
- **An assignment is a `ChannelSet`, and a pool is still runs.** The mask can name any subset of
  `SCAN_CHANNELS`, so a run boundary is not something the planner steers around: it flattens the
  pool and deals round-robin, index `k` to node `k % node_count`, giving every node some of both
  bands. `IndexRun` still describes a *pool* and must not come back as the shape of an assignment.
  The plan has no phases and no timer; it changes when fleet membership changes and at no other
  time.
- **The planner deals only channels a node's own radio can tune.** `plan::plan_for` takes the
  fleet's `Radio`s, read out of the capability tokens, and an ESP32-C6 is dealt no 5 GHz index:
  a share it cannot tune is a share nobody scans, which is the failure the token exists to
  prevent, arriving through a node that is genuinely ours. A mixed fleet is dealt the constrained
  half first — in pool order the dual-band nodes take their 2.4 GHz share and then all of 5 GHz on
  top, which is the block split this planner was written to avoid — and shares then differ by more
  than one, which is unavoidable and is why `plan_for` minimises the largest rather than
  equalising. A uniform fleet has no constrained channels, so its plan is byte-identical to what
  `plan` has always produced, and `plan` is now that special case. Channels no radio present can
  reach come back in `Plan::unreachable` and are said in the footer rather than dealt. An
  assignment made by hand goes through `Radio::tunable` for the same reason and is the quieter
  case: nothing re-partitions afterwards to correct it and `Plan::unreachable` never sees it.
- **At most one node scans Bluetooth, and by default none does.** `ADMIN_FLAG_BLE` is a per-node
  decision the operator makes (`Command::AssignBle`, `b` in the view), not a property of the
  firmware that was flashed: the `ble` cargo feature decides whether the code exists and the flag
  decides whether it runs, off at every boot. The cost is measured — every assignment lost on a
  stock node, ~10% of the sweep period on ours — and is worth paying on one node, not on all. A
  node whose token says the feature is absent is refused the scan in both the view and the engine,
  and one already holding it loses it on the tick that learns so: it would adopt the flag,
  acknowledge, and scan nothing, so `ble_node` would name a holder that is not one. Losing it
  means an assignment re-issued without the flag, not just `ble_node` cleared — the flag lives in
  the frame, and the fleet table reads it off the frame — so `reissue` is the one funnel that
  refreshes it and refuses to set it for a node whose token says no.
- **An assignment is believed only on a MAC-layer ack** (`SendStatus::AckOk` from the transmit
  callback), never on a successful enqueue. The vendor core conflates the two, which is the bug
  this project exists downstream of.
- **A node adopts an assignment only when the epoch/version differs** from the one it holds, so
  re-sending an identical one is acknowledged and silently discarded. `node_index`/`node_count`
  travel in every assignment and drive each node's transmit stagger, so every fleet change re-cuts
  the whole plan, not just the affected node.
- **Only heartbeating nodes are assignable or in the plan** — a node that is merely being heard
  never opens an admin window. `stale`, `no heartbeat` and silence are deliberately distinct
  states; `SCAN_CHANNELS` order is load-bearing and must not be sorted or deduplicated.
- **Neither firmware may block on the USB endpoint.** `UsbSerialJtag` stops accepting bytes when
  its FIFO fills and nothing drains it unless a host is reading, so a blocking write stalls the
  radio in the field and nowhere else. The bridge sends everything through `wartui_proto::outbox`'s
  rings, which evict oldest-first and write a lone `0x00` behind a truncated frame so COBS can
  resynchronise. Never link `esp-println` with `jtag-serial` in the *bridge* — its link protocol
  shares that endpoint and diagnostics go out as `Log` frames. A node has the endpoint to itself
  and does use `esp-println`, whose serial-JTAG writer waits a bounded number of iterations and
  then remembers that nobody is reading.
- **Setting a node's channel is not `set_channel` alone.** On an unassociated station interface it
  does not stick unless promiscuous mode is on across the change, which is the whole of the
  vendor's `setFixedChannel` (`src/WiFiOps.cpp:600-618`) and of `radio::park`. A node whose channel
  silently did not change reports the right networks against the wrong frequency and hears no
  assignment, which reads as a dead node rather than a bug.
- **A node's promiscuous callback runs in the Wi-Fi task, on a buffer that dies when it returns.**
  It cannot capture state and must not block: reject, parse into a fixed-size `Sighting`, leave it
  in a `static` ring, return. Anything that could grow — allocation, a lock held across work, a
  transmit — belongs in the main loop.
- `unsafe_code` is **forbidden** and `clippy::all` is **denied** workspace-wide.

## Conventions

- Module-level `//!` docs explain *why* the design is the way it is, not what the code does; the
  reasoning in them is often the only record of a hardware constraint. Match that register, and
  update the reasoning when the decision changes.
- Rustfmt is configured with `max_width = 100` and `use_small_heuristics = "Max"`.
- Tests are mostly integration tests under `crates/*/tests/` with full-sentence names
  (`observations_keep_a_node_visible_but_only_heartbeats_keep_it_assignable`); `#[cfg(test)]`
  modules are used for parsing/formatting units (`nmea`, `position`, `export`, `outbox`).
- Commit messages are prose: an imperative one-line subject, then paragraphs explaining what was
  wrong and why the fix is shaped as it is — including what was deliberately *not* changed.
