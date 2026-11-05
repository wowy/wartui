# wartui P4 radio firmware

The radio half of the P4 split bridge. On a Waveshare ESP32-P4-WIFI6, the on-board ESP32-C6 runs
this firmware: it receives and sends ESP-NOW on the fleet's control channel, and talks to the P4
over UART1, with DMA and hardware flow control, on the board's SDIO traces
([wiring](../p4-bridge/README.md#the-link-to-the-c6)). It never talks USB; the P4 owns the host
link. The ESP-NOW handling is `wartui_firmware::radio` (`firmware/common/src/radio.rs`), and the queue sizes
are in `src/config.rs`.

It knows the link's messages, `esp-radio` and the magic that marks a frame as wartui's, and nothing
else. What an air frame is, what a heartbeat means, and how channels are assigned all live on the
host. There they are unit-testable, and a fix costs a `cargo run` rather than a reflash.

```sh
cargo build --release --features esp32c6
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
   `cargo run --release --features esp32c6 -- --port <adapter>`
   (the runner is `espflash flash --before no-reset --after no-reset --partition-table
   partitions.csv --erase-parts otadata`; no reset line may toggle).
4. Remove the jumper, then power-cycle the board so both chips run.

The P4 firmware is flashed as in [`firmware/p4-bridge`](../p4-bridge/README.md#first-flash).

A C6 whose hello has no image digest cannot take an update, so the P4 reports "radio predates
bundled updates; flash it through the header once". Flash it as above.

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

**The high-speed USB port always drives 5 V.** A cable from it to a PC must leave VBUS
unconnected, or it back-feeds the PC whenever the Type-C is also connected; see
[`firmware/p4-bridge`](../p4-bridge/README.md#usb).

## Transmitting

The radio answers a send with the **transmit-callback** status, not the enqueue result. The
receiver's own MAC hardware acknowledges unicast ESP-NOW, so `AckOk` means a node really has the
frame.

The callback returns in 2–4 ms, against the node's 100 ms admin window. The pessimistic case is an
_unacknowledged_ send, where it fires only after the radio exhausts its retry chain. That takes
28–35 ms ([`docs/phase-4-findings.md`](../../docs/phase-4-findings.md)). So the bridge has more than
an order of magnitude in hand when a send is acknowledged. When an assignment queues behind an
unacknowledged one, the margin is under three times.

The send does not block the loop. `wartui_firmware::radio` runs sends in their own future, joined
with the rest. Each `SendEspNow`, `AddPeer`, `RemovePeer` and `SetTxPower` goes on one queue, so the
radio acts on them in the order the host sent them, and goes on draining `esp-radio`'s ten-frame
receive queue while a send is in flight. A send's payload waits in a pool beside it. A send that
finds either full is answered `Rejected`, and a peer or power command that finds the queue full gets
an `Error` line. `Status.queues` reports both as `radio-jobs` and `radio-send-jobs`, with their
high-water marks and how many jobs each refused.

Peers are added on demand (`ensure_peer`) and never removed as a side effect of sending. The radio's
table holds twenty entries, and `esp-radio` spends one on the broadcast peer at init. This firmware
only _receives_ broadcasts, and ESP-NOW delivers a received frame whether or not its sender is a
peer. So it gives that slot up to make room for a twentieth node.

The host removes a node's peer (`RemovePeer`) a minute after its last heartbeat.

The two directions carry different frame sizes:

| Direction  | ESP-NOW payload                      | Link frame                    |
| ---------- | ------------------------------------ | ----------------------------- |
| host → air | up to 1470 bytes (`SendPayload`, v2) | up to 1600 (`MAX_HOST_FRAME`) |
| air → host | up to 1024 bytes (`EspNowPayload`)   | up to 1152 (`MAX_FRAME`)      |

Both the C5 and the C6 run ESP-NOW v2 and take 1470-byte frames intact
([`docs/espnow-v2-findings.md`](../../docs/espnow-v2-findings.md)). Nothing a wartui node sends is
longer than 1024 bytes. A received frame over 1024 bytes is somebody else's: the radio drops it
without a log line and counts it in `Status.rx_oversize`, not in `rx_count`.

## Queue sizes

Each is read at build time from an environment variable, so a bench run resizes one with a
rebuild, for example `WARTUI_RADIO_UP_QUEUE_BYTES=65536 cargo build --release --features esp32c6`.
`wartui status` reports each queue's high-water mark.

`WARTUI_RADIO_UP_QUEUE_BYTES` is what absorbs a burst above the link's rate
([`docs/p4-uart-link-findings.md`](../../docs/p4-uart-link-findings.md)).

| Variable | Default | Holds |
| --- | --- | --- |
| `WARTUI_RADIO_UP_QUEUE_BYTES` | 98304 | received frames waiting for a packet to the P4; a full queue evicts the oldest |
| `WARTUI_RADIO_UP_PRIORITY_BYTES` | 8192 | every other `Up` record (send results, lines, the C6's status), sent ahead of received frames; a full queue evicts the oldest |
| `WARTUI_RADIO_JOBS` | 32 | sends and peer and power commands waiting for the radio, in link order, and results waiting for the link; a full queue answers a send `Rejected` and any other job with an `Error` line |
| `WARTUI_RADIO_SEND_JOBS` | 4 | payloads of the sends among those jobs; a send that finds the pool full is answered `Rejected` |

## Dependency versions

`esp-radio 1.0.0-beta.1` requires `esp-hal = "~1.2.0"`, and that one requirement fixes the whole
family. It holds the family in two ways:

- **Walled:** `esp-hal` and `esp-rtos`. Cargo enforces this one.
- **Resolves, but doubles:** `esp-alloc`, the `esp-wifi-sys-*` bindings, `esp-sync`, and
  `esp-storage`.

The same dependency holds this firmware and the node at the same set, and the P4 firmware links the
same `esp-hal` and `esp-rtos`. Keep that true: they share `wartui-proto` and `firmware/common`,
which states the same `esp-hal` requirement.

`embassy-executor`, `embassy-futures`, `embassy-sync`, `embassy-time`, and `embedded-io-async` are
held the same way, through `esp-rtos` and `esp-hal`.

Each chip's PAC resolves twice, and that is known: `esp-hal 1.2.2` builds against `esp32c5 0.3` and
`esp32c6 0.24`, `esp-radio 1.0.0-beta.1` against `esp32c5 0.4` and `esp32c6 0.25`. Both build and
link. A later `esp-hal` release on the newer PACs lines them up.

### Crates that double

The other half of the family is the dangerous half, because it resolves. `esp-alloc` is a
dependency of `esp-radio` *and* a direct dependency of each firmware with a radio, and the
`esp-wifi-sys-*` bindings of `esp-radio` and the node. A version above what `esp-radio` asks for is
semver-incompatible with it, not in conflict with it. So Cargo adds a second copy instead of
refusing, and links both:

- Two `esp-wifi-sys-*` each export `__esp_radio_printf` as `no_mangle`, and each carries its own
  copy of the IDF Wi-Fi blobs. `lto = "fat"` refuses the build. That is the cheap version.
- Two `esp-alloc` are two heaps. Only the direct copy takes `#[global_allocator]` and the `malloc`
  shims. Only that one is handed regions by `heap_allocator!` in `src/main.rs`. But `esp-rtos` and
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

By hand, from this directory or the node's (there, drop `--features esp32c6`):

```sh
cargo tree --locked --features esp32c6 -e normal --prefix none --format '{p}' \
  | awk '{print $1, $2}' | sort -u \
  | grep -E '^esp-(alloc|hal|radio|rtos|storage|sync|wifi-sys-esp32c[56]) '
```

They are direct dependencies for reasons that do not go away. `esp-wifi-sys-*` is a direct
dependency of the node alone, for `esp_wifi_get_max_tx_power` (`tx_power` in
`firmware/node/src/radio.rs`), which `esp-radio` does not wrap. This firmware sets its transmit
power through `WifiController::set_max_tx_power` and reaches the bindings only through `esp-radio`.

`esp-alloc` is here because the firmware owns the heap and declares its regions.

`esp-storage` is a direct dependency of `firmware/common` alone, behind the `ota` feature the node
and this firmware turn on for over-the-air updates. It is held at the version
`esp-bootloader-esp-idf` resolves, and brings no crate the tree lacks: its `esp-sync` and
`esp-rom-sys` are the copies `esp-hal` uses, and `esp-rom-sys` is `links`-keyed besides. Each
firmware's CI job counts it with the rest, and Dependabot ignores its and
`esp-bootloader-esp-idf`'s minor and major releases.

### Embassy and `embedded-io-async`

`embassy-executor`, `embassy-futures`, `embassy-sync`, `embassy-time`, and `embedded-io-async` are
held at the versions `esp-rtos` and `esp-hal` build against, and move with them. `embedded-io-async`
stays at 0.7, the newer of the two versions `esp-hal` 1.2 implements its async traits for. Three
cannot double quietly:

- the executor's `Spawner` would not type-check against `esp-rtos`'s;
- `embassy-time-driver` is `links`-keyed;
- a second `embedded-io-async` would have no implementation on the USB half.

None of the five is in the check, and Dependabot ignores their minor and major releases.

`embassy-sync` is the GPS build's alone, for the reader's channels, and held at 0.8, the copy
`esp-hal` and `esp-rtos` use. 0.6 and 0.7 are in the lockfile too, both `esp-sync`'s, which depends
on all three under aliases. A second 0.x would double rather than conflict, and harmlessly: the
channels never pass through an `esp-hal` or `esp-rtos` API, and the crate keeps no global state. It
is held so the tree adds no copy, not because one would break the build.

`embassy-futures` is held at 0.1 because `esp-hal` 1.2.2 depends on `embassy-futures` 0.1 directly,
so a higher minor is a second copy. Nothing suggests one would break the build or the board. It too
is held so the tree adds no copy.

`esp-generate` is a version behind this set. Its scaffolding (`build.rs`, `.cargo/config.toml`) is
what was taken from it, not its dependency list.
