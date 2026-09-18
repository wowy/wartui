# wartui node firmware

A wardriving node. It listens on one channel at a time, reports every access point
it has not reported lately, and takes its share of the channel pool from whatever
is acting as the fleet's core — for this fleet, `wartui` on a laptop behind the
bridge.

It replaces the vendor firmware at
[ESP32DualBandWardriver](https://github.com/wowy/ESP32DualBandWardriver) rather
than porting it. The web interface, SD card, display, buttons, fuel gauge, GPS,
geofencing, uploads and dock mode did not come across at all, because a node in
this fleet has no use for any of them.

It shares no wire format with it either. A node broadcasts a 13-byte heartbeat
once per completed sweep and an 18-plus-SSID sighting per newly-seen BSSID —
whose trailer carries a Passpoint network's roaming consortium identifiers, or
a BLE advertiser's manufacturer identifier, when there are any — and accepts a
15-byte unicast assignment. All three sit behind wartui's own `WTUI`
magic and a wire version byte, checked before anything else, so neither fleet can
reach the other at all.

## What it does differently

**It listens instead of scanning.** It parks the radio and reads beacons and probe
responses, so it never transmits while looking — including on the DFS channels,
where the rules say listen and do not speak.

That is also what makes the rest possible. `WifiController::sniffer()` and
`::esp_now()` both borrow the controller immutably, so a node holds both for the
life of the program; `scan_async` wants `&mut`, and holding it alongside an
ESP-NOW instance is not expressible — the instance would have to be torn down and
rebuilt around every scan.

**It is on the control channel far more often.** Nothing owns the radio, so the
node returns to the control channel after *every* dwell to report what it heard,
and an assignment sent at any of those moments lands. The 100 ms window after a
heartbeat is still honoured and the host's timing model is unchanged; it simply
stops being the only chance.

**An unassigned node waits rather than sweeping.** It parks on the control channel
and heartbeats every second until it is told what to scan, and **collects nothing
until then**. wartui's planner answers the first heartbeat, so that lasts a single
beat — the intended trade, and worth knowing before wondering where the first
second of a capture went.

**It says what it is, in every heartbeat.** Three of a heartbeat's thirteen bytes
are a version and a feature byte, shown by the host as:

```
wartui/1.0;ble,5g
```

The protocol version, then what this build can do: `ble` if the cargo feature is
compiled in, `5g` if the chip has the radio for it — a C5 does, a C6 does not.
`ble` says the code exists, not that it is running; that is the core's decision
and is off at every boot.

The host deals no channels to a node until it has heard this. Every heartbeat
carries it rather than only the first, because one sent once is one lost to a
dropped frame — and because a board reflashed with something else should stop
claiming the old build's features.

Both bits are acted on rather than merely recorded. A build without `5g` is dealt
no 5 GHz channel, because it would adopt the share, acknowledge it and scan only
the part it could reach. A build without `ble` is refused the Bluetooth scan, and
loses it if it was already holding one when it announced itself. Neither refusal
needs anything of this firmware: these three bytes are the whole of what the host
has to go on, so getting them wrong here is indistinguishable from lying about it.

## Building and flashing

```sh
cargo run --release --features esp32c6            # or --features esp32c5
cargo run --release --features esp32c6,ble        # with Bluetooth
cargo run --release --no-default-features --features esp32c6   # without diagnostics
cargo run --release --features esp32c6,xiao-external-antenna  # XIAO C6, antenna on U.FL
```

Exactly one chip feature is required; `src/main.rs` rejects zero or both at compile
time. The C5 is dual band and gets `BandMode::Auto`; the C6 is 2.4 GHz only and has
no band mode to set, so half the channel pool is simply refused there. Both parts
are RISC-V and build on stable.

Diagnostics are on by default and go out over USB serial-JTAG. Turning them off
buys a few kilobytes and nothing else — a node that is not saying anything is hard
to tell from a node that is not working. The cargo runner is
`espflash flash --monitor` with no `--chip`, so espflash detects the part. Unlike
the bridge, the monitor is worth watching: nothing but diagnostics goes down that
pipe.

| Build | Flash |
| --- | --- |
| `esp32c6` | 512 KB |
| `esp32c6,ble` | 760 KB |

A `ble` build is not a node that scans Bluetooth. It is a node that *can*, if the
core sets the flag.

`xiao-external-antenna` is for a Seeed XIAO ESP32-C6 with an antenna on its U.FL
connector. The board switches its one RF pin between that connector and an onboard
ceramic antenna, and without the feature it stays on the ceramic one: an antenna
plugged in does nothing, and the node simply reads as weak. Build with it only when
an antenna is fitted, since a radio pointed at an empty connector is close to deaf.
It is rejected on a C5: the XIAO ESP32-C5 has a U.FL connector and no onboard
antenna, so there is nothing to switch.

## The regulatory domain is set here, and it is not a preference

`esp-radio` defaults `country_info` to **China** and applies it under
`WIFI_COUNTRY_POLICY_MANUAL`, so nothing on the air ever overrides it. China's
5 GHz allocation excludes 5470–5725 and the driver refuses those channels
outright, so a C5 left on the default silently loses 100–144, every sweep, and the
host cannot tell that from a node whose radio did not tune. `src/main.rs` passes
`US` for that reason and `firmware/bridge` does the same.

Channel limits otherwise belong to the controller, not here — the node parks and
listens, transmitting only on the control channel, so where it dwells is a
coverage decision rather than a legal one. This firmware gates nothing: it parks on
whatever it is dealt and reports a refusal, which in practice means a C5 tunes 41
of the 42 channels in `SCAN_CHANNELS` and a C6 the 13 of them that are 2.4 GHz.
The one a C5 will not take is **channel 14**, which `esp-radio` refuses through a
hardcoded `nchan: 13` that no exposed setting can reach. That refusal is the
driver's rather than a rule made here, so the firmware still attempts it — but no channel pool
contains it, because a node reports the refusal by printing to a serial console
nobody is watching, and a fleet given channel 14 would spend a dwell of every
sweep on nothing with no way for the host to notice.
[`docs/phase-1-findings.md`](../../docs/phase-1-findings.md) has the measurements
and the reading of the driver.

## Bluetooth runs only when the core asks

**At most one node scans, and the core decides which.** Bit 0 of the assignment's
flags byte carries it, clear at every boot regardless of how the firmware was
built: the `ble` cargo feature decides whether any of this is compiled in, and the
flag decides whether it runs. That shape is a measured cost showing through — a
stock node with BLE on acknowledged none of thirty-two assignments, where this one
acknowledges first attempt every time and runs about 10% slower per sweep while it
holds the scan ([`docs/phase-0-findings.md`](../../docs/phase-0-findings.md) and
[`docs/phase-2-findings.md`](../../docs/phase-2-findings.md)). The two radios share
the one 2.4 GHz antenna and the admin window is precisely when a node is otherwise
idle, so it is worth paying on one node and not on all of them — and which node is
an operator's decision rather than a property of whatever binary is on the board.

Three things keep that cost where it belongs. The controller is told
`HCI_LE_Set_Scan_Enable(0)` at the end of every sweep rather than merely being
waited on. The scan runs at the far end of the cycle from the admin window. And it
is rate-limited to one scan per `NUM_SCAN_CHANNELS` dwells — about four seconds —
rather than one per completed sweep, because a sweep is only as long as the
assignment: a node holding a single channel finishes one every 125 ms, and "once
per sweep" would put a 500 ms scan against nearly every admin window it has.
Narrowing a node to one channel is a supported thing to do, so that case has to be
the safe one.

The controller is brought up at boot on a `ble` build, because initialising a
radio between a dwell and an admin window is exactly the kind of surprise this
firmware exists to avoid. It is initialised and never enabled —
`HCI_LE_Set_Scan_Enable` is only ever sent from inside a sweep.

There is no host stack. `esp-radio` exposes the controller as a raw HCI pipe and
all this firmware wants is an address, a signal strength and — when an advertiser
offers one — its manufacturer identifier, so the four commands and one event live
in `wartui_proto::hci` where they are unit-tested, and `src/ble.rs` is only the
conversation.

## What is testable, and where

Almost none of the interesting logic is in this crate. The 802.11 beacon and
RSN/WPA element parser, the sighting encoder, the dedup ring and the HCI packets
all live in `wartui-proto`, on the other side of the path dependency, because a
`no_std` binary for `riscv32imac` cannot run a test and a misread information
element found by reflashing is found expensively.

```sh
cargo test -p wartui-proto            # from the repository root
```

`crates/wartui-proto/tests/beacon.rs` assembles frames to exercise the
classification ladder; `tests/beacon_vectors.txt` holds ninety-one real ones,
seeded by `tools/beacons/extract.py` from a monitor-mode capture.

**That extractor scrubs, and it does so by default.** A capture of the air around
you is a geolocation fingerprint — BSSIDs are what WiGLE and the phone vendors run
their location databases on, SSIDs carry surnames and house numbers, WPS elements
carry device names and serials, and a probe response is addressed to a real client
nearby. So addresses come out synthetic, SSIDs become `ap-NNN` padded to the
length the real one had, and every element the parser does not read has its
contents replaced; every tag and length stays at its original offset, and the SSID
length, DS Parameter Set, RSN, HT Operation, WPA and Roaming Consortium elements
arrive untouched, which is the whole of what is being tested. `--raw` turns it
off for a fixture you are keeping to yourself, and its output does not belong in
a repository.

## Two things that are easy to get wrong

**Setting the channel is not `set_channel` alone.** On an unassociated station
interface it does not stick unless promiscuous mode is on across the change, which
is what `radio::park` does on every hop. A node whose channel silently did not
change would report the right access points against the wrong frequency and hear
no assignment at all, which reads as a dead node.

**The promiscuous callback runs in the Wi-Fi driver's task, on a buffer that dies
when it returns.** It cannot capture state — `set_receive_cb` takes a bare `fn` —
and it cannot block. So it rejects non-management frames before touching a lock,
parses what it needs into a fixed-size `Sighting`, and leaves it in a `static` ring
for the main loop. It also drops an access point already pending: one beacons
roughly ten times in a 125 ms dwell, and without that the ring would fill with
copies of whichever network is loudest and drop the ones not yet seen.

## Dependency versions

Same set as the bridge and pinned by the same thing — see
[`firmware/bridge/README.md`](../bridge/README.md).

`esp-println` is the one thing outside that wall. Its `esp-metadata-generated
^0.5` is a **build** dependency, which resolves independently of the 0.4 the rest
of the graph uses, and its `esp-sync ^0.3` is pulled only by the
`critical-section` feature — left off here, so there is exactly one `esp-sync` in
the tree. Verify with:

```sh
cargo tree -e normal --features esp32c6 | grep esp-sync   # 0.2.1 only
```

Its serial-JTAG writer gives the FIFO a bounded number of attempts and then sets a
sticky flag saying nobody is draining it, so a node with no monitor attached drops
diagnostics rather than stalling mid-sweep. That is the only property this firmware
needs from it, and the reason it is allowed here at all.
