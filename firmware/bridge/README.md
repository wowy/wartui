# wartui bridge firmware

The dongle. It parks an ESP32 radio on the fleet's ESP-NOW channel and forwards
every frame it hears up a USB link to `wartui` on the host.

It knows about COBS framing and it knows about `esp-radio`. It does not know what
an air frame is, what a heartbeat means, or how channels are assigned — all of
that lives on the host, where it is unit-testable and a fix costs a `cargo run`
rather than a reflash.

It transmits, and answers with the **transmit-callback** status rather than the
enqueue result. Unicast ESP-NOW is acknowledged by the receiver's own MAC
hardware, so `AckOk` means a node really has the frame. The callback comes back in
2–4 ms against the node's 300 ms admin window, and in 28–35 ms even in the
pessimistic case — an *unacknowledged* send, where it fires only once the radio has
exhausted its retry chain
([`docs/phase-4-findings.md`](../../docs/phase-4-findings.md)). A dumb bridge has
two orders of magnitude in hand.

Peers are added on demand (`ensure_peer`) and never removed as a side effect of
sending. The radio's table holds twenty entries, one of which `esp-radio` spends
on the broadcast peer at init; since this bridge only ever *receives* broadcasts
and ESP-NOW delivers a received frame whether or not its sender is a peer, that
slot is given up to make room for a twentieth node.

A `PeerTableFull` after that is genuine, but it does not have to mean a fleet above
twenty. Nothing removes a peer, so a long session accumulates slots for nodes that
have since gone and can fill the table with a handful still on the air. The table
starts empty on every boot, which is why the host clears the refusal on each
bridge announcement and why `wartui reset` is the cheap thing to try first.

## Building and flashing

```sh
cargo run --release --features esp32c6    # or --features esp32c5
cargo +esp run --release --features esp32s3 --target xtensa-esp32s3-none-elf
```

Exactly one chip feature is required. The first line is the whole story for the
RISC-V parts: stable, one flag, and `rust-toolchain.toml` pins it.

The S3 is Xtensa and needs `espup` — `cargo install espup && espup install`, then
`. ~/export-esp.sh` in any shell that will link one, which is what puts
`xtensa-esp32s3-elf-gcc` on `PATH`. `+esp` overrides the pin; `--target` is needed
because `[build] target` in `.cargo/config.toml` names the RISC-V triple.
Everything else the S3 wants — its runner, `build-std`, the linker flags — is in
that file and is inert on stable, so the C5 and C6 commands are unchanged. It
needs the second toolchain because `xtensa-lx-rt` still requires
`#![feature(asm_experimental_arch)]` and no channel ships a prebuilt `core` for
the target; that is structural rather than a version lag.

An S3 is a bridge and never a node: it has no 5 GHz radio, which costs a board
that parks on the control channel nothing, and its devkits are cheap.

The cargo runner is `espflash flash --monitor` with no `--chip`, so espflash
detects the part. `--monitor` renders the framed link bytes as text and looks like
line noise; use `wartui sniff` to read them.

## Checking it works

```sh
wartui status     # channel, counters, uptime — proves the link runs both ways
wartui sniff      # every frame, decoded
```

`wartui status` is the first thing to reach for when frames are not arriving: a
bridge that answers is listening, and `received 0 frames` then points at the nodes
rather than at the link. A bridge that does *not* answer gets a named diagnosis
after five seconds rather than a wait, because the three things it can be need
three different actions and produce an identical symptom — the wrong firmware on
the board, another program holding the port, or a bridge that has stopped
answering while still enumerating as a USB device.

That last one is not a hung bridge. It is a bridge whose USB *transmit* endpoint
has stopped draining while its receive endpoint carries on decoding and executing
everything you send it. One flag decides it: `SERIAL_IN_EP_DATA_FREE`, which
`WR_DONE` clears and which, per the TRM, comes back only when the USB host reads
the FIFO. If that read never lands, nothing on the device can clear it.

