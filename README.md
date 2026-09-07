# wartui

A terminal UI for managing a cluster of wardriving ESP32-C5 and ESP32-C6 nodes.

The nodes talk [ESP-NOW](https://www.espressif.com/en/solutions/low-power-solutions/esp-now),
which a laptop cannot speak. wartui drives a USB-attached ESP32 as a radio
bridge and **takes the place of the mesh's CORE node**: it owns the node table,
issues channel assignments, and collects every observation the fleet produces.

The nodes run `firmware/node`, wartui's own firmware, which is why the fleet is
C6 as well as C5. It speaks the wire format of
[ESP32DualBandWardriver](https://github.com/justcallmekoko/ESP32DualBandWardriver)
— the firmware this project grew up against, and still the reference for
everything on the air — so a stock node also works, with the caveats below.

## Layout

| Path | What it is |
| --- | --- |
| `crates/wartui-proto` | `no_std` wire formats, shared by the host and the bridge firmware |
| `crates/wartui-bridge` | Host-side link to the dongle: transport, port discovery, simulator |
| `crates/wartui-core` | Headless fleet engine, SQLite store, position, WiGLE export |
| `crates/wartui` | The TUI binary, and the `sniff` / `status` / `ports` commands |
| `firmware/bridge` | Rust firmware for the dongle (own workspace, own target) |
| `firmware/node` | Rust firmware for the nodes: sniffs, reports, takes assignments |
| `tools/espnow-sniffer` | Passive Arduino sniffer for bring-up and frame capture |
| `tools/golden` | Emits ground-truth struct layouts using the firmware's own typedefs |
| `tools/beacons` | Turns a monitor-mode capture into fixtures for the beacon parser |

## Scope

Nodes accept exactly one command over the air — `MSG_ADMIN`, which says which
channels to scan and whether to scan Bluetooth. There is no start/stop, config,
reboot or query message, and node serial is output-only. So wartui monitors the
fleet, collects and exports observations, and controls channel and Bluetooth
assignment. Anything else would need changes to the node firmware — which, now
that the node firmware is in this repository, is a thing that can happen.

That frame is **wartui's own**, and has been since Phase 2. It is fourteen bytes
carrying a forty-bit channel mask; the vendor core's ten-byte version, which
carried a pair of bounds, is no longer spoken. A stock node will not understand
wartui and wartui will not drive one. If a stock core is powered up nearby it is
still recognised — `sniff` names its frames and the fleet view counts them —
because a second core assigning these nodes channels is an operational hazard
whether or not the two interoperate.

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

Which channels the fleet scans is selectable. A wartui node never transmits
while it is looking: it parks its radio and reads beacons, so the pool bounds
where it *listens*. A stock node does transmit — every `WiFi.scanNetworks` in
the vendor firmware passes `passive = false` — and sends a probe request on each
channel it is assigned, including the DFS channels where the rules say
otherwise. On a stock fleet the pool is what keeps that in check.

- **US** (`--pool us`, the default) — 2.4 GHz 1–11 and 5 GHz 36–165.
- **All** (`--pool all`) — every channel a node can tune: 2.4 GHz 1–13 and all of
  5 GHz, including the UNII-4 channels 169, 173 and 177 that `us` leaves out.

**Channel 14 is unsupported and is in neither pool.** `esp-radio` hardcodes the
country blob's `nchan: 13` and exposes no way to reach it, so a node handed that
channel refuses the hop — once per sweep, every sweep, for as long as it holds
the assignment, and it says so only on a serial console nobody is watching.
`docs/phase-1-findings.md` has the measurement and the reading of the driver.
It stays in the node's scan table because that table's indices are the wire
format; it is simply never dealt.

`MSG_ADMIN` carries a forty-bit channel mask, so an assignment can name any
subset of the pool and the gap at channels 12–14 is not something the planner
has to steer around. It deals the pool out round-robin instead: index *k* of the
pool goes to node *k mod n*, so every node carries some 2.4 GHz and some 5 GHz.
Block-splitting would put one node on the whole of 2.4 GHz and another on the
whole of 5 GHz, and losing that node would blind the fleet to a band until the
next re-cut landed.

## Trying it

No hardware needed — the simulator runs a fake fleet on a fake clock:

```sh
cargo run -p wartui -- --sim 3 --lat 37.7749 --lon -122.4194
```

The simulated nodes model the parts of the firmware that matter: they park doing
nothing until they are assigned, they adopt an assignment only when its version
differs, they dedup against a 200-entry ring, only the node given the Bluetooth
assignment reports any, and their sweep — and so their heartbeat period — is
proportional to how many channels they hold. Pressing `a` or `b` on one does
what it would do to real hardware.

The fake neighbourhood does not move, so the observation stream goes quiet once
every node has reported everything on its own channels — a real fleet parked in
one room does the same, and the dedup ring is why. Press `b` on a node to give
it the Bluetooth scan: advertisers rotate their addresses, so they never dedup
and the stream stays alive.

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
selected node channels by hand. `--manual` starts with the planner off, and
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
| `a` | Give the selected node exactly **one** channel |
| `A` | Give it the whole pool |
| `b` | Move the Bluetooth scan to it, or take it off the fleet |
| `p` | Take the fleet back from the planner, or hand it over again |

The header says which of you is deciding: `manual`, or `auto — 4 of 5` for four
heartbeating nodes out of five seen. It starts on `auto`, so **`a` and `A` are
refused until you press `p`** (or started with `--manual`) — the planner would
honour a hand-assigned set and then take it back at the next re-cut, which reads
as the key having been ignored. `b` is not refused: the planner partitions
channels and has no opinion about Bluetooth, so there is nothing for it to take
back.

Nothing goes out at the moment the key is pressed. A node's radio is away
scanning some other channel for all but the 300 ms it holds open after its own
heartbeat, so the assignment waits for that window — the `channels` column reads
`1: 1…` until it lands, then drops the ellipsis. On a full sweep that is up to
four seconds. That delay is the protocol, not lag.

That column leads with a count because a share dealt round-robin is a dozen
scattered channels and no sane column is wide enough for all of them. The count
is the useful half anyway: it is what the `beat` column should be proportional
to.

An assignment is believed only when the node's own radio acknowledges it at the
MAC layer, never when the bridge reports a successful enqueue. The vendor core
cannot tell those apart, which is why it can sit with a node it believes is
assigned and is not.

`a` is also the Phase 4 proof that any of this works. Take the fleet back with
`p` first, or start with `--manual` so nothing has been assigned yet. A node
heartbeats once per completed sweep and reports nothing about what it is
scanning, so watch the `beat` column: narrowing a node from the whole pool to
one channel should collapse it from seconds to a fraction of one within three
sweeps. `A` puts it back.

If a node keeps showing `no admin ack`, the cause is nearly always BLE: the
Bluetooth and Wi-Fi radios share the one 2.4 GHz antenna, and the admin window
is precisely when the node would otherwise be idle. On a stock node, turn BLE
off in its web UI. On a wartui node, press `b` on it — see below.

### Bluetooth

**At most one node scans Bluetooth, and by default none does.** `b` on the
selected node moves the scan to it; `b` again on the node that holds it takes it
off the fleet. The `ble` column says who has it, and reads `on…` or `off…` while
a change is waiting for that node's next admin window, the same way the
`channels` column does.

It is a per-node choice rather than a build flag because the cost is real and
was measured on both firmwares. A stock node with BLE on acknowledged **none**
of the thirty-two assignments sent to it, while an identical node with it off
acknowledged both of its two (`docs/phase-0-findings.md`). A wartui node
acknowledged every time on the same board — including the frame that took the
scan away, mid-scan, in 5.8 ms — and ran 10.9% slower per sweep while it held it
(`docs/phase-2-findings.md`): 5.221 s against 4.709 s either side, on the same
thirty-four channels. A cost worth paying on one node for Bluetooth coverage,
and not worth paying on all of them.

The `ble` cargo feature decides whether the code is in the binary at all; the
assignment decides whether it runs, and it is off at every boot regardless of
the build. A node whose firmware was compiled with Bluetooth is not a node that
is scanning it.

### Letting wartui assign them

This is on by default, and it is wartui doing the core's whole job: it deals the
pool out across the fleet and re-cuts it whenever the fleet changes shape.
`--manual` starts without it; `p` toggles it either way. It partitions channels
and nothing else — the Bluetooth assignment is yours, and survives a re-cut.

A node is in the plan while it is **heartbeating**. Not while it is merely being
heard — a node that has stopped heartbeating never opens an admin window, so a
share of the pool held open for it is a share nobody is scanning. It drops out
after the same 60 s the firmware uses, and whatever it was owed is dropped with
it.

Every change re-cuts the pool for the *whole* fleet, not just the node that
joined or left. `node_index` and `node_count` travel in every assignment and are
what each node computes its transmit stagger from (`src/RadioTuning.cpp:3-13`),
so a fleet whose members disagree about the count keys up on top of itself. Each
node takes its new share in its own next admin window, so a fleet converges in
about one sweep.

An unchanged fleet is left alone. A node adopts an assignment only when the
epoch differs from the one it holds, so re-sending one it already has is a frame
it acknowledges and then discards — indistinguishable from success. The planner
only speaks when it has something new to say.

**A lone node holds the whole pool.** It used not to: `MSG_ADMIN` carried one
contiguous range and the US pool is two runs, so a single node covered them in
turn on a 60-second dwell and half the pool went unscanned at any instant. The
channel mask says both runs in one frame, and the rotation — with its timer, its
phase in the header, and the fresh epoch it spent every minute — is gone.

**A node can report a channel that is not in its share.** The deal interleaves
2.4 GHz channels between nodes — one takes 1, 3, 5, the next 2, 4, 6 — and
2.4 GHz channels are 5 MHz apart but 20 MHz wide, so a node parked on 2 hears
beacons transmitted on 1 and 3. It reports the channel the beacon itself names,
which is the access point's real one; the alternative would be filing a real
network under the wrong frequency. Measured at two nodes on the US pool, about a
fifth of the access points were found by both
(`docs/phase-2-findings.md`). `export` picks one row per network, so this costs
store rows and nothing else. 5 GHz channels here do not overlap and do not do
it.

Over twenty nodes the planner stops re-cutting rather than partitioning among
nodes the bridge cannot address. Whatever is already assigned stays assigned,
capture is unaffected, and the footer says so. A single node the bridge has no
peer slot for — peers are never removed, so a long session can fill the table
with nodes that have since gone — leaves the plan the same way, and the rest
re-cut to cover its share. It is tried again the next time a bridge announces
itself, since that table starts empty.

Assigning channels by hand while the planner is running is refused — it would be
honoured and then taken back at the next re-cut, which reads as the key having
been ignored. Press `p` first; since the planner is what wartui starts with, that is
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
| `alive` | Heartbeating, so it can be given channels |
| `stale` | Still being heard, but not heartbeating — most often BLE coexistence on the node holding the radio through its admin window |
| `no heartbeat` | Seen, but has never completed a sweep |
| `no admin ack` | An assignment went out and its radio did not answer — nearly always BLE, see [Assigning channels](#assigning-channels) |
| `refused` | The bridge would not transmit it. Nearly always a full peer table, which means the fleet is over twenty nodes |
| `rebooted xN` | Its heartbeat counter went backwards, so it has forgotten any assignment; wartui re-issues under a fresh epoch |
| `encrypted` | It is sending core-protocol frames. wartui cannot talk to it; turn encryption off in that node's web UI. A wartui node never sends these |

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
port opened and the dongle is not answering. In that second case reset it with
`espflash reset --port …`, which is what `sniff` and `status` print after
five seconds of silence; unplugging and replugging does the same thing more
bluntly.

```sh
cargo run -p wartui -- status                  # exits in 5 s with the reason
cargo run -p wartui -- --log-file wartui.log run
```

`--log-file` is the only way to see the transport's own account of a run: the
view owns the terminal, so without it nothing is logged anywhere. It records
which port was resolved, whether it opened, and the reason a link went down —
once per reason rather than once per retry, since a port that is somebody else's
is retried every 750 ms for as long as the capture runs. The GPS reader thread
reports itself the same way. `RUST_LOG=debug` adds each individual retry, every
frame that would not decode, and dropped bulk commands.

If the same port keeps being the wrong device — a C5 node plugged in by USB
looks identical to the bridge, same vendor and product ID — pin it with
`--port`. `wartui ports` lists the candidates.

## Development

```sh
cargo test --workspace
cargo clippy --workspace --all-targets
cargo fmt --check

cd firmware/bridge && cargo clippy --release --features esp32c6   # and esp32c5
cd firmware/node   && cargo clippy --release --features esp32c6   # and with ,ble
```

Each firmware is excluded from the workspace and is its own: a different target,
its own toolchain pin and its own lockfile. Both take a path dependency up into
`crates/wartui-proto`, which is why those wire types are `no_std` — and why the
node's beacon parser, dedup ring and HCI packets live there too, where
`cargo test` reaches them and a fix costs a `cargo run` rather than a reflash.

The wire codec is tested byte-for-byte against layouts produced by a real C++
compiler; see `tools/golden`. Frames captured off the air by
`tools/espnow-sniffer` can be pasted into
`crates/wartui-proto/tests/golden_vectors.txt` to become regression tests, and
802.11 beacons pulled out of a monitor-mode capture by `tools/beacons` go into
`tests/beacon_vectors.txt` the same way — scrubbed of addresses and network
names on the way, because a capture of the air around you locates you as surely
as anything this project collects on purpose.
