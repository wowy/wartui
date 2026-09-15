# wartui — the CLI and the view

The clap CLI and the ratatui fleet view. Both are front ends over
`wartui-core`, which draws nothing and parses no arguments: the fleet's
behaviour is decided there and tested without a terminal, and this crate is the
conversation with the operator.

This is the operator's manual. The root [`README.md`](../../README.md) is the
short version.

## Commands

`run` is the default, so the subcommand can be left off.

| Command | What it does |
| --- | --- |
| `run` | Capture a fleet into the store and watch it live |
| `export` | Write a WiGLE CSV from a capture |
| `sniff` | Print every frame the bridge hears, decoded |
| `status` | Ask the bridge for its channel, counters and uptime |
| `reset` | Reboot a bridge that has stopped answering |
| `ports` | List serial ports that look like an Espressif device |

`run` takes:

| Flag | Default | What it is |
| --- | --- | --- |
| `--db PATH` | `wartui.db` | Where to keep the capture |
| `--port PATH` | discovered | Serial port of the bridge |
| `--channel N` | `6` | The fleet's ESP-NOW control channel |
| `--pool us\|all` | `us` | Which channels the fleet should scan |
| `--manual` | off | Do not partition the pool; nothing goes out unasked |
| `--lat` `--lon` `--alt` | — | A static position for every observation |
| `--gps PATH` | — | An NMEA receiver, preferred over `--lat`/`--lon` |
| `--gps-baud N` | `9600` | Line rate of that receiver |
| `--gps-max-age S` | `5` | How old a fix may be before falling back |
| `--sim N` | — | Run a fake fleet instead of hardware |
| `--sim-c6 N` | `0` | Make that many of them C6s, from the end of the fleet |
| `--record-raw` | off | Also keep the undecoded bytes of every frame |
| `--notes TEXT` | — | A note about this run, stored with the session |

