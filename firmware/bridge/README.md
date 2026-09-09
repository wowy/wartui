# wartui bridge firmware

The dongle. It parks an ESP32 radio on the mesh's ESP-NOW channel and forwards
every frame it hears up a USB link to `wartui` on the host.

It knows about COBS framing and it knows about `esp-radio`. It does not know
what an `ENOW` frame is, what a heartbeat means, or how channels are assigned —
all of that lives on the host, where it is unit-testable and a fix costs a
`cargo run` rather than a reflash.

It transmits, and answers with the **transmit-callback** status rather than the
enqueue result. That distinction is the point: unicast ESP-NOW is acknowledged
by the receiver's own MAC hardware, so `AckOk` means a node really has the
frame. The vendor core clears its dirty flag from `esp_now_send`'s return value
(`src/WiFiOps.cpp:679`) and so believes every assignment it *queued* arrived.

Measured on a C6: 28-35 ms from the heartbeat that opens a node's admin window
to the transmit callback, against a 300 ms window — and that is the pessimistic
figure, taken from unacknowledged sends where the callback waits out the radio's
whole retry chain. A dumb bridge has an order of magnitude in hand.

Peers are added on demand (`ensure_peer`) and never removed as a side effect of
sending. The radio's table holds twenty entries in total, one of which
`esp-radio` spends on the broadcast peer at init; since this bridge only ever
receives broadcasts and ESP-NOW delivers a received frame whether or not its
sender is a peer, that slot is given up to make room for a twentieth node.
A refusal after that is a genuine `PeerTableFull`, and means a fleet above the
twenty nodes wartui supports.

## Building and flashing

```sh
cargo run --release --features esp32c6    # or --features esp32c5
```

Exactly one chip feature is required; `src/main.rs` rejects zero or both at
compile time. Both parts are RISC-V and build on stable — an ESP32-S3 bridge
would need the `espup` nightly fork, which is why it is not supported.

The cargo runner is `espflash flash --monitor` with no `--chip`, so espflash
detects the part. `--monitor` renders the framed link bytes as text and looks
like line noise; use `wartui sniff` to read them.

## Checking it works

```sh
wartui status     # channel, counters, uptime — proves the link runs both ways
wartui sniff      # every frame, decoded
```

`wartui status` is the first thing to reach for when frames are not arriving: a
bridge that answers is listening, and `received 0 frames` then points at the
nodes rather than at the link.

A bridge that does *not* answer gets a named diagnosis rather than a wait, after
five seconds, because the three things it can be need three different actions
and they produce an identical symptom: the wrong firmware on the board, another
program holding the port, or a bridge that has stopped answering while still
enumerating as a USB device.

That last one is no longer a mystery. It is not a hung bridge — it is a bridge
whose USB *transmit* endpoint has stopped draining while its receive endpoint
carries on. Measured in Phase 3 on a board in that state: twelve `Identify`
frames and a `GetStatus` were decoded and acted on while not one byte came back,
and a single `Reset` frame down the same wire rebooted it immediately. One flag
decides it — `SERIAL_IN_EP_DATA_FREE`, which `WR_DONE` clears and which,
per the TRM, comes back only when the USB host reads the FIFO. If that read
never lands, nothing this end can do will clear it.

So the firmware now notices and reboots itself. [`Bridge::note_tx`] times how
long the endpoint has refused bytes *while somebody was waiting for them* — from
when that contradiction started, never from the last byte written, which would
count the hours a bridge spent powered with nobody reading against a transmit
path with nothing wrong with it. It also needs the host to have spoken recently,
which is what keeps a bridge on a bench with no host attached quiet for ever. The reset *is* the message — every
way of explaining would go out through the path that is broken — and the `Ready`
behind it says `TxStalled`.

Reach for `wartui reset --port <path>` first, not `espflash`: the receive path
is alive in this state, so it reboots on being asked, and a software reset keeps
the device path where an `espflash` reset re-enumerates it and can move
`ttyACM0` to `ttyACM1` underneath a script. `espflash reset --port <path>` is
the fallback for when even that goes unanswered, which means the firmware really
is hung. `docs/phase-3-findings.md` has the measurements, including why there is
no watchdog behind that last case.

To check transmit, run `wartui run`, select a node and press `a`. Its `beat`
column should fall from seconds to a fraction of one within three sweeps — a
node heartbeats once per completed sweep, so that is the only evidence
available that an assignment was adopted rather than merely acknowledged.

## The two things that are easy to get wrong

**Never block on the USB endpoint.** (The one deliberate exception is the wait
for a transmit callback in `transmit`, which is milliseconds against a 300 ms
admin window and is what makes `AckOk` mean anything. `esp-radio`'s `SendWaiter`
busy-waits in `Drop` as well as in `wait`, so there is no way to start a send
and walk away.)

 `UsbSerialJtag` stops accepting bytes as
soon as its FIFO fills, and nothing drains that FIFO unless a host is reading.
A blocking write from the receive path would stall the radio for as long as the
TUI is wedged or the cable is out. Everything outbound goes through the rings in
`wartui_proto::outbox`, which are drained by whatever the FIFO will take and
never waited on. Both rings evict oldest-first under pressure and count it into
`Status.dropped_tx`; priority frames (`Ready`, `SendResult`, `Status`, `Error`)
are only ever served ahead of `Rx` and `Log`, never given an unbounded queue.

That module lives in `wartui-proto` rather than here so its eviction and
resynchronisation rules can be unit-tested on the host — a `no_std` binary for
`riscv32imac` cannot run a test, and this is the only real logic on this side of
the wire.

**Never link `esp-println` with `jtag-serial`.** It writes to the same USB
endpoint and would interleave into the COBS stream. Diagnostics go through the
`Log` frame instead. For the same reason the panic handler resets rather than
printing: the message is lost, but a bridge that panics repeatedly says so by
re-announcing itself, which the host is already listening for.

## Dependency versions

`esp-radio 1.0.0-beta.0` requires `esp-hal = "~1.1.0"`, which is
`>=1.1.0, <1.2.0`. `esp-hal 1.2.0` is published and will not resolve, and behind
it sit `esp-rtos 0.4` (which wants `esp-hal ~1.2.0-rc.0`), `esp-alloc 0.11` and
`esp-sync 0.3`. `cargo update` reports all of them as available and moves none
of them; it will keep doing that until `esp-radio` publishes again. Both
firmwares are held at the same set by the same dependency, which is worth
keeping true — they share `wartui-proto`.

`esp-generate` is a version behind this set; its scaffolding (`build.rs`,
`.cargo/config.toml`) is what was taken from it, not its dependency list.
