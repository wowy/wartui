# wartui — the CLI and the view

The clap CLI and the ratatui fleet view. Both are front ends over `wartui-core`, which draws nothing
and parses no arguments: the fleet's behaviour is decided there and tested without a terminal, and
this crate is the conversation with the operator.

This is the operator's manual. The root [`README.md`](../../README.md) is the short version.

## Commands

`run` is the default, so the subcommand can be left off.

| Command  | What it does                                                |
| -------- | ----------------------------------------------------------- |
| `run`    | Capture a fleet into the store and watch it live            |
| `export` | Write a WiGLE CSV from a capture                            |
| `sniff`  | Print every frame the bridge hears, decoded                 |
| `status` | Ask the bridge for its channel, counters and uptime         |
| `reset`  | Reboot a bridge that has stopped answering                  |
| `ports`  | List the Espressif boards attached, and the address of each |

`run` takes:

| Flag                    | Default      | What it is                                                   |
| ----------------------- | ------------ | ------------------------------------------------------------ |
| `--db PATH`             | dated        | Where to keep the capture                                    |
| `--bridge PATH\|MAC`    | detected     | Which board the bridge is, by path or by address             |
| `--channel N`           | `6`          | The fleet's ESP-NOW control channel                          |
| `--pool us\|eu\|all`    | `all`        | Which channels the fleet should scan                         |
| `--lat` `--lon` `--alt` | —            | A static position for every observation                      |
| `--gps PATH`            | detected     | An NMEA receiver, preferred over `--lat`/`--lon`             |
| `--no-gps`              | off          | Do not look for a receiver at all                            |
| `--gps-baud N`          | detected     | Line rate of that receiver                                   |
| `--gps-max-age S`       | `5`          | How old a fix may be before falling back                     |
| `--sim N`               | —            | Run a fake fleet instead of hardware                         |
| `--sim-c6 N`            | `0`          | Make that many of them C6s, from the end of the fleet        |
| `--record-raw`          | off          | Also keep the undecoded bytes of every frame                 |
| `--node-tx-power DBM`   | `2`          | Wi-Fi transmit power for the nodes                           |
| `--bridge-tx-power DBM` | `2`          | Wi-Fi transmit power for the bridge                          |
| `--commit-interval MS`  | `1000`       | How often the store commits; a crash loses at most this much |
| `--notes TEXT`          | —            | A note about this run, stored with the session               |

`--db` names the capture for the minute the run started — `wartui-2026-09-18-14-30.db` — so a
directory of them sorts into the order they were made rather than being one file every run appends
to. The date is in ISO order whatever the locale reading it: that is what makes them sort, and what
lets `export` pick out the last one. Two runs begun inside the same minute share a name, and the
second adds its session to the first one's file.

`--node-tx-power` is how loudly the nodes transmit — their heartbeats and sightings — in whole dBm,
2 to 20, 2 by default. `--bridge-tx-power` sets the bridge's assignments the same way, completely
independent: neither flag falls back to the other, and each defaults to 2 dBm on its own. 20 dBm is
the ceiling: the firmware would accept 21, but whether anything above 20 works correctly is
unverified. Either can also be set in `wartui.toml`; see "Config file" below for precedence.

