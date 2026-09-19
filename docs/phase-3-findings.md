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

Since the state is unambiguous locally, the bridge now detects it. Four facts
have to hold together (`StallWatch::note_tx`): something is queued, a host frame
decoded recently, another decoded since the stall began, and the endpoint has
refused every byte for three seconds since all of that first became true. The
host-present half is what keeps a bridge sitting on a bench with nothing attached
quiet for ever, which is the case that must never be reset; the fourth fact is
what keeps it quiet after an operator quits, which cost a section of its own
below; and the section immediately after this one is about the clock.

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
again, which is enough to make a device path in a script wrong. That is one more
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
halves are true. `StallWatch` keeps a `stall_since: Option<u64>`,
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

### Re-measured, against the finished detector

Those three rows were taken before the fourth clause existed and before the rule
moved into `wartui_proto::stall`. They were argued to be unaffected by both,
which is not the same thing as having been run again, so they were run again —
same board, two nodes transmitting, a host polling `GetStatus` twice a second,
and a build whose `UsbSink::write_byte` returns `WouldBlock` 200 ms after a
marker frame that gives the bench a timestamp for the wedge itself.

| scenario | re-measured |
| --- | --- |
| wedged, host polling throughout | ROM banner 3.10 s after the endpoint died |
| wedged, ~10 s of total silence across the wedge, then the host returns | not one byte while silent; ROM banner at +13.16 s |
| main loop blocked 8 s — *not* a wedge — then the host returns | **no reset**; traffic resumed 8.00 s later and the run went on to 40 s |

The tenths are the ROM banner rather than the detector: the reset itself lands at
`TX_STALL_TIMEOUT` to the millisecond the loop can measure, and the banner is
what the host sees. Row two is worth reading twice. The host's last frame was at
20.0 s and the endpoint died at 20.3 s, so the stall armed immediately and then
sat there for ten seconds with a host it had every reason to believe in — and
said nothing, because that host had not spoken *since*. Presence lapsed at 30.0 s,
the clock was dropped, the host came back at 30.5 s, and the reset came three
seconds after that and not three seconds after the wedge.

Row three is the false positive the first version had, and it survives the move:
during the eight seconds the loop is blocked the detector is not consulted at
all, and the pass that follows moves bytes, which is what clears the clock.

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

## Three things the first version of this fix got wrong

Found by review, before any of it merged. All three are recorded because each
one is a case the bench did not cover — the bench had no nodes on it, and every
run started from a host that was already attached. The first of them has since
been reproduced, and the measurement is under it.

**The stall detector rebooted a healthy hostless bridge for ever.** `last_host`
was seeded with the boot instant rather than with "nothing has been heard".
`note_tx` reads presence as `last_host.elapsed() < HOST_PRESENT_WINDOW`, so
every life believed a host was there for its first ten seconds. Put that bridge
on a battery beside a running fleet: `drain_radio` fills the bulk ring, the IN
endpoint is draining to nobody, `pump` moves nothing, and at 3.1 s it resets —
into another ten-second window, and another reset, for ever. The bench never saw
it because the bench had no fleet: with nothing being received the outbox stays
empty and the first of the three conditions never holds. It is now
`Option<Instant>`, and presence is something a host demonstrates rather than
something a fresh boot assumes.

That paragraph was written from reasoning. Two parked nodes make it
reproducible in ninety seconds, so it is no longer reasoning. The A/B is a
one-line change — `last_host: Some(now)` against `last_host: None`, nothing
else in the tree touched, both builds carrying the `uptime_ms` the host needs
to read the result. Same protocol for each: flash, leave the board powered
beside the two nodes with **nothing reading the port** for 90 s, then attach
and ask for status. Attaching unwedges the endpoint, but the uptime and the
reset cause in that first `Ready` are the evidence and they survive being read.

| after 90 s | `last_host: Some(now)` | `last_host: None` |
|---|---|---|
| uptime | **0 s** | **1 m 29 s** |
| last reset | software; *its transmit path had stopped draining* | over USB, by espflash |
| received | 2 frames | 178 frames |
| dropped | 0 frames | 154 frames |