`export` takes `--db`, `--wigle PATH` and `--session ID`, and writes the
[WiGLE v1.6 format](https://api.wigle.net/csvFormat.html). It is a view over the
store rather than a second copy of it, so it can be re-run after a decoder fix,
against a session that ended last week, or against one still going.
`--log-file` is global and is the only way to see the transport's own account of
a run.

### Benchmarking the store

`bench` is hidden, and is for judging a change to the store on the card it will run
from. It drives the simulator through the engine into a fresh database with nothing
drawn, then reports rows, commit times, and on Linux what the kernel and the block
device actually wrote:

```sh
wartui bench --db /path/on/the/card/bench.db --fresh --duration 300 --json
```

`--profile drive` (the default) is ten nodes at real time; `--profile burst` is twenty.
The simulated neighbourhood is sized so every node reports on every sweep, measuring
starts once the whole fleet holds its assignments, and `idle_node_windows` must read 0
for a run to count. `--interval` (60 s by default) adds a timeline to the report, so a
long run shows when the card slowed down. Each SQLite and batching setting has a flag,
so two settings compare on one binary. `--defer-bssid-index` captures without the
`obs_bssid` index and builds it once at close, and `--checkpoint-every MS` copies the WAL
back from a thread of its own instead of inside a commit. [`docs/store-io-findings.md`](../../docs/store-io-findings.md)
has the method and the numbers so far.

## Assigning channels

| Key | What it does |
| --- | --- |
| `↑` `↓` / `k` `j` | Move the cursor down the fleet table |
| `a` | Give the selected node exactly **one** channel |
| `A` | Give it the whole pool |
| `b` | Move the Bluetooth scan to it, or take it off the fleet |
| `p` | Take the fleet back from the planner, or hand it over again |
| `q` / `Esc` / `ctrl-c` | Stop, committing the last batch |

The header says which of you is deciding: `manual`, or `auto — 4 of 5` for four
heartbeating nodes out of five seen. It starts on `auto`, so **`a` and `A` are
refused until you press `p`** (or started the capture with `--manual`, in which case
`p` would hand the fleet *to* the planner rather than take it back — it is a toggle
against whatever the header says). The planner would honour a hand-assigned set and
then take it back at the next re-cut, which reads as the key having been ignored.
`b` is not refused: the planner partitions channels and has no opinion about
Bluetooth.

Nothing goes out at the moment the key is pressed. A node's radio is away
scanning some other channel for all but the 100 ms it holds open after its own
heartbeat, so the assignment waits for that window — the `channels` column reads
`1: 1…` until it lands, then drops the ellipsis. On a full sweep that is up to
four seconds. That delay is the protocol, not lag.

That column leads with a count because a share dealt round-robin is a dozen
scattered channels and no sane column is wide enough for all of them. The count
is the useful half anyway: it is what the `beat` column should be proportional
to. Narrowing a node from the whole pool to one channel should collapse its
`beat` from seconds to a fraction of one within three sweeps, which is the only
evidence available that an assignment was adopted rather than merely
acknowledged — a node heartbeats once per completed sweep and reports nothing
about what it is scanning.

An assignment is believed only when the node's own radio acknowledges it at the
MAC layer, never when the bridge reports a successful enqueue.

If a node keeps showing `no admin ack`, the cause is nearly always Bluetooth:
the two radios share the one 2.4 GHz antenna, and the admin window is precisely
when the node would otherwise be idle. Press `b` on it.

## Bluetooth

**At most one node scans Bluetooth, and by default none does.** `b` on the
selected node moves the scan to it; `b` again on the node that holds it takes it
off the fleet. The `ble` column says who has it, and reads `on…` or `off…` while
a change waits for that node's next admin window, the same way `channels` does.

It is a per-node choice rather than a build flag because the cost is real: a node
holding the scan runs about 10% slower per sweep, measured in
[`docs/phase-2-findings.md`](../../docs/phase-2-findings.md). Worth paying on one
node for Bluetooth coverage; not worth paying on all of them.

The `ble` cargo feature decides whether the code is in the binary at all; the
assignment decides whether it runs, and it is off at every boot regardless of the
build. **`b` is refused on a node built without the feature**, which its
heartbeat says. Nothing about the frame would fail — it would adopt the flag,
acknowledge, and scan nothing — so the `ble` column would name a holder that is
not one.

## Channel pools

`--pool us` (the default) is 2.4 GHz 1–11 and 5 GHz 36–165. `--pool all` is
every channel a node can tune: 2.4 GHz 1–13 and all of 5 GHz, including the
UNII-4 channels 169, 173 and 177. Channel 14 is in neither and is never dealt —
`esp-radio` exposes no way to reach it.

An assignment carries a forty-bit channel mask, so it can name any subset of the
pool. The planner deals the pool out round-robin: index *k* of the pool goes to
node *k mod n*, so every node carries some 2.4 GHz and some 5 GHz.
Block-splitting would put one node on the whole of 2.4 GHz and another on the
whole of 5 GHz, and losing that node would blind the fleet to a band until the
next re-cut landed.

**An ESP32-C6 is never dealt a 5 GHz channel.** It has no radio for one and says
so in every heartbeat, so a share of 5 GHz cut for it would be a share nobody
scans. A mixed fleet is dealt the 5 GHz half first for that reason: in pool order
the C5s would take their share of 2.4 GHz and then all of 5 GHz on top of it,
which is the block split above arrived at sideways. Shares are then no longer
within one channel of each other and cannot be — a C6 beside a C5 holding 5 GHz
sweeps faster however the rest is dealt. What the deal minimises is the
*largest* share, which is what sets how stale the slowest node's observations
get. A fleet whose radios are all alike has no constrained channels.

If no node in the fleet has a 5 GHz radio at all, those channels are left out of
every assignment rather than given to a node that would ignore them, and the
footer says how many of the pool are going unscanned.

Assigning by hand is cut down the same way: `A` offers the whole pool, so the set
is narrowed to what that node's heartbeat says its radio can reach, and the
notice names the narrowed set rather than what was asked for. With the planner
off, nothing re-partitions afterwards to notice.

Every fleet change re-cuts the pool for the *whole* fleet, not just the node that
joined or left. `node_index` and `node_count` travel in every assignment and are
what each node computes its transmit stagger from, so a fleet whose members
disagree about the count keys up on top of itself. Each node takes its new share
in its own next admin window, so a fleet converges in about one sweep. An
unchanged fleet is left alone: a node adopts an assignment only when its epoch
differs from the one it holds.

Two consequences worth knowing:

- **A node can report a channel that is not in its share.** The deal interleaves
  2.4 GHz channels between nodes — one takes 1, 3, 5, the next 2, 4, 6 — and
  those channels are 5 MHz apart but 20 MHz wide, so a node parked on 2 hears
  beacons transmitted on 1 and 3. It reports the channel the beacon itself names,
  which is the access point's real one; the alternative would be filing a real
  network under the wrong frequency. About a quarter of access points are found
  by more than one node. `export` picks one row per network, so this costs store
  rows and nothing else. 5 GHz channels do not overlap and do not do it.
- **A fleet with no 5 GHz radio in it can cover only 11 channels on `us`**, so
  from twelve such nodes onward there are more nodes than channels to give them.
  The surplus nodes keep whatever they last held rather than being told to scan
  nothing — there is no frame that means that — so their shares double up with
  someone else's.

## Positions

Every observation is stamped with the best position available, resolved fresh
each time: a GPS on `--gps`, then a static `--lat`/`--lon`, then nothing. Which
tier answered is recorded per row, so a capture that starts in a garage and ends
on a road is honest about both halves. A record is never dropped for want of a
position — but **WiGLE will not accept a row without coordinates**, so a capture
with no position given exports nothing and says how many networks it left out.

```sh
wartui run --db drive.db --gps /dev/ttyUSB0 --lat 37.7749 --lon -122.4194
```

Giving both is the useful combination: satellite positions whenever the receiver
has one, the typed-in position the rest of the time, rather than nothing at all
while the receiver is still finding itself.

`--gps` takes any receiver that speaks NMEA 0183 over a serial port. `GGA` and
`RMC` are read and everything else ignored; the altitude, the satellite count and
an accuracy estimated from the reported HDOP all reach the export. `--gps-baud`
defaults to 9600, which is what most receivers ship at — u-blox modules are often
38400, and the wrong rate shows up in the footer as unreadable lines with no fix
rather than as silence.

**A fix has to be recent to be used.** Past `--gps-max-age` seconds the position
falls back to the tier below and the header says `gps fix is stale`, because at
driving speed a minute-old fix is a different neighbourhood and a row that
quietly claimed it would be worse than one admitting to the static position.

The receiver runs on its own thread and nothing waits for it: a capture starts
immediately, reconnects on its own if the puck is unplugged and put back, and
says what it is doing on the header — `gps searching`, `gps ok, 8 sats`,
`gps fix is stale`, or the error from the port.

## Reading the fleet table

| State | Meaning |
| --- | --- |
| `alive` | Heartbeating, so it can be given channels |
| `stale` | Still being heard, but not heartbeating — most often Bluetooth coexistence on the node holding the radio through its admin window |
| `no heartbeat` | Seen, but has never completed a sweep |
| `no admin ack` | An assignment went out and its radio did not answer — nearly always Bluetooth, see [Assigning channels](#assigning-channels) |
| `refused` | The bridge would not transmit it — nearly always a full peer table. Its heartbeats are still arriving; what is missing is a slot to address it through |
| `rebooted xN` | Its heartbeat counter went backwards, so it has forgotten any assignment; wartui re-issues under a fresh epoch |

A node that is `stale`, `refused` or `no heartbeat` is out of the plan, and for
the same reason it cannot be assigned by hand: nothing wartui sends it would
reach it, or nothing yet says which band its radio can tune — so a share of the
pool cut for it is a share that may be nobody's. Pressing `a`, `A` or `b` on one
says which of the three it is, because the next move differs for each: wait for the
first heartbeat, clear the peer table, or go and find out why the heartbeats stopped.

**`refused` is usually not a fleet above twenty nodes.** The bridge never removes a
peer, so a long session accumulates slots for nodes that have since gone, and a
fleet of three can run out of room. The table starts empty on every boot, so the
host clears the refusal whenever a bridge announces itself: `wartui reset` — or
replugging — is what to try before counting nodes. The rest of the fleet is re-cut
to cover a refused node's share in the meantime, and over twenty nodes the planner
stops re-cutting altogether and the header says `auto — too many nodes`.

`stale` and `no heartbeat` are deliberately distinct from silence, and from each
other. A node streaming observations whose heartbeats are lost would otherwise
age out and churn the whole fleet's topology; wartui keeps two clocks so the
difference is visible rather than fatal.

An `alive` node that has stopped reporting is usually not broken: a node reports
an address once and then holds it back, so a node that is standing still goes quiet
once it has reported everything in range. That memory is on the node, not the host,
so it carries over into your next session. A held address is reported again five
minutes after it was last reported, or sooner if the node hears it at least 10 dB
louder than it has reported it before. Rebooting a node clears the memory
(`crates/wartui-proto/src/dedup.rs` explains why it works this way).

The header has three ways of saying it has nothing to drive:
`auto — nothing heartbeating yet`, `auto — no node it can drive` (nodes are
alive but none is assignable), and `auto — too many nodes` (over twenty, so the
planner has stopped re-cutting; whatever is assigned stays assigned and capture
is unaffected).

The footer only shows faults once they have happened, so a clean run reads as a
clean footer. Three entries there are about somebody else's equipment rather than
yours: `N frames from a vendor fleet` and `N admin frames from another core` are
another fleet transmitting on the channel these nodes listen on, and
`N frames from an older firmware — reflash` is a fleet part-way through an
upgrade — those nodes speak a wire format this host does not, so the footer is
the only place they appear. `bridge dropped` counts frames lost since this host
attached; `wartui status` reports the bridge's own total since it booted, which
on a dongle left powered with nothing listening is large and not a fault.

## When nothing arrives

The header says `waiting for a bridge to announce itself` for two quite different
reasons, and the fault box says which. `link down: could not open …` means the
port is not ours — nearly always another `wartui`, a `screen` session or an IDE's
serial monitor still holding it. No fault at all means the port opened and the
dongle is not answering.

In that second case the bridge is usually not dead but deaf in one direction: its
USB transmit endpoint has stopped draining while it goes on reading every frame
you send it. The firmware notices within three seconds and reboots itself, so
this should clear on its own and show up afterwards as
`bridge rebooted itself: USB transmit had stalled`. If it does not, `wartui reset`
asks it to reboot, which works because the receive path is the half that still
runs — and keeps the device path, where `espflash reset --port …` re-enumerates
the board and can move `ttyACM0` to `ttyACM1` under a script that named it.
`espflash` is the fallback for a bridge that answers nothing at all, and
unplugging is the last resort.

```sh
wartui status                          # exits in 5 s with the reason
wartui reset                           # reboot a bridge that stopped answering
wartui --log-file wartui.log run
```

`--log-file` is the only way to see the transport's own account of a run: the
view owns the terminal, so without it nothing is logged anywhere. It records
which port was resolved, whether it opened, and the reason a link went down —
once per reason rather than once per retry, since a port that is somebody else's
is retried every 750 ms for as long as the capture runs. The GPS reader thread
reports itself the same way. `RUST_LOG=debug` adds each individual retry, every
frame that would not decode, and dropped bulk commands.

### Telling the boards apart

If the same port keeps being the wrong device — a node plugged in by USB looks
identical to the bridge, same vendor and product ID — pin it with `--port`.
`wartui ports` lists the candidates but cannot say which is which, and probing
answers one port at a time and slowly: pointed at a node, `wartui status` waits five
seconds for a link protocol the node does not speak and tells you only that this one
is not the bridge.

Ask the USB tree instead. An ESP32's serial number *is* its MAC, so the OS
already knows, with no esp tool, no reflash and without opening anything. On
Linux, udev has paired them:

```sh
for l in /dev/serial/by-id/usb-Espressif_*; do
  mac=${l##*unit_}; printf '%s\t%s\n' "$(readlink -f "$l")" "${mac%-if00}"
done
```

On macOS the same fact comes out of `ioreg`. The two properties sit on different
nodes of the tree and come out in either order, so they are paired as they arrive
rather than assumed adjacent:

```sh
ioreg -l -w0 | LC_ALL=C awk -F'"' '
  BEGIN { printf "%-24s %s\n", "PORT", "ADDRESS" }
  /USB Serial Number/ && $4 ~ /^([0-9A-Fa-f]{2}:){5}[0-9A-Fa-f]{2}$/ { mac = $4 }
  /IOCalloutDevice/ && $4 ~ /usbmodem/ { dev = $4 }
  mac != "" && dev != "" { printf "%-24s %s\n", dev, mac; mac = dev = "" }
'
```

```
PORT                     ADDRESS
/dev/cu.usbmodem2101     02:00:5E:10:9D:24
/dev/cu.usbmodem142201   02:00:5E:10:4F:98
```

The address shape is what picks the ESP32s out: everything else on the bus
carries a manufacturing serial. The bridge is then the row whose address the
fleet table shows as the bridge's, and a node the row whose heartbeats
`wartui sniff` attributes to that address. Where the board generations differ,
the OUI separates them too. An S3 in the list is a bridge and never a node.
