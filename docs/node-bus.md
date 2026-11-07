# The node bus

The node bus is SPI between the P4 bridge and its nodes, and the nodes' only link. The P4 is the
controller on two buses, A and B, and each node has its own CS line, called a slot: `A1` to `A7`
and `B1` to `B7`. Wired straight to the P4, each bus takes six nodes. The wartui-carrier takes
three, five or seven per bus, and names its sockets by the silk, `N1` to `Nn`
([On the carrier board](#on-the-carrier-board)).

## Pins

On the ESP32-P4-WIFI6:

| Signal | Bus A (SPI2) | Bus B (SPI3) |
| --- | --- | --- |
| SCLK, P4 → node | 20 | 31 |
| MOSI, P4 → node | 21 | 30 |
| MISO, node → P4 | 22 | 29 |
| IRQ, node → P4, one per bus | 23 | 28 |

| CS, P4 → node | Slot 1 | Slot 2 | Slot 3 | Slot 4 | Slot 5 | Slot 6 | Slot 7, 14-node carrier |
| --- | --- | --- | --- | --- | --- | --- | --- |
| Bus A | 26 | 27 | 32 | 33 | 46 | 47 | 25 |
| Bus B | 50 | 49 | 5 | 4 | 3 | 2 | 24 |

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
- **CS is active low and idles high.** The P4 drives each slot its board wires as a plain output,
  high from boot, and leaves the rest unconfigured. The node pulls its CS up, so it reads idle with
  no P4 attached.
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
| 1 | `Heartbeat` | 27 | 1 | `Assign` | 8 |
| 2 | `Sightings` | up to 1,024 | 2 | `Clear` | 0 |
| 3 | `UpdateStatus` | 6 | 3 | `UpdateBegin` | 41 |
| | | | 4 | `UpdateChunk` | 6 to 1,029 |
| | | | 5 | `UpdateCommit` | 1 |
| | | | 6 | `UpdateAbort` | 1 |
| | | | 7 | `UpdateConfirm` | 0 |

A heartbeat is never resent. Every other record waits for the peer to acknowledge the transfer that
carried it, and is resent until it is, up to three tries.

## On the carrier board

The carrier comes with 6, 10 or 14 node sockets: 3, 5 or 7 per bus, from slot 1. Its I/O expander
sits at 0x20 on I2C0 (SDA GPIO7, SCL GPIO8, 400 kHz), which it shares with the GPS receiver at 0x42.
Driving a port high powers its node:

| Bus A slot | 1 | 2 | 3 | 4 | 5 | 6 | 7 |
| --- | --- | --- | --- | --- | --- | --- | --- |
| Port | P17 | P16 | P15 | P14 | P13 | P12 | P11 |

| Bus B slot | 1 | 2 | 3 | 4 | 5 | 6 | 7 |
| --- | --- | --- | --- | --- | --- | --- | --- |
| Port | P00 | P01 | P02 | P03 | P04 | P05 | P06 |

- **The P4 reads the carrier from the expander at boot.** Each node enable is pulled down on the
  board, so with every port an input, a wired socket's port reads low and a spare one high. No
  answer at 0x20 is the bench, wired straight to the P4.
- **A cold boot powers every node, one every 100 ms**, in slot order A1, B1, A2, B2. The P4 makes
  every port an input, which turns every node off, writes the outputs low, makes only the node ports
  outputs, then raises them in turn. At power-up every port is an input, so every node starts off.
- **A P4 reset leaves running nodes running.** The expander keeps its registers through one, and the
  P4 finds its own node ports set as outputs. It powers only a node whose output is low.
- **Inputs that match no carrier are a broken board.** The P4 reports an error, powers nothing, and
  drives no slot.
- **A power-cycle holds one node off for 1 s**, then powers it again. The host asks for it with `p`
  in the fleet view or `wartui power-cycle`. A node with its own USB plugged in stays powered.
- **Two or more present nodes dropping at once, and none left, read as a lost node supply**,
  unless the host was power-cycling them. The P4 cannot sense the carrier's node supply and stays
  up on its own USB-C. It writes every node port low, then switches on one slot at a time, giving
  each 5 s to answer: the slots that were present first, in slot order, then every other slot.
  Once a node answers, or the host asks for a power-cycle, it powers the rest, one every 100 ms.
- **No node answering within 5 s of powering every node reads as a lost node supply too.** The P4
  may boot before the node supply, or a power-cycle request may end the wait while the supply is
  still out. The P4 then serves any waiting power-cycle, writes every node port low and switches on
  one slot at a time as above, every slot in slot order. With no node seated, it goes on doing so,
  and a node seated later is powered when its slot's turn comes.
- **IRQ is active low**, as [Pin behavior](#pin-behavior) says, and the carrier pulls it up.
- **GPIO25 and GPIO24 are the 14-node carrier's slot 7 CS lines.** They are the P4's
  USB-Serial-JTAG pads, so only the 14 takes them from it. The carrier's CS pull-ups hold both high
  until the P4 drives them.
- **The silk names the sockets `N1` to `Nn`**, bus A first: `N1` to `Nk` is bus A, with *k* per
  bus, and bus B follows. So `B1` is `N4` on the 6, `N6` on the 10 and `N8` on the 14. The fleet
  view, `sniff`, `status` and `--slot` use the silk names. A capture stores the bus name, beside the
  carrier in `capture.node_board`.

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

The P4 logs `A1 appeared` when the first good transfer arrives, then one line of counters per
slot every 5 s. On a carrier the line names the socket, `N1 appeared`. Run
`wartui --log-file wartui.log run` to see them. Each node prints its own counters on its USB port
every 10 s.
