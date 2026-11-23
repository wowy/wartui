# P4 microSD card — which core drives it, at what clock, in what transfers

Measured on 2026-11-22 on a Waveshare ESP32-P4-WIFI6, the bridge `96:C4`, with no carrier and a
64 GB card in its TF slot. The question was whether the card can be driven while the bridge runs,
on which core, and whether a settings write or a `wartui flash-bridge` update still passes while
it writes.

The firmware is the spike on the branch `spike/sd-bring-up` (`c778bb6`), `src/sd.rs` and
`src/last_panic.rs`, not for merging. Build-time switches picked the core
(`WARTUI_P4_SD_CORE`), the bus clock asked for (`WARTUI_P4_SD_HZ`), the input sampling phase
(`WARTUI_P4_SD_PHASE`) and a gap between writes (`WARTUI_P4_SD_GAP_MS`).

## The setup

- SDMMC slot 0 through esp-hal 1.2.2's `sdmmc` and the `sdio` 0.5 crate, 4-bit. CLK GPIO43, CMD
  GPIO44, D0–D3 GPIO39–42. GPIO45 low powers the card.
- One loop: 256 writes of 16 KiB per cycle to the card's last 8192 blocks, then a read-back of the
  cycle's last write, compared.
- Every transfer is 32 blocks from one 64-byte-aligned internal-RAM buffer.

## The card

123,596,800 blocks (63,281 MB). Block 0 ends `55 aa` and its first partition is type `0x0c`,
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
  `reacquire` identifies it again on the same slot.
- A 32-block write is one CMD25.
- `csd.block_count()` is a `u64`.
- `MmcError::Crc` covers response CRC, data CRC and start-bit errors. `Io` covers FIFO overrun,
  DMA and response errors.

## Not answered here

The P4 keeps no record of its own last panic, as the C6 does; the spike carried one, and the
bridge does not. A board on a carrier with nodes, under a two-node burst, was not tested.
