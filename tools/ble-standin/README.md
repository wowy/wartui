# ble-standin

Spike only, never merged. A PC stands in for a phone host on the P4 bridge's Bluetooth link,
so the link can be measured before any phone app exists.

The C6 advertises `wartui XX:YY`, the last two octets of its MAC, with one GATT service
(`wartui_proto::spike::SERVICE_UUID`):

| Characteristic | UUID | Carries |
|---|---|---|
| Up | `f2410002-d870-4eb6-a5a0-ed645732a877` | notify: the `BridgeToHost` frame stream a USB host reads, split anywhere |
| Down | `f2410003-d870-4eb6-a5a0-ed645732a877` | write without response: `HostToBridge` frames |

It runs on BlueZ through the first adapter, and is its own workspace. libdbus is vendored, so
the build needs a C compiler and no `dbus-devel`.

```sh
cd tools/ble-standin
cargo build --release
./target/release/ble-standin scan --secs 5
./target/release/ble-standin run --name "wartui 96:C4" --profile fleet20 --secs 60 --rtt 100
./target/release/ble-standin run --name "wartui 96:C4" --profile burst --count 2000 \
    --reconnect 2 --start-scan --json run.json
./target/release/ble-standin run --port /dev/serial/by-id/usb-1a86_USB_Single_Serial_XXXX-if00 \
    --profile burst --count 500 --secs 60 --start-scan --json run.json
./target/release/ble-standin frame get-status        # hex for nRF Connect's Down write
./target/release/ble-standin frame spike-load --profile saturate
```

`run --port` runs the same checks with a USB host, at UART0's rate. It never moves DTR or RTS,
sends an untimed `GetStatus` after 5 s without a host frame to hold the P4's host gate open, and
takes no `--reconnect`.

`run` checks each attachment and prints every 2 s:

- the first record is `Ready`, and the first `SpikeLoad` is seq 0: no backlog from before it
  attached;
- `SpikeLoad` seq gaps, listed by range, and body CRC failures;
- frames that failed to decode;
- the P4's latest `SpikeStats`, and the stand-in's own throughput over 2 s and the attachment;
- with `--start-scan`, heartbeats per slot, with missed beats inferred from the P4's timestamps,
  and sighting batches per slot, with batches lost read from their `seq` the way `wartui-core`
  reads them (`src/batches.rs`).

Only the first attachment sends `--profile` and `--start-scan`. The P4 keeps both across a
reconnect, so a later attachment tests that the queue starts empty. The last one sends
`SpikeLoad off` and `StopScan`. `--rtt N` times N `GetStatus` round trips, one at a time, beside
the load. The run ends by setting the seq gaps against `SpikeStats.evicted` and `refused`.

The first attachment asks for a `Status` before anything else, and the last asks for one when its
time is up, or on Ctrl-C, and waits up to 10 s for it before stopping the load and the scan. The run
prints each node bus, radio link, drop and queue counter as `name first -> last (+delta)`, and the
last `Fleet` report's members. `--json` carries all of it.

`run` also prints each event as it happens, on a line starting `# event `, and `--json` lists them
under `events`. Each carries the local time, the attachment and seconds since it started, and the
P4's `rx_us` where the record has one:

| Event | When |
|---|---|
| heartbeat spacing | a slot's beats spaced past 1.5 assigned beats, with ~missed (inferred from timing) |
| sightings seq | a slot's batch `seq` skipped, with the batches lost |
| spike stats | a `radio_link` error, `evicted`, `refused` or `ble.dropped_*` counter rose since the last `SpikeStats` |
| load seq | `SpikeLoad` seqs that never arrived |
| fleet | each `Fleet` report: mode, members and their epochs |
| log, error | each bridge `Log` or `Error` |
