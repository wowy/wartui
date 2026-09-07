# wartui node firmware

A wardriving node. It listens on one channel at a time, reports every access
point it has not reported lately, and takes its share of the channel pool from
whatever is acting as the mesh's core — for this fleet, `wartui` on a laptop
behind the bridge.

It replaces the vendor firmware at
[ESP32DualBandWardriver](https://github.com/justcallmekoko/ESP32DualBandWardriver)
rather than porting it. The scanning and the ESP-NOW comms come from there and
are cited `file:line` throughout the source; the web interface, SD card,
display, buttons, fuel gauge, GPS, geofencing, uploads and dock mode do not,
because a node in this fleet has no use for any of them and each is a kilobyte
that can go wrong somewhere only a reflash reaches.

Phase 1 speaks the vendor's wire format exactly, so `wartui run` drives it with
no host change: broadcast `MSG_HEARTBEAT` once per completed sweep, broadcast
`MSG_TEXT` per newly-seen BSSID, and accept the ten-byte unicast `MSG_ADMIN`.

## Four deliberate differences

**It listens instead of scanning.** Every `WiFi.scanNetworks` in the vendor tree
passes `passive = false` (`src/WiFiOps.cpp:745,755,779`), so a stock node
transmits a probe request on each channel it is assigned — including the DFS
channels, where the rules say listen and do not speak. This one parks the radio
and reads beacons and probe responses.

That is also what makes the rest possible. `WifiController::sniffer()` and
`::esp_now()` both borrow the controller immutably, so a node can hold both for
the life of the program; `scan_async` wants `&mut`, and holding it alongside an
ESP-NOW instance is not expressible — the instance would have to be torn down
and rebuilt around every scan.

**It is on the control channel far more often.** A stock node is deaf to its
core for all but the 300 ms it holds open once per sweep, because the scan owns
the radio for everything else. Here nothing owns the radio: the node returns to
the control channel after *every* dwell to report what it heard, so an
assignment sent at any of those moments lands. The 300 ms window is still
honoured and the host's timing model is unchanged; it simply stops being the
only chance.

**An unassigned node waits rather than sweeping.** The vendor default is all
forty channels (`src/WiFiOps.cpp:77-80`), so a node that has never heard a core
duplicates whatever the rest of the fleet is doing and is addressable for 300 ms
per sweep while it does. This one parks on the control channel and heartbeats
every second until it is told what to scan. Under wartui's planner that lasts a
single heartbeat. Under `--manual` it lasts until a key is pressed, and **the
node collects nothing until then** — which is the intended trade and worth
knowing before wondering where the observations went.

**It says what it is, in every heartbeat.** Node to core is byte-identical to a
stock node's, which is what lets golden vectors captured off a vendor fleet keep
testing this tree — and it is also why the host cannot otherwise tell the two
apart. So the heartbeat's text field, which a stock node leaves empty
(`src/WiFiOps.cpp:1456-1457`), carries an ASCII token:

```
wartui/0.1;ble,5g
```

The protocol version, then what this build can do: `ble` if the cargo feature is
compiled in, `5g` if the chip has the radio for it — a C5 does, a C6 does not.
`ble` says the code exists, not that it is running; that is still the core's
decision and is off at every boot.

The host refuses to plan for a node that sends no token, which is the point.
Without it, a stranger in the fleet is worse than an absent one: it heartbeats
so it is planned for, its radio acknowledges the assignment so the host believes
it landed, and its firmware cannot decode the frame so the share it was given
goes unscanned. Every heartbeat carries the token rather than only the first,
because one sent once is one lost to a dropped frame — and because a board
reflashed with something else should stop claiming to be this.

## Building and flashing

```sh
cargo run --release --features esp32c6            # or --features esp32c5
cargo run --release --features esp32c6,ble        # with Bluetooth
cargo run --release --no-default-features --features esp32c6   # without diagnostics
```

Diagnostics are on by default and go out over USB serial-JTAG. Turning them off
buys a few kilobytes and nothing else — a node that is not saying anything is
hard to tell from a node that is not working.

Exactly one chip feature is required; `src/main.rs` rejects zero or both at
compile time. The C5 is dual band and gets `BandMode::Auto`; the C6 is 2.4 GHz
only and has no band mode to set, so half the channel pool is simply refused
there. Both parts are RISC-V and build on stable.

## The regulatory domain is set here, and it is not a preference

`esp-radio` defaults `country_info` to **China** and applies it under
`WIFI_COUNTRY_POLICY_MANUAL`, so nothing on the air ever overrides it. China's
5 GHz allocation excludes 5470–5725, and the driver refuses those channels
outright: a C5 left on the default silently loses 100–144, every sweep, and the
host cannot tell that from a node whose radio did not tune. `src/main.rs` passes
`US` for that reason and `firmware/bridge` does the same.

Channel limits otherwise belong to the controller, not here — the node parks and
listens, transmitting only on the control channel, so where it dwells is a
coverage decision rather than a legal one. On a C5 the firmware permits 39 of the
40 channels in `SCAN_CHANNELS` and lets the host choose among them; on a C6 it is
the 13 that are 2.4 GHz, for the reason above — that part has no 5 GHz radio, and
its refusals are not a regulatory matter at all. The one channel a C5 will not
take is **channel 14**, which `esp-radio` refuses through a hardcoded
`nchan: 13` that no setting it exposes can reach; `docs/phase-1-findings.md` has
the measurements and the reading of the driver.

This firmware still permits it — the refusal is the driver's, not a rule made
here — but **no channel pool contains it**, so a wartui core never assigns it.
That is deliberate: the node reports the refusal by printing a line to a serial
console nobody is watching, so a fleet given channel 14 spends a dwell of every
sweep on nothing and the host has no way to notice. It is unsupported rather
than merely unused.

The cargo runner is `espflash flash --monitor` with no `--chip`, so espflash
detects the part. Unlike the bridge, the monitor is worth watching: nothing but
diagnostics goes down that pipe.

| Build | Flash |
| --- | --- |
| `esp32c6` | 512 KB |
| `esp32c6,ble` | 760 KB |

A `ble` build is not a node that scans Bluetooth. It is a node that *can*, if
the core sets the flag.

## Bluetooth runs only when the core asks, and that is not timidity

Measured on real hardware and written up in `docs/phase-0-findings.md`: two
nodes differing only in whether BLE was enabled, both sent assignments inside
their own admin windows, frames 1 dB apart at the sniffer.

| Node | BLE | `MSG_ADMIN` sent to it | Acknowledged |
| --- | --- | --- | --- |
| `…59:50` | off | 2, each transmitted once, no retry | **2 of 2** |
| `…57:84` | on | 32, one sequence number, retry bit on 31 | **0 of 32** |

An 802.11 acknowledgement comes from the receiver's MAC hardware, so its absence
means the radio was not on the channel. NimBLE shares the one 2.4 GHz antenna
and the admin window is precisely when the node is otherwise idle. In the same
170 seconds the BLE-off node completed sixteen sweeps and the BLE-on node nine.

**The core decides which node scans, and the answer is at most one.** Bit 0 of
`MSG_ADMIN`'s flags byte carries it, and it is clear at every boot regardless of
how the firmware was built — the `ble` cargo feature decides whether any of this
is compiled in, and the flag decides whether it runs. That shape is the measured
cost showing through: it is worth paying on one node for Bluetooth coverage and
not worth paying on all of them, and which node is an operator's decision rather
than a property of whatever binary happens to be on the board.

The Bluetooth controller is still brought up at boot on a `ble` build, because
initialising a radio between a dwell and an admin window is exactly the kind of
surprise this firmware exists to avoid. It is initialised and never enabled —
`HCI_LE_Set_Scan_Enable` is only ever sent from inside a sweep. Whether an
initialised-but-disabled controller costs anything has not been measured;
`docs/phase-1-findings.md` lists it among the things that have not.

This firmware tries to avoid repeating that in three ways. The controller is
told `HCI_LE_Set_Scan_Enable(0)` at the end of every sweep rather than merely
being waited on. The sweep runs at the far end of the cycle from the admin
window. And it is rate-limited to one scan per `NUM_SCAN_CHANNELS` dwells —
about four seconds — rather than one per completed sweep, because a sweep is
only as long as the assignment: a node holding a single channel finishes one
every 125 ms, and "once per sweep" would have put a 500 ms scan against nearly
every admin window it has. Narrowing a node to one channel is a supported thing
to do, so that case has to be the safe one.

That was a hardware question and it now has a hardware answer, three times: on the
very board that acknowledged none of its thirty-two assignments under vendor
firmware, this one acknowledged in 5.8 ms, 6.6 ms and 5.8 ms, first attempt every
time, and ran 10.0%, 7.6% and 9.6% slower with BLE on rather than the vendor's
~78%. `docs/phase-1-findings.md` has the run, including the bug that
made the first attempt of it meaningless — a scan that is enabled, answered
`status 0`, and reports nothing, because the controller's event mask hides the
one event it exists to produce.

There is no host stack. `esp-radio` exposes the controller as a raw HCI pipe and
all this firmware wants is an address and a signal strength, so the four
commands and one event live in `wartui_proto::hci` where they are unit-tested,
and `src/ble.rs` is only the conversation.

## What is testable, and where

Almost none of the interesting logic is in this crate. The 802.11 beacon and
RSN/WPA element parser, the wardrive-line writer, the dedup ring and the HCI
packets all live in `wartui-proto`, on the other side of the path dependency,
because a `no_std` binary for `riscv32imac` cannot run a test and a misread
information element found by reflashing is found expensively.

```sh
cargo test -p wartui-proto            # from the repository root
```

`crates/wartui-proto/tests/beacon.rs` assembles frames to exercise the
classification ladder; `tests/beacon_vectors.txt` holds ninety-one real ones,
seeded by `tools/beacons/extract.py` from a monitor-mode capture. The assembled
frames check the parser against a reading of `src/WiFiOps.cpp:154-408`, which is
a different question from whether it agrees with the air.

**The extractor scrubs, and it does so by default.** A capture of the air around
you is a geolocation fingerprint: BSSIDs are exactly what WiGLE and the phone
vendors run their location databases on — the premise of this entire project —
SSIDs carry surnames and house numbers, WPS elements carry device names and
serials, and a probe response is addressed to a real client device nearby. So
addresses come out synthetic, SSIDs become `ap-NNN` padded to the length the
real one had, and every element the parser does not read has its contents
replaced. Every tag and length stays at its original offset, and the SSID
length, DS Parameter Set, RSN, HT Operation and WPA elements arrive untouched,
which is the whole of what is being tested. `--raw` turns it off for a fixture
you are keeping to yourself; its output does not belong in a repository.

## Two things that are easy to get wrong

**Setting the channel is not `set_channel` alone.** On an unassociated station
interface it does not stick unless promiscuous mode is on across the change —
the vendor's `setFixedChannel` (`src/WiFiOps.cpp:600-618`) exists entirely for
this. `radio::park` does the same dance on every hop. A node whose channel
silently did not change would report the right access points against the wrong
frequency and hear no assignment at all, which reads as a dead node.

**The promiscuous callback runs in the Wi-Fi driver's task, on a buffer that
dies when it returns.** It cannot capture state — `set_receive_cb` takes a bare
`fn` — and it cannot block. So it rejects non-management frames before touching
a lock, parses what it needs into a fixed-size `Sighting`, and leaves it in a
`static` ring for the main loop. It also drops an access point already pending:
one beacons roughly ten times in a 125 ms dwell, and without that the ring would
fill with copies of whichever network is loudest and drop the ones not yet seen.

## Dependency versions

Same set as the bridge, and pinned by the same thing: `esp-radio 1.0.0-beta.0`
requires `esp-hal ~1.1.0`, which is `>=1.1.0, <1.2.0`. So `esp-hal 1.2.0` exists
and cannot be used, and `esp-rtos 0.4` (wants `esp-hal ~1.2.0-rc.0`), `esp-alloc
0.11` and `esp-sync 0.3` are all held behind it. `cargo update` moves nothing
until `esp-radio` publishes again.

`esp-println` is the one thing outside that wall. Its `esp-metadata-generated
^0.5` is a **build** dependency, which resolves independently of the 0.4 the
rest of the graph uses, and its `esp-sync ^0.3` is pulled only by the
`critical-section` feature — left off here, so there is exactly one `esp-sync`
in the tree. Verify with:

```sh
cargo tree -e normal --features esp32c6 | grep esp-sync   # 0.2.1 only
```

Its serial-JTAG writer gives the FIFO a bounded number of attempts and then sets
a sticky flag saying nobody is draining it, so a node with no monitor attached
drops diagnostics rather than stalling mid-sweep. That is the only property this
firmware needs from it, and it is the reason it is allowed here at all.
