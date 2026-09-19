#!/usr/bin/env python3
"""Print what the running view drew, as text.

`crates/wartui/src/tui.rs`'s tests render widgets through ratatui's `TestBackend`:
a `Snapshot` built by hand, one frame drawn, assertions against the text. That is
the regression net and it stays the first thing to reach for. It also never runs
the event loop, never runs `ratatui::try_init()`, and never sees the simulator, so
nothing in the repository covers what the binary actually puts on a screen. The one
record of that is `docs/images/wartui-example.png`, which a person can read and a
diff cannot.

This closes that gap with pyte, a VT100 emulator with no terminal behind it. The
binary runs on a pty — `try_init()` refuses anything that is not a tty, and the
pty is also what fixes the width, which decides which layout the view chooses —
and everything it draws is fed to a `pyte.Screen`. What comes back is the grid:

    tools/render.py --sim 3 --cols 120 --rows 30

`--keys` presses keys before the capture, so the states behind `j`, `k` and `b`
are reachable too; `b` shows as `pending` until the next admin window, which is
the protocol rather than lag.

The binary is built before it is spawned rather than run through `cargo run`,
because a first `draw` writes only the cells it considers non-empty: cargo's
compile chatter would sit under the view for the rest of the render. Building
first also puts a compile error in front of you instead of inside the grid.

`--sim` is assumed. It needs no hardware, and it is also what keeps the GPS search
from opening every serial port on the machine. The capture goes to a temporary
database that is deleted on the way out, since `run` otherwise leaves a
`wartui-<date>.db` wherever it was started.
"""

import argparse
import codecs
import fcntl
import json
import os
import select
import shutil
import struct
import subprocess
import sys
import tempfile
import termios
import time

try:
    import pyte
except ImportError:
    sys.exit("pyte is not installed: dnf install python3-pyte, or pip install pyte")

# Somewhere with enough access points to be worth drawing, and the same position
# the READMEs use in their examples.
DEFAULT_LAT, DEFAULT_LON = 37.7749, -122.4194

PLAIN = ("default", "default", False, False)


def build():
    """Build wartui and return the binary, letting cargo diagnose to our own stderr.

    Cargo is asked where it put the binary rather than being assumed to have put
    it under `debug/`: a `build.target` in anyone's cargo config moves it under
    the triple, and a build that succeeds followed by a spawn that cannot find
    what it built is a confusing way to learn that.
    """
    built = subprocess.run(
        ["cargo", "build", "-q", "-p", "wartui", "--message-format", "json-render-diagnostics"],
        stdout=subprocess.PIPE, text=True, check=True,
    )
    for line in built.stdout.splitlines():
        event = json.loads(line)
        if event.get("reason") == "compiler-artifact" and event.get("executable"):
            if event["target"]["name"] == "wartui":
                return event["executable"]
    sys.exit("cargo built nothing called wartui")


def spawn(command, cols, rows):
    """Start `command` on a pty of exactly this size, as its own session leader.

    The size is set on the slave before the fork so the first frame is already
    drawn at it: a view that lays out at 80 columns and is then told it has 200
    is a different bug from the one being looked for. The session and the
    `TIOCSCTTY` are for crossterm, which reads keys from `/dev/tty` when there
    is one.
    """
    master, slave = os.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))

    def session():
        os.setsid()
        fcntl.ioctl(0, termios.TIOCSCTTY, 0)

    proc = subprocess.Popen(command, stdin=slave, stdout=slave, stderr=slave, preexec_fn=session)
    os.close(slave)
    return proc, master


def pump(master, stream, decoder, proc, seconds):
    """Feed everything drawn for this long into `stream`; False if it stopped drawing.

    The decoder belongs to the whole render rather than to one call. A
    box-drawing character is three bytes and every frame is full of them, so a
    read that splits one across two pumps — which `--keys` guarantees — would
    lose it to a replacement glyph in a decoder that is then thrown away.

    A dead child is read as EOF or EIO on the master rather than as an exit
    status, so both are the same answer here.
    """
    deadline = time.monotonic() + seconds
    while True:
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            return True
        ready, _, _ = select.select([master], [], [], min(remaining, 0.1))
        if not ready:
            if proc.poll() is not None:
                return False
            continue
        try:
            chunk = os.read(master, 65536)
        except OSError:
            return False
        if not chunk:
            return False
        stream.feed(decoder.decode(chunk))


def press(master, stream, decoder, proc, keys, settle):
    """Press each key and let the view redraw before the next one."""
    for key in keys:
        os.write(master, key.encode())
        if not pump(master, stream, decoder, proc, settle):
            return False
    return True


