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
| `crates/wartui` | The TUI binary |
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

## Channel pools

Which channels the fleet scans is selectable, because every scan the firmware
performs is *active* — nodes transmit probe requests on each channel they are
assigned.

- **`Us`** (default) — 2.4 GHz 1–11 and 5 GHz 36–165.
- **`All`** — every channel the firmware knows, matching stock behaviour.

`MSG_ADMIN` can only express one contiguous range, and the US pool is two runs
with a gap at channels 12–14, so the planner distributes nodes across runs and
never lets an assignment straddle the gap.

## Development

```sh
cargo test --workspace
cargo clippy --workspace --all-targets
cargo fmt --check
```

The wire codec is tested byte-for-byte against layouts produced by a real C++
compiler; see `tools/golden`. Frames captured off the air by
`tools/espnow-sniffer` can be pasted into
`crates/wartui-proto/tests/golden_vectors.txt` to become regression tests.
