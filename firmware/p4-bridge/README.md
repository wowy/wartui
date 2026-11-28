# wartui P4 bridge firmware

The host half of the P4 split bridge. On a Waveshare ESP32-P4-WIFI6, the P4 runs this firmware: it
talks to the host with wartui's link protocol over UART0, which the board's CH343 carries to the
Type-C, and to the on-board ESP32-C6 over UART1, with hardware flow control, on the board's SDIO
traces. The C6 runs a Bluetooth host link over it
([`firmware/p4-radio`](../p4-radio/README.md)), so a bonded central can be the host instead, or a
phone on the WardriveESP service (§ "Bluetooth hosts"). The host sees one bridge, reporting both chips' diagnostics. The queue
sizes and the microSD card's clock are in `src/config.rs`.

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
node takes them in order. It reads the heartbeat, for its fleet planner, and sighting batches
while a phone is linked, and writes three: the planner's `Assign` and `Stop`, and the update chunks it builds from its node
image cache, below.

## The fleet planner

The P4 decides what every node scans, so a board scans with no host attached
(`src/fleet.rs`). The planner itself is `wartui_proto::fleet`, which `cargo test` covers on the
host; this firmware feeds it and sends what it owes.

- **It boots stopped.** Every node idles until a host's `StartScan`, and a node still scanning
  from the P4's last life is sent a `Stop`.
- **It scans until a `StopScan`, or a `Begin` it takes for its own image**, whether or not a host
  is listening. The host gate closing changes nothing.
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

**Each `Status` carries the bridge's own health** (`health`): the P4's die temperature, read from
its temperature sensor for that `Status` (`src/tsens.rs`), and the C6's latest `Up::Health`, its die
temperature, rail and heap, or none before the C6 has sent one. esp-hal has no P4 sensor driver, so
`src/tsens.rs` follows ESP-IDF's by hand, at one range that reads up to about 91 °C.

## First flash

A new board is set up by `wartui flash-bridge`, over the same Type-C cable
(`crates/wartui/README.md` § "Flashing the bridge"). It writes this firmware's merged image whole,
which clears `nvs`, where the P4 keeps its settings and bonds.

The runner below is for a board that already ran wartui, and leaves `nvs` as it was. The Type-C
reaches the P4's UART0 through a CH343, so the runner needs `--port <that port>`. It is the port
the bridge's link then runs on, so quit any `wartui` holding it first. The runner writes
`partitions.csv` and erases `otadata`, so the P4 boots the image just written.

`partitions.csv` is the P4's own table: two app slots of 0x7F0000 bytes, about 7.9 MiB each,
`ota_0` and `ota_1`, and a `trial` sector at 0xFF0000. An update writes only the spare slot,
`otadata` and `trial`, so every later image runs on the table of the board's last wired flash
(`crates/wartui-proto/src/link/mod.rs` `//!`). The table ends at 0xFF1000 and needs a 16 MB flash;
the board's is 32 MB, by Waveshare's listing and `espflash board-info`, and the top half stays
unassigned.

The runner, the release's images and the image `flash-bridge` builds all pass `--flash-size 16mb`.
espflash writes the size into the app image's header, which the image's digest covers, so a board
flashed by wire reports the digest `flash-bridge` builds from the same source, and `flash-bridge`
leaves it alone. Left to itself, `espflash flash` writes the 32 MB it detects, and
`espflash save-image` reads no board and assumes 4 MB, which this table does not fit.

espflash resets the board through the CH343 and reboots it into the new image, so run
`cargo run --release -- --port <that port>` with no button held. The CH343P's DTR and RTS drive
EN and GPIO35 through a transistor pair. If a P4 shows `303a:0012` on USB and espflash cannot reach
it, replug the board: one bench P4 in that state ignored CH343 resets until it was replugged.

