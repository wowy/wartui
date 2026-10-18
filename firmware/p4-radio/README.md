# wartui P4 radio firmware

The radio half of the P4 split bridge. On a Waveshare ESP32-P4 board, the on-board ESP32-C6 runs
this firmware: it receives and sends ESP-NOW on the fleet's control channel, and talks to the P4 as
a 40 MHz SPI slave over the board's SDIO traces. It never talks USB; the P4 owns the host link. The
ESP-NOW handling is `wartui_firmware::radio` (`firmware/common/src/radio.rs`), shared with the
single-chip bridge, and the queue sizes are in `src/config.rs`.

```sh
cargo build --release --features esp32c6
```
