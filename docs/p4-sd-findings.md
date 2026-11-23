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

## Not answered here

The P4 keeps no record of its own last panic, as the C6 does; the bench build carried one, and
the bridge does not. A board on a carrier with nodes, under a two-node burst, was not tested.
