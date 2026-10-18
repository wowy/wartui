# wartui P4 bridge firmware

The host half of the P4 split bridge. On a Waveshare ESP32-P4 board, the P4 runs this firmware: it
talks to the host over USB 2.0 high speed with wartui's link protocol, and to the on-board
ESP32-C6 as a 40 MHz SPI master over the board's SDIO traces. The C6 runs the radio
([`firmware/p4-radio`](../p4-radio/README.md)). The host sees a bridge like the single-chip ones,
reporting `Esp32P4` and both chips' diagnostics. The queue sizes are in `src/config.rs`.

```sh
cargo build --release
```
