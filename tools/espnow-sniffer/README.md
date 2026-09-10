# Phase 0 — ESP-NOW sniffer

Passive listener that proves your nodes are audible and captures real frames to
test the Rust codec against. It registers no peers and transmits nothing, so it
cannot disturb a running fleet.

## Run it

```sh
cd tools/espnow-sniffer
pio run -e c6 -t upload          # or -e c5 / -e s3
pio device monitor -b 115200 | tee /tmp/capture.txt
```

Power up a node with `use_encryption` off. Within a few seconds you should see
`type=3` (heartbeat) and `type=4` (observation) frames.

## If you see no output at all

These devkits have **two USB sockets** and they do different things. Flashing
works over either, so a silent monitor does not mean a broken board.

- **Native USB Serial/JTAG** — shows up as `/dev/cu.usbmodem*`. This is what
  `platformio.ini` is configured for.
- **UART bridge** (CP2102N or similar) — shows up as `/dev/cu.usbserial-*` or
  `/dev/cu.SLAB_USBtoUART`.

Arduino's `Serial` goes to UART0 by default, because `HardwareSerial.h` leaves
`ARDUINO_USB_CDC_ON_BOOT` at 0 and then does `#define Serial Serial0`. That
sends every print out the UART socket. `platformio.ini` overrides it with
`-DARDUINO_USB_MODE=1 -DARDUINO_USB_CDC_ON_BOOT=1` so `Serial` is the native
USB CDC instead. **If you are plugged into the UART socket, drop that second
flag and monitor `/dev/cu.usbserial-*`.**

Check which you have with `pio device list`, or `ls /dev/cu.*`.

## Reading the output

The sniffer listens two ways at once, and comparing the counters in the
`# alive:` line tells you which situation you are in:

| `esp-now` | `promiscuous` | Meaning |
| --- | --- | --- |
| > 0 | > 0 | Plaintext fleet. This is what wartui needs. |
| 0 | > 0 | Traffic is there but **unicast**, so encryption is ON. Turn it off in each node's web UI. |
| 0 | 0 | Nothing on this channel — wrong channel, out of range, or nothing transmitting. |

The distinction matters because the ESP-NOW receive callback — the mechanism
the wartui bridge itself relies on — only ever fires for frames addressed to
broadcast or to us. An encrypted fleet unicasts node to core, so the core works
perfectly while a sniffer sees absolute silence. Promiscuous mode sees those
frames anyway, which is what makes the two cases distinguishable.

The sniffer stays parked on channel 6 and never hops. That is `CONTROL_CHANNEL`
in `wartui-proto`, and the vendor firmware hard-codes the same value
(`src/WiFiOps.cpp:15`), so there is nowhere else for either fleet's traffic to
be -- and leaving the channel, even briefly, risks missing the burst a node
emits in its first sweep after boot.

## What you are checking

- **Frames arrive at all.** Nodes drop to 2 dBm while wardriving, so start with
  the sniffer close to a node and walk it away to find the usable range. This is
  the main open risk in the whole project.
- **`capture_*` lines, not `vendor_*`.** A `capture_*` line is one of ours:
  `WTUI`, a wire version, a type byte. A `vendor_*` line is somebody else's
  fleet on the same channel -- worth knowing about, and nothing wartui can
  drive or be driven by.
- **`wartui v1`** on every one of ours. A different version means part of the
  fleet is running a build this host cannot read, which is what a half-finished
  reflash looks like.
- **Lengths in the tens, not 212.** A heartbeat is 13 bytes, an assignment 15,
  and a sighting 17 plus its SSID. Anything near 212 is the format this one
  replaced.

## Reading the captures

The hex is `<name> <length> <hex>`, which is the fastest way to see what a fleet
is actually saying. There is no golden-vector fixture to paste it into: the wire
format has one implementation of each end and no external authority to check it
against, so `crates/wartui-proto/tests/wire.rs` writes its vectors out by hand
and says in each one what it pins.
