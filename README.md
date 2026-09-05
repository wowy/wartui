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
every observation to SQLite, and draws the fleet while it does. It transmits
only when asked: `a` and `A` assign the selected node a channel range, and
nothing else reaches the air.

```sh
wartui run --db tonight.db --lat 37.7749 --lon -122.4194
wartui export --db tonight.db --wigle tonight.csv
```

Press `q` to stop; the last batch is committed and the session closed out before
it exits.

### Assigning channels

| Key | What it does |
| --- | --- |
| `↑` `↓` / `k` `j` | Move the cursor down the fleet table |
| `a` | Give the selected node a range of exactly **one** channel |
| `A` | Give it the widest run in the pool |

Nothing goes out at the moment the key is pressed. A node's radio is away
scanning some other channel for all but the 300 ms it holds open after its own
heartbeat, so the assignment waits for that window — the `range` column reads
`1…` until it lands, then drops the ellipsis. On a full sweep that is up to four
seconds. That delay is the protocol, not lag.

An assignment is believed only when the node's own radio acknowledges it at the
MAC layer, never when the bridge reports a successful enqueue. The vendor core
cannot tell those apart, which is why it can sit with a node it believes is
assigned and is not.

`a` is also the Phase 4 proof that any of this works. A node heartbeats once per
completed sweep and reports nothing about what it is scanning, so watch the
`beat` column: narrowing a node from forty channels to one should collapse it
from seconds to a fraction of one within three sweeps. `A` puts it back.

If a node keeps showing `no admin ack`, the cause is nearly always BLE: NimBLE
and Wi-Fi share the one 2.4 GHz antenna, and the admin window is precisely when
the node would otherwise be idle. Turn BLE off in that node's web UI.

The store is the system of record and the CSV is a view of it, not the other way
round: an export can be re-run after a decoder fix, run against a session that
ended last week, or run against one that is still going.

### Positions

Every observation is stamped with the best position available, resolved fresh
each time: host GPS, then a static `--lat`/`--lon`, then nothing. A record is
never dropped for want of a position — but **WiGLE will not accept a row without
coordinates**, so a capture with no position given exports nothing and says how
many networks it left out. The GPS tier arrives in Phase 6.

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

`stale` and `no heartbeat` are deliberately distinct from silence. The vendor
firmware refreshes liveness only on a heartbeat, so a node streaming
observations whose heartbeats are lost would age out and churn the whole fleet's
topology — wartui keeps two clocks so the difference is visible rather than
fatal.

The footer only shows faults once they have happened, so a clean run reads as a
clean footer. `bridge dropped` there counts frames lost since this host
attached; `wartui status` reports the bridge's own total since it booted, which
on a dongle left powered with nothing listening is large and not a fault.

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
