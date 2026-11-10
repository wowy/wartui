# wartui P4 bridge firmware

The host half of the P4 split bridge. On a Waveshare ESP32-P4-WIFI6, the P4 runs this firmware: it
talks to the host with wartui's link protocol over UART0, which the board's CH343 carries to the
Type-C, and to the on-board ESP32-C6 over UART1, with hardware flow control, on the board's SDIO
traces. The C6 holds that link for a future Bluetooth host link
([`firmware/p4-radio`](../p4-radio/README.md)). The host sees one bridge, reporting both chips'
diagnostics. The queue sizes are in `src/config.rs`.

```sh
cargo build --release
```

The build also builds the C6's image ([`firmware/p4-radio`](../p4-radio/README.md)) and embeds
it, so it needs the C6's target and espflash:

```sh
rustup target add riscv32imac-unknown-none-elf
cargo install espflash --locked
```

`build.rs` runs `cargo build --release` in `firmware/p4-radio`, with that crate's own toolchain
and config, then `espflash save-image` on the result. A build fails if the saved image carries no
appended SHA-256 digest. `cargo clippy` runs the same build script, so it builds the C6's image
too.

It is also the SPI controller of two node buses; [`docs/node-bus.md`](../../docs/node-bus.md) has
the pins. It forwards every record a node sends to the host, heartbeats included, and queues what
the host sends a node in that node's slot, answering each `Delivered` once the node acknowledges
it, or `Undelivered` (`src/bus.rs`). Two transfers of records are out to a node at once, and the
node takes them in order. It reads one kind of record, the heartbeat, for its fleet planner, and
writes three: the planner's `Assign` and `Stop`, and the update chunks it builds from its node
image cache, below.

## The fleet planner

The P4 decides what every node scans, so a board scans with no host attached
(`src/fleet.rs`). The planner itself is `wartui_proto::fleet`, which `cargo test` covers on the
host; this firmware feeds it and sends what it owes.

- **It boots stopped.** Every node idles until a host's `StartScan`, and a node still scanning
  from the P4's last life is sent a `Stop`.
- **It scans until a `StopScan`**, whether or not a host is listening. The host gate closing
  changes nothing.
- **A member** is a present slot that has heartbeated within 20 s, on a node firmware of this
  build's major and minor. The pool is cut across the members in slot order, and cut again when
  membership, the mode or the pool changes.
- **Bluetooth**, when the scan asks for it, goes to one member once there are two: the highest
  slot, kept while its node stays.
- **It reports its plan** to the host as a `Fleet` frame on every change, after each `Status` and
  when a host attaches. Each `Status` names every member's slot and MAC.

Send ids at or above `0x8000` are the planner's, and their outcomes go to it rather than to the
host, whose ids stay below.

## Boards

This firmware runs on the Waveshare ESP32-P4-WIFI6, with its nodes on the wartui-carrier. A P4
without a carrier drives no node.

## On the carrier

At boot, before it builds the node buses, the P4 reads the carrier's I/O expander at 0x20 on I2C0
(`src/carrier.rs`). The carrier it finds picks the CS pins it drives, and `Ready` reports it to the
host. No answer at 0x20 is no carrier: an error in every announcement, and no slot driven.

- **A cold boot** powers every node, one every 100 ms.
- **A reset of the P4** leaves running nodes running, and powers only a node that is off.
- **A power-cycle** from the host holds one node off for 1 s, then powers it again.
- **Two or more present nodes dropping at once** read as a lost node supply. The P4 powers one
  slot at a time, any socket, until a node answers or the host asks for a power-cycle, then the
  rest, one every 100 ms.
- **No node answering within 5 s of powering every node**, at boot or after a power-cycle request
  ended that wait, reads as a lost node supply too, and the P4 powers one slot at a time again,
  first the slots present before a lost supply. With no node seated, it goes on doing so. A
  power-cycle request is served at once, also during those 5 s, and the 5 s start again after it.
- **A node powered over its own USB** keeps the P4 from seeing a lost node supply; unplug it, or
  power the carrier first.
- **An expander that fails, or reads as no carrier,** is an error in every announcement. With
  power out of reach, a power-cycle answers `Failed`. When the registers named no board, the
  bridge reports the fourteen-node carrier and drives no slot, and the fan still runs.

- **The lid fan** runs at the duty stored in the P4's settings (§ "Settings"), or 100 % with
  none, from boot and with no host present. A host's `SetFan` changes it at once. `Status` reports
  the duty and the tach's speed (`src/fan.rs`). With no carrier neither GPIO51 nor GPIO52 is
  configured and `SetFan` is ignored.

Each announcement also carries a line naming the carrier and what it powered at boot:
`carrier-10: 10 node slots; at boot 3 powered, 7 already on`.
[`docs/node-bus.md`](../../docs/node-bus.md) § "On the carrier board" has the port map and the pins.

## The link to the C6