`export` takes `--db`, `--out PATH` and `--session ID`, and writes the [WiGLE v1.6
format](https://api.wigle.net/csvFormat.html). Both ends default, so exporting the evening that just
ended is `wartui export` and nothing else: without `--db` it opens the newest capture in the working
directory, which is the one a finished `run` left there, and without `--out` it writes beside that
capture under the same name — `wartui-2026-09-18-14-30.db` exports to
`wartui-2026-09-18-14-30.csv`. A name arrived at that way is never written over, since the file it
would replace may be the export already uploaded: `--out PATH` (short `-o`) names another, or the
same one again to mean it, and `--out -` writes to standard output. The CSV is a view over the store
rather than a second copy of it, so it can be re-run after a decoder fix, against a session that
ended last week, or against one still going.

`--recapture SECONDS` (default 3600 — one hour) folds each network's sightings into windows that
wide and writes one row per window: the strongest positioned sighting, with `FirstSeen` from the
window's own first sighting. WDGWars' scan cooldown is one hour per user and MAC — a re-scan within
the hour is silently skipped from scoring, though its GPS may still refine the entry — so the window
is that rule exactly: a re-hearing within the hour stays in the row, where its stronger reading can
still improve the position, and one past the hour is a second row the site scores. `--recapture 0`
folds a network's whole capture into a single row.

Rows carry what the nodes took off the air: a Passpoint access point's roaming consortium
identifiers in `RCOIs`, a BLE advertiser's manufacturer identifier in `MfgrId`, both blank when the
network offered none — the store records NULL rather than a guess. A BLE row's `Frequency` is blank
on purpose: the column means a Bluetooth "device type" code that only an active inquiry produces,
and the nodes never transmit while scanning.

`--log-file` is global and is the only way to see the transport's own account of a run.
`--config PATH` is global too, and names a `wartui.toml` somewhere other than the default
location; see "Config file" below.

### Config file

`wartui.toml` holds settings an operator wants to stop typing every run. It is read only for
`run`, so a broken config cannot stop `ports`, `status` or `reset` from working. It is edited by
hand.

The default location is per OS:

| OS               | Path                                               |
| ---------------- | -------------------------------------------------- |
| macOS            | `~/Library/Application Support/wartui/wartui.toml` |
| Linux and others | `$XDG_CONFIG_HOME/wartui/wartui.toml`, or `~/.config/wartui/wartui.toml` when that variable is unset or relative |

`--config PATH` (global, so it can go before or after a subcommand) reads a file somewhere else
instead, for testing and debugging. Naming a file that does not exist
is refused; a missing default file is not — an operator who has never written one gets the
built-in defaults, silently.

Today it holds one table:

```toml
[tx-power]
fleet = 10    # dBm, nodes
bridge = 15   # dBm, bridge
```

`fleet` and `bridge` are independent, the same as `--node-tx-power` and `--bridge-tx-power` are.
Precedence is flag, then file, then default, resolved separately for each: `--node-tx-power` beats
`fleet`, which beats 2 dBm; `--bridge-tx-power` beats `bridge`, which beats 2 dBm. Neither value
falls back to the other's.

An unknown key or table makes wartui refuse to start, naming the file and the line; an
out-of-range value does too, naming the file and the key — the same way an out-of-range
`--node-tx-power` is refused.

### Benchmarking the store

`bench` is hidden, and is for judging a change to the store on the card it will run from. It drives
the simulator through the engine into a fresh database with nothing drawn, then reports rows, commit
times, and on Linux what the kernel and the block device actually wrote:

```sh
wartui bench --db /path/on/the/card/bench.db --fresh --duration 300 --json
```

`--profile drive` (the default) is ten nodes at real time; `--profile burst` is twenty. The
simulated neighbourhood is sized so every node reports on every sweep, measuring starts once the
whole fleet holds its assignments, and `idle_node_windows` must read 0 for a run to count.
`--interval` (60 s by default) adds a timeline to the report, so a long run shows when the card
slowed down. Each SQLite and batching setting has a flag, so two settings compare on one binary. The
store checkpoints from a thread of its own right after each commit; `--checkpoint-every MS` spaces
those passes out, and `--inline-checkpoint` puts SQLite's own back inside the commit for comparison.
[`docs/store-io-findings.md`](../../docs/store-io-findings.md) has the method and the numbers so
far.

## At the keyboard

| Key                    | What it does                                                           |
| ---------------------- | ---------------------------------------------------------------------- |
| `↑` `↓` / `k` `j`      | Move the cursor down the fleet table                                   |
| `b`                    | Make the selected node the Bluetooth scanner, or take the scan off    |
| `q` / `Esc` / `ctrl-c` | Stop, committing the last batch                                        |

`b` is the only one that reaches the air, and it decides _which_ node scans Bluetooth instead of
Wi-Fi. It reaches that node's share only by being an input to the planner, which is still the only
author of one. Channels are not a key.

## How channels are assigned

**The planner cuts the pool, and nothing else does.** It deals it across every heartbeating node
that is sniffing and re-cuts it whenever that set changes; no key and no flag writes one node's
share. `b` changes the set rather than the shares — see § "Bluetooth". The header says
what it has to work with: `auto — 4 of 5` for four heartbeating nodes out of five seen.

Nothing goes out at the moment a node's share changes. Its radio is away scanning some other channel
for all but the 100 ms it holds open after its own heartbeat, so the assignment waits for that
window — the `channels` column reads `1: 1…` until it lands, then drops the ellipsis. On a full
sweep of the default pool that is up to about five seconds, and four and a half on `us`. That delay
is the protocol, not lag.

That column leads with a count because a share dealt round-robin is a dozen scattered channels and
no sane column is wide enough for all of them. The count is the useful half anyway: it is what the
`beat` column should be proportional to. Bringing a second node up halves the first one's share, and
its `beat` should halve with it within three sweeps — the only evidence available that an assignment
was adopted rather than merely acknowledged, since a node heartbeats once per completed sweep and
reports nothing about what it is scanning. The Bluetooth node is the exception: it sweeps nothing,
so its `beat` reads a flat 1.0 s, which is the same kind of evidence and proportional to nothing.

An assignment is believed only when the node's own radio acknowledges it at the MAC layer, never
when the bridge reports a successful enqueue.

If a node keeps showing `no admin ack`, the cause is nearly always Bluetooth: the two radios share
the one 2.4 GHz antenna, and the admin window is precisely when the node would otherwise be idle.
Press `b` on it — and note that the node holding the scan is the least likely to show this, because
its Wi-Fi radio never leaves the control channel.

## Bluetooth

**At most one node scans Bluetooth, by default none does, and it is that node's whole job.** `b` on
the selected node gives it the scan; `b` again on the node that holds it takes it off the fleet. The
`ble` column says who has it, and reads `on…` or `off…` while a change waits for that node's next
admin window, the same way `channels` does — and that node's `channels` column reads `bluetooth`,
because it is dealt none.

It costs a whole node because the two radios share the one 2.4 GHz antenna, and a node that also
sweeps has to hand it back in time for every admin window it must answer in — which a stock node
did not, acknowledging none of thirty-two assignments
([`docs/phase-2-findings.md`](../../docs/phase-2-findings.md)). A node with nothing else to do has
nothing to hand it back to, so the scan runs **once a second** rather than once per sweep, and the
node's own transmits are the only thing it competes with.

The fleet is one sniffer short while it does, and the pool is re-cut across the rest the moment the
scan moves: giving it away grows every other node's share, and taking it back shrinks them again.
A fleet of **one** node holding the scan sweeps no Wi-Fi at all, which is a real hole and is
reported in the fault box rather than left to be inferred.

The `ble` cargo feature decides whether the code is in the binary at all; the assignment decides
whether it runs, and it is off at every boot regardless of the build. **`b` is refused on a node
whose heartbeat does not claim `ble`**, which covers a build without the feature and a build whose
Bluetooth controller would not start. Nothing about the frame would fail — such a node would adopt
the flag, acknowledge, and scan nothing — so the `ble` column would name a holder that is not one,
and the node would now be sniffing nothing either.

## Channel pools

`--pool all` is the default: every channel a node can tune, 2.4 GHz 1–13 and all of 5 GHz including
the UNII-4 channels 169, 173 and 177. The other two are narrower, and the choice is about coverage
rather than legality — a node parks and reads beacons, so a pool says where it listens and never
what it emits.

| Pool  | 2.4 GHz | 5 GHz  | Channels |
| ----- | ------- | ------ | -------- |
| `all` | 1–13    | 36–177 | 41       |
| `us`  | 1–11    | 36–165 | 36       |
| `eu`  | 1–13    | 36–140 | 32       |

`us` is what the FCC permits: no 12 or 13, and no UNII-4. `eu` is what ETSI permits: 2.4 GHz all the
way to 13, and 5 GHz stopping at 140, because channel 144's twenty megahertz run past the 5725 MHz
edge and 149 upwards is another band again. Channel 14 is in no pool and is never dealt —
`esp-radio` exposes no way to reach it.

An assignment carries a forty-two-bit channel mask, so it can name any subset of the pool. The
planner deals the pool out round-robin: index _k_ of the pool goes to node _k mod n_, so every node
carries some 2.4 GHz and some 5 GHz. Block-splitting would put one node on the whole of 2.4 GHz and
another on the whole of 5 GHz, and losing that node would blind the fleet to a band until the next
re-cut landed.

**An ESP32-C6 is never dealt a 5 GHz channel.** It has no radio for one and says so in every
heartbeat, so a share of 5 GHz cut for it would be a share nobody scans. A mixed fleet is dealt the
5 GHz half first for that reason: in pool order the C5s would take their share of 2.4 GHz and then
all of 5 GHz on top of it, which is the block split above arrived at sideways. Shares are then no
longer within one channel of each other and cannot be — a C6 beside a C5 holding 5 GHz sweeps faster
however the rest is dealt. What the deal minimises is the _largest_ share, which is what sets how
stale the slowest node's observations get. A fleet whose radios are all alike has no constrained
channels.

If no node in the fleet has a 5 GHz radio at all, those channels are left out of every assignment
rather than given to a node that would ignore them, and the footer says how many of the pool are
going unscanned.

Every fleet change re-cuts the pool for the _whole_ fleet, not just the node that joined or left.
`node_index` and `node_count` travel in every assignment and are what each node computes its
transmit stagger from, so a fleet whose members disagree about the count keys up on top of itself.
Each node takes its new share in its own next admin window, so a fleet converges in about one sweep.
An unchanged fleet is left alone: a node adopts an assignment only when its epoch differs from the
one it holds.

Two consequences worth knowing:

- **A node can report a channel that is not in its share.** The deal interleaves 2.4 GHz channels
  between nodes — one takes 1, 3, 5, the next 2, 4, 6 — and those channels are 5 MHz apart but 20
  MHz wide, so a node parked on 2 hears beacons transmitted on 1 and 3. It reports the channel the
  beacon itself names, which is the access point's real one; the alternative would be filing a real
  network under the wrong frequency. About a quarter of access points are found by more than one
  node. `export` folds each network's sightings into recapture windows and picks the strongest per
  window, so this costs store rows and nothing else. 5 GHz channels do not overlap and do not do it.
- **A fleet with no 5 GHz radio in it covers 2.4 GHz and nothing else**, which is 13 channels on
  `all` and `eu` and 11 on `us`, so from fourteen such nodes onward — twelve on `us` — there are
  more nodes than channels to give them. The surplus nodes keep whatever they last held rather than
  being told to scan nothing — the only frame carrying no channels is the Bluetooth node's, and it
  means "Bluetooth is the whole job" rather than "stop" — so their shares double up with someone
  else's. A node that has nothing to keep is the one exception: the node that *was* the Bluetooth
  scanner holds no channels at all, so taking the scan off it hands it everything its radio can
  reach instead. That node then sweeps the whole pool on its own and its `beat` says so.

## Positions

Every observation is stamped with the best position available, resolved fresh each time: a GPS on
`--gps`, then a static `--lat`/`--lon`, then nothing. Which tier answered is recorded per row, so a
capture that starts in a garage and ends on a road is honest about both halves. A record is never
dropped for want of a position — but **WiGLE will not accept a row without coordinates**, so a
capture with no position given exports nothing and says how many networks it left out.

```sh
wartui run --db drive.db --lat 37.7749 --lon -122.4194
```

**A receiver is found without being named.** Every serial port that is not one of the fleet's own
boards is listened to in turn, at 9600, then 38400, 4800 and 115200, and the first that produces two
sentences passing their checksum is the one read for the rest of the capture. A port that names
itself — `u-blox`, `GPS`, `GNSS` — is tried first, but that only decides the order: the common pucks
sit behind a general-purpose USB-to-UART chip that says nothing about what is behind it, so what
settles it is reading the port.

The search **writes nothing to any port**, and never opens an Espressif one. That is what keeps it
from meeting the other half of wartui, which is transmitting into whatever it opens while it looks
for the bridge.

Giving `--lat`/`--lon` as well is the useful combination: satellite positions whenever the receiver
has one, the typed-in position the rest of the time, rather than nothing at all while the receiver is
still finding itself.

`--gps PATH` pins one receiver, for when several are attached or the search settles on the wrong
device; the rates are still tried unless `--gps-baud` names one, and one sentence is enough to
settle a port you named rather than the two an unknown port has to produce. **Naming both opens the
port and reads it with no probe at all** — there is nothing left to detect, and a receiver
configured to say very little would otherwise be refused for saying too little inside one window.
`--no-gps` turns the search off, and `--sim` implies it unless `--gps` names a receiver: a
simulated fleet is how wartui is worked on with nothing plugged in, and a search that opens every
serial port on the machine is not part of that.
Any receiver that speaks NMEA 0183 over a serial port will do: `GGA` and `RMC` are read and
everything else ignored, and the altitude, the satellite count and an accuracy estimated from the
reported HDOP all reach the export.

**A receiver that is named and not found is a fault; one that was never found is not.** Searching is
the default, so most captures made without a GPS are captures where there was never going to be one,
and the header says nothing about it. `--gps` is the operator asking for a particular port, so its
absence is reported.

**A fix has to be recent to be used.** Past `--gps-max-age` seconds the position falls back to the
tier below and the header says `gps fix is stale`, because at driving speed a minute-old fix is a
different neighbourhood and a row that quietly claimed it would be worse than one admitting to the
static position.

The receiver runs on its own thread and nothing waits for it: a capture starts immediately, and says
what it is doing on the header — `gps scanning /dev/… @38400`, `gps searching`, `gps ok, 8 sats`,
`gps fix is stale`, or the error from the port. The search living on that thread is also what lets a
puck be unplugged and put back into a *different* socket: it comes back under another name, and the
next pass finds it there.

## Reading the fleet table

Each row names its node by chip and the last two octets of its address — `C5 57:84`, `C6 9D:24` —
read off the band its heartbeats announce, so a node not yet heartbeating shows `—` in place of the
chip.

| State          | Meaning                                                                                                                                                |
| -------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------ |
| `alive`        | Heartbeating, so it can be given channels                                                                                                              |
| `stale`        | Still being heard, but not heartbeating — most often Bluetooth coexistence on the node holding the radio through its admin window                      |
| `no heartbeat` | Seen, but has never completed a sweep                                                                                                                  |
| `no admin ack` | An assignment went out and its radio did not answer — nearly always Bluetooth, see [How channels are assigned](#how-channels-are-assigned)             |
| `refused`      | The bridge would not transmit it — nearly always a full peer table. Its heartbeats are still arriving; what is missing is a slot to address it through |
| `rebooted xN`  | Its heartbeat counter went backwards, so it has forgotten any assignment; wartui re-issues under a fresh epoch                                         |

A node that is `stale`, `refused` or `no heartbeat` is out of the plan: nothing wartui sends it
would reach it, or nothing yet says which band its radio can tune — so a share of the pool cut for
it is a share that may be nobody's. Pressing `b` on one says which of the three it is, because the
next move differs for each: wait for the first heartbeat, clear the peer table, or go and find out
why the heartbeats stopped.

**`refused` is usually not a fleet above twenty nodes.** The bridge never removes a peer, so a long
session accumulates slots for nodes that have since gone, and a fleet of three can run out of room.
The table starts empty on every boot, so the host clears the refusal whenever a bridge announces
itself: `wartui reset` — or replugging — is what to try before counting nodes. The rest of the fleet
is re-cut to cover a refused node's share in the meantime, and over twenty nodes the planner stops
re-cutting altogether and the header says `auto — too many nodes`.

`stale` and `no heartbeat` are deliberately distinct from silence, and from each other. A node
streaming observations whose heartbeats are lost would otherwise age out and churn the whole fleet's
topology; wartui keeps two clocks so the difference is visible rather than fatal.

An `alive` node that has stopped reporting is usually not broken: a node reports an address once and
then holds it back, so a node that is standing still goes quiet once it has reported everything in
range. That memory is on the node, not the host, so it carries over into your next session. A held
address is reported again five minutes after it was last reported, or sooner if the node hears it at
least 10 dB louder than it has reported it before. Rebooting a node clears the memory
(`crates/wartui-proto/src/dedup.rs` explains why it works this way).

The header has three ways of saying it has nothing to drive: `auto — nothing heartbeating yet`,
`auto — no node it can drive` (nodes are alive but none is assignable), and `auto — too many nodes`
(over twenty, so the planner has stopped re-cutting; whatever is assigned stays assigned and capture
is unaffected).

The footer only shows faults once they have happened, so a clean run reads as a clean footer. Three
entries there are about somebody else's equipment rather than yours: `N frames from a vendor fleet`
and `N admin frames from another core` are another fleet transmitting on the channel these nodes
listen on, and `N frames from an older firmware — reflash` is a fleet part-way through an upgrade —
those nodes speak a wire format this host does not, so the footer is the only place they appear.
`bridge dropped` counts frames lost since this host attached; `wartui status` reports the bridge's
own total since it booted, which on a dongle left powered with nothing listening is large and not a
fault.

## The bridge panel

A LilyGO T-Dongle-C5 has a screen, and a bridge flashed for it shows five lines of the capture
while it runs. Nothing turns this on: the bridge says whether it has a panel when it announces
itself, `wartui status` prints what it said, and a bridge without one is sent nothing at all.

| Line | What it says |
| --- | --- |
| `gps ok, 9 sats` | Where the position is coming from |
| `nodes 3` | How many nodes are heartbeating, and how many of those can be driven |
| `APs ~12.3k` | Distinct Wi-Fi access points this session, estimated |
| `BLE ~840` | Distinct BLE addresses, estimated the same way |
| `avg -55 min -72` | How strongly the *bridge* is hearing the fleet |

The screen is seventeen characters wide, so the wording is terse on purpose. A fleet
that cannot all be driven reads `nodes 2 of 5` — drivable first, alive second. The RSSI
line drops its own label when it has two figures to name, and reads `rssi -55` when the
average and the weakest are the same number. A reading of −100 dBm or worse prints as
`BAD`: three digits and a sign do not fit, and the exact figure stopped meaning anything
well above it.

The numbers are the same ones the view shows, from the same snapshot, about once a second.

**Each line is coloured by its own state**, so the panel can be read from across a car without
being read closely: green for fine, amber for working but not ideal, red for a fault.

- **GPS is green only on a live, current fix**, which is a stricter reading than the view's.
  A receiver that is connecting, scanning, searching, or holding a fix too old to still be
  believed is amber — it is attached and trying. No receiver, a port that will not read, and a
  pinned `--lat`/`--lon` are all red, however deliberate any of them was: none of the three is
  going to produce another fix, and a capture being written against a constant position is the
  thing most worth noticing from a distance.
- **Nodes** turn amber when something is heartbeating that cannot be driven — the two counts
  differ, so the line prints both — and red when nothing is alive or the link is down.
- **RSSI** turns amber when the fleet's average falls below −65 dBm or any one node below −70.
  Both sit above the −74 dBm a 24 Mbps ESP-NOW link needs, so the line warns while there is
  still something to do about it: close a window, move the dongle off the floor, walk a node
  back. It says `rssi: none heard` in red if nodes are alive and the bridge has measured none
  of them, and `rssi n/a` when nothing is alive to measure.
- **The AP and BLE counts have no bad state** and stay green.

Before any host speaks, and for about ten seconds after one goes away, the bridge shows what it
knows by itself instead — chip, address, channel and uptime, all in amber, because no host is
exactly "working but not ideal". Start a capture and it takes the panel back within a second,
with no replug and no reflash.

## When nothing arrives

**wartui finds the bridge by asking.** With several Espressif boards attached — a node plugged in
by USB is one — it opens each in turn and keeps the first that answers the link protocol, then
writes that board's address to `~/.local/state/wartui/bridge` so later runs open one port and no
others. A board that answers is held for the rest of the run: unplug the bridge mid-session to
reflash a node and wartui waits for the bridge to come back rather than transmitting into the node.

That file is state, not settings. Deleting it is always safe and costs one slower start, and it is
what to delete if wartui keeps opening the wrong board. It corrects itself two ways without being
asked: whichever board answers writes its own address over it, and a board that is the only one
attached and stops answering is dropped from it — which is what a reflashed bridge looks like. A
board merely passed over during a sweep is not dropped, because being slower than the board beside
it is not evidence of anything.

The header says `waiting for a bridge to announce itself` for two quite different reasons, and the
fault box says which. `link down: could not open …` means the port is not ours — nearly always
another `wartui`, a `screen` session, an IDE's serial monitor or ModemManager still holding it.
ModemManager is the one that clears on its own: on a distribution that runs it, it opens every
freshly-attached CDC-ACM device for a few seconds to ask whether it is a modem. No fault at all
means the port opened and the dongle is not answering. `swept N boards and none answered` is the
third: every Espressif board attached was tried and none of them was a bridge.

In that second case the bridge is usually not dead but deaf in one direction: its USB transmit
endpoint has stopped draining while it goes on reading every frame you send it. The firmware notices
within three seconds and reboots itself, so this should clear on its own and show up afterwards as
`bridge rebooted itself: USB transmit had stalled`. If it does not, `wartui reset` asks it to
reboot, which works because the receive path is the half that still runs — and keeps the device
path, where `espflash reset --port …` re-enumerates the board and can move `ttyACM0` to `ttyACM1`.
Naming the board by its address rather than by a path is what survives that: `--bridge
10:BD:A3:EC:44:C0` means the same board whichever node it came back on. `espflash` is the fallback
for a bridge that answers nothing at all, and unplugging is the last resort.

```sh
wartui status                          # exits in 5 s with the reason
wartui reset                           # reboot a bridge that stopped answering
wartui --log-file wartui.log run
rm ~/.local/state/wartui/bridge        # forget which board was the bridge
```

`wartui reset` takes about thirty seconds to fail when it is pointed at a board that is not a
bridge, and that is the cost of what it does: it transmits before anything has identified itself,
because the board it is for answers nothing. A board that is not reading takes the first packet and
leaves the rest queued, and closing the port waits for them. `run` never pays this: it asks with one
frame and no more, however long it then waits for the answer.

`wartui reset` never sweeps. A `Reset` reaching a node reboots it and costs it the addresses it was
holding back, so it goes to the board named with `--bridge`, else the remembered one, else the only
one attached — and with several attached and none of them known, it says so and asks for a
`--bridge` rather than guessing.

`--log-file` is the only way to see the transport's own account of a run: the view owns the
terminal, so without it nothing is logged anywhere. It records which port was resolved, whether it
opened, and the reason a link went down — once per reason rather than once per retry, since a port
that is somebody else's is retried every 750 ms for as long as the capture runs. The GPS reader
thread reports itself the same way. `RUST_LOG=debug` adds each individual retry, every frame that
would not decode, and dropped bulk commands.

### Telling the boards apart

A node plugged in by USB is the same vendor and product ID as the bridge and sits on an adjacent
device node, so a path says nothing about which board it reaches. `wartui ports` names them:

```
$ wartui ports
/dev/serial/by-id/usb-Espressif_USB_JTAG_serial_debug_unit_10:BD:A3:EC:44:C0-if00
  10:BD:A3:EC:44:C0     USB JTAG/serial debug unit  (303a:1001)
/dev/serial/by-id/usb-Espressif_USB_JTAG_serial_debug_unit_02:00:5E:10:9D:24-if00
  02:00:5E:10:9D:24     USB JTAG/serial debug unit  (303a:1001)
```

**An ESP32's USB serial number is its MAC**, so the operating system has already paired each device
node with the address that board's radio transmits from — with nothing opened, no `esp` tool and no
reflash. That one fact is what the whole command rests on, and it holds on Linux and macOS alike. It
is also why every board gets a name of its own above: udev builds those from the serial number, so
no two ESP32s share one. Everything else on the bus carries a manufacturing serial instead, and a
device that reports none at all reads as `address not reported` and is named by its socket.

The bridge is the row whose address the fleet table shows as the bridge's, and a node the row whose
heartbeats `wartui sniff` attributes to that address. Where the board generations differ, the OUI
separates them too.

Either name works as `--bridge`. The address is the one worth keeping: a device node moves when the
board re-enumerates, and a `by-path` name moves when the cable does, while an address moves only when
the board does.

```sh
wartui --bridge 10:BD:A3:EC:44:C0
```
