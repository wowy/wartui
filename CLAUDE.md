# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

A terminal fleet controller for ESP32-C5 wardriving nodes running the vendor
[ESP32DualBandWardriver](https://github.com/justcallmekoko/ESP32DualBandWardriver)
firmware. wartui does not assist a vendor CORE node — it **replaces** one: it owns the node
table, issues channel assignments and collects every observation, speaking ESP-NOW through a
USB-attached ESP32 dongle running our own Rust bridge firmware.

`README.md` is the operator's manual and is unusually complete — read it before changing
behaviour, and keep it true when behaviour changes. `docs/phase-0-findings.md` records what was
measured on real hardware and is why several of the invariants below exist.

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

The bridge firmware is a **separate workspace** (`exclude = ["firmware"]`): different target,
own toolchain pin, own lockfile. `cargo test --workspace` never touches it.

```sh
cd firmware/bridge
cargo clippy --release --features esp32c6     # and --features esp32c5; exactly one is required
cargo run --release --features esp32c6        # runner is `espflash flash --monitor`
```

Regenerating the wire golden vectors (only when the C++ typedefs in `tools/golden/gen_golden.cpp`
are re-copied from the vendor firmware):

```sh
c++ -std=c++17 -Wall -Wextra -o /tmp/gen_golden tools/golden/gen_golden.cpp
/tmp/gen_golden > crates/wartui-proto/tests/golden_vectors.txt
```

## Architecture

Four host crates, strictly layered, plus firmware that shares the bottom one.

- **`crates/wartui-proto`** — `no_std`, allocation-free wire formats. `air` (the vendor's packed
  ESP-NOW structs), `link` (our own COBS/postcard/CRC USB protocol), `outbox` (the firmware's
  bounded TX rings), `plan` (channel pools and the partitioning planner). Compiled into *both* the
  host and the firmware by path dependency, which is the only thing keeping the two ends in step.
- **`crates/wartui-bridge`** — host side of the USB link. Everything above talks to a `LinkHandle`
  and cannot tell a real dongle (`serial`) from the fake fleet (`sim`).
- **`crates/wartui-core`** — the headless half. Draws nothing, parses no arguments.
- **`crates/wartui`** — clap CLI (`run`/`export`/`sniff`/`status`/`ports`) and the ratatui view.
- **`firmware/bridge`** — dumb radio bridge: COBS framing and `esp-radio` calls, no protocol
  knowledge. Fixes there cost a reflash, so logic belongs on the host.

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

- **The on-air format is a contract with a separately compiled C++ program.** Encode/decode is
  written out by hand, never by transmuting a packed struct, and is checked byte-for-byte against
  `tests/golden_vectors.txt`. Frames captured by `tools/espnow-sniffer` can be pasted into that
  file (`name len hex`) to become regression tests. Doc comments cite `file:line` into the vendor
  firmware repo; keep those references when touching the code they annotate.
- **Twenty nodes is the hard maximum** (`plan::MAX_NODES`) — an ESP-NOW radio's peer table. Above
  it, capture continues and the planner refuses to re-cut rather than partitioning among nodes the
  bridge cannot address.
- **`MSG_ADMIN` expresses exactly one contiguous run of `SCAN_CHANNELS` indices.** The US pool is
  two runs with a gap at 12–14, so an assignment must never straddle it; a lone node rotates
  between runs on a dwell timer instead.
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
- **The firmware must never block on the USB endpoint** (the one exception is the millisecond wait
  for a transmit callback). Everything outbound goes through `wartui_proto::outbox`'s rings, which
  evict oldest-first and write a lone `0x00` behind a truncated frame so COBS can resynchronise.
  Never link `esp-println` with `jtag-serial`; diagnostics go out as `Log` frames.
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
