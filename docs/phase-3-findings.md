# Phase 3 — what the bridge wedge actually is

Measured on 2026-09-08 on Linux (Fedora, kernel 7.1.13), against the ESP32-C6
bridge `9D:24` on `/dev/ttyACM0`. No nodes on the bench: everything here is
about the USB link, and the fleet is not part of it.

The question was the one `firmware/bridge/README.md` had been carrying since
Phase 2 — *"a bridge that has stopped answering while still enumerating as a USB
device … what causes it is not yet known"* — and the answer turned out to be
almost the opposite of what the code's own comments assumed.

## The wedge is transmit-only. The bridge was reading the whole time

The board presented exactly the documented symptom: enumerated, `/dev/ttyACM0`
present, the port opening cleanly, and `wartui` reporting

```
link down: nothing on /dev/ttyACM0 answered the link protocol; it is not a bridge
```

That message comes from `serial.rs:296-299` and fires on one condition only —
twelve `Identify` frames sent over six seconds with **no decodable frame** back.
Four observations, in the order they were taken, each of which had to come
before anything reset the board:

1. Nothing held the tty (`fuser`, `/proc/*/fd`), so this was not the
   port-holding wedge that `serial.rs:43-53` describes.
2. **Zero bytes in fifteen seconds** read passively, with no writes at all.
3. Hand-encoded `Identify` and `GetStatus` frames — byte-identical to
   `encode_frame`'s own output, checked against the crate — drew no answer.
4. A hand-encoded `Reset` frame **rebooted it immediately**, and the ROM banner
   came back down the same file descriptor:

   ```
   rst:0x3 (LP_SW_HPSYS),boot:0x1e (SPI_FAST_FLASH_BOOT)
   ```

`LP_SW_HPSYS` is a software reset — this firmware's own
`esp_hal::system::software_reset()` at `firmware/bridge/src/main.rs`, reached
through `HostToBridge::Reset`. So at the moment the bridge appeared dead it was
reading the USB OUT endpoint, framing with `FrameAccumulator`, passing the CRC
and the version byte, deserialising, and dispatching a command. **Every part of
the receive path was alive. Only the transmit direction was gone.** It came back
healthy — `Ready` in 3 ms, channel 6 — and `espflash` was never needed.

This falsifies what the code assumed. `crates/wartui/src/main.rs:176-183` and
`firmware/bridge/README.md` both describe the wedge as a bridge that "stopped
answering", and the reasoning around `SendWaiter` (`main.rs:405-406`) makes an
unbounded spin in the main loop the obvious suspect. It was not that. A hung
main loop cannot execute a `Reset`.

## Why the transmit side cannot recover on its own

One flag decides whether the bridge can put a byte on the wire. From the C6's
own register description (`esp32c6` PAC, `usb_device/ep1_conf.rs`, wording from
the TRM):

> `SERIAL_IN_EP_DATA_FREE` — "1'b1: Indicate UART Tx FIFO is not full and can
> write data into in. **After writing USB_DEVICE_WR_DONE, this bit would be 0
> until data in UART Tx FIFO is read by USB Host.**"

`WR_DONE` drives it to zero and *only the host reading the FIFO* brings it back.
Nothing the firmware can do clears it. So if that read never lands — a host that
stops fetching at the wrong moment, a `WR_DONE` against a FIFO the host has just
emptied — the endpoint is closed for the rest of that power cycle, silently,
while the receive endpoint carries on. Which silicon path gets there was not
pinned down; doing so needs a logic analyser on the USB lines rather than more
reading, and it does not change the response.

The outbox is *not* the cause and was left alone. `Outbox::pump` only calls
`sink.flush()` when `unflushed` is set, and that is set only when a byte was
actually staged, so it never pulses `WR_DONE` against an empty FIFO of its own
accord.

## What was built, and what it does on the bench

Since the state is unambiguous locally, the bridge now detects it. Three facts
have to hold together (`Bridge::note_tx`): something is queued, a host frame
decoded recently, and the endpoint has refused every byte for three seconds since
both of those first became true. The host-present half is what keeps a bridge
sitting on a bench with nothing attached quiet for ever, which is the case that
must never be reset; the section below is about the clock.

Verified with a build whose `UsbSink::write_byte` returns `WouldBlock` from a
command onwards — a stand-in for the endpoint that has stopped draining — while
a host polled `GetStatus` twice a second, as the real one does:

```
  + 0.00s  ... 12 bytes   (healthy: a Status reply every 500 ms)
  + 1.50s   12 bytes
  + 2.00s   >>> transmit path wedged
            (2.5 s of nothing)
  + 4.50s   ROM BANNER  rst:0x3 (LP_SW_HPSYS)
  + 5.10s   12 bytes   (Status replies resume, every 500 ms, indefinitely)
```

The reset lands 3.0 s after the last byte that moved, which is
`TX_STALL_TIMEOUT` exactly, and inside the six seconds the host waits before it
gives up. The operator sees a bridge that hesitated rather than one that died.

## A software reset keeps the port; an espflash reset does not

Worth knowing because it decides which recovery costs you a device path.
`esp_hal::system::software_reset()` — what `HostToBridge::Reset`, the panic
handler and the stall detector all use — does **not** re-enumerate the USB
device: the same open file descriptor kept reading straight through the reboot,
and `/dev/ttyACM0` stayed `/dev/ttyACM0`. `espflash`'s DTR/RTS reset does
re-enumerate: the board came back as `/dev/ttyACM1`, and later as `ttyACM0`
again, which is enough to make a `--port` in a script wrong. That is one more
reason `wartui reset` is the first thing to reach for.

## Ruled out

- **USB autosuspend.** `power/control` was `on`, `runtime_status` `active`,
  `runtime_suspended_time` 0.
