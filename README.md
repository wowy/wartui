# wartui

A terminal UI for managing a cluster of wardriving ESP32-C5 nodes.

The nodes run [ESP32DualBandWardriver](https://github.com/justcallmekoko/ESP32DualBandWardriver)
firmware and talk [ESP-NOW](https://www.espressif.com/en/solutions/low-power-solutions/esp-now),
which a laptop cannot speak. wartui drives a USB-attached ESP32 as a radio
bridge and **takes the place of the mesh's CORE node**: it owns the node table,
issues channel assignments, and collects every observation the fleet produces.

## Layout

| Path | What it is |
| --- | --- |
| `crates/wartui-proto` | `no_std` wire formats, shared by the host and the bridge firmware |
| `crates/wartui-bridge` | Host-side link to the dongle: transport, port discovery, simulator |
| `crates/wartui-core` | Headless fleet engine, SQLite store, position, WiGLE export |
| `crates/wartui` | The TUI binary, and the `sniff` / `status` / `ports` commands |
| `firmware/bridge` | Rust firmware for the dongle (own workspace, own target) |
| `tools/espnow-sniffer` | Passive Arduino sniffer for bring-up and frame capture |
| `tools/golden` | Emits ground-truth struct layouts using the firmware's own typedefs |

## Scope

Nodes accept exactly one command over the air — `MSG_ADMIN`, a contiguous range
of channel indices to scan. There is no start/stop, config, reboot or query
message, and node serial is output-only. So wartui monitors the fleet, collects
and exports observations, and controls channel assignment. Anything else would
need changes to the node firmware.

wartui speaks **plaintext ESP-NOW only**; nodes must have encryption turned off
in their web UI. A `MSG_CORE_REQUEST` arriving from a node means that node still
has it on.

**Twenty nodes is the maximum, and anything above it is unsupported.** That is
how many peers an ESP-NOW radio can hold, so a twenty-first node is one the
bridge cannot address: its assignments would be refused, and a planner
partitioning the channel pool among nodes that never hear the result would be
describing a fleet that does not exist. The vendor firmware's own table is
twenty-four; the four it has spare are unreachable from here. Over twenty,
wartui keeps capturing everything every node reports — nothing is dropped — but
refuses to assign, and says so.

wartui is not a companion to a vendor core — it *replaces* one. If a real core
is powered up on the same channel it will fight for the same fleet, and the
footer says so (`admin frames from another core`).

## Channel pools

Which channels the fleet scans is selectable, because every scan the firmware
performs is *active* — nodes transmit probe requests on each channel they are
assigned.

- **`Us`** (default) — 2.4 GHz 1–11 and 5 GHz 36–165.
- **`All`** — every channel the firmware knows, matching stock behaviour.

`MSG_ADMIN` can only express one contiguous range, and the US pool is two runs
with a gap at channels 12–14, so the planner distributes nodes across runs and
never lets an assignment straddle the gap.

## Trying it

No hardware needed — the simulator runs a fake fleet on a fake clock:

```sh
cargo run -p wartui -- --sim 3 --lat 37.7749 --lon -122.4194
```

The simulated nodes model the parts of the firmware that matter: they adopt an
assignment only when its version differs, they dedup against a 200-entry ring,
and their sweep — and so their heartbeat period — is proportional to the range
they hold. Pressing `a` on one does what it would do to real hardware.

With a bridge plugged in:

```sh
cargo run -p wartui -- ports      # which device is it
cargo run -p wartui -- status     # is the link alive, is anything being dropped
cargo run -p wartui -- sniff      # every frame the fleet sends, decoded
```

Flashing the bridge itself is in `firmware/bridge/README.md`.

## Capturing

`wartui run` — the default, so the subcommand can be left off — listens, writes
every observation to SQLite, and draws the fleet while it does. It also
partitions the pool across the fleet without being asked, which is the core's
job and the reason this exists; `p` takes that back and `a`/`A` then assign the
selected node a range by hand. `--manual` starts with the planner off, and
nothing reaches the air until a key is pressed.

```sh
wartui run --db tonight.db --lat 37.7749 --lon -122.4194
wartui run --db tonight.db --manual --gps /dev/cu.usbserial-1420
wartui export --db tonight.db --wigle tonight.csv
```

Press `q` to stop; the last batch is committed and the session closed out before
it exits.

The store is the system of record and the CSV is a view of it, not the other way
round: an export can be re-run after a decoder fix, run against a session that
ended last week, or run against one that is still going.

### Assigning channels

| Key | What it does |
| --- | --- |
| `↑` `↓` / `k` `j` | Move the cursor down the fleet table |
| `a` | Give the selected node a range of exactly **one** channel |
| `A` | Give it the widest run in the pool |
| `p` | Take the fleet back from the planner, or hand it over again |

The header says which of you is deciding: `manual`, or `auto — 4 of 5` for four
heartbeating nodes out of five seen. It starts on `auto`, so **`a` and `A` are
refused until you press `p`** (or started with `--manual`) — the planner would
honour a hand-assigned range and then take it back at the next re-cut, which
reads as the key having been ignored.

Nothing goes out at the moment the key is pressed. A node's radio is away
scanning some other channel for all but the 300 ms it holds open after its own
heartbeat, so the assignment waits for that window — the `range` column reads
`1…` until it lands, then drops the ellipsis. On a full sweep that is up to four
seconds. That delay is the protocol, not lag.

An assignment is believed only when the node's own radio acknowledges it at the
MAC layer, never when the bridge reports a successful enqueue. The vendor core
cannot tell those apart, which is why it can sit with a node it believes is
assigned and is not.

`a` is also the Phase 4 proof that any of this works. Take the fleet back with
`p` first, or start with `--manual` so nothing has been assigned yet. A node
heartbeats once per completed sweep and reports nothing about what it is
scanning, so watch the `beat` column: narrowing a node from forty channels to
one should collapse it from seconds to a fraction of one within three sweeps.
`A` puts it back.

If a node keeps showing `no admin ack`, the cause is nearly always BLE: NimBLE
and Wi-Fi share the one 2.4 GHz antenna, and the admin window is precisely when
the node would otherwise be idle. Turn BLE off in that node's web UI.

### Letting wartui assign them

This is on by default, and it is wartui doing the core's whole job: it cuts the
pool into one contiguous range per node and re-cuts it whenever the fleet
changes shape. `--manual` starts without it; `p` toggles it either way.

A node is in the plan while it is **heartbeating**. Not while it is merely being
heard — a node that has stopped heartbeating never opens an admin window, so a
share of the pool held open for it is a share nobody is scanning. It drops out
after the same 60 s the firmware uses, and whatever it was owed is dropped with
it.

Every change re-cuts the pool for the *whole* fleet, not just the node that
joined or left. `node_index` and `node_count` travel in every assignment and are
what each node computes its transmit stagger from (`src/RadioTuning.cpp:3-13`),
so a fleet whose members disagree about the count keys up on top of itself. Each
node takes its new range in its own next admin window, so a fleet converges in
about one sweep.

An unchanged fleet is left alone. A node adopts an assignment only when the
epoch differs from the one it holds, so re-sending one it already has is a frame
it acknowledges and then discards — indistinguishable from success. The planner
only speaks when it has something new to say.

**One node on the US pool rotates.** `MSG_ADMIN` carries a single contiguous
range and the US pool is two runs, so a lone node cannot hold both at once; it
covers them in turn, 60 s each, and the header says which phase it is on.
Coverage becomes intermittent rather than incorrect.

Over twenty nodes the planner stops re-cutting rather than partitioning among
nodes the bridge cannot address. Whatever is already assigned stays assigned,
capture is unaffected, and the footer says so. A single node the bridge has no
peer slot for — peers are never removed, so a long session can fill the table
with nodes that have since gone — leaves the plan the same way, and the rest
re-cut to cover its share. It is tried again the next time a bridge announces
itself, since that table starts empty.

Assigning by hand while the planner is running is refused — it would be honoured
and then taken back at the next re-cut, which reads as the range having been
ignored. Press `p` first; since the planner is what wartui starts with, that is
the normal way round.

### Positions

Every observation is stamped with the best position available, resolved fresh
each time: a GPS on `--gps`, then a static `--lat`/`--lon`, then nothing. A
record is never dropped for want of a position — but **WiGLE will not accept a
row without coordinates**, so a capture with no position given exports nothing
and says how many networks it left out.

```sh
wartui run --db drive.db --gps /dev/cu.usbserial-1420 --lat 37.7749 --lon -122.4194
```

Giving both is the useful combination: the rows carry satellite positions
whenever the receiver has one, and the typed-in position the rest of the time,
rather than nothing at all while the receiver is still finding itself.

`--gps` takes any receiver that speaks NMEA 0183 over a serial port. `GGA` and
`RMC` are read and everything else is ignored; the altitude, the satellite count
and an accuracy estimated from the reported HDOP all reach the WiGLE export.
`--gps-baud` defaults to 9600, which is what most receivers ship at — u-blox
modules are often 38400, and the wrong rate shows up in the footer as unreadable
lines with no fix rather than as silence.

**A fix has to be recent to be used.** Past `--gps-max-age` seconds (5 by
default) the position falls back to the tier below and the header says
`gps fix is stale`, because at driving speed a minute-old fix is a different
neighbourhood, and a row that quietly claimed it would be worse than one
admitting to the static position. Which tier answered is recorded per row, so a
capture that starts in a garage and ends on a road is honest about both halves.

The receiver runs on its own thread and nothing waits for it: a capture starts
immediately, reconnects on its own if the puck is unplugged and put back, and
says what it is doing on the header line — `gps searching`, `gps ok, 8 sats`,
`gps fix is stale`, or the error from the port.

### What the fleet table is telling you

| State | Meaning |
| --- | --- |
| `alive` | Heartbeating, so it can be given a channel range |
| `stale` | Still being heard, but not heartbeating — most often BLE coexistence on the node holding the radio through its admin window |
| `no heartbeat` | Seen, but has never completed a sweep |
| `no admin ack` | An assignment went out and its radio did not answer — nearly always BLE, see [Assigning channels](#assigning-channels) |
| `refused` | The bridge would not transmit it. Nearly always a full peer table, which means the fleet is over twenty nodes |
| `rebooted xN` | Its heartbeat counter went backwards, so it has forgotten any assignment; wartui re-issues under a fresh epoch |
| `encrypted` | It is sending core-protocol frames. wartui cannot talk to it; turn encryption off in that node's web UI |

A node that is `stale` is also out of the plan, for the same reason it cannot be
assigned by hand: no heartbeat, no window.

`stale` and `no heartbeat` are deliberately distinct from silence. The vendor
firmware refreshes liveness only on a heartbeat, so a node streaming
observations whose heartbeats are lost would age out and churn the whole fleet's
topology — wartui keeps two clocks so the difference is visible rather than
fatal.

The footer only shows faults once they have happened, so a clean run reads as a
clean footer. `bridge dropped` there counts frames lost since this host
attached; `wartui status` reports the bridge's own total since it booted, which
on a dongle left powered with nothing listening is large and not a fault.

### When nothing arrives

The header says `waiting for a bridge to announce itself` for two quite
different reasons, and the fault box says which: `link down: could not open …`
means the port is not ours — nearly always another `wartui`, a `screen` session
or an IDE's serial monitor still holding it — while no fault at all means the
port opened and the dongle is not answering. In that second case reset it: unplug
and replug, or `espflash board-info --port …`, and try again.

```sh
cargo run -p wartui -- status                  # exits in 5 s with the reason
cargo run -p wartui -- --log-file wartui.log run
```

`--log-file` is the only way to see the transport's own account of a run: the
view owns the terminal, so without it nothing is logged anywhere. It records
which port was resolved, whether it opened, every reconnect and its reason, and
the same for the GPS reader thread. `RUST_LOG` sets the level (`info` by
default), so `RUST_LOG=debug` adds the dropped-command detail.

If the same port keeps being the wrong device — a C5 node plugged in by USB
looks identical to the bridge, same vendor and product ID — pin it with
`--port`. `wartui ports` lists the candidates.

## Development

```sh
cargo test --workspace
cargo clippy --workspace --all-targets
cargo fmt --check

cd firmware/bridge
cargo clippy --release --features esp32c6   # and --features esp32c5
```

`firmware/` is excluded from the workspace: a different target, its own
toolchain pin and its own lockfile. It takes a path dependency up into
`crates/wartui-proto`, which is why those wire types are `no_std`.

The wire codec is tested byte-for-byte against layouts produced by a real C++
compiler; see `tools/golden`. Frames captured off the air by
`tools/espnow-sniffer` can be pasted into
`crates/wartui-proto/tests/golden_vectors.txt` to become regression tests.