The drop counters are the part worth reading twice. The fix does not work by
avoiding the condition: the healthy bridge sat in it for the whole 89 s, heard
178 frames and threw 154 of them away because nothing was draining the
endpoint — outbox non-empty, every byte refused, continuously — and stayed
quiet, because no host had ever spoken to it. The faulted build never lived
long enough to discard anything. Its two received frames are one node beat and
a bit, which is all a four-second life has time for: parked nodes beat at
`IDLE_BEAT_MS`, one second, so every life got a frame well inside its
ten-second window, and every life ended three seconds later.

**The host threw away the announcement that says the bridge rebooted.** The
transport reported only the first `Ready` per connection, on the reasoning that
"a reboot re-enumerates the USB device and so ends this connection outright" —
which is contradicted by the measurement three sections up in this same
document. A software reset keeps the file descriptor, so the post-reboot `Ready`
arrived on the same connection and was dropped as a duplicate. The whole
observability half of this work was therefore invisible in exactly the case it
was built for: the fault box stayed empty, `snapshot.bridge` kept the dead
life's info, and the engine went on believing in a peer table the reboot had
emptied.

Fixing it needed something in the frame, because nothing already there
distinguishes the two ways a second `Ready` arrives. A duplicate answer to an
`Identify` already in flight and a bridge that has just rebooted are otherwise
identical — two consecutive `wartui reset`s produce byte-identical `Ready`s, so
comparing `reset_cause` and `last_phase` does not separate them, and no time
threshold does either: the duplicate arrives milliseconds later and a reboot
completes in about 300 ms. So `Ready` now carries `uptime_ms`, and a clock that
went backwards is the test. It is unambiguous, it needs no constant, and it is
the same evidence `wartui reset` was already using from `Status`.

**`wartui reset` reported the previous life's reset cause.** Two faults, one
consequence. It kept the `Connected` it saw on the way in — which on a healthy
bridge is the life about to be replaced — and its announcement grace timer was
750 ms while the status poll was 500 ms, so a fresh status re-armed it before it
could ever fire. It now arms once, and every uptime it sees is measured against
the highest it saw before, so the old life cannot be mistaken for the new one.
`FRESH_UPTIME_MS` survives only for the wedged case, where nothing was heard
before the reset and there is nothing to compare against.

## `SIGTERM` was being caught and then dropped on the floor

Not this bug, but next to it, and worse than the gap it filled. The signal
future was built inside the `select!` arm, so a fresh `tokio::signal` stream was
created and dropped on every iteration of the view's loop. The first one
installs a process-wide handler and permanently replaces `SIGTERM`'s default
action; a stream subscribes only from the moment it exists. A signal delivered
in the gap between one being dropped and the next being built is therefore seen
by nobody, and the default that would have killed the process is gone. The
capture carries on, deaf, and the operator reaches for `kill -9` — which is the
replug this was written to prevent. The streams are now built once, before the
loop.

## A host that quits looks exactly like a host that is waiting

Found by bringing the two nodes up, which is the whole argument for having done
so: the reproduction above cleared the fault it was aimed at and immediately
exposed a second one in the half of `note_tx` that had not been touched.

`wartui status`, then eight seconds later another. The bridge reported an uptime
of four seconds and `its transmit path had stopped draining`. Again, and again —
every host command that exits leaves a bridge that reboots about three seconds
later, as long as a fleet is in earshot.

The test was `last_host.elapsed() < HOST_PRESENT_WINDOW`, read as "a host is
here and waiting on us". It is not what that measures. A host that has **quit**
goes on satisfying it for a further `HOST_PRESENT_WINDOW`, and quitting is
precisely what stops the endpoint draining: the port closes, `cdc_acm` stops
submitting reads, the FIFO stops emptying, and a fleet beating once a second
fills the rings within a second of the operator letting go. Because
`TX_STALL_TIMEOUT` (3 s) is the shorter of the two intervals, the reset always
won that race. Every session ended with a reboot nobody asked for, and the
following `wartui run` opened with `bridge rebooted itself: USB transmit had
stalled` in the fault box — a false alarm, reporting a fault that was the
operator closing a window.

