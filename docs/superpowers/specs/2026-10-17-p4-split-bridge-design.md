# P4 split bridge: design

A second kind of wartui bridge. A Waveshare ESP32-P4 board whose on-board ESP32-C6 runs the
radio, while the P4 talks to the host over USB high speed. It lives alongside the single-chip
C5/C6 bridges and does not replace them.

The spike behind it is [`docs/p4-spi-link-findings.md`](../../p4-spi-link-findings.md) (PR #214,
code on the local branch `spike/p4-spi-link`). Read it first. This document builds on what it
proved and does not repeat its measurements.

## Why

- **Headroom under load.** The goal is that the bridge is never what limits a fleet growing
  toward 20 nodes. This is a nice-to-have, not a fix. Today's bridge has not been shown to
  fail, so "better under load" is something the finished bridge is measured on, not a premise.
- **It is more interesting to build.** The design may be more ambitious than the minimum.
- **Room for more later.** Several P4+C6 boards have large displays. A display is a later
  project, but nothing here may block it. In practice that means the P4 keeps spare CPU, memory
  and pins, and nothing time-critical on the radio side depends on the P4.
- **The ESP32-S31 is a long-term goal of the project.** It is expected to have its own radio and
  high-speed USB on one chip. So the radio half and the host half split in code the same way
  they split in hardware, and an S31 bridge later puts the same pieces together on one chip.
  (The S31's specs were not checked for this design; treat that as an assumption.)

## Goals

1. The host runs wartui exactly as it does today against a P4 bridge. Nothing above the bridge
   layer (node handling, channel planning, GPS parsing, captures, uploads) changes behavior.
2. Under the heaviest burst load the bench can generate, the P4 bridge loses nothing the
   single-chip bridge kept, with the same nodes and the same load.
3. One firmware image runs on both the ESP32-P4-WIFI6-POE-ETH (on the bench now) and the
   ESP32-P4-WIFI6 (arriving 2026-10-18). Both stay supported.
4. Buffer sizes on both chips are compile-time configurable, for testing and right-sizing. The
   C6 uses its spare memory for bigger buffers where that helps.
5. After the first flash, updates to both chips go over one USB cable.

## Non-goals

- A panel or display. It comes later, probably as a large P4 display, not the five-line ST7735.
- Parsing GPS on the P4. The host already parses NMEA and UBX, and the bridge stays a byte pipe.
- Runtime configuration of buffer sizes.
- Ethernet or PoE.
- Changing the single-chip bridge's behavior. It gets restructured to share code, and that is all.

## Anti-goals (these would make it a failure even if it works)

- **The radio code drifts into two copies.** Different configuration for the C6-in-a-P4 case is
  fine. A second implementation of ESP-NOW handling (receive, send-and-wait-for-result, peers,
  rate, transmit power) is not.
- **Setup gets fussier than it is interesting.** The first flash of the C6 may stay awkward. That
  is accepted. Everything after it should not be.

## Scope, in order

Each step is usable and benchmarkable on its own.

1. **Radio and link.** The full single-chip bridge feature set except the panel, through the
   split. That covers receive, send with the true delivered-or-failed result, peers (including
   the twentieth slot), 24 Mbps, transmit power, Identify, Status, Reset, logs and errors. It
   includes the host changes needed to find, identify and use the bridge, plus the benchmark
   against the single-chip bridge.
2. **GPS over I2C on the P4.** Bytes pass through in both directions, as the T-Dongle-C5's GPS
   does (`firmware/bridge/src/gps.rs`, `HostToBridge::GpsWrite`, `BridgeToHost::Gps`).
3. **Updates over one cable.** `wartui flash-bridge` updates both the P4 and its C6 through the
   high-speed USB link. The P4 receives its own image, and passes the C6's image down the SPI
   link. This reuses the node's OTA machinery (`firmware/node/src/ota.rs`,
   `wartui_proto::ota`). The first flash of each chip stays a wired flash.

## Hardware facts

Both boards are the same for this design. The schematics were checked on 2026-10-17.

