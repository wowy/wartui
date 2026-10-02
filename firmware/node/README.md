# wartui node firmware

A wardriving node. It listens on one channel at a time and reports every access point it hasn't
reported lately. It takes its share of the channel pool from the fleet's core: here, `wartui` on a
laptop behind the bridge.

It replaces the vendor firmware at
[ESP32DualBandWardriver](https://github.com/justcallmekoko/ESP32DualBandWardriver) rather than
porting it. The web interface, SD card, display, buttons, fuel gauge, GPS, geofencing, uploads, and
dock mode don't come across. A node in this fleet has no use for any of them.

It shares no wire format with the vendor firmware either. A node sends and receives four frames:

| Frame          | Direction           | Size                    |
|----------------|---------------------|-------------------------|
| Heartbeat      | Broadcast           | 20 bytes                |
| Sighting batch | Unicast, to bridge  | Up to 250 bytes         |
| Admin          | Unicast, to node    | 15 bytes                |
| Clear          | Unicast, to node    | 6 bytes                 |

- **Heartbeat**: every 5 s once assigned. It is broadcast because that is how a bridge discovers a
  node before either knows the other's address. It carries a counter of completed sweeps (scans,
  on the Bluetooth node), the epoch of the assignment held or 0, the capability bytes below,
  running counts of Wi-Fi and BLE sightings turned away by a full buffer, and the count of
  heartbeats attempted since boot, so the host can count the ones it missed.
- **Sighting batch**: sent at the end of each dwell or scan, to the bridge that last sent an admin
  or clear frame. A 9-byte header, then one 12-plus-SSID record per newly seen BSSID or
  advertiser, packed to fill the 250-byte ESP-NOW payload. A record's trailer carries a Passpoint
  network's roaming consortium identifiers, or a BLE advertiser's manufacturer identifier, when
  there are any.
- **Admin**: the assignment.
- **Clear**: empties the dedup ring.

All four sit behind wartui's own `WTUI` magic and a wire version byte, checked first, so neither
fleet can reach the other.

## What it does differently

**It listens instead of scanning.** It parks the radio and reads beacons and probe responses. So it
never transmits while looking, including on the DFS channels, where the rules say listen and don't
speak.

Listening also makes the rest possible. `WifiController::sniffer()` and `::esp_now()` both borrow
the controller immutably, so a node holds both for the life of the program. `scan_async` wants
`&mut`, and holding it alongside an ESP-NOW instance isn't expressible. The instance would have to
be torn down and rebuilt around every scan.

**It is on the control channel far more often.** Nothing owns the radio, so the node returns to the
control channel after _every_ dwell, not only after a heartbeat. There it reports what it heard and
checks for an assignment already waiting. A heartbeat opens a 100 ms window for the host to answer
with an assignment, once every 5 seconds rather than once a sweep. A missed window costs one more
heartbeat interval, not the assignment, since the host re-sends on the next one.

**An unassigned node waits rather than sweeping.** It parks on the control channel and heartbeats
every second until told what to scan. It **collects nothing until then**. wartui's planner answers
the first heartbeat, so the wait lasts a single beat. That is the intended trade, and explains
where the first second of a capture went.

**The node given the Bluetooth scan sweeps nothing.** It is dealt no channels and never leaves the
control channel, so an assignment can always reach it. It runs one scan after another, back to back.
§ "Bluetooth is a whole node's job" has the rest.

**It forgets what it has reported in two cases.** An assignment that changes its channels or its
Bluetooth flag empties the dedup ring, because the entries describe a neighborhood it no longer
listens to. So does a clear frame, which the host sends on the operator's `r` or `R` and which
carries no other instruction. `crates/wartui-proto/src/dedup.rs` has the reasoning.

**It says what it is, and what it holds, in every heartbeat.** One byte is the epoch of the
assignment adopted, or 0 for none. That is how the host tells adoption from a MAC-layer ack without
asking the node's own console. Three more are a version and a capability byte, shown by the host as:

```
wartui/1.0;5g
```

That is the protocol version, then `5g` if the chip has the radio for it. A C5 does; a C6 doesn't.
Bluetooth isn't a capability. Every node can scan it, and the core's `ADMIN_FLAG_BLE` decides
whether one does, off at every boot.

The host deals no channels to a node until it has heard this. Every heartbeat carries it, not only
the first, because one sent once is lost to a dropped frame.

