# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

A terminal fleet controller for ESP32-C5 and ESP32-C6 wardriving nodes. wartui does not
assist a vendor CORE node — it **replaces** one: it owns the node table, issues channel
assignments and collects every observation, speaking ESP-NOW through a USB-attached ESP32
dongle running our own Rust bridge firmware. That dongle may be a C5, a C6 or an ESP32-S3;
the nodes are C5 and C6 only, because a node is the thing that has to reach 5 GHz and a
bridge never leaves the control channel. The nodes run our own firmware too
(`firmware/node`).

**Every frame on the air is wartui's own**, in both directions, and is deliberately
unrecognisable to the firmware this project grew up against —
[wowy/ESP32DualBandWardriver](https://github.com/wowy/ESP32DualBandWardriver) on
`feat/node-interference-mitigation`, a fork of
[justcallmekoko's](https://github.com/justcallmekoko/ESP32DualBandWardriver). That repo is
still the record of measured *behaviour* — the enqueue-versus-ack bug, BLE coexistence,
`passive = false`, `setFixedChannel` — and the `src/*.cpp:NNN` citations throughout this
tree point at it for that reason and no other. It is not a specification anything here
matches, and there is no checkout of it on this machine.

`README.md` is the operator's manual and is unusually complete — read it before changing
behaviour, and keep it true when behaviour changes. `docs/phase-0-findings.md` records what was
measured on the vendor fleet and is why several of the invariants below exist;
`docs/phase-1-findings.md` records what our own node firmware then did on the
same bench, including which of those invariants it has actually been checked
against and which are still only reasoning; `docs/phase-2-findings.md` does the
same for channel masks and Bluetooth-by-assignment, and is where the measured
cost of the Bluetooth scan comes from; `docs/phase-3-findings.md` is the USB
link — what the long-standing "bridge stops answering" wedge turned out to be,
and what does and does not recover from it. `docs/phase-4-findings.md` is the
wire becoming wartui's own in both directions; unlike the others it is written
ahead of the bench rather than after it, and says so.

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

### Telling the boards apart without opening a port

`wartui ports` prints one identical `USB JTAG/serial debug unit` line per attached board and
says nothing about which is the bridge. Probing for it is slow and answers one port at a time:
`wartui status` on a node waits six seconds for a protocol the node does not speak, then says
only that this one is not the bridge.

The USB serial number *is* the device's MAC, so the OS answers it for free, with no esp tool,
no reflash and without opening anything. On Linux, udev has already paired the two:

```sh
for l in /dev/serial/by-id/usb-Espressif_*; do
  mac=${l##*unit_}; printf '%s\t%s\n' "$(readlink -f "$l")" "${mac%-if00}"
done
```

On macOS the same fact comes out of `ioreg`:

```sh
ioreg -l -w0 | grep -E '"USB Serial Number"|"IOCalloutDevice"' | sed 's/^ *[|+ -]*//' \
  | grep -A1 "Serial Number" | grep -v '^--' | paste - - | grep usbmodem
```

Each line pairs a device path with the MAC of the board behind it — in either order for
`ioreg`, since it emits the two properties per device in whatever order it stored them. The
bridge is then the one whose address the fleet table shows as the bridge, and a node is the one
whose heartbeats `wartui sniff` attributes to that address. The OUI also separates board
generations where they differ. Refer to the results by their last two octets, per the global
rule.

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

- **The wire is ours, in both directions, and shares nothing with the vendor's.** Every frame is
  `WTUI`, a wire version byte, a type byte and a body: `HeartbeatMsg` (13 bytes), `SightingMsg`
  (17 plus the SSID) and `AdminMsg` (15). This is not tidiness. ESP-NOW has no addressing above
  the MAC layer and a node broadcasts, so a shared format is a shared conversation: a vendor core
  hearing vendor-shaped heartbeats from our nodes cut *its* plan around nodes that would never
  obey it, and — worse, because the vendor's handlers test `if (len < sizeof(...)) return;` rather
  than for equality — a stock node in range **adopted a garbage channel range out of wartui's
  longer assignment frame**. The magic is checked before anything else at both ends, so each
  costs the other one `memcmp`. Encode/decode is written out by hand, never by transmuting a
  packed struct, and pinned byte-for-byte in `crates/wartui-proto/tests/wire.rs` — hand-written
  vectors, because there is no second implementation of either end left to check against.
- **Somebody else's fleet is recognised in order to be reported.** `air::foreign::classify` matches
  `ENOW` and nothing more: no decode, no fields, no fixture. A vendor core's assignment counts as
  `foreign_admin` and its nodes' traffic as `foreign_fleet`, both said in the footer, because
  another fleet transmitting on the channel these nodes listen on is an operational fact. Nothing
  is done to keep a mixed fleet working — the vendor node is the thing being replaced.
- **The version byte is how a half-flashed fleet says so.** A frame carrying our magic and a
  version this build does not know is counted as `incompatible` and named in the footer
  (`N frames from an older firmware — reflash`), never admitted to the node table and never
  half-decoded. The consequence to keep in mind when flashing: **`wartui-proto` is a path
  dependency of both firmwares, so a wire change means reflashing every node at once.** The bridge
  is format-blind and does not need it.
- **Nothing here is compatible with an earlier wartui, and that is the policy until 1.0.** No
  migration path is built for a fleet mid-upgrade and no code reads an older wire format to be
  helpful about it; a node on a previous build is somebody else's traffic as far as this host is
  concerned. Flash the fleet together. The one thing that does survive a shape change is the
  store, which migrates (`store::SCHEMA_VERSION`) because a capture is data rather than a
  deployment.
- **A node is only in the plan once it has said what its radio is.** Every heartbeat carries
  `air::Capabilities` — major, minor, and a flags byte for `ble` and `5g` — and
  `FleetEngine::is_assignable` refuses a node that has none, alongside one the bridge has no peer
  slot for. `None` now means only that nothing but a sighting has been heard from that address
  yet, which is an ordinary few seconds in the life of a node about to be perfectly drivable: a
  node reports what it found on a channel before it gets back to the control channel to heartbeat.
  A share cut for a node whose band nothing has confirmed is a share that may be nobody's.
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
  fleet's `Radio`s, read out of the heartbeats' capabilities, and an ESP32-C6 is dealt no 5 GHz
  index: a share it cannot tune is a share nobody scans. A mixed fleet is dealt the constrained
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
  node whose capabilities say the feature is absent is refused the scan in both the view and the
  engine, and one already holding it loses it on the tick that learns so: it would adopt the flag,
  acknowledge, and scan nothing, so `ble_node` would name a holder that is not one. Losing it
  means an assignment re-issued without the flag, not just `ble_node` cleared — the flag lives in
  the frame, and the fleet table reads it off the frame — so `reissue` is the one funnel that
  refreshes it and refuses to set it for a node whose capabilities say no.
- **An assignment is believed only on a MAC-layer ack** (`SendStatus::AckOk` from the transmit
  callback), never on a successful enqueue. The vendor core conflates the two, which is the bug
  this project exists downstream of.
- **A heartbeat replayed out of the bridge's backlog is not an admin window.** The bridge buffers
  what it hears while no host is attached, so a fresh connection is handed a ring's worth of the
  recent past as fast as USB will carry it — measured at 8.5 minutes of bridge time inside 17 ms
  of host time, the oldest frame 532 seconds stale. Those heartbeats are still heartbeats and
  still admit their nodes, but the 300 ms windows they name shut long ago. `note_arrival` compares
  the bridge's own stamp against the host's clock — the two tick at the same rate, so a backlog
  is visible as the bridge running ahead — and `air_is_live` gates `send_admin` on the result; a
  connection assumes it is behind until a frame arrives that it had to wait for, because the first
  frame of a backlog is the one no comparison can catch. Delete either half and the symptom is not
  an error but a plausible-looking lie: nine assignments in a quarter of a second, eight
  unacknowledged, and an `assignment.latency_us` of eighty seconds. That column is `None` rather
  than a fabricated number when the figure exceeds the window it claims to measure.
- **A node adopts an assignment only when the epoch/version differs** from the one it holds, so
  re-sending an identical one is acknowledged and silently discarded. `node_index`/`node_count`
  travel in every assignment and drive each node's transmit stagger, so every fleet change re-cuts
  the whole plan, not just the affected node.
- **Only heartbeating nodes are assignable or in the plan** — a node that is merely being heard
  never opens an admin window. `stale`, `no heartbeat` and silence are deliberately distinct
  states; `SCAN_CHANNELS` order is load-bearing and must not be sorted or deduplicated.
- **The bridge's USB transmit endpoint can die on its own, and the bridge reboots when it does.**
  `SERIAL_IN_EP_DATA_FREE` goes to zero when `WR_DONE` is set and comes back only when the USB
  host reads the FIFO; if that read never lands, nothing on the device can clear it. The receive
  endpoint is unaffected, so the bridge goes on decoding and executing commands it cannot answer —
  which from the host is indistinguishable from a dead board, and is the failure that used to send
  operators to `espflash`. `wartui_proto::stall::StallWatch` times how long the endpoint has refused
  bytes *while somebody was waiting for them*, from when that contradiction started and never from
  the last byte written. It lives in `wartui-proto` and not in the firmware because it shipped two
  defects that only a bench found; every clause below is now a test, and a mutation of each one
  fails a named test rather than a board. Both halves are load-bearing. Time it from the last byte
  and a bridge left powered beside a fleet, with nobody reading for hours, reboots the moment
  `wartui` says hello; drop the host-present half and a bridge on a bench with nothing attached
  reboots for ever. For the same reason `last_host` starts at `None` and never at the boot instant:
  seeded with a time, it reads as a host present for the first `HOST_PRESENT_WINDOW_MS` of *every*
  life, and a bridge powered beside a talking fleet resets, boots into the same window, and does it
  again for ever. And a host that has *quit* is not a host that is waiting: it satisfies "spoke
  inside the window" for a further `HOST_PRESENT_WINDOW_MS`, while its quitting is exactly what
  stopped the endpoint draining, so the detector also needs a frame decoded *since the stall began*
  — strictly after it, since a host whose last frame lands in the same millisecond has not asked
  since. Without that clause every session ends in a reboot and the next `run` opens with a fault
  box blaming a wedge that was an operator closing a window. `HOST_PRESENT_WINDOW_MS` must also stay
  longer than the host's `status_interval`, or a live capture reads as an absent host between polls
  and a real wedge is never noticed. Measured in
  `docs/phase-3-findings.md`, along with why there is no watchdog behind the *hang* case: one was
  built, and esp-hal 1.1.2's RWDT never resets these parts — it counts, unfed, but its reset does
  not reach the CPU and `WDT_PROCPU_RESET_EN` will not be written.
- **Every reset goes through `reboot()`, and on a C5 that funnel is the reset.** Both firmwares
  reach `esp_hal::system::software_reset()` through one function that first clears
  `PCR.RESET_EVENT_BYPASS.reset_event_bypass` on the C5. Call `software_reset()` directly there
  and the board does not reboot, it stops: the ROM leaves the system bus frozen across a core
  reset, the next boot hangs before it can read flash, and neither `wartui reset` nor `espflash
  reset` nor a full reflash brings it back — only pulling the cable does, all four measured. The
  worst caller is the stall detector, whose whole job is unattended recovery. It is a `#[cfg]`
  for the C5 alone, it stays safe code, and it comes out when `esp-radio` lets the firmwares onto
  `esp-hal` 1.2, which has it in `pre_init` (esp-rs/esp-hal#5703; our issue #16).
- **A bridge reboot is invisible unless the host compares uptimes.** A software reset — the stall
  detector, the panic handler, `wartui reset` — does **not** re-enumerate the USB device: the
  host's file descriptor reads straight through it, measured. So the second `Ready` arrives on the
  connection the first one did, and it looks exactly like the other reason two announcements land
  together, which is a duplicate answer to an `Identify` that was already in flight. Nothing else
  in the frame separates them — two consecutive `wartui reset`s produce byte-identical `Ready`s —
  so `Ready` carries `uptime_ms` and a clock that went backwards is what says "new life". Suppress
  that and the reboot is silent in the worst way: the engine keeps the dead life's `BridgeInfo`,
  goes on believing in a peer table the reboot emptied, and the fault box never says a word.
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
