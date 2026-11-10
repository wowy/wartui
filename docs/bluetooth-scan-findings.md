# Bluetooth scan — a radio asleep hears nothing

Measured on 2026-11-09. Issue #294 was the starting point: a node given the Bluetooth scan kept
heartbeating, its counter advancing and its epoch adopted, and reported zero sightings with
`ble_refused` at 0. It reproduced with no bus attached, which rules out the node bus and the
carrier. The question was what stops the scan hearing anything.

## The setup

One C5 (`F1:34`) on USB, with no bridge and no node bus. The node tree was at `9e913b4` plus a bench
patch that ran 20 back-to-back 500 ms sweeps at boot and printed what crossed the HCI boundary:
each command's status, the advertising-report events read and the distinct advertisers per sweep.
That patch is not in the tree. Repeating the measurement means writing it again on top of
`Scanner`.

Each arm was a fresh flash and boot and changed one thing. Every HCI command returned status 0x00
in every arm.

## Findings

| Arm | What changed from the shipped build | Advertising events per sweep | Distinct advertisers per sweep, mean (min–max) |
| --- | --- | --- | --- |
| A0 | nothing (run three times) | 0 | 0 |
| A1 | promiscuous on, channel 6, promiscuous off before every sweep (what the node did before every scan until 5e8d5bd) | 0 | 0 |
| A2 | that park once at boot | 0 | 0 |
| A3 | promiscuous on then off before every sweep | 0 | 0 |
| A5 | `set_power_saving(PowerSaveMode::Minimum)` | 0 | 0 |
| A6 | no 2 dBm transmit power cap | 0 | 0 |
| A7 | ESP-NOW interface created (`controller.esp_now()`), never used | 131.0 | 56.5 (52–64) |
| A8 | A7, no power cap, park on 6 before every sweep | 125.9 | 55.7 (50–64) |
| A9 | A8 plus an ESP-NOW broadcast before every sweep | 124.2 | 54.5 (48–61) |
| A10 | `ControllerConfig::with_sta_disconnected_pm(false)`, nothing else | 139.9 | 56.4 (51–61) |
| A11 | default config, parked on 6 with promiscuous mode left on through the sweeps | 137.0 | 55.2 (50–60) |
| A12 | A10, Wi-Fi parked once on channel 36 (5 GHz) | 137.3 | 54.3 (47–59) |

## Reading

Three unrelated ways of keeping Wi-Fi awake each restore the scan: an ESP-NOW interface (A7),
disconnected power management off (A10) and promiscuous mode left on (A11). Nothing else does.
Transmit power, parking and Wi-Fi transmits change nothing. Where Wi-Fi rests, 2.4 or 5 GHz, does
not matter either (A12).

`esp-radio 1.0.0-beta.1`'s `ControllerConfig` defaults `sta_disconnected_pm` to true, which lets a
station that is not associated sleep. The node never associates. With promiscuous mode off and no
ESP-NOW interface, nothing holds Wi-Fi awake, the radio the two share sleeps, and the Bluetooth
controller acknowledges every command and hears nothing. Until 5e8d5bd took the node off ESP-NOW,
ESP-NOW's init kept the radio awake, by accident.

## The fix

The node builds its `ControllerConfig` with `with_sta_disconnected_pm(false)`
(`firmware/node/src/main.rs`). The scanner returns a refused command as an error, and the node's
console prints `ble: no advertising events in N scans` on the first silent scan and every tenth
after it, so a silent scan is visible without a bench image. `firmware/node/README.md` § "Bench
check" is the check to run on a fleet before a change to the radio start-up lands.

## What was not measured

- What `sta_disconnected_pm = false` costs an idle node in current or die temperature.
- Why the controller's own use of the radio does not keep it awake. The inference only:
  `esp-radio`'s C5 Bluetooth coexistence hooks, `coex_schm_status_bit_set` and `_clear`, are empty
  stubs in `src/ble/npl.rs`.