| What | Pins / part | Notes |
|---|---|---|
| P4 ↔ C6 link | P4 GPIO18→C6 IO19 SCLK, GPIO19→IO18 MOSI, GPIO14→IO20 MISO, GPIO15→IO21 CS | The C6's SDIO traces, 51 kΩ pull-ups, proven clean at 40 MHz |
| READY | C6 IO2 → P4 GPIO6 (0 Ω) | Driven by the C6 |
| C6 enable | P4 GPIO54 → C6 CHIP_PU (0 Ω) | The P4 can hold the C6 in reset |
| Spare SDIO lines | P4 GPIO16→C6 IO22, GPIO17→IO23 | Unused by the spike; available |
| C6 module | ESP32-C6-MINI-1-N4 | 4 MB flash, 512 KB HP SRAM |
| GPS | I2C on P4 GPIO7 (SDA), GPIO8 (SCL), 2.2 kΩ pull-ups on board | Shared with the ES8311 audio codec; the u-blox answers at 0x42. The user's receiver is wired to these pins on the POE-ETH |
| Host link | P4 USB 2.0 HS PHY (DP/DM pins) | POE-ETH: Type-A. WIFI6: 4-pin MX1.25 (`P1`) |
| Console | CH343P on P4 UART0, Type-C | Bench only |
| C6 first flash | 4-pin header: IO9, GND, U0RXD, U0TXD | IO9 to GND at power-on, P4 held in download mode |

Board differences, all physical:

- **Reset.** The WIFI6 has auto-reset (CH343P DTR/RTS → EN and GPIO35 through an EMH4T2R), so
  espflash resets it normally. The POE-ETH has none. Flashing means holding BOOT while plugging
  in, and running means replugging. `AGENTS.md`-level bench notes are in the findings doc.
- **The high-speed port's 5 V is always live on both boards.** On the POE-ETH a load switch with
  its enable tied high drives the Type-A's VBUS. On the WIFI6, MX1.25 pin 1 is the board's main
  5 V rail, which the Type-C feeds through an ideal-diode PMOS (AO3401). Either way, a cable to
  a PC that carries VBUS back-feeds the PC when the Type-C is also connected. The cable must
  leave VBUS unconnected. The user's bench cable has it taped off. Alternatively, a WIFI6 can be
  powered from that port alone, which is the single-cable setup goal 5 implies.

## The parts the user decided

- A second bridge kind alongside the single-chip ones, sharing the radio code.
- Version one targets the POE-ETH now, with the WIFI6 verified when it arrives. Both stay
  supported.
- GPS is a step of this work. It goes over I2C on the P4, with no parsing on the P4.
- No panel yet.
- The first C6 flash goes through the header. After that, updates go over one USB cable, as part
  of this project.
- Buffer sizes are compile-time configurable on both chips. The C6's spare memory goes into
  bigger buffers if needed.
- Success is goals 1 and 2 above.

## Design calls made in the brainstorm (the user may override)

These were stated to the user, who did not object. They are the designer's calls, not the
user's.

### Code layout

- **The ESP-NOW handling moves out of `firmware/bridge/src/main.rs` into shared code.** The
  single-chip bridge and the C6 radio firmware both use that one implementation. Today
  `firmware/common` (`wartui-firmware`) holds code shared by the bridge and node. The radio half
  probably belongs there, behind whatever feature keeps it out of the P4 build (the P4 has no
  radio). The bridge's command handling (`handle`, `transmit`, `add_peer`, `set_peer_rate`,
  `finish_radio_setup`, `forward`, `drain_radio`) is the material to move.
- **The single-chip bridge connects the radio half to its USB link in memory.** The C6 radio
  firmware connects it to the SPI link. The P4 firmware owns the USB link, buffering, GPS,
  identity and (step 3) updates.
- **The SPI link's framing lives in `wartui-proto`**, `no_std` like the rest, so both firmwares
  share it and its encode, decode, CRC and sequencing get host-side unit tests. This follows the
  rule the link protocol already follows.

### What the host sees

- **A new `Chip::Esp32P4` variant.** `Ready.mac` is the C6's MAC, because that is the address
  nodes see as the bridge.
- **The C6's own health travels with the bridge's.** That covers firmware version, reset cause,
  last loop phase, heap free and uptime. It goes in an added section of `Ready` and, for the
  counters, `Status`, so diagnostics cover both chips. **`LINK_PROTO_VERSION` stays at 1**, as
  `AGENTS.md` requires before 1.0. The new variant and fields go on the end of their enums and
  structs, and builds from different trees meet as an undecodable frame, as they already do.