UART1 on both chips: 4 Mbaud, 8N1, hardware RTS/CTS. The P4 reads and writes it on the FIFO, with
interrupts: its one UHCI goes to the host link, which has no flow control, and RTS holds the C6
back whenever the P4 reads late (`src/link.rs`). The C6's UART0 stays its flash port.
`wartui_proto::radio_link` has the packets and the frozen core,
[`docs/p4-uart-link-findings.md`](../../docs/p4-uart-link-findings.md) what the link was measured
to carry, and [`docs/p4-host-uart-findings.md`](../../docs/p4-host-uart-findings.md) how it held
on the FIFO.

| Signal | P4 GPIO | C6 GPIO |
| --- | --- | --- |
| P4 TX → C6 RX | 19 | 18 |
| C6 TX → P4 RX | 14 | 20 |
| P4 RTS → C6 CTS | 18 | 19 |
| C6 RTS → P4 CTS | 15 | 21 |
| C6 enable | 54 | CHIP_PU |
| Spare | 6, input with pull-down | 2, not configured |
| SDIO D2/D3 | 16, 17 inputs | 22, 23 inputs |

The board pulls the four UART traces up (51 kΩ on its schematic). A C6 in reset therefore reads as
CTS high, and the P4 waits rather than drops.

## First flash

The Type-C reaches the P4's UART0 through a CH343, so the runner needs `--port <that port>`. It is
the port the bridge's link then runs on, so quit any `wartui` holding it first. The runner
writes `partitions.csv`, the node's table with two app slots and a trial sector, and erases
`otadata`, so the P4 boots the image just written. It leaves `nvs`, where the P4 keeps its
settings, as it was.

espflash resets the board through the CH343 and reboots it into the new image, so run
`cargo run --release -- --port <that port>` with no button held. The CH343P's DTR and RTS drive
EN and GPIO35 through a transistor pair. If a P4 shows `303a:0012` on USB and espflash cannot reach
it, replug the board: one bench P4 in that state ignored CH343 resets until it was replugged.

