# The node bus

The node bus is SPI between the P4 bridge and its nodes. The P4 is master on two buses, A and B, and
each bus has up to six nodes, one per CS line, called slots `A1` to `A6` and `B1` to `B6`. Today a
node sends a copy of each heartbeat over the bus, and ESP-NOW carries everything as before.

## Pins

On the ESP32-P4-WIFI6:

| Signal | Bus A (SPI2) | Bus B (SPI3) |
| --- | --- | --- |
| SCLK, P4 → node | 20 | 31 |
| MOSI, P4 → node | 21 | 30 |
| MISO, node → P4 | 22 | 29 |
| IRQ, node → P4, one per bus | 23 | 28 |

| CS, P4 → node | Slot 1 | Slot 2 | Slot 3 | Slot 4 | Slot 5 | Slot 6 |
| --- | --- | --- | --- | --- | --- | --- |
| Bus A | 26 | 27 | 32 | 33 | 46 | 47 |
| Bus B | 50 | 49 | 5 | 4 | 3 | 2 |

On the XIAO ESP32-C5:

| Signal | Pin |
| --- | --- |
| SCLK | D8 (GPIO8) |
| MOSI | D10 (GPIO10) |
| MISO | D9 (GPIO9) |
| CS | D7 (GPIO12) |
| IRQ | D0 (GPIO1) |

## Pin behavior

- **SPI mode 0**, at 1 MHz. The target on the carrier board is 4 MHz.
- **CS is active low and idles high.** The P4 drives each slot's CS as a plain output, high from
  boot. The node pulls its CS up, so it reads idle with no P4 attached.
- **MISO is pulled up on the P4**, so an empty slot reads constant bytes and fails the magic.
- **IRQ is shared by a bus's nodes.** A node drives D0 high to ask for a transfer. On the carrier
  board the line is active low. A node wired straight to the P4 makes it active high, and the P4
  pulls it down.

Every transfer is a fixed 1,040 bytes each way. The framing, and the handshake that re-arms the
node between transfers, are in the `//!` of
[`crates/wartui-proto/src/bus/mod.rs`](../crates/wartui-proto/src/bus/mod.rs).

## Wiring two nodes without a carrier

One XIAO ESP32-C5 per bus, on slots `A1` and `B1`:

| XIAO ESP32-C5 | P4, bus A | P4, bus B |
| --- | --- | --- |
| D8 | 20 | 31 |
| D10 | 21 | 30 |
| D9 | 22 | 29 |
| D7 | 26 | 50 |
| D0 | 23 | 28 |
| GND | GND | GND |

- Power each board from its own USB.
- Wire GND between the boards, and nothing between their 5V or 3V3 pins.
- Keep the jumpers short.

The P4 logs `bus A1 appeared` when the first good transfer arrives, then one line of counters per
slot every 5 s. Run `wartui --log-file wartui.log run` to see them. Each node prints its own
counters on its USB port every 10 s.
