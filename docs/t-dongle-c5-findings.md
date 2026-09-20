# T-Dongle-C5 — what the panel costs

Measured on 2026-09-20 on Linux (Arch, kernel 7.2.6), against a LilyGO
T-Dongle-C5 bridge (`44:C0`) built `--features esp32c5,t-dongle-c5`, with two
ESP32-C6 nodes (`9A:24`, `75:40`) and an ESP32-C5 node (`0A:CC`) on the air.
The capture figures below were taken with the panel drawing `FONT_6X10`; the
timings were re-taken at `FONT_9X15`, which is what ships.

The question was whether a screen can share the bridge's single-threaded loop
with the radio. `firmware/bridge/README.md` names the two ways that could go
wrong — a blocking SPI transfer inside the receive path, and a redraw that
delays a transmit — and the answer is that it can, by a wide margin, but that
the transfer is about four times more expensive than a wire-rate estimate
suggests.

## A redraw costs more than the pixels do

Timed on the board with `Instant::elapsed()` around the render step, reported
as `Log` frames and read with `wartui sniff --verbose`. The panel draws
`FONT_9X15`, so a row is fifteen pixels tall and the screen holds five:

| What | Measured |
| --- | --- |
| One row (clear plus text) | 9.03–9.64 ms, median 9.27, n = 25 |
| A full five-row repaint | 35.4–35.8 ms, n = 3 |

A row is 160 x 15 pixels at two bytes each: 4.8 KB, which at the panel's
20 MHz is **1.92 ms of wire time**. So roughly four fifths of the cost is not
the bus. It is `embedded-graphics` walking the row through the interface's
pixel batching, and then a text draw that visits each glyph's bounding box as
its own transaction with a CS toggle either side.

A full repaint is not five times a single row — 7.1 ms a row rather than 9.3 —
because some of its rows are short or empty, and a row's cost is mostly its
glyphs.

The same bench at `FONT_6X10`, before the font was made readable at arm's
length, measured 5.76–5.98 ms a row and the same ratio against wire time. The
cost tracks the glyphs, not the font.

**This matters only against the loop's other blocking call.** An unacknowledged
ESP-NOW send occupies the same loop for 28–35 ms
([`phase-4-findings.md`](phase-4-findings.md)), so a full repaint is the same
order as the worst thing the bridge already does — and it happens only when the
screen changes meaning, which is a host arriving or going away. The steady state
is one to three rows a second.

If that ever needs to shrink, the cost is in the per-glyph transactions rather
than in the bus, so the thing to reach for is drawing a whole row into a buffer
and pushing it once — not a faster SPI clock.

**One row of the eight was being drawn off the glass.** The render loop ran over
`PANEL_ROWS`, the most the wire can carry, rather than the rows the font leaves
room for — so a repaint cleared three strips past the bottom of the screen and
paid to have them clipped. Measured at 36.25 ms for eight rows against 35.6 ms
for five: a small cost, and a wrong one.

## It does not cost the radio anything

Three and a half minutes of capture with the panel updating throughout, after a
`wartui reset` so the counter starts clean:

```
received   129 frames
dropped    0 frames
uptime     3m 24s
```

**Zero dropped frames**, and no reset the host did not ask for. `dropped_tx`
counts what the outbound ring threw away because the host was not draining it,
which is where a redraw stealing time from the loop would show up first.

## The host pushes less often than it could

47 pushes over 176 s — about one every 3.7 s, against a limit of one a second.
The rate limit is not what held it back; the lines simply did not change. A push
goes out only when the rendered lines differ from the last ones, and on a bench
where the fleet is static and the nodes have already reported everything in
range, most seconds say exactly what the previous second said.

That is worth knowing because it means the 1 Hz figure is a ceiling rather than
a duty cycle, and the panel's real cost on a quiet capture is far below anything
above.

## Numbers this replaces

An earlier note estimated a full-frame push at 6–15 ms at 20–40 MHz, reasoning
from wire time alone. That was the right order for a raw framebuffer blit and
about five times optimistic for drawing text through `embedded-graphics`. The
conclusion it drew — that the display is affordable if it refreshes at 1–2 Hz
and redraws only what changed — survives the correction intact.

## Still unmeasured

- **The APA102 LED and the TF slot**, both out of scope here. The slot shares
  MOSI/SCK/MISO with the panel, so using it would make the panel's
  `ExclusiveDevice` wrong and want a shared-bus device instead.
- **What fraction of the loop is idle.** The bridge sleeps 1 ms whenever
  nothing asked for anything, and accumulating that against wall time would
  turn "almost certainly low single-digit percent" into a figure. It is a
  `Status` wire change, so it has not been spent.
- **Whether the panel tolerates a faster SPI clock.** It does not matter at one
  redraw a second, and the cost is not the bus anyway.