The C6 is set up once, by `flash-bridge` or through its header, in which case the P4 stays in
download mode while it is: see [`firmware/p4-radio`](../p4-radio/README.md#first-flash). After
that, updating the P4 updates both chips.

A board on espflash's default table has no second app slot and refuses updates, saying so with
every `Ready`. Flash it by hand once more, with this table.

## Updates over the host link

After the first flash, `wartui flash-bridge` sends the P4 its app image over its host link: the
same Type-C cable, or a bonded central through the C6 (§ "Bluetooth hosts"), more slowly
(`crates/wartui/README.md` § "Flashing the bridge"). The P4 takes it through the update receiver
the C6 and the nodes use (`src/update.rs`), switches slots and reboots into it on trial. The new
image's bundled C6 image then reaches the C6 as below. A `Begin` the P4 takes stops the scan, as
`StopScan` does, whichever host sent it: an update needs the bridge's attention and ends in a
reboot that stops the scan anyway, and a host on another major cannot send `StopScan`. A session
that ends without `Install` leaves the scan stopped: resuming a scan unasked after a failed update
is not the bridge's call. An `Abort` drops the session it names. Every refusal is a status, and
an op the P4 cannot read is skipped. The P4 drops an update session when its host lets go, so a
Bluetooth link that drops mid-transfer costs the transfer. Its reboot resets the C6, which drops a
Bluetooth link too, and `flash-bridge` reads the outcome across the reconnects.

**The first host frame after the C6's hello confirms the image.** `flash-bridge` sends an
`Identify` every second while the P4 reboots, so a P4 that comes back and hears `wartui` keeps its
image. The `Ready` that answers says how it went, in its frozen `trial` field: settled, on trial,
or on trial with the confirm failed. The host reads the outcome from that and from `Ready.image`,
not from the `image confirmed by the host` line the P4 also sends.

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

The cache takes up to 2 MB, a node's app slot, of the board's 32 MB of PSRAM, and the host bulk
ring the next 1 MB (§ "Build-time settings"). A P4 whose PSRAM does not map refuses every upload, runs its
bulk ring over no bytes, and reports the error `no PSRAM: no node image cache, and the host bulk
ring holds nothing` to the first host it announces to; everything else runs. One whose PSRAM the
cache takes whole reports `too little PSRAM: the host bulk ring holds nothing` instead. The cache is
dropped when a host lets go of the queue.

## The bundled radio image

At boot the P4 compares the image digest in the C6's hello with the image it carries. When they
differ, it sends the C6 its image over the UART link, once per boot. The C6 writes it, reboots
into it and boots it on trial. The image is confirmed once it has heard the P4's hello and its
Bluetooth host has started advertising, and one that has not advertised 30 s after boot restarts
itself, which spends a trial boot ([`firmware/p4-radio/README.md`](../p4-radio/README.md)
§ "Updates"). The host sees the C6's reboot as a new bridge life, with a fresh `Ready`.

Meanwhile the bridge announces the C6's MAC. The C6 image is about 643 KiB. On the bench,
`flash-bridge` updated both chips in 11 s and 17 s with a 570 KB one
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

## Bluetooth hosts

A central bonded over Bluetooth, through the C6, can be the host instead of UART0 (`src/ble.rs`).
One host owns the host queue at a time, by the rules of `wartui_proto::host_owner`:

| What happens | Who owns the queue after |
| --- | --- |
| A bonded central subscribes on an encrypted link, with no owner | Bluetooth |
| A central subscribes while USB owns the queue | USB; the central waits |
| A UART0 frame other than `Identify` | USB. A central that owned the queue is first sent the error `the USB host took the bridge` |
| An `Identify` on UART0 while Bluetooth owns the queue | Bluetooth. UART0 gets one `Ready`, so `wartui ports` and `--bridge` lookups cost the central nothing |
| 15 s without a UART0 frame while USB owns the queue | Bluetooth if a central waits, else no one |
| The central unsubscribes or disconnects, or the C6 restarts | No one, or USB if it owns the queue |

A phone holds the queue the same way while the `wardrive esp` setting is on (`src/wardrive_esp.rs`,
[`docs/wardrive-esp.md`](../../docs/wardrive-esp.md)):

| What happens | Who holds the queue after |
| --- | --- |
| A phone subscribes to the WardriveESP stream, with no owner | The phone. The P4 starts its session and the fleet's scan, Wi-Fi until the phone asks for Bluetooth |
| A UART0 frame other than `Identify` | USB. The phone gets no notice; the C6 drops it and admits none while USB holds the queue |
| An `Identify` on UART0 while a phone holds the queue | The phone. UART0 gets one `Ready` |
| The phone unsubscribes or disconnects, the C6 restarts, or the setting goes off | No one. The scan stops |

A phone reads as no host to the rest of the firmware: no link frame is queued for it, and
`Status` reports no owner. The queue carries its session's text lines instead, one a record, cut
with `\n`, down the same stream and credit window, with stream lines held to 16 KB. Its commands
arrive from the C6 as their own record and never reach the host frame decoder, so none confirms
an image on trial. Whether the C6 admits a phone rides `Down::Bonds`.

So `wartui flash-bridge` over USB always gets in. Taking the queue clears it and announces the
bridge to the new owner, as a USB host's first frame does. The P4 sends a central the host stream
inside a 16 KB credit window, so a slow central backs it up in the host bulk ring rather than
holding the C6 link.

**Bonds.** The C6 serves only a central that has bonded (`firmware/p4-radio/src/ble.rs`). The P4
keeps up to four bonds in the second sector of its `nvs` partition and sends them to the C6 after
each hello, on each change and every 5 s (`src/bonds.rs`). The pairing window, when a new central
may pair, is open while no bond is stored, for 120 s after the P4 boots, and for 120 s after a
host's `OpenPairing`, from USB or a bonded central. `ClearBonds` drops every bond, which opens the window until a central bonds,
and the C6 disconnects a central connected on one of them.
A host that held a bond then fails every connect until it forgets the bridge and pairs again
([`docs/c6-ble-pairing-findings.md`](../../docs/c6-ble-pairing-findings.md)). `Status` reports
the owner, the bonds, the window, the connected central and the C6's Bluetooth counters.

## Settings

The P4 keeps six settings in the first sector of its `nvs` partition, on every board: the lid
fan's duty, the channel pool, whether it offers the WardriveESP service, whether it logs to its
microSD card, the planner and its sparse node count. It reads them once at boot and runs them with
no host present, or 100 %, `All`, off, on, `even` and `auto` with none stored. A host's `SetFan`,
`SetPool`, `SetWardriveEsp`, `SetSdLog` or `SetPlanner` applies at once and is written only when
the sector does not hold it; `SetPlanner` sets both the planner and its sparse node count.
`Status` reports all six (`src/settings.rs`). The scan mode is not stored, so the P4 boots
stopped.

## The host link

UART0 at 4 Mbaud, 8N1, on GPIO37 (TX) and GPIO38 (RX), the console pins the board's CH343
(`1a86:55d3`) carries to the Type-C. One cable powers the board and carries the link. The P4 reads
and writes it through UHCI's DMA, with no flow control: the CH343's DTR and RTS drive the P4's EN
and GPIO35, so neither line can carry it, and the host never moves either. `src/host.rs` has the
reasoning, and [`docs/p4-host-uart-findings.md`](../../docs/p4-host-uart-findings.md) the rates
measured: 4 Mbaud is the fastest the CH343 carries cleanly.

**The P4 writes to the host only while it hears one.** Any host frame that decodes opens the
gate, clears both host queues and announces the bridge; 15 s without one closes it. A host
connection that sends nothing for 10 s sends a status poll, so a reading host keeps it open. While
a Bluetooth central owns the host queue, an `Identify` is answered with a `Ready` alone, and any
other frame takes the queue (§ "Bluetooth hosts").

**Host bytes are lost while the P4 writes its own flash**, with interrupts off for about 40 ms.
`Status.host_uart_errors` counts the host-link reads that saw one or more line errors or
overflows. Only an update writes flash, and its sender waits
for each sector's status and rewinds over the loss.

**The bootloader prints on UART0 at every boot**, at 115200 baud, which the host reads as
garbage: a reset under a held connection costs a few undecodable frames, and the framing resyncs.

The serial number is the CH343's own, so `wartui ports` asks each CH343 which bridge it is.
`wartui flash-bridge` updates the bridge over this link, or over Bluetooth, and `wartui flash-fleet`
skips every CH343. The MX1.25 high-speed port `P1` is unused.

**The board's Type-A port always drives 5 V**, since it is wired as a host port. A cable from it to
a PC must leave VBUS unconnected, or it back-feeds the PC whenever the Type-C is also connected
([`docs/p4-spi-link-findings.md`](../../docs/p4-spi-link-findings.md)).

## GPS

The default build reads a u-blox receiver over I2C and passes its bytes to the host. It parses
neither NMEA nor UBX: the host splits the two and composes the receiver's configuration, so a change
to the rate or the sentences costs a `cargo run` rather than a reflash. `wartui sniff --verbose`
prints each `Gps` frame's bytes as text, which is how to check the wiring. Like every other frame, bytes read while no host is present are dropped (see
[Build-time settings](#build-time-settings)), so a host that attaches late gets live fixes rather than a backlog.
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

## The microSD card

The board's TF slot is SDMMC slot 0, and the P4 drives it from boot (`src/sd.rs`). It writes two
things to the card: a format the host asks for, and its diagnostics log (§ "The SD log").

| Signal | P4 GPIO |
| --- | --- |
| CLK | 43 |
| CMD | 44 |
| D0–D3 | 39–42 |
| Card power | 45, low = on |

GPIO45 switches the card's supply through a P-channel MOSFET, so the P4 holds it low. The SDMMC
driver powers the slot's I/O from LDO4 itself. The slot has no card-detect line, so the P4 tries
to identify a card every 5 s until one answers, then reads block 0 every 5 s to see it is still
there, and reads a card that failed a read again in full. A card swapped in is found within about
10 s. Finding it again relies on two fixes to `sdio`, which the build takes from a fork pinned in
`Cargo.toml` ([`docs/p4-sd-findings.md`](../../docs/p4-sd-findings.md) § "A card identified
again").

`Status` reports the card:

| State | Meaning |
| --- | --- |
| `NoCard` | No card answered |
| `NoFat32` | A card answered and holds no FAT32 volume the P4 reads, such as exFAT |
| `Ready` | A FAT32 volume, with its free space when its FSInfo sector records it |
| `Formatting` | A format is asked for or running |
| `Failed` | The card failed a read, until it reads again, or the last format, until the next one |

**`FormatSd` erases the card and writes one FAT32 volume** labeled `WARTUI`, in the layout
`wartui_proto::sd` describes, which a laptop or a phone reads as a new card. The MBR goes last, so
a format cut short leaves no partition table. Each transfer is tried three times. The P4 answers
`Done` once the card reads back as FAT32, `NoCard` with no card, `Busy` while a format runs, and
`Failed` otherwise. A format of the bench's 64 GB card writes about 20 MiB.

**Every transfer is at most 16 KiB, from a 64-byte-aligned buffer in internal RAM.** The driver's
DMA ring covers 16 KiB in one fill, and a longer transfer needs its interrupt mid-way, while a
flash erase masks the P4's interrupts for about 40 ms. A buffer with less alignment is split
around its middle and needs the interrupt too.

**The bus clock is 20 MHz.** At 40 MHz the bench card failed about one write in 6,100 with a CRC
error; at 20 MHz none in 24,000.

**The card runs on core 0, with everything else, and core 1 is never started.** esp-storage
stalls the other core for every flash write, wherever it is. On the bench, a core 1 driving the
card made `flash-bridge` updates restart or hang the P4.
[`docs/p4-sd-findings.md`](../../docs/p4-sd-findings.md) has the measurements.

### The SD log

The P4 logs its diagnostics to the card (`src/sd_log.rs`), by default. The SD log setting, tagged
`L` in the settings sector (§ "Settings"), turns it on or off: `wartui sd log on|off`, or the
settings modal's `log` row. `wartui sd-log` prints the files as text
([`crates/wartui/README.md`](../../crates/wartui/README.md) § "Reading the SD log").

It logs whoever owns the host queue, and with no host at all:

- every frame the P4 produces for a host but sighting batches, update statuses, GPS bytes,
  `Delivered` and `Undelivered`, so every node heartbeat and panic record is kept as forwarded;
- a `Status` snapshot every 30 s, when a file opens and at each change of who holds the host
  queue, built from the C6's counters and health as last received and the P4's die temperature
  read for it;
- each change of who holds the host queue;
- a phone's commands, and the lines it is sent but its `WD:AP` and `WD:BT` sightings.

Each file's `Opened` record carries the text of the panic that ended the P4's previous boot, when
that reset kept one. The panic handler keeps it in RTC memory (`wartui_firmware::last_panic`), and
the boot copies it out and clears it.

The log holds the addresses of the nodes and of Bluetooth peers, so a card identifies the fleet as
a capture does.

**Files are `LOG/NNNNNNNN.BIN`**, each numbered past the highest there. A new one opens at each
boot, each time the setting turns on, for each card found or formatted, and after a failed write.
At `SD_LOG_FILE_MAX` bytes a file ends with a `Closed` record and the next opens. The size is
checked before each flush, so a file runs up to one flush, 16 KiB, past it. When a flush would
leave less free than `SD_LOG_KEEP_FREE_MB`, or an eighth of the volume if that is less, the log
deletes its oldest files first, but only when deleting its files can restore that reserve.
Otherwise, as when other files took the space, it stops, `card full`, and keeps every file.

**Records wait in a ring of `SD_LOG_BYTES` in PSRAM**, past the host bulk ring. A producer copies
its record in and returns; a full ring evicts its oldest records, and the next flush says how many
in a `Dropped` record. The writer flushes the ring every 5 s, and sooner once 12 KiB is queued.
While no card takes the log, or while an update of the P4 or a node image upload runs, the ring
keeps the newest records for the next file. Turning the setting off flushes the ring and ends the
file with a `Closed` record.

**The writer is the lowest-priority work on the P4.** It yields after every card transfer and
every block it stages, so every other future runs between them. A failed transfer, a fault it
finds on the card or a volume it refuses stops only the writer. `wartui_proto::sd::append`'s docs
say what a power cut leaves on the card.

## Build-time settings

Each is read at build time from an environment variable, so a bench run changes one with a
rebuild, for example `WARTUI_P4_HOST_BULK_BYTES=131072 cargo build --release`. `wartui status`
reports each queue's high-water mark.

The host queues fill only while a host owns them, on UART0 or over Bluetooth. Frames produced
without one are dropped, and a host's arrival clears both host queues before it reads anything, so
the bulk ring absorbs congestion while a host reads and never holds a backlog for an absent one.
Neither discard counts as a drop in `Status`.

The bulk ring is in PSRAM, past the node image cache, and the other queues in internal RAM. At
1 MB it holds two 500-batch bursts on the slowest central measured, and bounds the lag a backlog
costs at about 27 s on it, since a sighting is positioned when it reaches the host
([`docs/c6-ble-host-findings.md`](../../docs/c6-ble-host-findings.md), run 15). A board that maps
less PSRAM gets a smaller ring.

| Variable | Default | Holds |
| --- | --- | --- |
| `WARTUI_P4_HOST_BULK_BYTES` | 1048576 | frames for the host that may be evicted: node records, GPS bytes and logs, in PSRAM |
| `WARTUI_P4_HOST_PRIORITY_BYTES` | 16384 | frames that answer the host |
| `WARTUI_P4_DOWN_QUEUE_BYTES` | 1024 | `Down` records waiting for a packet to the C6 |
| `WARTUI_P4_NODE_DOWN_BYTES` | 4160 | per slot, the host's records for that node waiting behind the two flights out; a full queue answers `Undelivered` |
| `WARTUI_P4_SD_HZ` | 20000000 | the microSD card's bus clock asked for, in Hz; the driver caps it at 40 MHz |
| `WARTUI_P4_SD_LOG_BYTES` | 262144 | SD log records waiting for the card, in PSRAM past the host bulk ring |
| `WARTUI_P4_SD_LOG_FILE_MAX` | 67108864 | the size at which an SD log file ends and the next opens, in bytes |
| `WARTUI_P4_SD_LOG_KEEP_FREE_MB` | 256 | the free space the SD log keeps, in MiB, deleting its oldest files; at most an eighth of the volume |

The bench builds the SD log's sizes small to force rotation and deletion: a small ring overflows
and counts drops, and small files rotate within minutes and, on a card filled to its reserve, are
deleted one at a time.

The C6 link's baud rate (4 Mbaud) is fixed, as part of its frozen core. The host link's is
`wartui_proto::link::HOST_BAUD`.
