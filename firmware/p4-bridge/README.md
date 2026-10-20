# wartui P4 bridge firmware

The host half of the P4 split bridge. On a Waveshare ESP32-P4 board, the P4 runs this firmware: it
talks to the host over USB 2.0 high speed with wartui's link protocol, and to the on-board
ESP32-C6 as a 40 MHz SPI master over the board's SDIO traces. The C6 runs the radio
([`firmware/p4-radio`](../p4-radio/README.md)). The host sees a bridge like the single-chip ones,
reporting `Esp32P4` and both chips' diagnostics. The queue sizes are in `src/config.rs`.

```sh
cargo build --release
```

## Boards

One image serves the Waveshare ESP32-P4-WIFI6-POE-ETH and the ESP32-P4-WIFI6.

## First flash

The Type-C reaches the P4's UART0 through a CH343, so the runner needs `--port <that port>`.

- **POE-ETH:** there is no auto-reset, because the CH343's RTS connects to nothing. Hold BOOT while
  plugging in the Type-C, run `cargo run --release` (or `espflash flash --chip esp32p4`), then
  unplug and replug to run. Pressing RST lands in download mode.
- **WIFI6:** the CH343P's DTR and RTS drive EN and GPIO35 through a transistor pair, so espflash
  resets it normally.

The C6 is flashed separately, and the P4 stays in download mode while it is: see
[`firmware/p4-radio`](../p4-radio/README.md#first-flash).

Updates over the one cable are step 3 of the design and are not built yet; until then both chips
are flashed by hand.

## USB

The bridge enumerates as `1209:0001`, product `wartui p4 bridge`, with the bridge (C6) MAC as its
serial number, or the P4's own MAC when the C6 never answered its hello. `wartui ports` lists it,
`wartui flash-bridge` refuses it and `wartui flash-fleet` skips it.

**The high-speed port always drives 5 V.** It is the Type-A on the POE-ETH (a load switch with its
enable tied high) and the 4-pin MX1.25 connector `P1` on the WIFI6 (pin 1 is the board's VCC_5V
rail). A cable to a PC that carries VBUS back-feeds the PC whenever the Type-C is also connected,
so the cable must leave VBUS unconnected. A WIFI6 can instead be powered from that port alone.

## GPS

The default build reads a u-blox receiver over I2C and passes its bytes to the host, as the
T-Dongle-C5 does. Bytes read while no host holds DTR are dropped, so a host that attaches late
gets live fixes rather than a backlog. So are the lines saying the receiver stopped or came back;
the announcement carries the current fault instead. Wire it to the P4's header:

| Receiver | P4 |
| --- | --- |
| SDA | GPIO7 |
| SCL | GPIO8 |
| VCC | 3V3 |
| GND | GND |

The receiver answers at `0x42`. The board has 2.2 kΩ pull-ups on both lines, and its ES8311 codec
shares the bus at another address. Without a receiver, every announcement carries "no GPS receiver
answered on the I2C bus (GPIO7/GPIO8)". `cargo build --release --no-default-features` builds
without GPS: the bridge reports none and never touches the bus.

## Queue sizes

Each is read at build time from an environment variable, so a bench run resizes one with a
rebuild, for example `WARTUI_P4_HOST_BULK_BYTES=131072 cargo build --release`. `wartui status`
reports each queue's high-water mark.

| Variable | Default | Holds |
| --- | --- | --- |
| `WARTUI_P4_HOST_BULK_BYTES` | 262144 | frames for the host that may be evicted: received frames and logs |
| `WARTUI_P4_HOST_PRIORITY_BYTES` | 16384 | frames that answer the host |
| `WARTUI_P4_DOWN_QUEUE_BYTES` | 16384 | `Down` records waiting for a transfer to the C6 |

The SPI clock (40 MHz) is fixed.
