# wartui P4 radio firmware

The radio half of the P4 split bridge. On a Waveshare ESP32-P4 board, the on-board ESP32-C6 runs
this firmware: it receives and sends ESP-NOW on the fleet's control channel, and talks to the P4 as
a 40 MHz SPI slave over the board's SDIO traces. It never talks USB; the P4 owns the host link. The
ESP-NOW handling is `wartui_firmware::radio` (`firmware/common/src/radio.rs`), shared with the
single-chip bridge, and the queue sizes are in `src/config.rs`.

```sh
cargo build --release --features esp32c6
```

## Boards

One image serves the C6 on both the Waveshare ESP32-P4-WIFI6-POE-ETH and the ESP32-P4-WIFI6.

## First flash

The C6 has no USB, and is flashed through its header once. After that, every update reaches it
through the P4: the P4 image carries this one (see [Updates](#updates)).

Flash it through the 4-pin header with a USB-to-TTL adapter: H7 on the POE-ETH, H4 on the WIFI6,
carrying IO9, GND, RXD and TXD.

1. With the board unpowered, jumper IO9 to GND, so the C6 boots into its download mode.
2. Hold the P4's BOOT button while powering the board, so the P4 stays in download mode and
   quiet.
3. Run, with the adapter's port:
   `cargo run --release --features esp32c6 -- --port <adapter>`
   (the runner is `espflash flash --before no-reset --after no-reset --partition-table
   partitions.csv --erase-parts otadata`; no reset line may toggle).
4. Remove the jumper, then power-cycle the board so both chips run.

The P4 firmware is flashed as in [`firmware/p4-bridge`](../p4-bridge/README.md#first-flash).

A C6 flashed from a build older than the update path sends a hello without an image digest. The
P4 cannot update it and reports "radio predates bundled updates; flash it through the header
once". Flash it as above.

## Updates

`partitions.csv` is the node's table: two 1.9 MB app slots, `ota_0` and `ota_1`, and a `trial`
sector. The flash half is the node's too (`firmware/common/src/ota.rs`).

1. Every hello carries the SHA-256 digest espflash appends to the booted image.
2. When it differs from the image the P4 carries, the P4 sends that image over the SPI link. The
   C6 writes it into the slot it is not running, checks its hash, switches slots and reboots.
3. The new image boots on trial. The P4's hello confirms it, as hearing the host confirms a
   node's.
4. A trial image that never hears the P4 is rolled back on its third boot. Without a watchdog, a
   hung image counts boots through the P4, which resets the C6 when no hello follows the
   install.

The runner erases `otadata`, so a wired flash always boots `ota_0`, where it writes the app.

**The high-speed USB port always drives 5 V.** A cable from it to a PC must leave VBUS
unconnected, or it back-feeds the PC whenever the Type-C is also connected; see
[`firmware/p4-bridge`](../p4-bridge/README.md#usb).

## Queue sizes

Each is read at build time from an environment variable, so a bench run resizes one with a
rebuild, for example `WARTUI_RADIO_UP_QUEUE_BYTES=65536 cargo build --release --features esp32c6`.
`wartui status` reports each queue's high-water mark.

| Variable | Default | Holds |
| --- | --- | --- |
| `WARTUI_RADIO_UP_QUEUE_BYTES` | 98304 | received frames waiting for a transfer to the P4; a full queue evicts the oldest |
| `WARTUI_RADIO_UP_PRIORITY_BYTES` | 8192 | every other `Up` record (send results, lines, the C6's status), sent ahead of received frames; a full queue evicts the oldest |
| `WARTUI_RADIO_JOBS` | 32 | sends and peer and power commands waiting for the radio, in link order, and results waiting for the link; a full queue answers a send `Rejected` and any other job with an `Error` line |
| `WARTUI_RADIO_SEND_JOBS` | 4 | payloads of the sends among those jobs; a send that finds the pool full is answered `Rejected` |
