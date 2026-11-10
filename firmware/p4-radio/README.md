# wartui P4 radio firmware

The C6 half of the P4 split bridge. On a Waveshare ESP32-P4-WIFI6, the on-board ESP32-C6 runs this
firmware: it holds the UART link to the P4, answers the P4's status poll, sends its lines and
takes its own updates. On the Bluetooth spike branch it also carries the host link to a central
([below](#bluetooth-spike)). It talks to the P4 over UART1, with DMA and
hardware flow control, on the board's SDIO traces
([wiring](../p4-bridge/README.md#the-link-to-the-c6)). It never talks USB; the P4 owns the host
link and the node buses. The queue size is in `src/config.rs`.

The `sink-discard` feature discards the host stream the P4 sends for a Bluetooth central,
crediting it at once, and claims a subscriber after the hello exchange; Bluetooth does not start.
`firmware/p4-bridge`'s feature of the same name bundles it.

## Bluetooth spike

The C6 advertises `wartui XX:YY` and serves the GATT service in `tools/ble-standin/README.md`,
through TrouBLE 0.8 over `esp-radio`'s controller (`src/ble.rs`). It notifies the P4's host
stream at up to an ATT MTU of 517, and counts link-layer delivery from the HCI traffic
(`src/hci.rs`). It boots with one `Log` naming the controller's address.

On this branch it links `esp-radio` with `ble`, and has a 96 KB heap: 64 KB of reclaimed
bootloader RAM and 32 KB more. The Bluetooth controller is the only thing that allocates.
`BleStats` reports the heap's use and high-water mark.

```sh
cargo build --release
```

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
3. The new image boots on trial. The P4's hello confirms it, as hearing the host confirms a
   node's.
4. A trial image that never hears the P4 is rolled back on its third boot. Without a watchdog, a
   hung image counts boots through the P4, which resets the C6 when no hello follows the
   install.

The runner erases `otadata`, so a wired flash always boots `ota_0`, where it writes the app.
It also names the flash mode, frequency and size (DIO, 40 MHz, 4 MB), as the P4's build does
when it saves the bundled image. They sit in the header the digest covers, so a C6 flashed by
wire from the same tree carries the bundled digest and is not reflashed.

## Queue sizes

The C6 has two queues. Each size is read at build time from an environment variable, so a bench
run resizes it with a rebuild, for example
`WARTUI_RADIO_UP_QUEUE_BYTES=16384 cargo build --release`. `wartui status` reports its high-water
mark as `radio-up`.

| Variable | Default | Holds |
| --- | --- | --- |
| `WARTUI_RADIO_UP_QUEUE_BYTES` | 8192 | the C6's lines and status waiting for a packet to the P4; a full queue evicts the oldest |
| `WARTUI_RADIO_BLE_WINDOW_BYTES` | 16384 | the host stream waiting to be notified; matches the P4's `WARTUI_P4_BLE_WINDOW_BYTES` |

## Dependency versions

`esp-radio 1.0.0-beta.1`, the node's, requires `esp-hal = "~1.2.0"`, and that one requirement
fixes the whole family. It holds the family in two ways:

- **Walled:** `esp-hal` and `esp-rtos`. Cargo enforces this one.
- **Resolves, but doubles:** `esp-alloc`, the `esp-wifi-sys-*` bindings, `esp-sync`, and
  `esp-storage`.

The P4's firmware has no `esp-radio` of its own; this one links the node's, on the Bluetooth
spike. Both link the same `esp-hal`, `esp-rtos`, `esp-storage` and `esp-sync` as the node. Keep that true: the three share
`wartui-proto` and `firmware/common`, which states the same `esp-hal` requirement.

`embassy-executor`, `embassy-futures`, `embassy-sync`, `embassy-time`, and `embedded-io-async` are
held the same way, through `esp-rtos` and `esp-hal`.

The C5's PAC resolves twice in the node, and that is known: `esp-hal 1.2.2` builds against
`esp32c5 0.3`, `esp-radio 1.0.0-beta.1` against `esp32c5 0.4`. Both build and link. A later
`esp-hal` release on the newer PAC lines them up. The C6's does the same here, `esp32c6` 0.24
and 0.25, and so does `bt-hci`, 0.9 for `esp-radio` and 0.10 for TrouBLE: types only.

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

They are direct dependencies for reasons that do not go away. `esp-alloc` is the node's because the node owns the heap and declares its regions, and this
firmware's on the Bluetooth spike for the same reason. The P4's allocates nothing and has no heap.

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
