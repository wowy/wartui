# P4 microSD card — which core drives it, at what clock, in what transfers

Measured on 2026-11-22 on a Waveshare ESP32-P4-WIFI6, the bridge `96:C4`, with no carrier and a
64 GB card in its TF slot. The question was whether the card can be driven while the bridge runs,
on which core, and whether a settings write or a `wartui flash-bridge` update still passes while
it writes.

The firmware was a bench build of the P4 bridge that is not in the tree. It ran the write loop
below beside the bridge's own futures, and kept the P4's panic text across a reset. Build-time
switches picked the core the loop ran on, the bus clock asked for, the input sampling phase and a
gap between writes. Of those, the tree keeps the clock, as `WARTUI_P4_SD_HZ`.

## The setup

- SDMMC slot 0 through esp-hal 1.2.2's `sdmmc` and the `sdio` 0.5 crate, 4-bit. CLK GPIO43, CMD
  GPIO44, D0–D3 GPIO39–42. GPIO45 low powers the card.
- One loop: 256 writes of 16 KiB per cycle to the card's last 8192 blocks, then a read-back of the
  cycle's last write, compared.
- Every transfer is 32 blocks from one 64-byte-aligned internal-RAM buffer.

## The card

123,596,800 blocks: 63,281 MB, or the 60,350 MiB that `Status` reports. Block 0 ends `55 aa` and its first partition is type `0x0c`,
FAT32 with LBA, as the card shipped.

## Core 1 broke updates

| Run | Result |
| --- | --- |
| Core 1, 40 MHz, idle | 16 KiB write 1.2 / ~1.6 / ~13 ms (min / mean / max), ~10 MB/s. 14 `Crc` write errors in 85,760 writes, one in ~6,100. No read or compare errors |
| Core 1, 31 settings writes (pool US/EU) | All passed. One `Crc` error in 65 cycles, the idle rate |
| Core 1, `flash-bridge`, run 1 | The P4 restarted twice mid-update (reset `Software`, phases `Pump` then `Command`); the update was still confirmed. Writes up to 65 ms, two `Io` errors |
| Core 1, `flash-bridge`, runs 2 and 3 | The P4 hung ("the bridge stopped answering the update"). An EN reset recovered it |
| `main` firmware, `flash-bridge` | Clean: no restart mid-update, 14.2 s from attach to the new life |

The cause is in `esp-storage` 0.10.0 (`src/common.rs`, `with`). For every flash call it stalls
core 1, wherever core 1 is, with no lock held, runs the call, then releases it. A core 1 frozen
while it holds the global critical section, or a scheduler or timer lock, leaves core 0 spinning
on that lock. ESP-IDF instead has the other core acknowledge at a safe point before it stalls.

**Decided: the card is one more future joined on core 0, and core 1 is never started.** The SD
code shares `Shared` the way its neighbors do, with no cross-core statics.

## Core 0 at 20 MHz was clean

| Run | Result |
| --- | --- |
| Core 0, 20 MHz, idle | 16 KiB write 2.1 / ~2.6 / ~7.6 ms, ~6 MB/s. No errors in ~24,000 writes |
| Core 0, 20 MHz, `flash-bridge` | Clean and confirmed. No SD errors, worst write 68 ms, 15.6 s from attach to the new life |

**Decided: the bus clock asked for is 20 MHz** (`WARTUI_P4_SD_HZ`). At 40 MHz the card failed
about one write in 6,100 with a CRC error. A format still tries each transfer three times.

## Every transfer is at most 16 KiB, from an aligned buffer

The driver's DMA ring is four descriptors of 4 KiB. A transfer longer than 16 KiB needs the
SDMMC interrupt mid-way to refill the ring, and a flash erase masks core 0's interrupts for about
40 ms. A buffer aligned only to the 4 bytes `sdio` asks for is split into a bounced head and tail
around its middle, which takes more descriptors than the ring holds and brings the refills back.

**Decided: every transfer is at most 32 blocks, from a 64-byte-aligned `InternalMemory` buffer.**
The worst write during the core 0 update was 68 ms, and no transfer failed.

## GPIO45 powers the card

GPIO45 switches the card's VDD through a P-channel MOSFET, so low powers it. The driver's
`with_power_enable` drives its pin high.

**Decided: GPIO45 is a plain output held low**, and `with_power_enable` is never used.

## What `sdio` does

- `BlockDevice::new_uninit_sd_card(slot, Delay)`, then `reacquire(hz)`, identifies the card, and
  `reacquire` identifies it again on the same slot, once the two fixes below are in.
- A 32-block write is one CMD25.
- `csd.block_count()` is a `u64`.
- `MmcError::Crc` covers response CRC, data CRC and start-bit errors. `Io` covers FIFO overrun,
  DMA and response errors.

## A card identified again

Measured on 2026-11-23 on the same board and card, with the bridge's own firmware. The card was
pulled while the bridge ran, then put back about 15 s later.

| Firmware | Result |
| --- | --- |
| `sdio` 0.5.2 from crates.io | `no card` until the P4 restarted, though the card was back. A P4 reset found it in under 3 s, with GPIO45 held low, so the card stayed powered |
| `sdio` with the RCA fix only | The card was identified again, then every block read timed out |
| `sdio` with both fixes | Found within one 5 s check: `FAT32, 60326 MiB free` |

Each attempt dumped the SDMMC registers. Two faults in `sdio` 0.5.2 caused it, and 0.5.3 has both:

- **The previous card's RCA.** After reinsertion, CMD8 was answered and the CMD55 before ACMD41
  timed out, every 5 s. `BusAdapter` keeps the RCA from the last CMD3, and nothing clears it, so
  the CMD55 carried the old card's address. CMD0 had returned the card to RCA 0, and a card answers
  only its own address. **Fix:** `init_idle` sets the RCA to 0 with its CMD0.
- **A sticky error flag.** After re-identification, the last command was CMD12 with a response
  timeout, and no read had been sent. `BlockDevice` sets `error` before each read or write and
  clears it only on success. While it is set, a read or write first sends CMD12 and returns if that
  fails, and a card with no transfer open ignores CMD12. **Fix:** `reacquire` clears the flag. The
  same fault follows any failed read with the card in place, which the firmware then recovers by
  identifying it again.

**Decided: the P4 bridge builds against a fork of `sdio`**, the published 0.5.2 plus those two
changes, pinned by commit in `firmware/p4-bridge/Cargo.toml`. esp-hal builds against the same copy.

## The log writer on the bench

Measured on 2026-11-23 on the same board, `96:C4`, with no carrier and a 32 GB card formatted by
the bridge: 30,436 MiB, 32 KiB clusters. The bridge ran its own firmware with the SD log on. The
card was read on a Mac with `fsck_msdos -n`, through `diskutil verifyVolume`, and with
`wartui sd-log`.

### How long the writer holds the executor

The writer yields after every card step, and times the work between two yields. Each kind of step
keeps its own longest, in µs:

| Build | next | fed | head | opened | copy | plan |
| --- | --- | --- | --- | --- | --- | --- |
| A flush staged in one stretch | | | | | | 695, all of a flush |
| Staging yields after its head and each block | 89–107 | 57 | 21–35 | 320–447 | 47–72 | 59–79 |
| The same, `Opened` encoded again at once | | | 7 | 24 | | |

`opened` is the head of a flush that starts a file, with its `Opened` record. It runs once per
file. Encoding the same record a second time right after took 24 µs, and the head without it
dropped from 35 to 7, so the rest is that code and the CRC table coming in from flash, cold. It
costs 0.3–0.45 ms, varying with the build, once per boot and once per rotation, as any code
run that seldom does.

**Decided: every step but a file's first stays under 200 µs, and the first is left as it is.**
The P4 masks no interrupt in any of it, so a UART's FIFO is still served.

### What the bench did to it

| Test | Result |
| --- | --- |
| The card pulled while the bridge wrote, twice | `fsck` clean both times. Every file decoded, 0 unreadable, 0 dropped. Put back, the next file opened |
| `sd log off`, then `on` | The file ended with `Closed`, logging turned off, and stopped growing; `on` opened the next |
| `sd log off`, then a restart | Still off after the restart; `on` opened the next file |
| A format while writing | Answered in 2.7 s, connecting included. The fresh card started again at file 1 |
| A file limit of 8 KiB | Files closed at 13–15 KB with `Closed`, the next file taking over, nothing lost. The size is checked before each flush, so a file runs up to one flush over |
| The card filled from the Mac to 7 clusters above the 256 MiB reserve, then 12 rotations | The oldest files went first, one per new cluster: files 1–11 deleted, 12–23 kept. `fsck` clean |
| About ten cuts of the board's power, at random moments, on that full card | `fsck` clean, with no lost clusters and the two FATs equal. Four boots got as far as opening a file; each decoded whole up to its last flush |
| Five `flash-bridge` updates with the log writing | All installed and confirmed |
| A card formatted FAT32 by macOS's `diskutil`, 16 KiB clusters | Mounted and logged: one file of 21 KB over two clusters, 332 records, 0 unreadable. `fsck` clean |

### On a carrier with ten nodes

The same firmware on the bridge `D0:1C`, a carrier-10 with ten nodes and the card formatted by
macOS. `flash-bridge` installed it, the C6's bundled image included, with the log writing. The
load was a Wi-Fi and Bluetooth scan on every node with `wartui sniff` taking every frame over USB,
for 3 minutes with the log on, then 3 with it off:

| Log | Records to the host | Radio link | Host link | Queues | SD log |
| --- | --- | --- | --- | --- | --- |
| On | 439: 88 sightings in 79 batches, 360 heartbeats, 0 undecodable | 198 packets, 0 CRC, magic or UART errors, no new gap | 0 UART errors | 0 dropped | 64 KB, 0 dropped, 0 errors |
| Off | 497: 221 sightings in 137 batches, 360 heartbeats, 0 undecodable | 198 packets, the same | 0 UART errors | 0 dropped | |

The bench hears few networks, so this was a light load. The writer's steps on this board were next
102, fed 63, copy 59 and plan 83 µs; opening a file took 407–439 µs.

### Deleting the history for nothing

After the power cuts, the Mac's own Spotlight and fsevents files took about 22 clusters while the
card was mounted, and left it below the reserve. The writer deleted every old file, about 12
clusters, could not get back over the reserve, and stopped with `card full` and nothing kept.

**Decided: the writer deletes the oldest only when deleting its own files can restore the
reserve.** Otherwise it keeps every file and stops with `card full`. With that build, the next
boot on the same card stopped with `Full` at its first flush and deleted nothing. The 40 records
that flush had staged, the boot's first, were counted dropped, as any failed flush counts what it
staged.

## Not answered here

The P4 keeps no record of its own last panic, as the C6 does; the bench build carried one, and
the bridge does not. A carrier under a heavy load, a drive or a two-node burst, was not tested
with the log, nor was a WardriveESP phone session, the case the log is for.
