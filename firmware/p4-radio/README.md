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

The C6 has no USB. Flash it through the 4-pin header with a USB-to-TTL adapter: H7 on the
POE-ETH, H4 on the WIFI6, carrying IO9, GND, RXD and TXD.

1. With the board unpowered, jumper IO9 to GND, so the C6 boots into its download mode.
2. Hold the P4's BOOT button while powering the board, so the P4 stays in download mode and
   quiet.
3. Run, with the adapter's port:
   `cargo run --release --features esp32c6 -- --port <adapter>`
   (the runner is `espflash flash --before no-reset --after no-reset`; no reset line may toggle).
4. Remove the jumper, then power-cycle the board so both chips run.

This crate has no partition table of its own. The P4 firmware is flashed as in
[`firmware/p4-bridge`](../p4-bridge/README.md#first-flash).

No C6 build implements the update path yet, so a C6 flashed from this build needs one more wired
flash when single-cable updates (step 3 of the design) land.

**The high-speed USB port always drives 5 V.** A cable from it to a PC must leave VBUS
unconnected, or it back-feeds the PC whenever the Type-C is also connected; see
[`firmware/p4-bridge`](../p4-bridge/README.md#usb).

## Queue sizes

Each is read at build time from an environment variable, so a bench run resizes one with a
rebuild, for example `WARTUI_RADIO_UP_QUEUE_BYTES=65536 cargo build --release --features esp32c6`.
`wartui status` reports each queue's high-water mark.

| Variable | Default | Holds |
| --- | --- | --- |
| `WARTUI_RADIO_UP_QUEUE_BYTES` | 98304 | `Up` records waiting for a transfer to the P4; a full queue evicts the oldest |
| `WARTUI_RADIO_SEND_JOBS` | 4 | send jobs waiting for the radio, and results waiting for the link; a job that finds the queue full is answered `Rejected` |