- **DTR/RTS.** It looks damning and is not. The ESP32 USB Serial/JTAG reproduces
  the classic two-transistor auto-reset circuit, where EN drops only on
  `(RTS=1, DTR=0)` and IO0 only on `(DTR=1, RTS=0)`. Linux `cdc_acm` raises and
  lowers both in a single `SET_CONTROL_LINE_STATE`, so it lands atomically on
  `(1,1)` at open and `(0,0)` at close — both no-ops. wartui never touches the
  lines itself, and `serialport` is left at `dtr_on_open: None`.
- **A stale process holding the port.** Nothing held it. That is a real and
  separate failure (`serial.rs:43-53`), and it announces itself differently —
  `could not open …: Device or resource busy`.

## The stall clock runs from the contradiction, not from the last byte

The first version of `tx_stalled` compared `last_tx.elapsed()` against the
timeout — how long since a byte last moved. It passes the obvious tests and it
is wrong, which the bench showed twice.

A bridge left powered beside a talkative fleet fills its rings and its endpoint
with nobody reading. By the time an operator finally attaches, the last byte
moved *hours* ago. Every one of those hours counts against a transmit path with
nothing whatever wrong with it, so the bridge would reset itself the moment
`wartui` said hello — every time, on a healthy board. The same arithmetic
punishes any long blocking call inside one iteration: a deliberately hung loop
that recovered was reset on the next host frame, with a perfectly good endpoint.

What is actually being measured is how long the endpoint has refused bytes
*while somebody was waiting for them*, and that clock can only start once both
halves are true. `Bridge::note_tx` keeps a `stall_since: Option<Instant>`,
cleared whenever a byte moves or whenever nothing is waiting. It also needs
`HOST_PRESENT_WINDOW` (10 s) to be longer than the host's own five-second
`status_interval`: set it shorter and an established capture reads as an absent
host for two seconds in every five, which keeps resetting the stall clock and
would never notice a real wedge.

Measured on the fixed build, with the endpoint wedged on command:

| scenario | result |
| --- | --- |
| wedged, host polling throughout | reset at +3.00 s |
| wedged, 10 s of total silence, then the host returns | nothing while silent; reset at +13.01 s |
| main loop blocked 8 s — *not* a wedge — then the host returns | **no reset** |

The middle row is the guard that keeps a bench bridge with no host quiet for
ever. The last row is the false positive the first version had.

## The RTC watchdog does not work here, and was removed

A watchdog was the obvious companion fix: `esp_hal::init` disables every
watchdog on the chip (`esp-hal-1.1.2/src/lib.rs:751-761`), so a genuine hang has
nothing behind it. It was built for both firmwares and it does not fire. Tested
with a build that spins in `loop {}` on command — bounded to fifteen seconds
after the first attempt left a board that had to be physically replugged.

Reading the registers back through a `Log` frame settles most of it. At boot,
before any of our configuration, `WDTCONFIG0` is `0x00000000` — `esp_hal::init`
really has turned it off. After `Rwdt::enable()`, `set_stage_action` and
`set_timeout(5 s)`:

```
before    cfg0=00000000  cfg1=000956a0  lp_rst_en=00000000
after     cfg0=b007e214  cfg1=00056fe0  lp_rst_en=40000000
identify  cfg0=b007e214  cfg1=00056fe0  lp_rst_en=40000000   (seconds later)
```

`cfg0` decodes to `WDT_EN` set and `WDT_STG0 = 3` (`ResetCore`), `cfg1` to about
2.6 s of RTC ticks. So the configuration lands, is correct, and **stays** — the
suspicion that something re-disables it is dead, and neither `esp-rtos-0.3.0`
nor `esp-radio-1.0.0-beta.0` mentions the RWDT anywhere, so nothing feeds it
either.

Two enables that esp-hal never touches turned up on the way:

- `LP_CLKRST.LP_RST_EN.WDT_RESET_EN` reads 0 by default. Setting it works —
  `lp_rst_en` goes from `00000000` to `40000000` and stays. It made no
  difference on its own.
- `LP_WDT.WDTCONFIG0.WDT_PROCPU_RESET_EN` (bit 11) also reads 0, and unlike the
  first, **writing it does not take** — `cfg0` keeps bit 11 clear afterwards,
  with the write-protect key lifted around the write.

Also tried: configuring immediately after `esp_hal::init`; configuring after
`esp_rtos::start` and the radio; `ResetSystem` instead of `ResetCore` (esp-hal's
own default — `cfg0` becomes `c007e214`, confirming the field takes); and
`enable()` called first, which is a genuine bug in the original attempt worth
recording on its own — `Rwdt::enable` writes `wdtconfig0` wholesale, stage
action included (`esp-hal-1.1.2/src/rtc_cntl/mod.rs:566-591`), so any
`set_stage_action` before it is silently undone. None of them reset a hung loop.

Where it stops is therefore known to about one register: the timer is enabled,
correctly configured, running, unfed, and its reset never reaches the CPU, with
`WDT_PROCPU_RESET_EN` the one enable that will not be written. Fixing that needs
more than an afternoon against esp-hal, and **a safety net that provably does
not catch anything is worse than none**, since the next person to read the code
will trust it. So it is not in the tree. The finding that matters is the one
above it: the failure this project has actually seen was never a hang, and a
watchdog would not have caught it either.

One more thing came out of that test, and it is the strongest argument for ever
making the watchdog work. A C6 spinning in a tight loop **cannot be recovered by
`espflash`**: `board-info` and `flash` both failed with "Failed to connect to the
device" on every attempt, on a board still enumerating with the right serial
number. Only a physical replug brought it back — which is exactly the symptom
`docs/phase-1-findings.md:492-513` chased and blamed on a host USB port. It is
worth knowing that a hung CPU produces it too.