- **A C6 restart under a running P4 looks to the host like a bridge restart.** The C6 loses its
  peer table and transmit power when it restarts. The P4 therefore announces a fresh `Ready`
  with the C6's reset cause, so the host's existing recovery runs (it re-adds peers on demand,
  and transmit power comes with every status poll). A P4 restart resets the C6 through GPIO54,
  so the pair always starts together.
- **`wartui ports` and `flash-bridge` learn the P4 bridge's USB ID.** Today they accept only
  Espressif's VID (`crates/wartui-bridge/src/ports.rs`, `ESPRESSIF_VID`). The spike used
  `0x1209:0x0001`, the pid.codes test ID, which is not for release.
  - **The user is requesting a product ID under Espressif's VID** (`0x303A`) from
    espressif/usb-pids. Until it is granted, the test ID ships on `main` (the user's decision:
    they are the only user before 1.0). The host matches it as the exact pair `0x1209:0x0001`
    **and** the product string `wartui`, so no other test-ID device gets an `Identify`.
  - Once the real ID arrives, the firmware and the host's match change to it. Nodes and
    single-chip bridges are `0x303A:0x1001` (the built-in USB-serial-JTAG). The new product ID
    must therefore keep `flash-fleet` from treating the P4 bridge as a node, and keep
    `flash-bridge` from treating it as a single-chip bridge.

### The SPI link

- It builds on the spike's proven shape. The P4 is master and the C6 is a DMA slave at 40 MHz.
  The C6 drives READY when it has armed a transfer. Every transfer has a fixed length, a header,
  a sequence number and a CRC.
- **It is two-way.** Host commands (`SendEspNow` up to 1,470 payload bytes, peers, transmit
  power, Status, Reset) go down to the C6, and frames, send results, logs and C6 status come up.
  The spike only carried test patterns downward, but did so error-free at every clock. The
  transfer size has to grow, or fragment, to carry a 1,470-byte send.
- **The C6 prepares the next transfer while the current one is in flight** (double-buffering).
  The spike measured C6 preparation (avg 243 µs) as the throughput ceiling, about 1,900
  transfers/s at 40 MHz. Double-buffering should approach the wire limit.
- **Send latency has a budget.** The send result arrives 2–4 ms after a send on a single-chip
  bridge, and up to 35 ms when unacknowledged, against the node's 100 ms admin window
  (`firmware/bridge/README.md` § "Transmitting"). The split adds at least one transfer each way.
  It must stay a small fraction of that window, including when the link is carrying a burst of
  received frames upward.
- **A small, versioned core of the SPI protocol is frozen from the first commit** (the user's
  decision). The core is "hello, which version are you" and "here is a new image", plus the
  framing they need. It never changes, even before 1.0. Everything else on the link may change
  freely between builds. Any P4 build can therefore always identify, and update, any C6 build,
  so a change to the link never sends the user back to the header. A P4 that finds a C6 whose
  non-frozen protocol differs from its own does not try to bridge with it. It reports the
  mismatch and offers the update path only.
  - **Future, not this project:** the P4 image bundles the C6's, and the P4 reflashes the C6 at
    boot when their versions differ. It is the user's preferred long-term strategy. Tracked in
    issue #215. Nothing here may block it.
- **The handshake recovers from a missed READY pulse,** as the spike's `ready_stale_recover` fix
  does. It also recovers from the other side restarting mid-stream, without the host having to
  intervene beyond the `Ready` described above.

### Buffers and right-sizing

- **Every queue that can drop has a compile-time size and a high-water mark.** That means at
  least:
  - the C6's received-frame queue (radio → SPI)
  - the C6's command queue (SPI → radio)
  - the P4's queue toward the host (the spike used a 300 KB ring in internal SRAM)
  - the P4's command queue from the host
  - the GPS buffers

  The high-water marks, and the drop counts where a queue can drop, go into `Status` (or the C6
  section of it). They are what make right-sizing possible: a bench run under burst load shows
  how much of each buffer was used.
