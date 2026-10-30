# P4 ↔ C6 over UART HCI, with a phone as the host: spike plan

The question is whether a phone can replace the USB-connected host. The setup:

- The C6 runs only as a Bluetooth controller, talking HCI to the P4 over a UART.
- The P4 runs the Bluetooth host stack (TrouBLE) as a peripheral.
- A phone connects to the P4 and receives the same stream the USB host receives today.

The spike also checks that the P4 can drive this while running two 8 MHz SPI buses. Those buses
are for the planned wired node link, and moving the C6 off SPI2 is what frees a controller for
them.

Like `spike/p4-spi-link`, the code lives on a branch that is never merged (`spike/p4-uart-hci`,
under `firmware/spike/`). The results go in `docs/p4-uart-hci-findings.md`.

**On terms:** "host" means two things here.

- **The wartui host** is the program that consumes the bridge's stream. Today that is `wartui`
  on a PC. In this spike it is the phone.
- **The Bluetooth host** is the stack above the controller, TrouBLE on the P4.

The phone is the Bluetooth *central*. It is not the Bluetooth host of the P4's controller.

## Why the stream needs so little

- **Real drive:** the 52-minute, six-node drive in `batch-loss-findings.md` carried 23,496 frames,
  about 7.5 a second. Batches average about 45 bytes, so with `Rx` framing that is roughly
  0.5 KB/s.
- **20-node fleet:** scaled to the project's 20-node goal, the average is about 25 frames a second,
  or about 1.6 KB/s.
- **Bursts:** the burst-test node's bursts (550–700 full batches a second,
  `link-encode-findings.md`) are a bench load, not a drive. A phone link will not carry them at
  burst rate. The P4 buffers them instead, as its 256 KB bulk ring does today.

So the bar is a phone link that comfortably beats the fleet's average, with the P4 absorbing
bursts. The bar is not a phone link that matches USB.

**What 16 KB/s carries in sightings,** with today's framing:

- **Per record:** a sighting record is 12 bytes plus its SSID and trailer, about 22 bytes for a
  typical SSID.
- **Per frame:** wrapping a batch as an `Rx` frame adds about 24 bytes: the version byte, the
  variant, two MACs, RSSI, `rx_us`, the payload length, the CRC, COBS and the terminator.

| Batches | Frame on the stream | Frames/s | Sightings/s | Sightings/min |
| --- | --- | --- | --- | --- |
| Real-drive size, 45 B, about 1.7 records | about 69 B | about 237 | about 400 | about 24,000 |
| Full, 1,024 B, about 46 records | about 1,054 B | about 15.5 | about 715 | about 43,000 |

The 52-minute drive averaged about 4.4 sightings a second (about 8,000 batches at 1.7 records),
so even the small-batch row is about 90 times what it carried. Heartbeats come out of the same
budget, but 20 nodes' heartbeats take under 1 KB/s. A wired node link may frame batches
differently, which would change the per-frame overhead but not the size of the margin.

## Pass criteria (proposed)

1. **UART link clean:** the P4 ↔ C6 UART carries HCI with zero framing, CRC or sequence errors at
   the chosen baud rate. That holds for 10 minutes at full Bluetooth load, with both SPI buses
   running flat out at the same time.
2. **Fast enough:** sustained application throughput to each test phone is **at least 16 KB/s**,
   10× the 20-node average. The Linux stand-in carries the same.
3. **Same data:** the Linux stand-in receives every record the P4 queued, in order and intact,
   decoded with `wartui-proto`'s own link decoder.
4. **Commands work:** a command written by the phone gets its answer back. Measure p50 and p99
   round-trip times.
5. **Attach and detach work:** detaching and reattaching behaves as DTR does on USB today
   (`firmware/p4-bridge/README.md` § "Queue sizes"). No backlog is held for an absent host, and
   the queues clear when a new host attaches.
6. **No starvation:** neither SPI bus loses a transfer or falls below its rate while Bluetooth runs.

The user accepted criterion 2's 16 KB/s on 2026-10-30. No burst seen on a drive has come close
to it. Drives in denser areas may still revise it.

## Hardware and pins

**Board:** Waveshare ESP32-P4-WIFI6-POE-ETH, the bench board. The C6 is flashed through its header
as today (`firmware/p4-radio/README.md` § "First flash").

**UART on the SDIO traces:** this uses the same traces as today's SPI link, reassigned through
both chips' GPIO matrices. The 51 kΩ pull-ups hold the lines idle-high, which suits UART.

| Signal | P4 GPIO | C6 GPIO | Was |
| --- | --- | --- | --- |
| P4 TX → C6 RX | 19 | 18 | MOSI |
| C6 TX → P4 RX | 14 | 20 | MISO |
| P4 RTS → C6 CTS | 18 | 19 | SCLK |
| C6 RTS → P4 CTS | 15 | 21 | CS |
| C6 enable | 54 | CHIP_PU | unchanged |

- **Ports:** UART1 on both chips. UART0 stays the console on the P4 (GPIO37) and the flashing
  port on the C6.
- **Flow control:** hardware RTS/CTS. H4 has no flow control of its own, and at several Mbaud a
  full FIFO drops bytes. esp-hal 1.2.2 has `HwFlowControl` and `with_rts`/`with_cts`, marked
  unstable.

**The two SPI buses:**

- **Controllers:** SPI2 and SPI3 masters on the P4 at 8 MHz, with DMA.
- **Wiring:** a loopback jumper (MOSI → MISO) on free header pins. The wired node hardware does
  not exist yet, so the buses check what they read back.