A host that is genuinely waiting keeps asking: `run` polls for status every
`status_interval`, and `supervise` re-sends `Identify` twice a second until
something answers. So the detector now needs a host frame decoded *since the
stall began*, not merely one decoded lately, and that single clause separates the
two cases. It costs the real wedge nothing:

| | before | after |
|---|---|---|
| `status` ×3, eight seconds apart | 51 s → 4 s → 4 s | 6 s → 14 s → 22 s |
| last reset reported | software; transmit path stopped draining | over USB, by espflash |

None of the three measurements in "what it does on the bench" move, which is
worth saying because it is not obvious. In the `+3.00 s` row the host is polling
twice a second, so it satisfies the new clause almost immediately. In the
`+13.01 s` row the host's ten-second silence outlasts `HOST_PRESENT_WINDOW`, so
`stall_since` is already cleared before the new clause could matter, and the
clock restarts when the host returns exactly as it did before. The third row
never reaches `note_tx` at all. The clause only bites where the host goes quiet
*without* the window expiring, which is the disconnect and nothing else.

The hostless measurement in the section above predates this clause, so it was
taken again against the final code: 3 m 43 s uptime and 411 frames dropped, then
100 s later 5 m 23 s and 607 dropped. The full hundred seconds, no reset, and the
drop counter climbing throughout — the failing condition present the whole time
and correctly ignored, with the reset cause still the espflash flash that started
the life.

Which leaves `HOST_PRESENT_WINDOW` doing a different job from the one it looks
like it is doing, and it is still load-bearing: it is what stops `stall_since`
starting before there is any host to be contradicted by, so a bridge that filled
its rings over hours alone is not reset the instant one attaches. It must stay
longer than the host's `status_interval` for the same reason as before — the
stall clock has to survive the gaps between polls — and the new clause is what
stops it also meaning "for ten seconds after the last operator went home".

## The rule moved to where a test can reach it

Two defects, both in four lines of arithmetic against a clock, and neither found
by review, by reading, or by `cargo test`. The first took a bridge left on a
bench beside a talking fleet. The second took an operator quitting a session and
watching the board reset three seconds later. Both were in `firmware/bridge`,
where the only way to run them is to reflash a board and wait.

That is the same argument `wartui_proto::outbox`'s module docs already make — a
`no_std` binary built for `riscv32imac` cannot run a test — so the rule now lives
next to it as `wartui_proto::stall::StallWatch`, keeping millisecond timestamps
instead of `esp_hal::Instant`s. The firmware feeds it one observation per pass of
the loop and acts on the answer; it decides nothing itself.

Eight tests, and each of the four clauses is load-bearing under mutation:

| break this | and this fails |
| --- | --- |
| `at > since` becomes `>=` | `a_host_that_quits_is_not_a_host_that_is_waiting` |
| drop the host-present clause | `the_clock_starts_when_the_host_arrives_and_not_when_the_rings_filled` |
| drop the queued clause | `an_empty_outbox_is_not_a_stall` |
| ignore the timeout | `a_wedged_endpoint_with_the_host_still_asking_is_reset_at_the_timeout` |
| arm the clock on every pass instead of the first | `the_clock_starts_when_the_host_arrives_and_not_when_the_rings_filled` |

The first two rows are the two defects. Both now cost microseconds to catch
rather than a reflash, two nodes and ninety seconds.

The strictness of `at > since` is the one that looks like a typo and is not. A
host whose last frame lands in the same millisecond the stall arms has not asked
for anything *since*, and `>=` there restores the disconnect bug exactly.