def stop(proc):
    """Wait for it to go, and insist if it will not.

    Nothing here waits without a deadline. A child that has closed its stdio and
    carried on reads exactly like one that has exited, and a render that hangs
    with the screen unprinted is worse than one that kills something.
    """
    for insist in (proc.terminate, proc.kill):
        try:
            return proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            insist()
    return proc.wait()


def quit_cleanly(master, proc):
    """`q` commits the last batch; anything still running after that is signalled."""
    try:
        os.write(master, b"q")
    except OSError:
        pass
    return stop(proc)


def styled_runs(screen):
    """Every run of cells drawn in something other than the default style.

    The fleet table says what a node is doing in colour as much as in words, so
    the grid on its own is only most of what was drawn.
    """
    for y in range(screen.lines):
        row = screen.buffer[y]
        start, style, text = 0, PLAIN, ""
        for x in range(screen.columns + 1):
            cell = row[x] if x < screen.columns else None
            here = (cell.fg, cell.bg, cell.bold, cell.reverse) if cell else PLAIN
            if here != style:
                if style != PLAIN and text.strip():
                    yield y, start, text, style
                start, style, text = x, here, ""
            if cell:
                text += cell.data


def describe(style):
    fg, bg, bold, reverse = style
    parts = [] if fg == "default" else [f"fg={fg}"]
    if bg != "default":
        parts.append(f"bg={bg}")
    if bold:
        parts.append("bold")
    if reverse:
        parts.append("reverse")
    return " ".join(parts)


def main(args):
    if (args.lat is None) != (args.lon is None):
        sys.exit("--lat and --lon go together; a half-given position is a wrong one")
    lat = DEFAULT_LAT if args.lat is None else args.lat
    lon = DEFAULT_LON if args.lon is None else args.lon

    binary = args.bin or build()
    # The capture is a temporary file from here on, so every way out of this
    # function goes through the removal rather than just the expected one.
    scratch = tempfile.mkdtemp(prefix="wartui-render-")
    try:
        return render(args, binary, os.path.join(scratch, "render.db"), lat, lon)
    finally:
        shutil.rmtree(scratch, ignore_errors=True)


def render(args, binary, db, lat, lon):
    command = [binary, "run", "--sim", str(args.sim), "--db", db]
    if args.sim_c6:
        command += ["--sim-c6", str(args.sim_c6)]
    command += ["--lat", repr(lat), "--lon", repr(lon)]
    command += args.rest

    screen = pyte.Screen(args.cols, args.rows)
    stream = pyte.Stream(screen)
    decoder = codecs.getincrementaldecoder("utf-8")("replace")
    proc, master = spawn(command, args.cols, args.rows)
    try:
        drawing = pump(master, stream, decoder, proc, args.after)
        if drawing:
            drawing = press(master, stream, decoder, proc, args.keys, args.settle)

        # Read the screen while it is still being drawn on. What the view emits
        # on its way out is the terminal's business, not part of the frame.
        for line in screen.display:
            print(line.rstrip())
        if args.attrs:
            print()
            for y, x, text, style in styled_runs(screen):
                print(f"  {y:>3},{x:<3} {text.strip():<40} {describe(style)}")

        if drawing:
            quit_cleanly(master, proc)
        else:
            # Whatever it managed to say is on the screen above — a clap usage
            # error, or a panic — so the note goes after it rather than into it.
            sys.stdout.flush()
            stop(proc)
            print(f"\n# wartui stopped early, status {proc.returncode}", file=sys.stderr)
            return 1
    finally:
        os.close(master)
    return 0


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description="Render the running view as text.")
    parser.add_argument("--sim", type=int, default=3, metavar="NODES", help="fake nodes to run")
    parser.add_argument("--sim-c6", type=int, default=0, metavar="NODES", help="of which C6s")
    parser.add_argument("--cols", type=int, default=120, help="terminal width to lay out at")
    parser.add_argument("--rows", type=int, default=30, help="terminal height to lay out at")
    parser.add_argument("--after", type=float, default=4.0, metavar="SECONDS",
                        help="how long to let the fleet fill in before reading the screen")
    parser.add_argument("--keys", default="", help="keys to press first, e.g. jjb")
    parser.add_argument("--settle", type=float, default=1.0, metavar="SECONDS",
                        help="how long to let the view redraw between keys")
    parser.add_argument("--attrs", action="store_true", help="also list everything drawn in colour")
    parser.add_argument("--lat", type=float, help="position to record; both halves or neither")
    parser.add_argument("--lon", type=float)
    parser.add_argument("--bin", metavar="PATH", help="an existing binary, instead of building")
    parser.add_argument("rest", nargs="*", metavar="-- ARGS", help="passed on to wartui run")
    sys.exit(main(parser.parse_args()))