So the firmware notices and reboots itself, within about three seconds.
`wartui_proto::stall::StallWatch` times how long the endpoint has refused bytes
*while somebody was waiting for them*, and needs the host to have spoken both
recently and since the stall began — which is what keeps a bridge on a bench with
no host attached quiet for ever, and keeps it from resetting every time an
operator closes a window. The reset *is* the message, since every way of
explaining would go out through the path that is broken; the `Ready` behind it
says `TxStalled`. The rule lives in `wartui-proto` rather than here so that it is
tested in microseconds instead of on a bench.

Reach for `wartui reset --port <path>` before `espflash`: the receive path is
alive in this state, so the bridge reboots on being asked, and a software reset
keeps the device path where an `espflash` reset re-enumerates the board and can
move `ttyACM0` to `ttyACM1` underneath a script.
`espflash reset --port <path>` is the fallback for when even that goes unanswered,
which means the firmware really is hung;
[`docs/phase-3-findings.md`](../../docs/phase-3-findings.md) has why there is no
watchdog behind that case.

## The two things that are easy to get wrong

**Never block on the USB endpoint.** `UsbSerialJtag` stops accepting bytes as soon
as its FIFO fills, and nothing drains that FIFO unless a host is reading, so a
blocking write from the receive path would stall the radio for as long as the TUI
is wedged or the cable is out. Everything outbound goes through the rings in
`wartui_proto::outbox`, which are drained by whatever the FIFO will take and never
waited on: both evict oldest-first under pressure and count it into
`Status.dropped_tx`, and priority frames (`Ready`, `SendResult`, `Status`,
`Error`) are served ahead of `Rx` and `Log` rather than given an unbounded queue.
That module is in `wartui-proto` so those rules are unit-testable on the host.

The one deliberate exception is the wait for a transmit callback in `transmit`,
which is milliseconds against a 300 ms window and is what makes `AckOk` mean
anything. `esp-radio`'s `SendWaiter` busy-waits in `Drop` as well as in `wait`, so
there is no way to start a send and walk away.

**Never link `esp-println` with `jtag-serial`.** It writes to the same USB endpoint
and would interleave into the COBS stream; diagnostics go out as `Log` frames
instead. For the same reason the panic handler resets rather than printing — the
message is lost, but a bridge that panics repeatedly says so by re-announcing
itself, which the host is already listening for.

## Dependency versions

`esp-radio 1.0.0-beta.0` requires `esp-hal = "~1.1.0"`, and that one requirement
holds the whole family back: `esp-hal 1.2.0`, `esp-rtos 0.4`, `esp-alloc 0.11` and
`esp-sync 0.3` are all published and none of them will resolve. `cargo update`
reports them as available and moves nothing, and will keep doing that until
`esp-radio` publishes again. Both firmwares are held at the same set by the same
dependency, which is worth keeping true — they share `wartui-proto`.

The pin is no longer free. The ESP32-C5 fix for a software reset that leaves the
board unbootable until it loses power is upstream from `esp-hal` 1.2.0-rc.0
(esp-rs/esp-hal#5703), and being unable to take it is why `reboot()` writes that
register out by hand on the C5. Cargo cannot be talked round it either:
`SoftwareInterruptControl` is gone in 1.2.1, so `esp-rtos 0.3.0` and this
firmware's `esp_rtos::start` would both stop compiling against a version faked
into range. Issue #16 has the shape of the real upgrade — `[patch.crates-io]`
across the family at one monorepo rev — and what to delete when it lands.

`esp-generate` is a version behind this set; its scaffolding (`build.rs`,
`.cargo/config.toml`) is what was taken from it, not its dependency list. One
piece of that does not survive the S3: `build.rs`'s `linker_be_nice` registers
itself with `--error-handling-script`, an LLD option, and the S3 links with
`xtensa-esp32s3-elf-gcc` — left on, the hook whose job is to explain link errors
becomes the link error on every build. It is skipped for `target_arch = "xtensa"`
rather than rewritten, so the function stays diffable against upstream.