The refactored firmware was then put back on the same board and left alone with
the two nodes for a hundred seconds with nothing reading the port: uptime 25 s
and 21 frames dropped, then 2 m 5 s and 218 dropped. A hundred seconds to the
second, no reset, and the drop counter climbing throughout — the failing
condition present the whole time and correctly ignored.

## The C5 was losing two of its reset reasons

`wartui reset` and the fault box have only ever been exercised on a C6. The C5
path is compile-checked in CI and has never been on a bench, so the mapping from
`esp_hal`'s `SocResetReason` was read against both chips' definitions instead.

The two enums are nearly identical, and the differences are all on the side that
had never been tested. `CoreSDIO` (0x06) is C6-only and uninteresting — nothing
here uses SDIO. But the C5 defines two the C6 does not:

| variant | C5 | C6 | was reported as |
| --- | --- | --- | --- |
| `PowerGlitch` (0x19) | yes | — | `Unknown` |
| `CpuLockup` (0x1A) | yes | — | `Unknown` |

`reset_cause` matched only variants both chips define, deliberately, to avoid a
`cfg` per chip — and so threw both away. A power glitch is a supply problem and
now reports as `Brownout`, whose printed advice (check the cable and the hub) is
the right advice for it.

`CpuLockup` is the one worth having. There is no working watchdog on these parts
(above), so the hang class has nothing behind it at all — and on the C5 the
silicon's lockup detector is the only mechanism that would ever say a hang
happened. Reporting it as `Unknown` throws away the single signal available for
the one failure mode this document admits it cannot catch. It gets its own
`ResetCause::Lockup` rather than being folded into `Watchdog`, which would name a
mechanism known not to fire and send the next person looking in the wrong place.

That is a wire change, so `LINK_PROTO_VERSION` goes to 4. Without the bump an
older host would meet the new variant, fail to decode the `Ready` carrying it,
and report a bridge that answered nothing — which is precisely the misdiagnosis
this protocol's version byte exists to prevent.

What stays unmapped is deliberate: `CoreDeepSleep` cannot happen because nothing
here sleeps, and `CoreEfuseCrc` says nothing an operator can act on that
`Unknown` does not already say.

**Still not done:** none of this has been run on a C5. It is an audit of the
mapping, not a bench test of it, and the gap it closes is the one the audit could
see. Reflashing a node as a bridge would answer it; no C5 was attached.

*Partly answered since, by the last section of this document.* A C5 bridge has
now reported `Software` and `External`, and the phase marker behind them, on
hardware. `PowerGlitch` and `CpuLockup` — the two this audit was about — remain
unseen, which is in the nature of both: nothing provokes them on demand.

## The S3 inherits the detector, and it was measured there too

The bridge now builds for the ESP32-S3, which puts a third chip behind
everything above. What carries over and what does not is worth separating,
because the whole point of this document is that the transmit-stall rule was
wrong twice and only a bench caught it.

**The premise carries over on the strength of the silicon, not a measurement.** The S3 has
the same `USB_SERIAL_JTAG` block, with the same `EP1_CONF` register: `WR_DONE`
clears `SERIAL_IN_EP_DATA_FREE`, and the TRM says it comes back only when the
USB host reads the FIFO. That is the entire premise of the wedge, so the wedge
should exist there in the same shape. The firmware side is unchanged — the same
`write_byte_nb` / `flush_tx_nb` pair through `wartui_proto::outbox` — and the
arithmetic is `wartui_proto::stall::StallWatch`, which is host-tested and does
not know what a chip is. The Phase 3 *register* dumps remain all C6, so the
silicon premise above is still read out of one part's TRM and confirmed on the
other's; what has now been run on an S3 is the detector, which is the half that
was wrong twice.

**Measured on a Waveshare ESP32-S3-LCD-1.47** (S3 rev v0.2, 16 MB flash, MAC
`…6A:4C`), flashed with the generic bridge firmware — the board's screen, LED and
SD slot are simply never initialised:

| what | result |
| --- | --- |
| `wartui status` | `44:1B:F6:85:6A:4C on Esp32S3, firmware 0.1.0`, channel 6, heap free 55764 |
| link round trip | works; a `GetStatus` is answered, so the USB-C really is the native USB Serial/JTAG |
| reset cause after `espflash flash` | `External` — and via `CoreUsbJtag`, since the S3 has no `Cpu0JtagCpu` |
| `wartui reset` | `Software`, `uptime 1ms, so it did reboot` |
| unplug and replug | `powered on`, at 16s uptime, so `ChipPowerOn` and this life rather than a stale reading |
| phase marker across that reset | survived — `it was carrying out a host command`, so `rtc_fast, persistent` holds on this part |
| image size | 387,088 bytes, 2.36% of flash |

Then against three C6 nodes (`…9A:24`, `…75:40`, `…00:08`, all
`wartui/0.1;ble`):

| what | result |
| --- | --- |
| receive | 1542 frames in 1h41m; heartbeats, capability tokens and wardrive lines all decode |
| `RxControlInfo` | RSSI reads correctly, −48 to −62 dBm for the fleet and −78 to −97 for the APs it reported |
| transmit | five assignments, every one `acked` — the transmit-callback status, not the enqueue result |
| admin latency | 5.5–7.8 ms from the heartbeat that opened the window to the callback, against a 300 ms window |
| planning | re-cut on each join (`node_count` 1→2→3) and dealt 2.4 GHz only, since no node claimed `5g` |

The RSSI row is the one that could have gone wrong quietly. The S3 is
`wifi_mac_version = "1"` where the C5 and C6 are "2" and "3", so its
`RxControlInfo` is a different struct; `drain_radio` already read nothing from it
but `rssi`, which all three layouts carry, and that narrowing turns out to be
exactly what made the port free.

The latency row needs one caveat, because the first run measured 5.1–6.0
*seconds* and that is not what it looks like. `latency_us` is
`tx_us - rx_us` on the bridge's own clock, so it counts however long a heartbeat
sat in the outbox ring before a host read it — and that bridge had been powered
for 1h41m with nobody attached, dropping 1518 frames. Every assignment in a
session is issued in its first few milliseconds, so all of them inherit that
backlog. Reset the bridge first and the same five assignments measure 5.5–7.8 ms.
Worth knowing before reading the figure as a blown window: it is an artifact of
attaching to a long-idle bridge, it is not specific to this chip, and the
wall-clock round trip in that first run was 5–7 ms throughout.

**Does not carry over: the reset reasons.** The S3's `SocResetReason` is not a
superset of the C6's, which is the trap `03de799` fell into from the other side.
Its differences are *renames* — `Cpu0Sw`, `Cpu0Mwdt0`, `Cpu0Mwdt1` and
`Cpu0RtcWdt` are `CpuSw`, `CpuMwdt0`, `CpuMwdt1` and `CpuRtcWdt`, because the S3
has two cores and neither is privileged — so unlike the C5's missing variants
these are compile errors rather than silence, and the build says so immediately.
The numeric codes are identical on all three parts; only the Rust spellings
differ. Two variants are genuinely new, and they are *not* the same story told
twice: esp-hal names `CorePwrGlitch` (0x17) "glitch on power" and `SysClkGlitch`
(0x13) "glitch on clock". Only the first is a brownout by another name and takes
the C5's `PowerGlitch` mapping; the second got its own `ResetCause::ClockGlitch`,
because the host prints "check the cable and the hub" for a brownout and that is
the wrong errand for a clock fault. Folding them together is the mistake
`03de799` made from the other direction, and it survived first review here. One is genuinely absent: `Cpu0JtagCpu`, so an `espflash reset` on an S3
should report `External` via `CoreUsbJtag` alone — unverified.

**The gap the S3 does not close.** It has no `CpuLockup`. The C5 remains the only
part in the fleet whose silicon reports a hang, and since no watchdog on any of
these parts fires (above), an S3 bridge that stops turning its loop over has to
be unplugged, with nothing in the `Ready` behind it to say why. That is the same
position the C6 has always been in; it is not a new hole, but adding a chip was
a chance to close it and did not.