The host acts on the bit rather than just recording it. A build without `5g` is dealt no 5 GHz
channel, because it would adopt the share, acknowledge it, and scan only the part it could reach.
This one byte is all the host has to go on, so getting it wrong is indistinguishable from lying.

## Building and flashing

```sh
cargo run --release --features esp32c6            # or --features esp32c5
cargo run --release --no-default-features --features esp32c6   # without diagnostics
cargo run --release --features esp32c6,xiao-external-antenna  # XIAO C6, antenna on U.FL
```

To flash every attached board of one chip at once, and never the bridge, run `wartui flash-fleet
--features esp32c5` from the host. [`crates/wartui/README.md`](../../crates/wartui/README.md) §
"Flashing the fleet" has how it picks boards.

Exactly one chip feature is required; `src/main.rs` rejects zero or both at compile time. The C5 is
dual band and gets `BandMode::Auto`. The C6 is 2.4 GHz only and has no band mode to set, so it
refuses half the channel pool. Both parts are RISC-V and build on stable.

Diagnostics are on by default and go out over USB serial-JTAG. Turning them off saves a few
kilobytes and nothing else: a silent node is hard to tell from a broken one. The cargo runner is
`espflash flash --monitor` with no `--chip`, so espflash detects the part. Unlike the bridge's, this
monitor is worth watching, because nothing but diagnostics goes down that pipe.

Every node can scan Bluetooth. It does so only if the core sets the flag, and a node that does scans
nothing else.

`xiao-external-antenna` is for a Seeed XIAO ESP32-C6 with an antenna on its U.FL connector. The
board switches its one RF pin between that connector and an onboard ceramic antenna. Without the
feature it stays on the ceramic one, so a plugged-in antenna does nothing and the node reads as
weak. Build with it only when an antenna is fitted, since a radio pointed at an empty connector is
close to deaf. A C5 build rejects it: the XIAO ESP32-C5 has a U.FL connector and no onboard
antenna, so there is nothing to switch.

## The regulatory domain is set here

`esp-radio` defaults `country_info` to **China** and applies it under `WIFI_COUNTRY_POLICY_MANUAL`,
so nothing on the air overrides it. China's 5 GHz allocation excludes 5470–5725, and the driver
refuses those channels outright. A C5 left on the default silently loses 100–144 every sweep. The
host can't tell that from a node whose radio didn't tune. So `src/main.rs` passes `US`, and
`firmware/bridge` does the same. This is not a preference.

Otherwise, channel limits belong to the controller, not here. The node transmits only on the
control channel, so where it dwells is a coverage decision rather than a legal one. This firmware
gates nothing: it parks on whatever it is dealt and reports a refusal. In practice a C5 tunes 41 of
the 42 channels in `SCAN_CHANNELS`, and a C6 tunes the 13 that are 2.4 GHz.

The one a C5 won't take is **channel 14**. `esp-radio` refuses it through a hardcoded `nchan: 13`
that no exposed setting can reach. The refusal is the driver's, so the firmware still attempts the
channel. But no channel pool contains it. A node reports the refusal only on a serial console nobody
is watching, so a fleet given channel 14 would waste a dwell every sweep and the host would never
notice. [`docs/phase-1-findings.md`](../../docs/phase-1-findings.md) has the measurements and the
reading of the driver.

## Bluetooth is a whole node's job

**At most one node scans, and the core decides which.** Bit 0 of the assignment's flags byte
carries it, clear at every boot on every build. Bluetooth scanning is always compiled in; the flag
decides whether it runs. The frame that sets it carries **no channels**, by design. An empty mask
with the flag means "Bluetooth is all of it". An empty mask without the flag is the one thing the
core never sends, and this firmware treats it as never having been told anything.

The reason is a measured cost. A stock node with BLE on acknowledged none of thirty-two
assignments. The two radios share one 2.4 GHz antenna, and the admin window is exactly when a node
is otherwise idle and the Bluetooth controller is free to take it
([`docs/phase-0-findings.md`](../../docs/phase-0-findings.md),
[`docs/phase-2-findings.md`](../../docs/phase-2-findings.md)). A node with no sweep has nothing to
hand the antenna back to, so the conflict is arranged away rather than scheduled around. That costs
one node's Wi-Fi coverage. Which node is the operator's decision, not a property of the binary on
the board.

