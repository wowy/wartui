# Over-the-air updates — what an update costs and what survives a failure

Measured on 2026-10-14 and 2026-10-15, on the bench at close range. There were two questions:

- how long `wartui ota` takes to update a node, and what sets that time;
- whether a node always ends on a working image when the update or the image goes wrong.

**Boards and firmware**

- Bridge: `75:40`, a XIAO ESP32-C6 with an external antenna.
- Nodes:
  - `F5:50`, an ESP32-C5 with 8 MB flash;
  - `9A:24`, a XIAO ESP32-C6 with 4 MB flash.
- Firmware: 0.1.1, flashed over USB. 0.1.2 and 0.1.3 were built on the bench and never committed.
  The runs, in order:
  1. both nodes 0.1.1 → 0.1.2, into blank slots;
  2. the C5 0.1.2 → a 0.1.3 that panics at boot;
  3. both nodes 0.1.2 → 0.1.1 (`--force`), over old images;
  4. 0.1.1 → 0.1.1 (`--force`) for the hang, reset and assigned-node cases.
- App images: about 849 KB for the C5 and 877 KB for the C6.
- Link: one 1 KB chunk in flight at a time over ESP-NOW v2 (`docs/espnow-v2-findings.md`).

Per-chunk and per-sector timings come from the old-image runs, logged with `--log-file` and
`RUST_LOG=wartui_core=debug`. That records every send, send result, node status and phase change.
The blank-slot runs were not logged that way, so the progress line's KB/s is all there is for
them.

## Throughput

| node | slot written | KB/s | wait per sector, median | waits over 20 ms | transfer |
| ---- | ------------ | ---- | ----------------------- | ---------------- | -------- |
| F5:50 (C5) | blank      | 123 | not logged | not logged         | ~7 s   |
| 9A:24 (C6) | blank      | 151 | not logged | not logged         | ~6 s   |
| F5:50 (C5) | old image  | 47  | 68 ms      | 207, 14.3 s in all | 17.6 s |
| 9A:24 (C6) | old image  | 88  | 29 ms      | 211, 6.1 s in all  | 9.7 s  |

"Blank" is the first update after a USB flash, which leaves the update slot erased. Every later
update writes over the image in that slot.

A "wait" is the time from one chunk's `SendResult` to the next chunk's send. It is near zero
inside a sector, and the node's flash work at a sector boundary. The C6's 877,024-byte image has
214 full sectors, and 211 of their boundaries waited over 20 ms. The other three were not singled
out; the median and total are for the 211.

- **The radio is not the limit.** In the two logged old-image runs:
  - every one of 860 sends to the C6 and 833 to the C5 was acked first time;
  - no chunk was sent twice;
  - a chunk took a median 3.5 ms from send to its `SendResult`, against 3.6 ms in the v2 bench.

  The reset runs below do resend: the transfer starts over from 0 after the node reboots.
- **Erasing flash is the limit.**
  - At each 4 KB sector the host waits for the node to erase and write it before sending more.
  - Over an old image that wait is 29 ms on the C6 and 68 ms on the C5, with a maximum of 99 ms.
  - Those waits are 63% of the C6's transfer and 81% of the C5's.
  - A node's first update, into a blank slot, ran 1.7 (C6) to 2.6 (C5) times faster. Erasing a
    sector that holds nothing is the likely reason, but it was not timed per sector.
- **What an update takes:** about 10 s of transfer for a C6 and 18 s for a C5, plus a few seconds
  to reboot and confirm. Updating both nodes, including building two images and the 6 s listen,
  took 30 s from a blank slot.
- **The C5's slowness is its flash chip, not its radio.** Both chips show the same 3.5 ms per
  chunk.

## Failures

| case | what happened | outcome |
| ---- | ------------- | ------- |
| image panics at boot (C5, 0.1.2 → 0.1.3) | The node rebooted three times on trial and switched back by itself. | `failed: still running 0.1.2: the new image rolled back or never booted`; node on 0.1.2 |
| image hangs at boot (C5) | The host gave up after 60 s. Two hard resets over USB made the third trial boot, which rolled back. | `failed: no heartbeat from the new image within 60 s`; node on its old image |
| corrupted file, one byte flipped | The host refuses it before sending, because its checksum byte reads 0x9d and its segments give 0x62. | refused; nothing sent |
| node reset at 34% (C5) and 54% (C6) | The node stops acking while it reboots. The session goes back to `begin`, which the node accepts on its next heartbeat, and the transfer starts over from 0. | `done (confirmed)` |
| node holding an assignment (C5) | The first `begin` landed in one of the node's 100 ms admin windows. | `done (confirmed)` |

- **The node's own rollback covers both a crash and a hang.**
  - A crash reboots through the panic handler.
  - A hang needs an external reset, because the node has no watchdog. In the field that is a power
    cycle. On the bench it was a USB hard reset, and each one counted a trial boot.
  - Either way the third unconfirmed boot switches back.
- **The SHA-256 in `OtaBegin` protects the transfer, not the file.** The host hashes the file it is
  given, so a corrupt file passes the node's check.
  - The first bench run sent one anyway, and the node installed it.
  - The bootloader then refused it: `Checksum failed. Calculated 0x62 read 0x9d`, `OTA app
    partition slot 1 is not bootable`. It booted the other slot, so the node stayed safe.
  - The host now checks the image's own checksum and appended SHA-256 first.
- **Recovering from a reset takes about 2 s.** On the C5, the reset came at 12:47:52.3 with
  288 KB acknowledged. The session was back to begin at once, streaming again at 12:47:54.4, and
  done at 12:48:14.2.
  - The first bench run failed this case in under half a second: ten unacked sends during the
    reboot ended the update.
  - The session now starts over instead, bounded by `MAX_RESTARTS`.

## Not measured

- A power cut, rather than a USB hard reset: to count a hung image's trial boots, or during a
  sector erase or an otadata write.
- Sending the next sector's chunks while the node erases. The node's receive queue is ten frames
  deep, so up to a sector's four chunks could wait there during an erase. That could hide the link
  time, about 14 ms a sector, behind the erase. It was not tried, and whether the radio takes frames
  during an erase was not measured.
- Range: every run was at close range with the external antenna fitted.
- A whole fleet. Two nodes were updated at a time, one after the other.
- A `HashMismatch` from corruption in transit. That needs a lost or altered chunk that is still
  acked, and none occurred. The receiver's tests cover it.
- Erasing in 64 KB blocks rather than 4 KB sectors. NOR flash usually erases a block faster per
  byte than a sector. Whether `esp-storage` can, and whether it shortens an update, is open.

## What this decided

- **One chunk in flight stays for now.** The link never lost a frame, and most of the time is the
  node's flash work, which no change on the host can shorten. Overlapping the link time with the
  erase is the open question above.
- **`wartui ota` checks the image's own integrity before sending,** because the transfer hash
  cannot catch a bad file.
- **A node that stops acking mid-transfer is started over, not failed.**
- **`crates/wartui/README.md` § "Updating over the air" quotes these timings.**