Every reset cause the S3 can be made to report has now been seen on hardware:
`PowerOn`, `Software` and `External`. `Brownout` needs a bad supply and `Lockup`
does not exist on this part, so neither is reachable on demand.

### The three wedge rows, re-measured on the S3

The one thing the first S3 bench did not test was the detector this document is
about, so it was run again on the S3 with the same method the C6 rows used: a
build whose `UsbSink` refuses every byte from a fixed uptime onwards, standing in
for an endpoint that has stopped draining, with a marker `Log` frame 200 ms
before it so the host has a timestamp for the wedge itself. Three C6 nodes
transmitting throughout, so the outbox always had something queued, and a host
polling `GetStatus` twice a second — the same shape as the C6 run, on the same
fleet the rest of this section was measured against.

| scenario | C6 | S3 |
| --- | --- | --- |
| wedged, host polling throughout | ROM banner 3.10 s after the endpoint died | banner at +3.03 s |
| wedged, ~10 s of total silence across the wedge, then the host returns | not one byte while silent; banner at +13.16 s | not one byte for 13.24 s; banner at +13.05 s, which is 3.0 s after the host came back |
| main loop blocked 8 s — *not* a wedge — then the host returns | **no reset**; traffic resumed 8.00 s later | **no reset**; traffic resumed 8.00 s later and one continuous life ran to 44.2 s across 90 status frames |

Row one's reset announced itself correctly: `wartui status` on the next life
reported `its transmit path had stopped draining`, so the `TxStalled` cause
survives the reboot on this part too and the operator gets the diagnosis rather
than a bridge that merely hesitated.

Row two is the row worth reading twice, and the S3 puts the same numbers in the
same places. The host's last frame was at 20.0 s and the endpoint died at 20.5 s,
so the stall armed at once, against a host it had every reason to believe in —
and said nothing for thirteen seconds, because that host had not spoken *since*.
Presence lapsed at 30.0 s, the clock was dropped, the host came back at 30.5 s,
and the reset landed three seconds after that. This is the fourth clause and the
host-present window doing their jobs on silicon neither was written against.

Row three is the false positive the first version of the rule had. During the
eight blocked seconds the detector is not consulted, and the pass that follows
moves bytes, which clears the clock; the 90 status frames afterwards never once
showed the uptime going backwards.

One caveat on method, unchanged from the C6: `UsbSink` returning `WouldBlock` is
a stand-in, not the silicon. It exercises every clause of the rule and the reset
behind it, and it cannot exercise the claim that `SERIAL_IN_EP_DATA_FREE` gets
stuck in the first place — which is read out of the TRM for both parts and has
only ever been *observed* on a C6, because the state cannot be provoked from the
device end.

**Still not done on the S3:** nothing in this document, now. No C6 or S3 can
report the hang class at all, which is a gap in the firmware rather than in the
bench. The C5 remained unbenched entirely — the section below is why, and it is
no longer true.

## The C5's software reset was ending the board, not restarting it

Measured on 2026-09-10 on Linux (Fedora, kernel 7.2.4), against the ESP32-C5
bridge `4F:08` on `/dev/ttyACM0` — a *different* board from the `C5:B8` that
issue #12 was written against, which is the first thing this bench established:
the fault is the part, not that board.

Everything above this section was measured on a C6 or an S3. The C5 had never
been on a bench, and the reason turned out to be that it could not survive being
put on one. `esp_hal::system::software_reset()` — the call behind `wartui reset`,
the panic handlers and the stall detector's own recovery — does not reboot a C5.
It ends it.