- **Pins to avoid:**
  - GPIO7/8 (GPS I2C)
  - GPIO37 (console)
  - the USB high-speed pins
  - the traces above

  Record the pins in the findings.

**The wartui hosts:**

- **Linux stand-in:** a script on the bench PC using `bleak` (`host/ble_rate.py`), in the role
  `host/rate.py` and `host/bridge.py` had in the SPI spike. It checks every record.
- **Phones and tablet:** the user's iPhone, iPad and two Android phones, each running nRF Connect
  to connect and subscribe.
  - Apple devices limit the connection interval and MTU more than Android does, so both the
    iPhone and the iPad are measured.
  - Android's limits depend on the phone's Bluetooth chip and vendor, so both phones are measured.
  - The findings record each device's model and OS version.
  - nRF Connect cannot verify records, so the P4 measures each device's throughput itself (see
    below). The Linux stand-in alone checks integrity.

## Firmware

### C6: a controller and nothing else

- **Radio:** `esp_radio::ble::controller::BleConnector`. It implements `embedded_io` `Read` and
  `Write` over raw H4 packets, so the firmware is a byte pump between it and UART1, in both
  directions.
- **No logic:** no HCI parsing, apart from counters.
- **Counters:** H4 packets and bytes each way, UART errors (framing, overrun) and the time the
  C6 spends with RTS deasserted.
- **Where counters are logged:** to its console every 5 s. The bench can watch the C6's UART0
  through the header.

### P4: TrouBLE as a peripheral

- **Transport:** TrouBLE over `bt-hci`'s serial (H4) transport on UART1, on the embassy executor
  the P4 already runs.
- **Advertising:** a name carrying the C6's MAC's last two octets, as the USB serial number does.
- **One GATT service, "wartui link":**
  - **Up (notify):** the exact byte stream the USB CDC-ACM endpoint carries, COBS-framed
    `BridgeToHost` records, split across notifications at whatever MTU was negotiated. The phone
    reassembles it with the same `FrameAccumulator` the host uses.
  - **Down (write without response):** `HostToBridge` frames, framed the same way.
- **Attaching:** subscribing to Up stands in for DTR rising. It clears the host queues and sends
  `Ready`. Unsubscribing or disconnecting stands in for DTR falling.

**The load generator:** the P4 has no radio in this setup, so it makes up the stream.

- `Rx` records whose payloads carry a sequence number and a CRC, encoded with `encode_frame`.
- Three profiles, switched by a `HostToBridge` command:
  - **drive:** 7.5 frames a second of 45-byte batches
  - **fleet-20:** 25 a second
  - **saturate:** as fast as notifications drain
- A `Status`-like record every 2 s carries what the P4 measured:
  - bytes queued, bytes notified, and bytes the controller reported delivered (its
    Number-of-Completed-Packets events)
  - queue high-water mark and drops
  - the negotiated MTU, PHY and connection interval
  - UART error counters and SPI bus counters

  The delivered count is the phone's throughput, measured without a phone app.

**The SPI buses:** both run flat out at 8 MHz from their own tasks, with 1 KB DMA transfers. Each
checks its loopback. They count transfers, mismatches and the longest gap between transfers.

## Runs

1. **UART alone.** Before any Bluetooth runs, a loopback build of the C6 echoes bytes, so the UART
   can be checked on its own. The P4 checks what comes back at 921,600, 2M, 3M and 4M baud, with
   and without RTS/CTS, for 2 minutes each. This finds the highest clean rate and whether flow
   control works on these traces.
2. **Bluetooth up.** The controller build on the C6, at the fastest clean baud rate. Then check:
   - the Linux stand-in connects and subscribes, and gets `Ready`
   - a `GetStatus` written to Down is answered on Up
3. **Throughput, Linux.** The saturate profile for 2 minutes at each PHY the stand-in's adapter
   supports.
4. **Throughput, phones.** The saturate profile for 2 minutes on each of the four devices. Record
   the negotiated MTU, PHY and interval for each, as the P4 reports them. Criterion 2 must hold on
   the slowest of them, which is expected to be one of the Apple devices.
5. **Integrity and attaching.** The fleet-20 profile for 10 minutes to the Linux stand-in. During
   it:
   - disconnect and reconnect it three times
   - walk the phone out of range once

   Check sequence continuity within each attachment, and that no backlog arrives after a
   reattach.
6. **Contention.** Run 3 again with both SPI buses running, then once more with a burst from the
   generator queued at the same time. This is criteria 1 and 6.
7. **Command latency.** 1,000 `GetStatus` round trips from the stand-in, under the fleet-20
   profile. Report p50 and p99.

Each run's numbers go in the findings, with the commits and the baud rate.

## Out of scope

- **Pairing, bonding and encryption.** The service is open for the spike. A real one needs at
  least Just Works bonding, and that is the follow-up's first question.
- **The C6 update path over UART.** If this spike passes, it gets designed next: either
  vendor-specific HCI packets or a thin frame header in front of H4.
- **A phone app.** nRF Connect and the Linux stand-in are enough to measure.
- **The wired node protocol.** The buses carry only loopback here.
- **Running the USB host and the phone at once.**

## Where this fits

These were decided with the user on 2026-10-30:

- **Wired USB stays the host link for now.** Bluetooth to a phone is a near-term addition beside
  it, not a replacement.
- **Firmware updates from the phone come eventually,** but not in the first Bluetooth version.
  - Nothing in the spike's service design may rule them out: `HostToBridge::Update` goes over the
    Down characteristic like any other frame.
  - At 16 KB/s a P4 image takes tens of seconds. Run 3's measured rates give the real figure for
    the findings.