- **esp-radio's own ESP-NOW receive queue is fixed at 10 frames** (`RECEIVE_QUEUE_SIZE`, private,
  `esp-radio-1.0.0-beta.1/src/esp_now/mod.rs:33`). It silently drops the oldest frame when full.
  It cannot be configured without patching esp-radio. The C6 drains it promptly into its own,
  configurable queue. The spike's burst run showed a 32-deep queue of its own had no drops at
  40 MHz, where an 8-deep one dropped at 5–20 MHz.
- **Sizes are set in one place per firmware**, with documented defaults. Changing one for a test
  is a one-line or one-variable change, not a code edit spread across files.
- The single-chip bridge's outbox depths are constants inside `wartui-proto`'s `Outbox`
  (`BULK_DEPTH` 24, `PRIORITY_DEPTH` 8, `crates/wartui-proto/src/outbox/mod.rs`). If the P4 path
  reuses `Outbox`, those need to become parameters without changing the single-chip bridge's
  sizes.

### How we know it worked

- **Step 1:** the same burst load runs against the single-chip bridge and then the P4 bridge, and
  the batch-sequence gaps are compared. The burst-test node build is on the local branch
  `bench/burst-load-1.2` (commits ca5a6de, f67ec33), and the spike's method of counting gaps from
  `batch.seq` is in the findings doc. Then a normal `wartui run` session with the bench fleet
  must look the same as on a single-chip bridge. Both checks run on the POE-ETH and the WIFI6.
- **Step 2:** fixes and the receiver's configuration round-trip through `wartui run`, as on the
  T-Dongle-C5.
- **Step 3:** both chips update over the high-speed link with no other cable carrying data, and
  a bad image rolls back as the node's does.

### Settled after the builder check (designer's calls)

- **The first wired flash of each chip stays manual.** The repo gets a README procedure, and
  builds produce both images. `wartui flash-bridge` does not drive the CH343, the BOOT button or
  the header. The user expects the first flash to be more of a hassle.
- **The shared radio code never waits on a send while received frames pile up.** The C6 cannot
  busy-wait up to 35 ms on an unacknowledged send, because esp-radio's 10-frame queue overflows
  under burst in that time. The shared code uses `send_async`, as the spike did, and keeps
  draining the receive queue while a send is in flight. The single-chip bridge adopts the same
  behavior. That is a deliberate change to it, and probably a fix. Step 1's benchmark must show
  the single-chip bridge no worse after the change than before it, so measure it before and
  after.

## Left to the builder

- The crate layout for the two new firmwares (a new crate per chip, or the C6 side as a
  configuration of `firmware/bridge`), provided the radio logic exists once.
- What the SPI link carries: existing link messages relayed, or a narrower message set of its
  own. Also the transfer size, or fragmentation, for 1,470-byte sends.
- Whether the P4 clocks transfers continuously (the spike ran about 1,760/s of filler) or only
  on demand, within the latency budget above.
- How buffer sizes are set (constants in one module, `option_env!` with defaults, esp-config
  keys) and the default values, chosen from bench high-water marks.
- How the P4's executor is arranged. The spike's async SPI DMA, edge-wait READY and `join4`
  worked.
- The partition layouts and the OTA transport over the link and the SPI link in step 3, reusing
  `wartui_proto::ota` where it fits.
- Whether the spare SDIO lines (GPIO16/17 ↔ IO22/23) get used for anything.

## Waiting: async over polling

**A general rule from the user.** Wherever a driver offers an async or select-style wait, use it
instead of polling. That covers SPI DMA completion, the READY edge, USB, I2C, the radio's receive
and send results, and the queues between tasks. Combine waits with `select`/`join`, as the spike's
P4 did (`wait_for_rising_edge` under a timeout, `transfer_buffers` with `wait_for_done().await`).
Poll only where no async form exists, and say so in a comment. The spike's C6 checked
`SpiDmaTransfer::is_done()` in a loop, and that is the kind of spot to revisit first.

## Repo rules that apply

`AGENTS.md` governs. In particular:

- The main session plans and the `rust-developer` agent writes code.
- Commit and push only when asked.
- Never upload to WDGWars without asking the user directly first, every time.
- Board references use the last two MAC octets. The P4 is `8B:65` and its C6 is `D8:1C`, and
  `bench-boards` memory has the bench routine for flashing both.
