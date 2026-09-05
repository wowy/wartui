# wartui bridge firmware

The dongle. It parks an ESP32 radio on the mesh's ESP-NOW channel and forwards
every frame it hears up a USB link to `wartui` on the host.

It knows about COBS framing and it knows about `esp-radio`. It does not know
what an `ENOW` frame is, what a heartbeat means, or how channels are assigned —
all of that lives on the host, where it is unit-testable and a fix costs a
`cargo run` rather than a reflash.

**This build is receive-only.** `SendEspNow` and the peer commands are answered
and refused, so the bridge cannot transmit and therefore cannot disturb a live
fleet. Transmit arrives in Phase 4.

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

## The two things that are easy to get wrong

**Never block on the USB endpoint.** `UsbSerialJtag` stops accepting bytes as
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

`esp-radio 1.0.0-beta.0` pins `esp-hal ~1.1`, which in turn fixes `esp-rtos` at
0.3 and `esp-alloc` at 0.10 — a newer `esp-hal` will not resolve. `esp-generate`
is a version behind this set; its scaffolding (`build.rs`, `.cargo/config.toml`)
is what was taken from it, not its dependency list.