Scans run back to back: the controller off, the new advertisers reported, and straight into the next
scan, with no deadline of its own. A `plan::ASSIGNED_BEAT_MS` timer runs alongside, independent of
the scan cadence. When it comes due, the node's next dwell-free pass sends a heartbeat and holds a
full admin window open before resuming scans. A const-assert beside `SCAN_MS` in `src/ble.rs` keeps
a scan shorter than that timer, so the timer is always checked between scans. A scan that overruns
costs the tail of one window, not the assignment, because the core re-sends on the next heartbeat.

Two things keep the antenna where it belongs:

- The controller is told `HCI_LE_Set_Scan_Enable(0)` at the end of every scan, not just waited on.
  An initialized host stack left behind a finished scan can keep the radio.
- The node's Wi-Fi radio never leaves the control channel, so the scan is the only thing that ever
  takes the antenna from it.

The controller comes up at boot on every build. Initializing a radio between a dwell and an admin
window is exactly the kind of surprise this firmware exists to avoid. It is initialized and never
enabled: `HCI_LE_Set_Scan_Enable` is only ever sent from inside a scan. If the controller won't
start, a node handed the scan parks, sniffs nothing, and says so on its own console. The host can't
tell, since capabilities carry no Bluetooth bit to withhold.

There is no host stack. `esp-radio` exposes the controller as a raw HCI pipe. This firmware wants
only an address, a signal strength, and a manufacturer identifier when an advertiser offers one. So
the four commands and one event live in `wartui_proto::hci`, where they are unit-tested, and
`src/ble.rs` is only the conversation.

## What is testable

Almost none of the interesting logic is in this crate. The 802.11 beacon and RSN/WPA element
parser, the sighting encoder, the dedup ring, and the HCI packets all live in `wartui-proto`, on the
other side of the path dependency. A `no_std` binary for `riscv32imac` can't run a test, and a
misread information element found by reflashing is found expensively.

```sh
cargo test -p wartui-proto            # from the repository root
```

`crates/wartui-proto/tests/beacon.rs` assembles frames to exercise the classification ladder.
`tests/beacon_vectors.txt` holds ninety-one real ones, seeded by `tools/beacons/extract.py` from a
monitor-mode capture.

**That extractor scrubs, by default.** A capture of the air around you is a geolocation
fingerprint:

- BSSIDs are what WiGLE and the phone vendors build their location databases on.
- SSIDs carry surnames and house numbers.
- WPS elements carry device names and serials.
- A probe response is addressed to a real client nearby.

So the extractor rewrites:

- addresses, to synthetic ones;
- SSIDs, to `ap-NNN` padded to the real one's length;
- the contents of every element the parser doesn't read.

Every tag and length stays at its original offset. The SSID length and the DS Parameter Set, RSN,
HT Operation, WPA, and Roaming Consortium elements arrive untouched, which is all that is being
tested. `--raw` turns scrubbing off for a fixture you keep to yourself. Its output doesn't belong in
a repository.

## Two things that are easy to get wrong

**Setting the channel is not `set_channel` alone.** On an unassociated station interface, the
channel doesn't stick unless promiscuous mode is on across the change. `radio::park` does that on
every hop. A node whose channel silently didn't change would report the right access points against
the wrong frequency and hear no assignment at all. It would read as a dead node.

**The promiscuous callback runs in the Wi-Fi driver's task, on a buffer that dies when it returns.**
It can't capture state, because `set_receive_cb` takes a bare `fn`, and it can't block. So it
rejects non-management frames before touching a lock, parses what it needs into a fixed-size
`Sighting`, and leaves it in a `static` ring for the main loop. It also drops an access point
already pending. One beacons roughly ten times in a 125 ms dwell. Without the check, the ring would
fill with copies of the loudest network and drop the ones not yet seen.

## Dependency versions

Same set as the bridge, pinned by the same thing. See
[`firmware/bridge/README.md`](../bridge/README.md).

`esp-println` is the one thing outside that wall. Its `esp-metadata-generated ^0.5` is a **build**
dependency, which resolves independently of the 0.4 the rest of the graph uses. Its `esp-sync ^0.3`
comes only with the `critical-section` feature, which is off here, so the tree has exactly one
`esp-sync`. Verify with:

```sh
cargo tree -e normal --features esp32c6 | grep esp-sync   # 0.2.1 only
```

Its serial-JTAG writer tries the FIFO a bounded number of times, then sets a sticky flag saying
nobody is draining it. So a node with no monitor attached drops diagnostics rather than stalling
mid-sweep. That is the only property this firmware needs from it, and the reason it is allowed here.
