# wartui node firmware

A wardriving node. It listens on one channel at a time and reports every access point it hasn't
reported lately. It takes its share of the channel pool from the P4 bridge, which plans the fleet,
and reports everything to `wartui` on a laptop behind it.

It replaces the vendor firmware at
[ESP32DualBandWardriver](https://github.com/justcallmekoko/ESP32DualBandWardriver) rather than
porting it. The web interface, SD card, display, buttons, fuel gauge, GPS, geofencing, uploads, and
dock mode don't come across. A node in this fleet has no use for any of them.

It shares no wire format with the vendor firmware either. It sends no Wi-Fi of its own: it is a node
on the P4 bridge's SPI bus, and that bus carries everything it says and hears.
[`docs/node-bus.md`](../../docs/node-bus.md) has the pins, and
[`crates/wartui-proto/src/bus/mod.rs`](../../crates/wartui-proto/src/bus/mod.rs) the framing. Each
transfer carries records, one message each:

| Message        | Direction   | Body                    |
|----------------|-------------|-------------------------|
| Heartbeat      | Node → P4   | 27 bytes                |
| Sighting batch | Node → P4   | Up to 1024 bytes        |
| Assign         | P4 → node   | 8 bytes                 |
| Stop           | P4 → node   | Empty                   |

- **Heartbeat**: every 5 s once assigned, every second until then. Fire-and-forget: a newer one
  replaces one the P4 has not clocked. Its body opens with a 5-byte prefix: the firmware version
  (major, minor, patch), the board and the capability flags. Then come the node's station MAC, a
  counter of completed sweeps (scans, on the Bluetooth node), the epoch of the assignment held or
  0, running counts of Wi-Fi and BLE sightings turned away by a full buffer, the reset cause, the
  chip's 3.3 V rail and die temperature, one byte each, and the bytes free in its heap.
- **Sighting batch**: sent at the end of each dwell or scan, and acknowledged by the P4. A 3-byte
  header, then one 12-plus-SSID record per newly seen BSSID or advertiser, packed to fill 1024
  bytes. A record's trailer carries a Passpoint network's roaming consortium identifiers, or a BLE
  advertiser's manufacturer identifier, when there are any.
- **Assign**: the assignment.
- **Stop**: drops the assignment and empties the dedup ring. The node idles at epoch 0, as at
  boot, and adopts the next assignment whatever its epoch.

It also takes an image over the bus, in the update records:

| Message        | Direction   | Body              |
|----------------|-------------|-------------------|
| Update begin   | P4 → node   | 41 bytes          |
| Update chunk   | P4 → node   | 6 to 1029 bytes   |
| Update commit  | P4 → node   | 1 byte            |
| Update abort   | P4 → node   | 1 byte            |
| Update confirm | P4 → node   | Empty             |
| Update status  | Node → P4   | 6 bytes           |

A chunk carries up to 1024 bytes of image. § "Over-the-air updates" has the rest.

## What it does differently

**It listens instead of scanning.** It parks the radio and reads beacons and probe responses. So it
never transmits while looking, including on the DFS channels, where the rules say listen and don't
speak.

**It sends no Wi-Fi frames of its own.** Everything it says goes over the node bus. What the radio
answers in hardware, such as an acknowledgment to a frame addressed to this station, is capped at
2 dBm, the lowest power `set_max_tx_power` accepts (`src/main.rs`).

Listening also makes the rest possible. A node holds `WifiController::sniffer()` for the life of
the program, and owns the channel: it parks where it chooses. A scan from `scan_async` hops channels on its own schedule,
sends probe requests unless asked to be passive, and returns an `AccessPointInfo` per network with
no element bodies, so no roaming consortium trailer. The promiscuous callback sees each beacon
whole.

**It never waits on a channel to be heard.** Everything goes over the node bus, so the node hops
from dwell to dwell. Between dwells it reports what it heard and takes whatever the P4 has sent,
an assignment included. It heartbeats every 5 seconds from wherever it is.

**An unassigned node waits rather than sweeping.** It services the bus and heartbeats every second
until told what to scan, whether it has just booted or has been stopped. It **collects nothing
until then**. A scanning bridge answers the first heartbeat, so the wait lasts a single beat. That
is the intended trade, and explains where the first second of a capture went. A bridge whose scan
is stopped sends nothing, and the node idles until the scan starts.

**The node given the Bluetooth scan sweeps nothing.** It is dealt no channels and runs one scan
after another, back to back.
§ "Bluetooth is a whole node's job" has the rest.

**It forgets what it has reported in two cases.** An assignment that changes its channels or its
Bluetooth flag empties the dedup ring, because the entries describe a neighborhood it no longer
listens to. So does a stop, which also drops the assignment.
`crates/wartui-proto/src/dedup.rs` has the reasoning.

**It says what it is, and what it holds, in every heartbeat.** One byte is the epoch of the
assignment adopted, or 0 for none. That is how the bridge and the host tell adoption from a bus
ack without asking the node's own console. The five-byte prefix is the firmware version, the
board and a flags byte, shown by the host as:

```
0.1.0;5g
```

That is the firmware version, then `5g` for the C5's radio. The board byte is 1 for an ESP32-C5.
Flag bit 1 marks an image on trial, shown as `;trial`.
Bluetooth isn't a capability. Every node can scan it, and the bridge's `ASSIGN_FLAG_BLE` decides
whether one does, off at every boot.

The bridge deals no channels to a node until it has heard this. Every heartbeat carries it, not only
the first, because one sent once is lost to a dropped beat.

## Building and flashing

```sh
cargo run --release
```

To flash every attached ESP32-C5 at once, and never the bridge, run `wartui flash-fleet` from the
host. [`crates/wartui/README.md`](../../crates/wartui/README.md) § "Flashing the fleet" has how it
picks boards.

Each `esp-*` dependency that takes a chip feature names the C5, the only chip. The C5 is dual
band and gets `BandMode::Auto`. It is RISC-V and builds on stable.

Diagnostics go out over USB serial-JTAG. The cargo runner is
`espflash flash --monitor --partition-table partitions.csv --erase-parts otadata` with no `--chip`,
so espflash detects the part. Run it from this directory, since the table's path is relative. Unlike
the bridge's, this monitor is worth watching, because nothing but diagnostics goes down that pipe.

Every node can scan Bluetooth. It does so only if the bridge sets the flag, and a node that does
scans nothing else.

## Versioning

`version` in `Cargo.toml` is bumped on every firmware change. Before 1.0, a minor bump may change
the wire and a patch bump may not. `wartui_proto::bus::NODE_VERSION` mirrors it, and a test in
`crates/wartui-proto/tests/version.rs` fails when the two differ. The host drives a node on its own
major and minor at any patch, and so does the bridge's planner. The host shows a node on an older
major or minor as `needs update`, and one on a newer as `host behind`, and the bridge assigns
neither.

The heartbeat's prefix and the MAC after it, and the update records, are part of the bus's frozen
core ([`crates/wartui-proto/src/bus/mod.rs`](../../crates/wartui-proto/src/bus/mod.rs)
`//!`), so a host finds and updates a node on any version.

The app descriptor carries the version and a project name for the board: `wartui-node-esp32c5`.

## Flash layout

`partitions.csv` needs 4 MB of flash:

| Name     | Type | SubType   | Offset   | Size     |
|----------|------|-----------|----------|----------|
| nvs      | data | nvs       | 0x9000   | 0x4000   |
| otadata  | data | ota       | 0xd000   | 0x2000   |
| phy_init | data | phy       | 0xf000   | 0x1000   |
| ota_0    | app  | ota_0     | 0x10000  | 0x1E0000 |
| ota_1    | app  | ota_1     | 0x1F0000 | 0x1E0000 |
| trial    | data | undefined | 0x3D0000 | 0x1000   |

There is no factory slot. Two app slots of 1920 KiB each leave no room for a third on 4 MB, and the
app is about 880 KB. With `otadata` erased, the bootloader boots `ota_0`, which is where a USB flash
puts the app. Both USB paths erase `otadata`: `wartui flash-fleet` after writing each node, and the
cargo runner with `--erase-parts otadata`. So an earlier over-the-air update cannot leave the
bootloader pointing at the other slot. `trial` counts an updated image's unconfirmed boots. Its
subtype is `undefined` because espflash 4.6 rejects a numeric data subtype.

The cargo runner, `wartui flash-fleet` and the release images all use this table. An update over
the bus writes only an app slot, so a node keeps the table of its last wired flash, and every later
image keeps to it ([`crates/wartui-proto/src/bus/mod.rs`](../../crates/wartui-proto/src/bus/mod.rs)
`//!`, the frozen core).

## Over-the-air updates

`wartui_proto::ota::Receiver` decides what to write and checks the hash. `wartui_firmware::ota`
(`firmware/common/src/ota.rs`, shared with the P4 bridge's C6) gives it the flash: the app slot
that is not booted, found in the partition table at boot. `src/ota.rs` hands it the bus's update
records. `wartui ota` is the
host's side (`crates/wartui/README.md` § "Updating over the air"). The update records are frozen
([`crates/wartui-proto/src/bus/mod.rs`](../../crates/wartui-proto/src/bus/mod.rs) `//!`), so any
later wartui can update this build.

**Update mode.** An update begin the receiver accepts puts the node in update mode:

- sniffing nothing and scanning no Bluetooth;
- servicing the bus throughout, and answering each update record with a status;
- heartbeating every second, so the host sees it alive.

The node keeps its assignment throughout and returns to it when the session ends. An abort ends
it at once. So does 30 s without an update record, which also drops the session.

A node whose table has no `trial` sector refuses every begin with `FlashError`, so it never runs
an update untrialed.

**The `trial` sector decides.** The bootloader has no rollback and reads no image state, so the
node does it. The sector records which slot is on trial and how many times it has booted: an
armed record naming the slot, then one cleared bit per boot. Clearing a bit needs no erase and
survives the power cuts these nodes see routinely, which RTC memory does not. The node also writes
the `otadata` states (`New`, `PendingVerify`, `Valid`, `Aborted`) but never decides from them. The
sector's layout is `wartui_proto::ota::trial`, unit-tested there and frozen with the bus's core,
since one image writes it and the next reads it.

Each step's flash writes come in an order a power cut cannot turn into an untrialed image:

| Step     | Writes, in order                                                                 |
|----------|----------------------------------------------------------------------------------|
| Install  | erase `trial`; arm it for the new slot; select the new slot; `New`; reboot       |
| Trial    | `PendingVerify`; clear one count bit; set the heartbeat's trial flag             |
| Rollback | `Aborted`; select the other slot; erase `trial`; reboot                          |
| Confirm  | `Valid`; erase `trial`; clear the trial flag                                     |

**The switch.** A commit whose hash matches sends `Verified` and installs. Before selecting the new
slot, the node points `otadata` at the running one if it names another. A USB flash leaves
`otadata` erased, and that step leaves the entry a rollback returns to. If the install fails
before the new slot is selected, the node stays on the running image, sends `FlashError` once and
drops the session, so a re-sent begin starts afresh.

**Trial boots.** A boot armed for the booted slot is a trial. One armed for the other slot is left
over from an interrupted install or rollback, and is erased. With `otadata` erased, the node was
flashed over USB and nothing is on trial.

**Rollback.** The third trial boot rolls back, and so does a trial boot the node cannot count.
Three, not one, because a USB-C PD supply can reset a node once at startup. There is no watchdog,
so a hung image rolls back too: each power cycle counts a boot. A power cut mid-rollback leaves
the sector armed and over the count, so the next boot rolls back again.

**Confirmation.** The first record from the P4 the node decodes confirms the image: assign,
stop, update confirm, or any update record. A confirmed image is one that has heard the bridge
over the bus, which is the whole test. A bridge whose scan is stopped sends an idle node nothing,
so there the update's own confirm, or the scan starting, is what confirms it. An update record
confirms before the receiver sees it, because an update taken on trial would write over the only
slot known to work.

## The regulatory domain is set here

`esp-radio` defaults `country_info` to **China** and applies it under `WIFI_COUNTRY_POLICY_MANUAL`,
so nothing on the air overrides it. China's 5 GHz allocation excludes 5470–5725, and the driver
refuses those channels outright. A C5 left on the default silently loses 100–144 every sweep. The
host can't tell that from a node whose radio didn't tune. So `src/main.rs` passes `US`. This is not
a preference.

Otherwise, channel limits belong to the controller, not here. The node sends no Wi-Fi, so where it
dwells is a coverage decision rather than a legal one. This firmware
gates nothing: it parks on whatever it is dealt and reports a refusal. In practice a C5 tunes 41 of
the 42 channels in `SCAN_CHANNELS`.

The one a C5 won't take is **channel 14**. `esp-radio` refuses it through a hardcoded `nchan: 13`
that no exposed setting can reach. The refusal is the driver's, so the firmware still attempts the
channel. But no channel pool contains it. A node reports the refusal only on a serial console nobody
is watching, so a fleet given channel 14 would waste a dwell every sweep and the host would never
notice. [`docs/phase-1-findings.md`](../../docs/phase-1-findings.md) has the measurements and the
reading of the driver.

## Bluetooth is a whole node's job

**At most one node scans, and the bridge decides which.** Bit 0 of the assignment's flags byte
carries it, clear at every boot on every build. Bluetooth scanning is always compiled in; the flag
decides whether it runs. The assignment that sets it carries **no channels**, by design. An empty
mask with the flag means "Bluetooth is all of it". An empty mask without the flag is the one thing
the bridge never sends, and this firmware treats it as never having been told anything.

The two radios share one 2.4 GHz antenna, and a Bluetooth controller left free takes it whenever
the node is otherwise idle ([`docs/phase-0-findings.md`](../../docs/phase-0-findings.md),
[`docs/phase-2-findings.md`](../../docs/phase-2-findings.md)). A node with no sweep has nothing to
hand the antenna back to, so the conflict is arranged away rather than scheduled around. That costs
one node's Wi-Fi coverage. Whether a node scans it is the operator's choice of scan, and which
node is the bridge's pick, never a property of the binary on the board.

Scans run back to back: the controller off, the new advertisers reported, and straight into the next
scan, with no deadline of its own. A `node::ASSIGNED_BEAT_MS` timer runs alongside, independent of
the scan cadence. When it comes due, the node sends a heartbeat between two scans. A const-assert
beside `SCAN_MS` in `src/ble.rs` keeps a scan shorter than that timer, so the timer is always
checked between scans.

Two things keep the antenna where it belongs:

- The controller is told `HCI_LE_Set_Scan_Enable(0)` at the end of every scan, not just waited on.
  An initialized host stack left behind a finished scan can keep the radio.
- The node sniffs no Wi-Fi while it holds the scan, so the scan is the only thing that takes the
  antenna.

The scan needs Wi-Fi awake. The two share one radio, and the node is a station that never
associates, which `esp-radio` lets sleep by default whenever promiscuous mode is off. The node
holding the scan sniffs nothing, so its radio sleeps: the controller acknowledges every command and
hears nothing. `src/main.rs` turns disconnected power management off for that reason
([`docs/bluetooth-scan-findings.md`](../../docs/bluetooth-scan-findings.md)).

The controller comes up at boot on every build. Initializing a radio between two dwells is exactly
the kind of surprise this firmware exists to avoid. It is initialized and never
enabled: `HCI_LE_Set_Scan_Enable` is only ever sent from inside a scan. If the controller won't
start, a node handed the scan idles, sniffs nothing, and says so on its own console. The host can't
tell, since capabilities carry no Bluetooth bit to withhold. A controller that refuses a command is
named on the console, and the node scans anyway.

There is no host stack. `esp-radio` exposes the controller as a raw HCI pipe. This firmware wants
only an address, a signal strength, and a manufacturer identifier when an advertiser offers one. So
the four commands and two events live in `wartui_proto::hci`, where they are unit-tested, and
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

## Bench check

Run this before merging a change to the node's radio start-up, `src/ble.rs` or an `esp-*` version.
No CI job can check it: a Bluetooth scan whose radio sleeps succeeds at every command.

On a fleet of at least two nodes:

```sh
wartui scan start --bluetooth
wartui status                   # its `bluetooth` line names the node holding the scan
wartui sniff                    # 30 s, then ctrl-c
wartui scan stop
```

Expect `ble` sightings from the slot `status` named, beside `wifi` sightings from the others. A
holder that hears nothing in ten scans in a row prints `ble: no advertising events in N scans` on
its own console.

## Two things that are easy to get wrong

**Setting the channel is not `set_channel` alone.** On an unassociated station interface, the
channel doesn't stick unless promiscuous mode is on across the change. `radio::park` does that on
every hop. A node whose channel silently didn't change would report the right access points against
the wrong frequency.

**The promiscuous callback runs in the Wi-Fi driver's task, on a buffer that dies when it returns.**
It can't capture state, because `set_receive_cb` takes a bare `fn`, and it can't block. So it
rejects non-management frames before touching a lock, parses what it needs into a fixed-size
`Sighting`, and leaves it in a `static` buffer for the main loop. It also drops an access point
already pending. One beacons roughly ten times in a 125 ms dwell. Without the check, the buffer
would fill with copies of the loudest network and drop the ones not yet seen.

## Dependency versions

Same set as the bridge's radio, pinned by the same thing. See
[`firmware/p4-radio/README.md`](../p4-radio/README.md#dependency-versions).

`esp-println` is the one thing outside that set, and brings in nothing of it. It depends on
`esp-sync` only through its `critical-section` feature, a default that `Cargo.toml` turns off, so
it pulls no `esp-sync` at all. Turning `critical-section` on would bring it in. Verify with:

```sh
cargo tree -e normal -i esp-sync | grep esp-println   # prints nothing
```

Its serial-JTAG writer tries the FIFO a bounded number of times, then sets a sticky flag saying
nobody is draining it. So a node with no monitor attached drops diagnostics rather than stalling
mid-sweep. That is the only property this firmware needs from it, and the reason it is allowed here.

`esp-storage`, the flash driver under updates, is held at the version `esp-bootloader-esp-idf`
resolves, with its `embedded-storage` feature on for the `NorFlash` traits
`firmware/common/src/ota.rs` writes through. Its default `critical-section` feature stays on: it holds interrupts off across each flash
operation, through the same `esp-sync`.
