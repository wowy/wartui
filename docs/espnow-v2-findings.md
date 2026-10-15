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

## What this decided

Updates send 1 KB chunks (`OtaChunk`, 1035 bytes in all) over v2. Only host→node frames are long.
Everything a node sends stays at 250 bytes or less, so the bridge→host link keeps its frame size.
