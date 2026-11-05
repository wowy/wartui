# wartui P4 bridge firmware

The host half of the P4 split bridge. On a Waveshare ESP32-P4-WIFI6, the P4 runs this firmware: it
talks to the host with wartui's link protocol over UART0, which the board's CH343 carries to the
Type-C, and to the on-board ESP32-C6 over UART1, with hardware flow control, on the board's SDIO
traces. The C6 runs
the radio ([`firmware/p4-radio`](../p4-radio/README.md)). The host sees one bridge, reporting
`Esp32P4` and both chips' diagnostics. The queue sizes are in `src/config.rs`.

```sh
cargo build --release
```

The build also builds the C6's image ([`firmware/p4-radio`](../p4-radio/README.md)) and embeds
it, so it needs the C6's target and espflash:

```sh
rustup target add riscv32imac-unknown-none-elf
cargo install espflash --locked
```

`build.rs` runs `cargo build --release --features esp32c6` in `firmware/p4-radio`, with that
crate's own toolchain and config, then `espflash save-image` on the result. A build fails if the
saved image carries no appended SHA-256 digest. `cargo clippy` runs the same build script, so it
builds the C6's image too.

## Boards

This firmware runs on the Waveshare ESP32-P4-WIFI6.

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
`otadata`, so the P4 boots the image just written.

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

## The bundled radio image

At boot the P4 compares the image digest in the C6's hello with the image it carries. When they
differ, it sends the C6 its image over the UART link, once per boot. The C6 writes it, reboots
into it and boots it on trial, and the P4's hello confirms it. The host sees the C6's reboot as a
new bridge life, with a fresh `Ready`.

Meanwhile the bridge announces the C6's MAC and refuses radio commands. With a C6 image of
about 570 KB, `flash-bridge` updated both chips in 11 s and 17 s on the bench
([`docs/p4-uart-link-findings.md`](../../docs/p4-uart-link-findings.md)). A bundled image that will not take is tried again on every power-up, about 30 s without a radio
each time, until a P4 with a fixed image is flashed.

If no hello follows the install within 10 s, the P4 resets the C6 through its enable pin, up to
three times, so a hung trial image counts the boots it needs to roll back.

Each state reaches the host as an error line, repeated with every `Ready`:

| Line | Meaning |
| --- | --- |
| `updating the radio to the bundled image (n%)` | The update is under way; sent again every tenth of the image |
| `radio runs another build; the bundled image did not take` | The C6 refused the image, stopped answering, or rolled back. The bridge still runs while the link revision matches |
| `radio predates bundled updates; flash it through the header once` | The C6's hello has no digest. It cannot take an update; flash it by wire |
| `radio speaks link revision a, this P4 speaks b; update the radio` | A different link revision, and no update took. The bridge refuses radio commands |
| `the radio coprocessor is not answering` | No hello at boot, or none after the resets that follow an install |
| `the radio has stopped answering; wartui reset restarts the bridge` | A host polled and no packet came from the C6 for 12 s, as when it hangs holding RTS. Ends when a packet arrives |

A refused or silent update also sends one line naming why: `the radio refused the bundled image:
…` or `the radio stopped answering its update`.

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
shares the bus at another address. Without a receiver, every announcement carries "no GPS receiver
answered on the I2C bus (GPIO7/GPIO8)". `cargo build --release --no-default-features` builds
without GPS: the bridge reports none and never touches the bus.

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
| `WARTUI_P4_HOST_BULK_BYTES` | 262144 | frames for the host that may be evicted: received frames and logs |
| `WARTUI_P4_HOST_PRIORITY_BYTES` | 16384 | frames that answer the host |
| `WARTUI_P4_DOWN_QUEUE_BYTES` | 16384 | `Down` records waiting for a packet to the C6 |

The C6 link's baud rate (4 Mbaud) is fixed, as part of its frozen core. The host link's is
`wartui_proto::link::HOST_BAUD`.