The C6 is flashed through its header once, and the P4 stays in download mode while it is: see
[`firmware/p4-radio`](../p4-radio/README.md#first-flash). After that, updating the P4 updates
both chips.

A board on espflash's default table has no second app slot and refuses updates, saying so with
every `Ready`. Flash it by hand once more, with this table.

## Updates over the host link

After the first flash, `wartui flash-bridge` sends the P4 its app image over the link it bridges
on, the same Type-C cable (`crates/wartui/README.md` § "Flashing the bridge"). The P4 takes it through the
update receiver the C6 and the nodes use (`src/update.rs`), switches slots and reboots into it on
trial. The new image's bundled C6 image then reaches the C6 as below.

**The first host frame after the C6's hello confirms the image.** `flash-bridge` polls every
second while the P4 reboots, so a P4 that comes back and hears `wartui` keeps its image.

**A trial boot arms the RTC watchdog for 30 s.** Confirming disarms it. A hung image is reset by
it, and the third unconfirmed boot rolls back to the image the P4 ran before. A trial P4 left
powered with no host rolls back the same way, after two watchdog periods, about 60 s.

A trial P4 whose C6 never says hello is not confirmed by any host frame, and the image rolls back.
It announces its own MAC rather than the C6's, which is not the bridge `flash-bridge` updated.

The P4 sends the host a line for each boot step: the trial boot's count, a rollback, the confirm,
and any step that failed.

## Node image cache

`wartui ota` uploads the node image once, into the P4's PSRAM, then asks for each node's
`UpdateChunk` by its offset in it, so the image crosses the host link once however many nodes take
it (`src/cache.rs`). The upload goes through the same update receiver the P4's own image does, and
the cache serves an image only once its SHA-256 checks. Until then, and for a chunk that names
another image or runs past its end, the P4 answers `Undelivered` `NoImage`.

The cache takes up to 2 MB, a node's app slot, of the board's 32 MB of PSRAM. A P4 whose PSRAM
does not map refuses every upload and logs `node image cache: no PSRAM` to the first host it
announces to; everything else runs. The cache is dropped when the host gate closes.

## The bundled radio image

At boot the P4 compares the image digest in the C6's hello with the image it carries. When they
differ, it sends the C6 its image over the UART link, once per boot. The C6 writes it, reboots
into it and boots it on trial, and the P4's hello confirms it. The host sees the C6's reboot as a
new bridge life, with a fresh `Ready`.

Meanwhile the bridge announces the C6's MAC. With a C6 image of about 570 KB, `flash-bridge`
updated both chips in 11 s and 17 s on the bench
([`docs/p4-uart-link-findings.md`](../../docs/p4-uart-link-findings.md)). A bundled image that will
not take is tried again on every power-up, about 30 s each time, until a P4 with a fixed image is
flashed.

If no hello follows the install within 10 s, the P4 resets the C6 through its enable pin, up to
three times, so a hung trial image counts the boots it needs to roll back.

Each state reaches the host as an error line, repeated with every `Ready`:

| Line | Meaning |
| --- | --- |
| `updating the radio to the bundled image (n%)` | The update is under way; sent again every tenth of the image |
| `radio runs another build; the bundled image did not take` | The C6 refused the image, stopped answering, or rolled back. The bridge still runs while the link revision matches |
| `radio speaks link revision a, this P4 speaks b; update the radio` | A different link revision, and no update took. The P4 sends the C6 no records |
| `the radio coprocessor is not answering` | No hello at boot, or none after the resets that follow an install |
| `the radio has stopped answering; wartui reset restarts the bridge` | A host polled and no packet came from the C6 for 12 s, as when it hangs holding RTS. Ends when a packet arrives |

A refused or silent update also sends one line naming why: `the radio refused the bundled image:
…` or `the radio stopped answering its update`.

## Settings

The P4 keeps two settings in the first sector of its `nvs` partition, on every board: the lid
fan's duty and the channel pool. It reads them once at boot and runs them with no host present, or
100 % and `All` with none stored. A host's `SetFan` or `SetPool` applies at once and is written
only when the sector does not hold it. `Status` reports both (`src/settings.rs`). The scan mode is
not stored, so the P4 boots stopped.

## The host link

UART0 at 4 Mbaud, 8N1, on GPIO37 (TX) and GPIO38 (RX), the console pins the board's CH343
(`1a86:55d3`) carries to the Type-C. One cable powers the board and carries the link. The P4 reads
and writes it through UHCI's DMA, with no flow control: the CH343's DTR and RTS drive the P4's EN
and GPIO35, so neither line can carry it, and the host never moves either. `src/host.rs` has the
reasoning, and [`docs/p4-host-uart-findings.md`](../../docs/p4-host-uart-findings.md) the rates
measured: 4 Mbaud is the fastest the CH343 carries cleanly.

**The P4 writes to the host only while it hears one.** Any host frame that decodes opens the
gate, clears both host queues and announces the bridge; 15 s without one closes it. A host
connection that sends nothing for 10 s sends a status poll, so a reading host keeps it open.

**Host bytes are lost while the P4 writes its own flash**, with interrupts off for about 40 ms.
`Status.host_uart_errors` counts the host-link reads that saw one or more line errors or
overflows. Only an update writes flash, and its sender waits
for each sector's status and rewinds over the loss.

**The bootloader prints on UART0 at every boot**, at 115200 baud, which the host reads as
garbage: a reset under a held connection costs a few undecodable frames, and the framing resyncs.

The serial number is the CH343's own, so `wartui ports` asks each CH343 which bridge it is.
`wartui flash-bridge` updates the bridge over this link and `wartui flash-fleet` skips every
CH343. The MX1.25 high-speed port `P1` is unused.

**The board's Type-A port always drives 5 V**, since it is wired as a host port. A cable from it to
a PC must leave VBUS unconnected, or it back-feeds the PC whenever the Type-C is also connected
([`docs/p4-spi-link-findings.md`](../../docs/p4-spi-link-findings.md)).

## GPS

The default build reads a u-blox receiver over I2C and passes its bytes to the host. It parses
neither NMEA nor UBX: the host splits the two and composes the receiver's configuration, so a change
to the rate or the sentences costs a `cargo run` rather than a reflash. `wartui sniff --verbose`
prints each `Gps` frame's bytes as text, which is how to check the wiring. Like every other frame, bytes read while no host is present are dropped (see
[Queue sizes](#queue-sizes)), so a host that attaches late gets live fixes rather than a backlog.
So are the lines saying the receiver stopped or came back; the announcement carries the current
fault instead. Wire it to the P4's header:

| Receiver | P4 |
| --- | --- |
| SDA | GPIO7 |
| SCL | GPIO8 |
| VCC | 3V3 |
| GND | GND |

The receiver answers at `0x42`. The board has 2.2 kΩ pull-ups on both lines, and its ES8311 codec
shares the bus at another address. So does a carrier's I/O expander, at `0x20`: the GPS reader and
the carrier each lock the bus for one transaction at a time. The bridge always reads the bus.
Without a receiver, every announcement carries "no GPS receiver answered on the I2C bus
(GPIO7/GPIO8)".

## Queue sizes

Each is read at build time from an environment variable, so a bench run resizes one with a
rebuild, for example `WARTUI_P4_HOST_BULK_BYTES=131072 cargo build --release`. `wartui status`
reports each queue's high-water mark.

The host queues fill only while a host is present. Frames produced without one are dropped, and a
host's arrival clears both host queues before it reads anything, so the bulk ring absorbs
congestion while a host reads and never holds a backlog for an absent one. Neither discard counts
as a drop in `Status`.

| Variable | Default | Holds |
| --- | --- | --- |
| `WARTUI_P4_HOST_BULK_BYTES` | 245760 | frames for the host that may be evicted: node records, GPS bytes and logs |
| `WARTUI_P4_HOST_PRIORITY_BYTES` | 16384 | frames that answer the host |
| `WARTUI_P4_DOWN_QUEUE_BYTES` | 1024 | `Down` records waiting for a packet to the C6 |
| `WARTUI_P4_NODE_DOWN_BYTES` | 4160 | per slot, the host's records for that node waiting behind the two flights out; a full queue answers `Undelivered` |

The C6 link's baud rate (4 Mbaud) is fixed, as part of its frozen core. The host link's is
`wartui_proto::link::HOST_BAUD`.
