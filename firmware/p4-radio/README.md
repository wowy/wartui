# wartui P4 radio firmware

The radio half of the P4 split bridge. On a Waveshare ESP32-P4 board, the on-board ESP32-C6 runs
this firmware: it receives and sends ESP-NOW on the fleet's control channel, and talks to the P4 as
a 40 MHz SPI slave over the board's SDIO traces. It never talks USB; the P4 owns the host link. The
ESP-NOW handling is `wartui_firmware::radio` (`firmware/common/src/radio.rs`), shared with the
single-chip bridge, and the queue sizes are in `src/config.rs`.

```sh
cargo build --release --features esp32c6
```

## First flash

The C6 has no USB. Flash it through the 4-pin header with a USB-to-TTL adapter: H7 on the
POE-ETH, H4 on the WIFI6, carrying IO9, GND, RXD and TXD.

1. Hold the P4 in download mode (hold BOOT at power-on), so it stays quiet.
2. Jumper IO9 to GND.
3. Power the board and run, with the adapter's port:
   `cargo run --release --features esp32c6 -- --port <adapter>`
   (the runner is `espflash flash --before no-reset --after no-reset`; no reset line may toggle).
4. Remove the jumper before the P4 next boots.

This crate has no partition table of its own. The P4 firmware is flashed as in
[`firmware/p4-bridge`](../p4-bridge/README.md#first-flash).

The high-speed USB port on these boards always drives 5 V; see the warning in
[`firmware/p4-bridge`](../p4-bridge/README.md#usb).

## Queue sizes

Each is read at build time from an environment variable, so a bench run resizes one with a
rebuild, for example `WARTUI_RADIO_UP_QUEUE_BYTES=65536 cargo build --release --features esp32c6`.
`wartui status` reports each queue's high-water mark.

| Variable | Default | Holds |
| --- | --- | --- |
| `WARTUI_RADIO_UP_QUEUE_BYTES` | 98304 | `Up` records waiting for a transfer to the P4; a full queue evicts the oldest |
| `WARTUI_RADIO_SEND_JOBS` | 4 | send jobs waiting for the radio, and results waiting for the link; a job that finds the queue full is answered `Rejected` |