| step | result |
| --- | --- |
| `wartui status`, bridge up 18 m 43 s | healthy, `Esp32C5`, `powered on` |
| **one** `wartui reset` | `the bridge did not come back within 10s` |
| `wartui status` after | nothing answers |
| `espflash reset` | **does not recover it** |
| `espflash flash` — connects, erases, writes, verifies, "Flashing has completed!" | **does not recover it either** |
| boot log | `ESP-ROM:esp32c5-eco2-20250121`, `rst:0x15 (USB_UART_HPSYS)`, `Core0 Saved PC:0x40800c2e`, then nothing |
| physical replug | back, instantly, running whatever was flashed |

The last two rows are the ones that matter. `SPI mode:`, `load:` and `entry`
never print, and those are the first things the ROM does that need flash — so
the board is not failing to *run* its firmware, it is failing to *read* it. And
because a full reflash lands perfectly and changes nothing, "reflash it" is not
a recovery either. Only power is. That narrows what
`firmware/bridge/README.md` says about `espflash reset` being the fallback for a
bridge that answers nothing: for this failure it was not a fallback at all.

**Why.** The C5 and the C61 boot with `PCR.RESET_EVENT_BYPASS.reset_event_bypass`
set — its reset value is `0x02` — and that bit keeps a core reset from also
resetting the system bus. The ROM's own MSPI core reset then leaves the AXI bus
frozen, and the next boot hangs on its first flash read. This is not our finding:
it is esp-rs/esp-hal#5703, fixed by #5745, and it mirrors what ESP-IDF has always
done in `bootloader_hardware_init` (`axi_icm_ll_reset_with_core_reset(true)`).
esp-hal clears the bit in the C5's `pre_init` from 1.2.0-rc.0 onwards.

**Why it is written out by hand here.** `esp-radio 1.0.0-beta.0` requires
`esp-hal = "~1.1.0"`, so 1.2 will not resolve — the standoff the dependency
section of `firmware/bridge/README.md` already describes. Lying to cargo about
the version does not work either: `SoftwareInterruptControl` is gone in 1.2.1,
and `esp-rtos 0.3.0` and this firmware's own `esp_rtos::start` are written
against it. Issue #16 carries what the real upgrade needs and what to delete
when it lands. The write itself is one safe `modify` on a named field, so
`unsafe_code = "forbid"` is untouched.

It lives in a `reboot()` funnel in each firmware rather than at the top of
`main`, where upstream puts it. A panic can land before `main` reaches any line
of its own — and the panic handler is one of the four callers.

**After the fix, same board, same session:**

| step | result |
| --- | --- |
| `wartui reset` ×3 | back in 0.82 s, 0.83 s, 0.83 s; `uptime 2ms, so it did reboot` each time |
| reset cause afterwards | `Software`, and the phase marker survived: `it was carrying out a host command` |
| device path | `/dev/ttyACM0` throughout — a software reset does not re-enumerate on this part either |

So two things measured on the C6 hold on the C5 as well, and were unmeasurable
there until now: `rtc_fast, persistent` survives a software reset, and the port
does not move under a script.

**The stall detector, which is the caller that mattered.** The same `WouldBlock`
stand-in used for the C6 and the S3 above, wedging the transmit path after the
fifth decoded command so each life answers a few frames before dying: the board
detected the stall, reset itself, came back announcing `Ready`, and did it again
— ten cycles in twenty seconds, every one of them reporting
`its transmit path had stopped draining`. On the firmware this section replaces,
the first of those ten would have been the last thing the board ever did. A
recovery that is worse than the failure is the reason this was worth a bench
rather than a `#[cfg]` that skips the reset on a C5.

**The C6 did not move.** Reflashed from the same tree and reset twice: back in
0.35 s and 0.30 s. The workaround is `#[cfg(feature = "esp32c5")]`, so it cannot
compile into a C6 or an S3 build, and this is a habit-check rather than a risk.

**Not exercised.** The two panic handlers, on either chip: both reach `reboot()`
by the same call the other three callers use, and provoking one costs another
temporary build per firmware. The node's `reboot()` is compile-checked for both
its chips and has not been run on hardware — a node that fails to come back
reads as `no heartbeat`, which is also what a node out of range reads as, so it
is the quietest of the four and the one most worth remembering is untested.
