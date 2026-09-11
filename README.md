# wartui

A terminal fleet controller for ESP32-C5 and ESP32-C6 wardriving nodes.

The nodes talk [ESP-NOW](https://www.espressif.com/en/solutions/low-power-solutions/esp-now),
which a laptop cannot speak, so wartui drives a USB-attached ESP32 as a radio
bridge. It does not assist the mesh's CORE node — it **replaces** one: it owns
the node table, issues channel assignments, and collects every observation the
fleet produces.

The nodes run `firmware/node` and the dongle runs `firmware/bridge`, both in this
repository, and every frame on the air is wartui's own in either direction. The
bridge may be a C5, a C6 or an ESP32-S3 — it parks on the control channel and
never needs 5 GHz. The nodes are C5 and C6, because a node is the thing that has
to reach both bands.

## Running it

### Simulated

No hardware needed — the simulator runs a fake fleet on a fake clock:

```sh
cargo run -p wartui -- --sim 3 --lat 37.7749 --lon -122.4194
```

The simulated nodes model the parts of the firmware that matter: they park doing
nothing until assigned, adopt an assignment only when its epoch differs, dedup
against a 200-entry ring, and sweep — and so heartbeat — proportionally to how
many channels they hold. `--sim-c6 N` makes that many of them ESP32-C6s, counting
from the end of the fleet, which is how to put a mixed fleet in front of the
planner without two kinds of board on the desk.

### On real hardware

To use wartui, install it so the binary is on `PATH`:

```sh
cargo install --path crates/wartui
```

With a bridge plugged in, `run` is the default and the subcommand can be left off:

```sh
wartui run --db tonight.db --lat 37.7749 --lon -122.4194
wartui export --db tonight.db --wigle tonight.csv
```

`q` stops a capture, committing the last batch and closing out the session. The
store is the system of record and the CSV is a view over it, re-runnable against a
finished session or one still going.

```sh
wartui ports      # which serial devices look like an Espressif board
wartui status     # is the link alive, is anything being dropped
wartui sniff      # every frame the fleet sends, decoded
wartui reset      # reboot a bridge that has stopped answering
```

## Subprojects

| Path | What it is |
| --- | --- |
| [`crates/wartui`](crates/wartui/README.md) | The clap CLI and the ratatui view — **the operator's manual** |
| `crates/wartui-core` | Headless fleet engine, SQLite store, position, WiGLE export |
| `crates/wartui-bridge` | Host-side link to the dongle: transport, port discovery, simulator |
| `crates/wartui-proto` | `no_std` wire formats and parsers, shared with both firmwares |
| [`firmware/bridge`](firmware/bridge/README.md) | The dongle: COBS framing and `esp-radio` calls, no protocol knowledge |
| [`firmware/node`](firmware/node/README.md) | The nodes: sniffs, reports, takes assignments |
| [`tools/espnow-sniffer`](tools/espnow-sniffer/README.md) | Passive Arduino sniffer for bring-up and frame capture |
| `tools/beacons` | Turns a monitor-mode capture into fixtures for the beacon parser |

Each firmware is its own workspace — a different target, its own toolchain pin and
its own lockfile — and both take a path dependency up into `crates/wartui-proto`,
which is the only thing keeping the two ends of the wire in step. The library
crates have no README of their own; their `//!` module docs are the detail.

## At the keyboard

| Key | What it does |
| --- | --- |
| `↑` `↓` / `k` `j` | Move the cursor down the fleet table |
| `a` / `A` | Give the selected node one channel / the whole pool |
| `b` | Move the Bluetooth scan to it, or take it off the fleet |
| `p` | Take the fleet back from the planner, or hand it over again |
| `q` | Stop, committing the last batch |

wartui partitions the channel pool across the fleet without being asked, which is
the core's job and the reason this exists. `p` takes that back and `--manual`
starts without it; **`a` and `A` are refused until one of them does**, because the
planner would honour a hand-assigned set and then take it back at the next re-cut.

Nothing goes out at the moment a key is pressed. A node's radio is away scanning
for all but the 300 ms it holds open after its own heartbeat, so the assignment
waits for that window — up to about four seconds on a full sweep. The `channels`
column reads `1: 1…` until it lands.

`--pool us` is the default: 2.4 GHz 1–11 and 5 GHz 36–165. `--pool all` is every
channel a node can tune, 2.4 GHz 1–13 and all of 5 GHz.

At most one node scans Bluetooth, and by default none does. `b` moves it.

**Fleet states, GPS, channel-pool detail and troubleshooting are in
[`crates/wartui/README.md`](crates/wartui/README.md).**

## Limits

- **Twenty nodes is the maximum** (`plan::MAX_NODES`) — that is how many peers an
  ESP-NOW radio holds, so a twenty-first is one the bridge cannot address. Above
  it, capture continues and nothing is dropped, but the planner stops re-cutting
  and says so.
- **An ESP32-C6 is never dealt a 5 GHz channel.** It has no radio for one, and a
  share cut for it would be a share nobody scans.
- **Channel 14 is in neither pool** and is never dealt; `esp-radio` exposes no way
  to reach it.
- **Plaintext ESP-NOW only**, in both directions. There is no pairing handshake
  and no key.
- **Nothing is compatible with an earlier wartui, and that is the policy until
  1.0.** `wartui-proto` is compiled into the host and both firmwares, so flash the
  fleet together; a node on a previous build is traffic this host cannot read. The
  bridge is format-blind and does not need reflashing for a wire change.

## Development

```sh
cargo test --workspace
cargo clippy --workspace --all-targets
cargo fmt --check

cargo test -p wartui-core --test engine                     # one test binary
cargo test -p wartui-core --test engine a_heartbeat_counter # one test by name

cd firmware/bridge && cargo clippy --release --features esp32c6   # and esp32c5
cd firmware/node   && cargo clippy --release --features esp32c6   # and with ,ble

# The S3 bridge is Xtensa: espup's toolchain, and its own target.
cd firmware/bridge && cargo +esp clippy --release --features esp32s3 \
  --target xtensa-esp32s3-none-elf
```

`wartui-proto` is `no_std` because it is compiled into the firmwares as well as
the host — which is also why the node's beacon parser, dedup ring and HCI packets
live there, where `cargo test` reaches them and a fix costs a `cargo run` rather
than a reflash. The wire codec is pinned byte-for-byte in
`crates/wartui-proto/tests/wire.rs` against hand-written vectors, since there is
no second implementation of either end to check against.

`--log-file` is the only way to see transport logs: the view owns the terminal, so
without it nothing is logged anywhere.
