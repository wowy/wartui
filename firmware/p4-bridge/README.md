# wartui P4 bridge firmware

The host half of the P4 split bridge. On a Waveshare ESP32-P4 board, the P4 runs this firmware: it
talks to the host over USB 2.0 high speed with wartui's link protocol, and to the on-board
ESP32-C6 as a 40 MHz SPI master over the board's SDIO traces. The C6 runs the radio
([`firmware/p4-radio`](../p4-radio/README.md)). The host sees a bridge like the single-chip ones,
reporting `Esp32P4` and both chips' diagnostics. The queue sizes are in `src/config.rs`.

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

One image serves the Waveshare ESP32-P4-WIFI6-POE-ETH and the ESP32-P4-WIFI6.

## First flash

The Type-C reaches the P4's UART0 through a CH343, so the runner needs `--port <that port>`. It
writes `partitions.csv`, the node's table with two app slots and a trial sector, and erases
`otadata`, so the P4 boots the image just written.

- **POE-ETH:** there is no auto-reset, because the CH343's RTS connects to nothing. Hold BOOT while
  plugging in the Type-C, run `cargo run --release` (or `espflash flash --chip esp32p4`), then
  unplug and replug to run. Pressing RST lands in download mode.
- **WIFI6:** the CH343P's DTR and RTS drive EN and GPIO35 through a transistor pair, so espflash
  resets it normally.

The C6 is flashed through its header once, and the P4 stays in download mode while it is: see
[`firmware/p4-radio`](../p4-radio/README.md#first-flash). After that, updating the P4 updates
both chips.

A board flashed before this table, on espflash's default one, has no second app slot and refuses
updates, saying so with every `Ready`. Flash it by hand once more.

## Updates over USB

After the first flash, `wartui flash-bridge` sends the P4 its app image over the high-speed link
it bridges on (`crates/wartui/README.md` § "Updating a P4 bridge"). The P4 takes it through the
update receiver the C6 and the nodes use (`src/update.rs`), switches slots and reboots into it on
trial. The new image's bundled C6 image then reaches the C6 as below.

**The first host frame confirms the image.** The host's `Identify` on reconnecting is that frame,
so a P4 that comes back and hears `wartui` keeps its image.

**A trial boot arms the RTC watchdog for 30 s.** Confirming disarms it. A hung image is reset by
it, and the third unconfirmed boot rolls back to the image the P4 ran before. A trial P4 left
powered with no host rolls back the same way, after about 90 s.

The P4 sends the host a line for each boot step: the trial boot's count, a rollback, the confirm,
and any step that failed.

## The bundled radio image

At boot the P4 compares the image digest in the C6's hello with the image it carries. When they
differ, it sends the C6 its image over the SPI link, once per boot. The C6 writes it, reboots
into it and boots it on trial, and the P4's hello confirms it. The host sees the C6's reboot as a
new bridge life, with a fresh `Ready`.

Meanwhile the bridge enumerates with the C6's MAC and refuses radio commands. The update adds
several seconds to that boot, for a C6 image of about 550 KB; the bench has not timed it. A
bundled image that will not take is tried again on every power-up, about 30 s without a radio
each time, until a P4 with a fixed image is flashed.

If no hello follows the install within 10 s, the P4 resets the C6 through its enable pin, up to
three times, so a hung trial image counts the boots it needs to roll back.

Each state reaches the host as an error line, repeated with every `Ready`:

| Line | Meaning |
| --- | --- |
| `updating the radio to the bundled image (n%)` | The update is under way; sent again every tenth of the image |
| `radio runs another build; the bundled image did not take` | The C6 refused the image, stopped answering, or rolled back. The bridge still runs while the link revision matches |
| `radio predates bundled updates; flash it through the header once` | The C6's hello has no digest. It cannot take an update; flash it by wire |
| `radio speaks SPI link revision a, this P4 speaks b; update the radio` | A different link revision, and no update took. The bridge refuses radio commands |
| `the radio coprocessor is not answering` | No hello at boot, or none after the resets that follow an install |

A refused or silent update also sends one line naming why: `the radio refused the bundled image:
…` or `the radio stopped answering its update`.

## USB

The bridge enumerates as `1209:0001`, product `wartui p4 bridge`, with the bridge (C6) MAC as its
serial number, or the P4's own MAC when the C6 never answered its hello. `wartui ports` lists it,
`wartui flash-bridge` updates it over this link and `wartui flash-fleet` skips it.

**The high-speed port always drives 5 V.** It is the Type-A on the POE-ETH (a load switch with its
enable tied high) and the 4-pin MX1.25 connector `P1` on the WIFI6 (pin 1 is the board's VCC_5V
rail). A cable to a PC that carries VBUS back-feeds the PC whenever the Type-C is also connected,
so the cable must leave VBUS unconnected. A WIFI6 can instead be powered from that port alone.

## GPS

The default build reads a u-blox receiver over I2C and passes its bytes to the host, as the
T-Dongle-C5 does. Like every other frame, bytes read while no host holds DTR are dropped (see
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

The host queues fill only while a host holds DTR. Frames produced without one are dropped, and
DTR rising clears both host queues before the new host reads anything, so the bulk ring absorbs
congestion while a host reads and never holds a backlog for an absent one. Neither discard counts
as a drop in `Status`.

| Variable | Default | Holds |
| --- | --- | --- |
| `WARTUI_P4_HOST_BULK_BYTES` | 262144 | frames for the host that may be evicted: received frames and logs |
| `WARTUI_P4_HOST_PRIORITY_BYTES` | 16384 | frames that answer the host |
| `WARTUI_P4_DOWN_QUEUE_BYTES` | 16384 | `Down` records waiting for a transfer to the C6 |

The SPI clock (40 MHz) is fixed.
