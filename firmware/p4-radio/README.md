# wartui P4 radio firmware

The C6 half of the P4 split bridge. On a Waveshare ESP32-P4-WIFI6, the on-board ESP32-C6 runs this
firmware: it carries the host link to a Bluetooth central ([below](#bluetooth-host-link)), and
holds the UART link to the P4, answers the P4's status poll, sends its lines and takes its own
updates. It talks to the P4 over UART1, with DMA and hardware flow control, on the board's SDIO
traces ([wiring](../p4-bridge/README.md#the-link-to-the-c6)). It never talks USB; the P4 owns the
host link and the node buses. The build-time settings are in `src/config.rs`.

```sh
cargo build --release
```

## Bluetooth host link

The C6 runs TrouBLE 0.8 as a GATT peripheral over `esp-radio`'s controller (`src/ble.rs`). It
advertises `wartui XX:YY`, the bridge's last two octets, with the `wartui_proto::ble` service in its
scan response. It notifies the P4's host stream on Up and forwards what a central writes on Down.

- **Only a bonded, encrypted central reaches the stream.** It pairs by LE Secure Connections Just
  Works, inside the P4's pairing window. The P4 keeps the bonds and sends them down after each
  hello and every 5 s; the C6 keeps no flash state. A change to them stops the advertisement while
  TrouBLE loads them, and a connected central whose bond the P4 dropped is disconnected. So is a
  bonded central that stays unsubscribed for 30 s, so none holds the one connection idle.
  `src/ble.rs` has the rules.
- **The C6 asks for the 2M PHY and data length 251** once a central connects.
- **The stream is credited by position.** The C6 holds up to `HOST_WINDOW_BYTES` of it, and its
  credit is the stream offset it is done up to, in the P4's offsets. A lost P4 packet costs its
  frames and not the window, and a restart of either chip cannot skew it
  (`wartui_proto::radio_link::Inflow`). The credit is repeated every second while a central is
  subscribed, so a lost one never shuts the window.
- **A central ends each write group with a write request**, which the C6 answers once it has
  forwarded the write, so a central never overruns TrouBLE's queue of 8.
- **A Bluetooth host that stops restarts the C6**, and the next boot reports the reason.
- **Its counters ride on each `RadioStatus`:** pairings, failed pairings, centrals disconnected
  for holding no bond (a cleared one included), the last disconnect reason, resynced and dropped bytes, the PHY, the interval and
  the heap's high-water mark.
- **The heap is 96 KB:** 64 KB of reclaimed bootloader RAM and 32 KB more. The Bluetooth controller
  and the scheduler under it are all that allocate.

Two upstream bugs shape the build, each worked around without a patch
([`c6-ble-host-findings.md`](../../docs/c6-ble-host-findings.md#workarounds-a-production-build-needs)):

| Workaround | Why |
| --- | --- |
| TrouBLE's packet pool is 186 bytes (`TROUBLE_HOST_DEFAULT_PACKET_POOL_MTU` in `.cargo/config.toml`), so the ATT MTU is 182 | esp-radio 1.0.0-beta.1 marks every ACL fragment as a start, so a central drops a fragmented notification and the stream stalls |
| `trouble-host`'s `connection-params-update` stays off | bt-hci 0.10.1 panics on the reply to a central's connection parameter request; off, the controller answers it itself |

The ATT MTU of 182 (`wartui_proto::ble::ATT_MTU`) works around a third. esp-radio
1.0.0-beta.1's `ble_hs_rx_data` copies only the first buffer of a received ACL packet that
arrives as a chain, and the controller chains a packet of 193 to 198 bytes, its 4-byte header
included. The packet reaches TrouBLE short, its host stops, and the C6 restarts. On that MTU a
central writes at most 179 value bytes to Down, and no packet it sends passes 190 bytes
([`ble-host-link-findings.md`](../../docs/ble-host-link-findings.md#the-host-stop-found)).
Build from this directory, whose `.cargo/config.toml` sizes the pool; `src/ble.rs` fails the
build when the pool and the MTU disagree.

The central picks the connection interval unless the build names one.
`WARTUI_RADIO_CONN_INTERVAL_MS=7.5-15 cargo build --release` asks for 7.5 to 15 ms, with
latency 0 and a 4 s supervision timeout, over L2CAP. Each bound is a multiple of 1.25 ms from
7.5 ms, under 2 s. A central that refuses changes nothing. An iOS central wants a minimum of at
least 15 ms (`src/config.rs`).

The C6 keeps its last panic's text in RTC memory across the reset, and sends it to the P4 as
`Error` lines once the link is up (`src/last_panic.rs`).

## Boards

This firmware runs on the C6 of the Waveshare ESP32-P4-WIFI6, an ESP32-C6-MINI-1-N4 with 4 MB of
flash.

## First flash

The C6 has no USB, and is flashed through its header once. After that, every update reaches it
through the P4: the P4 image carries this one (see [Updates](#updates)).

Flash it through the 4-pin header H4 with a USB-to-TTL adapter. H4 carries IO9, GND, RXD and TXD.

1. With the board unpowered, jumper IO9 to GND, so the C6 boots into its download mode.
2. Hold the P4's BOOT button while powering the board, so the P4 stays in download mode and
   quiet.
3. Run, with the adapter's port:
   `cargo run --release -- --port <adapter>`
   (the runner is `espflash flash --before no-reset --after no-reset --partition-table
   partitions.csv --erase-parts otadata`; no reset line may toggle).
4. Remove the jumper, then power-cycle the board so both chips run.

The P4 firmware is flashed as in [`firmware/p4-bridge`](../p4-bridge/README.md#first-flash).

A C6 not running this link cannot hear the P4, which reports "the radio coprocessor is not
answering". Flash it as above, and every update after that reaches it over the link.

## Updates

`partitions.csv` is the node's table: two 1.9 MB app slots, `ota_0` and `ota_1`, and a `trial`
sector. The flash half is the node's too (`firmware/common/src/ota.rs`).

1. Every hello carries the SHA-256 digest espflash appends to the booted image.
2. When it differs from the image the P4 carries, the P4 sends that image over the UART link. The
   C6 writes it into the slot it is not running, checks its hash, switches slots and reboots.
3. The new image boots on trial. It is confirmed once it has heard the P4's hello and its
   Bluetooth host has started advertising. A C6 update reaches a Bluetooth-only host through
   Bluetooth, so an image whose Bluetooth never comes up must not strand that host. Such an
   image restarts itself when it has not advertised 30 s after boot.
4. A trial image that is never confirmed is rolled back on its third boot. Without a watchdog, a
   hung image counts boots through the P4, which resets the C6 when no hello follows the
   install.

The runner erases `otadata`, so a wired flash always boots `ota_0`, where it writes the app.
It also names the flash mode, frequency and size (DIO, 40 MHz, 4 MB), as the P4's build does
when it saves the bundled image. They sit in the header the digest covers, so a C6 flashed by
wire from the same tree carries the bundled digest and is not reflashed.

## Build-time settings

Each is read at build time from an environment variable, so a bench run changes it with a
rebuild, for example `WARTUI_RADIO_UP_QUEUE_BYTES=16384 cargo build --release`. The P4's build
passes both through to the image it bundles. `wartui status` reports the queue's high-water mark
as `radio-up`.

| Variable | Default | Sets |
| --- | --- | --- |
| `WARTUI_RADIO_UP_QUEUE_BYTES` | 8192 | the C6's lines, status and Bluetooth messages waiting for a packet to the P4; a full queue evicts the oldest |
| `WARTUI_RADIO_CONN_INTERVAL_MS` | unset: the central's | the connection interval the C6 asks a central for, `MIN-MAX` in ms ([above](#bluetooth-host-link)) |

## Dependency versions

`esp-radio 1.0.0-beta.1`, the node's, requires `esp-hal = "~1.2.0"`, and that one requirement
fixes the whole family. It holds the family in two ways:

- **Walled:** `esp-hal` and `esp-rtos`. Cargo enforces this one.
- **Resolves, but doubles:** `esp-alloc`, the `esp-wifi-sys-*` bindings, `esp-sync`, and
  `esp-storage`.

This firmware links the node's `esp-radio`, with `ble` alone, and the node's `esp-alloc`. The P4's
has no `esp-radio` of its own. All three link the same `esp-hal`, `esp-rtos`, `esp-storage` and
`esp-sync`. Keep that true: the three share `wartui-proto` and `firmware/common`, which states the
same `esp-hal` requirement.

`embassy-executor`, `embassy-futures`, `embassy-sync`, `embassy-time`, and `embedded-io-async` are
held the same way, through `esp-rtos` and `esp-hal`.

The C5's PAC resolves twice in the node, and that is known: `esp-hal 1.2.2` builds against
`esp32c5 0.3`, `esp-radio 1.0.0-beta.1` against `esp32c5 0.4`. Both build and link. A later
`esp-hal` release on the newer PAC lines them up. The C6's does the same here, `esp32c6` 0.24
and 0.25.

`trouble-host` 0.8.0 is the newest release, and is outside the family: nothing in `esp-radio`
depends on it. It builds on `bt-hci` 0.10, and `esp-radio` on 0.9, so `bt-hci` resolves twice.
Both are plain types with no global symbols, and the controller plugs in through
`bt-hci-transport` 0.1, which both share. `Cargo.toml` names its two workaround features
([above](#bluetooth-host-link)).

### Crates that double

The other half of the family is the dangerous half, because it resolves. `esp-alloc` is a
dependency of `esp-radio` *and* a direct dependency of the node. A version above what `esp-radio`
asks for is semver-incompatible with it, not in conflict with it. So Cargo adds a second copy
instead of refusing, and links both. The `esp-wifi-sys-*` bindings double the same way when anything
names them beside `esp-radio`:

- Two `esp-wifi-sys-*` each export `__esp_radio_printf` as `no_mangle`, and each carries its own
  copy of the IDF Wi-Fi blobs. `lto = "fat"` refuses the build. That is the cheap version.
- Two `esp-alloc` are two heaps. Only the direct copy takes `#[global_allocator]` and the `malloc`
  shims. Only that one is handed regions by `heap_allocator!` in `firmware/node/src/main.rs`. But `esp-rtos` and
  `esp-radio` allocate every task stack and every Wi-Fi driver queue through *their* copy's
  `esp_alloc::InternalMemory`. It reads its own `HEAP` static rather than going through those shims.
  So the firmware builds, links, and passes CI, then panics at `esp_rtos::start`, before the radio
  is up. The panic handler resets, so the board does it again: 88 boots in 20 seconds, never
  reaching `Ready`. No host build can see any of that.

A second copy does not need a manifest to arrive, either. `esp-sync` is a direct dependency of the
node alone, and `esp-storage` of `firmware/common` alone, yet either doubles in each firmware if
anything above it moves out of step.

### Holding the set

Every crate `esp-radio` also depends on is held at the version `esp-radio` resolves, and they all
move when it does. Two things hold that without a board to test on:

- `.github/dependabot.yml` ignores the incompatible bumps in each firmware directory;
- each firmware's CI job counts the versions before it builds.

By hand, from any firmware's directory:

```sh
cargo tree --locked -e normal --prefix none --format '{p}' \
  | awk '{print $1, $2}' | sort -u \
  | grep -E '^esp-(alloc|hal|radio|rtos|storage|sync|wifi-sys-esp32c[56]) '
```

They are direct dependencies for reasons that do not go away. `esp-alloc` is the node's and this
firmware's because each owns its heap and declares its regions. The P4's allocates nothing and has
no heap.

`esp-storage` is a direct dependency of `firmware/common` alone, for the over-the-air updates
every firmware takes. It is held at the version `esp-bootloader-esp-idf` resolves, and brings no
crate the tree lacks: its `esp-sync` and `esp-rom-sys` are the copies `esp-hal` uses, and
`esp-rom-sys` is `links`-keyed besides. Each firmware's CI job counts it with the rest, and
Dependabot ignores its and `esp-bootloader-esp-idf`'s minor and major releases.

### Embassy and `embedded-io-async`

`embassy-executor`, `embassy-futures`, `embassy-sync`, `embassy-time`, and `embedded-io-async` are
held at the versions `esp-rtos` and `esp-hal` build against, and move with them. `embedded-io-async`
stays at 0.7, the newer of the two versions `esp-hal` 1.2 implements its async traits for. Three
cannot double quietly:

- the executor's `Spawner` would not type-check against `esp-rtos`'s;
- `embassy-time-driver` is `links`-keyed;
- a second `embedded-io-async` would have no implementation on the USB half.

None of the five is in the check, and Dependabot ignores their minor and major releases.

`embassy-sync` is held at 0.8, the copy `esp-hal` and `esp-rtos` use, for the GPS reader's
channels and the bridge's signals. 0.6 and 0.7 are in the lockfile too, both `esp-sync`'s, which depends
on all three under aliases. A second 0.x would double rather than conflict, and harmlessly: the
channels never pass through an `esp-hal` or `esp-rtos` API, and the crate keeps no global state. It
is held so the tree adds no copy, not because one would break the build.

`embassy-futures` is held at 0.1 because `esp-hal` 1.2.2 depends on `embassy-futures` 0.1 directly,
so a higher minor is a second copy. Nothing suggests one would break the build or the board. It too
is held so the tree adds no copy.

`esp-generate` is a version behind this set. Its scaffolding (`build.rs`, `.cargo/config.toml`) is
what was taken from it, not its dependency list.
