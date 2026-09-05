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
