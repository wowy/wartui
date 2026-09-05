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

If nothing turns up on channel 6 for 20 seconds it sweeps channels 1-13 and
reports where the traffic actually is.

## What you are checking

- **Frames arrive at all.** Nodes drop to 2 dBm while wardriving on the
  `feat/node-interference-mitigation` branch, so start with the sniffer close to
  a node and walk it away to find the usable range. This is the main open risk
  in the whole project.
- **`len=212`** on every `ENOW` frame. Anything else means the firmware's struct
  layout has moved and `wartui-proto` needs revisiting.
- **`type=1`** means that node has encryption enabled. wartui runs plaintext
  only; turn encryption off in the node's web UI.

## Feeding the captures back

Append the `capture_*` lines to
`crates/wartui-proto/tests/golden_vectors.txt` and run `cargo test -p
wartui-proto`. Every captured frame is then decoded and re-encoded, so a real
node's bytes become a permanent regression test. Lines starting with `#` are
ignored, so the whole log can be pasted in unedited.
