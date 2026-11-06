# The node bus

The node bus is SPI between the P4 bridge and its nodes, and the nodes' only link. The P4 is master
on two buses, A and B, and each bus has up to six nodes, one per CS line, called slots `A1` to `A6`
and `B1` to `B6`. Slot numbering leaves room for a seventh per bus (`A7`, `B7`).

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
- **No bus pin is a strapping pin** on either chip. The P4's are GPIO34–38 (ESP32-P4 datasheet
  v0.7). The C5's are GPIO2, 3, 7 and 25–28 (ESP32-C5 datasheet v1.5), which is why the node
  leaves D2 and D3 (GPIO25 and 7) unused. esp-hal's generated docs also flag the P4's GPIO32 and
  33, which the datasheet does not.
- **CS is active low and idles high.** The P4 drives each slot's CS as a plain output, high from
  boot. The node pulls its CS up, so it reads idle with no P4 attached.
- **MISO is pulled up on the P4**, so an empty slot reads constant bytes and fails the magic.
- **IRQ is one line per bus.** A node drives D0 push-pull, high to ask for a transfer.
  - On the carrier board, each node's D0 drives its own driver, which pulls the bus's shared line
    low. So the line is active low, and no two D0 pins are ever wired together.
  - Wired straight to the P4, one node has the IRQ line to itself. It is active high, and the P4
    pulls it down.

Every transfer is a fixed 1,044 bytes each way. The framing, the acknowledgment, and the handshake
that re-arms the node between transfers, are in the `//!` of
[`crates/wartui-proto/src/bus/mod.rs`](../crates/wartui-proto/src/bus/mod.rs).

## Messages

A transfer carries records, each a type byte, a length and a body. The type byte is the message,
numbered per direction. The P4 reads none of them: it forwards a node's records to the host as
`FromNode`, and the host's `ToNode` records to the node. The layouts are in the `//!` of each
message in [`crates/wartui-proto/src/bus/`](../crates/wartui-proto/src/bus/).

| Up, node → P4 | Message | Body bytes | Down, P4 → node | Message | Body bytes |
| --- | --- | --- | --- | --- | --- |
| 1 | `Heartbeat` | 23 | 1 | `Assign` | 8 |
| 2 | `Sightings` | up to 1,024 | 2 | `Clear` | 0 |
| 3 | `UpdateStatus` | 6 | 3 | `UpdateBegin` | 41 |
| | | | 4 | `UpdateChunk` | 6 to 1,029 |
| | | | 5 | `UpdateCommit` | 1 |
| | | | 6 | `UpdateAbort` | 1 |
| | | | 7 | `UpdateConfirm` | 0 |

A heartbeat is never resent. Every other record waits for the peer to acknowledge the transfer that
carried it, and is resent until it is, up to three tries.

## What never changes

Part of the bus is frozen, so firmware that keeps to it can always be updated over the bus by any
later wartui. The frozen core is the transfer and its handshake, records, the heartbeat's first 11
bytes, the update records, the `trial` sector, the partition table and what a P4 owes a node.
The list is in the `//!` of
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
