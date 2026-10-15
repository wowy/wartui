# ESP-NOW v2 — whether long frames reach a node

Measured on 2026-10-14, on the bench at close range. The question was whether host→node
frames longer than the v1 limit of 250 bytes work on our boards. The answer decides how large
an over-the-air update chunk can be.

**Boards and firmware**

- Bridge: `75:40`, a XIAO ESP32-C6 with an external antenna.
- Nodes:
  - `F5:50`, an ESP32-C5 with 8 MB flash;
  - `9A:24`, a XIAO ESP32-C6 with 4 MB flash.
- Firmware: `esp-radio 1.0.0-beta.0` with the `esp-wifi-sys 0.2.0` blobs. The ESP-NOW rate is
  802.11g 24 Mbps per peer.

**How it was run**

- The node was parked on the control channel with no assignment.
- The host sent one frame at a time, the next only after the previous one's `SendResult`.
- The node answered each frame with a 12-byte reply carrying the length and checksum it
  received.
- Every run was 300 frames, apart from the first 200-frame baseline.

| node | size (bytes) | acked | replies matched | median ms/frame | p95 ms | payload KB/s |
| ---- | ------------ | ----- | --------------- | --------------- | ------ | ------------ |
| F5:50 (C5) | 250  | 300/300 | 299/300 | 1.47 | 3.9 | 133 |
| F5:50 (C5) | 251  | 6×300, all acked | 291–300 of 300 | — | — | 136–150 |
| F5:50 (C5) | 1035 | 300/300 | 300/300 | 3.58 | 6.1 | 259 |
| F5:50 (C5) | 1470 | 300/300 | 300/300 | 4.74 | 8.6 | 278 |
| 9A:24 (C6) | 250  | 300/300 | 300/300 | 1.51 | 4.5 | 125 |
| 9A:24 (C6) | 251  | 300/300 | 300/300 | 1.51 | 4.9 | 123 |
| 9A:24 (C6) | 1035 | 300/300 | 300/300 | 3.58 | 6.4 | 252 |
| 9A:24 (C6) | 1470 | 300/300 | 300/300 | 4.74 | 8.9 | 272 |

## Findings

- **Both chips run ESP-NOW v2.** Both nodes and the bridge report `version() == 2` at boot.
  Every frame up to 1470 bytes was acked and arrived intact, on both chips. Nothing in
  `esp-radio` or the blobs needed configuring.
- **Time per frame grows with length, so the cost is on the bytes, not the air.**
  - A frame costs about 1.5 ms at 250 bytes, 3.6 ms at 1035 and 4.7 ms at 1470.
  - At 24 Mbps, 1035 bytes is about 0.35 ms on the air. The rest is moving the frame over the
    bridge's USB link.
  - Throughput still roughly doubles from 250 to 1035 bytes, and gains little beyond that.
- **A 1 KB chunk carries an 840 KB image in about 3.5 s of transfer**, against about 6.5 s at
  236 bytes. Erasing and writing flash, rather than the radio, will set the pace of an update.
- **The node's heap is unaffected.** It held at about 52 KB free on both chips. With one frame
  in flight, the receive queue never holds more than one long frame.
- **Small node→host replies are lost now and then: 0 to 9 per 300.** The host→node frames they
  answered were all acked. An update's progress report can therefore go missing even when the
  chunk landed. The host's timeout and go-back from the node's reported offset cover this.
- **One connect to the bridge timed out**, out of 15 back-to-back runs, between two that
  worked. Six repeats did not reproduce it. It is noted here in case it recurs, not explained.

## Sighting batches at 1024 bytes

Measured on 2026-10-15 on the same three boards. Both C6s were built with
`xiao-external-antenna`. The question was whether node→bridge batches of up to 1024 bytes cost
anything against 250, under a full load.

**How it was run**

- Nodes ran a bench-only build that filled the Wi-Fi pending buffer to its limit of 64 every
  dwell with made-up access points. Each had a fresh address, so every one was due. SSIDs cycled
  through 0, 8, 16 and 32 bytes, and every eighth record carried a roaming consortium trailer.
  The patch was never merged.
- The node logged, per dwell, the frames it sent and the time from its first `take` to its last
  `flush`.
- Two runs of 10 minutes each, with host, bridge and nodes all built from one tree each time:
  `main` for 250-byte batches, then the 1024-byte change. A static test position was used.
- Node log lines were counted within the host's run window.

| | 250-byte batches | 1024-byte batches |
| --- | --- | --- |
| frames per dwell (64 records) | 8 | 2 |
| frames the bridge received | 67,371 | 17,492 |
| send time per dwell, C6, median / p95 | 4.9 / 16.0 ms | 2.5 / 7.5 ms |
| send time per dwell, C5, median / p95 | 6.6 / 20.0 ms | 3.0 / 9.6 ms |
| dwells per second, C6 / C5 | 7.24 / 6.87 | 7.45 / 7.10 |
| records stored | 539,648 | 555,792 |
| bridge drops, lost batches, unacked sends | 0, 0, 0 | 0, 0, 0 |

**Findings**

- **A full dwell goes out in a quarter of the frames and half the time.** The time saved goes
  back into sniffing: about 3% more dwells, and so 3% more records, in the same 10 minutes.
- **Nothing was lost at either size.** Every record each node sent in the window was stored, and
  the bridge dropped nothing while a host was reading.
- **The bridge kept its heap.** It reported 52–53 KB free after both runs, with no oversize
  frames.
- **A bridge with no host reading drops frames at either size.** 5,787 frames at 250 bytes and
  3,300 at 1024 were dropped in the two minutes between the run ending and the status read,
  with the nodes still sending. These are outbox evictions when no host is draining, as
  `crates/wartui-proto/src/outbox/mod.rs` describes. They are not a cost of the frame size.
- **One frame arrived garbled in the 1024-byte run**, at the first status after connecting,
  while the host replayed the bridge's backlog. The 250-byte run had none. Every frame after
  the backlog decoded cleanly.

## What this decided

Updates send 1 KB chunks (`OtaChunk`, 1035 bytes in all) over v2. Sighting batches use v2 too, up
to 1024 bytes (`SIGHTING_BATCH_MAX`), and the bridge→host link frame (`MAX_FRAME`, 1152 bytes) is
sized to carry one. Everything else a node sends is fixed-size and short.
