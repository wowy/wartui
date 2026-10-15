# wartui — the CLI and the view

The clap CLI and the ratatui fleet view. Both are front ends over `wartui-core`, which decides the
fleet's behavior and is tested without a terminal.

This is the operator's manual. The root [`README.md`](../../README.md) is the short version.

## Commands

`run` is the default, so the subcommand can be left off.

| Command        | What it does                                                                        |
|----------------|-------------------------------------------------------------------------------------|
| `run`          | Capture a fleet into the store and watch it live                                    |
| `export`       | Write a WiGLE CSV from a capture                                                    |
| `analyze`      | Summarize a capture and what it lost; see "Analyzing a capture"                     |
| `upload`       | Upload a capture to the WDGWars leaderboard; see "Uploading to WDGWars"             |
| `sniff`        | Print every frame the bridge hears, decoded, a line per record                      |
| `status`       | Ask the bridge for its counters and uptime                                          |
| `reset`        | Reboot a bridge that has stopped answering                                          |
| `ports`        | List the Espressif boards attached, and the address of each                         |
| `flash-fleet`  | Flash node firmware onto every attached board of one chip; see "Flashing the fleet" |
| `flash-bridge` | Flash bridge firmware onto the bridge; see "Flashing the bridge"                    |
| `ota`          | Update node firmware through the bridge; see "Updating over the air"                |

Nodes broadcast heartbeats but unicast sighting batches to the bridge that last sent them an admin
or clear frame. So `sniff` on a second bridge on the channel hears every heartbeat and no sightings.
If the attached bridge is swapped mid-run, `run` re-sends every assignment so each node learns the
new address. The same bridge reconnecting or rebooting sends nothing; nodes already address it.

`run` takes:

| Flag                    | Default     | What it is                                             |
|-------------------------|-------------|--------------------------------------------------------|
| `--db PATH`             | dated       | New file for the capture; an existing one is refused   |
| `--bridge PATH\|MAC`    | detected    | Which board the bridge is, by path, address or `00:08` |
| `--lat` `--lon` `--alt` | —           | A fixed test position; the capture is never uploaded   |
| `--gps PATH`            | detected    | An NMEA receiver, preferred over `--lat`/`--lon`       |
| `--no-gps`              | off         | Do not look for a receiver at all                      |
| `--gps-baud N`          | detected    | Line rate of that receiver                             |
| `--gps-max-age S`       | `5`         | How old a fix may be before falling back               |
| `--sim N`               | —           | Run a fake fleet instead of hardware                   |
| `--sim-c6 N`            | `0`         | Make that many of them C6s, from the end of the fleet  |
| `--record-raw`          | off         | Also keep the undecoded bytes of every frame           |
| `--notes TEXT`          | —           | A note about this run, stored with the capture         |
| `--config PATH`         | per-OS path | See "Config file"                                      |

The channel pool and transmit powers have no flag; see "Config file". `--log-file PATH` is global;
see "When nothing arrives".

### Capture metadata

The capture records the host build and its settings in SQLite metadata:

| Key | Meaning |
|-----|---------|
| `capture.host_version` | Host crate version |
| `capture.host_build` | Content-derived host source identity, including uncommitted edits |
| `capture.core_version` | Core crate version |
| `capture.release_tag` | Release tag, when the host was built with one |
| `capture.scan_channels` | The host's ordered scan table, which gives stored channel indices their meaning |
| `capture.wire_version` | The wire version the host understands |
| `capture.settings.initial` | Initial host settings, normalized by the engine |
| `capture.settings.NNN` | Runtime's first settings snapshot and subsequent changes, in capture order |

Settings include the channel pool, transmit powers, raw recording, positioning configuration and
Bluetooth preference. They describe desired host settings, not a radio's acknowledgment or adoption.
The initial metadata is committed with capture creation. Runtime settings use zero-padded sequence
keys, so repeated or reversed timestamps cannot overwrite a change. They share the bounded store
queue: sequence gaps and store drops mean the history may be incomplete.
The source identity hashes sorted workspace-relative host Rust files, manifests and the lockfile
with noncryptographic FNV-1a. It excludes firmware and generated files. Neither it nor the saved scan
table proves which firmware a node ran: the table records only the host's interpretation.

An absent metadata field means unknown, not a default, in an older compatible capture. There are no
migrations before 1.0. A capture with another schema version or fingerprint is refused; start a new
capture at a new `--db` path.

### Exporting

A capture holds one run. Each run names its capture for the second it started:
`wartui-2026-09-18-14-30-07.db`. The date is in ISO order whatever the locale, so captures sort in
the order they were made and `export` can pick the newest. `run` refuses a `--db` that exists rather
than adding to it, so every export, analysis and upload covers exactly one run.

`export` writes the [WiGLE v1.6 format](https://api.wigle.net/csvFormat.html):

| Flag                  | Default                  | What it is                                       |
|-----------------------|--------------------------|--------------------------------------------------|
| `--db PATH`           | newest in this directory | The capture to read                              |
| `--out PATH`, `-o`    | the capture's name, .csv | Where to write; `--out -` writes to standard out |
| `--recapture SECONDS` | `3600`                   | How wide each row's window is                    |
| `--config PATH`       | per-OS path              | Where to read the excluded SSIDs                 |

So `wartui export` alone exports the run that just ended, `wartui-2026-09-18-14-30-07.db` to
`wartui-2026-09-18-14-30-07.csv`. A derived name is never overwritten, since that file may be the
export already uploaded; name it with `--out` to overwrite it on purpose. The CSV is a view over the
store, so it can be re-run after a decoder fix, for last week's run, or during a capture.

`--recapture` writes one row per network per window: its strongest positioned sighting, with
`FirstSeen` from the window's first sighting. WDGWars skips a re-scan of a MAC by the same user
within an hour from scoring, though its GPS may still refine the entry. The default matches: a
re-hearing within the hour can still improve its row's position, and one past it is a new row.
`--recapture 0` writes one row per network. On finishing, `export` prints exact counts to standard
error: rows written, distinct Wi-Fi networks (per band) and Bluetooth devices, sightings of each,
sightings by position source, what each node heard, and the capture's span. These totals cover
accepted Wi-Fi and BLE sightings, including those without a position. A capture made with
`--sim` or `--lat`/`--lon` still exports, with a warning not to submit the CSV anywhere. A sighting
whose stored kind is neither Wi-Fi nor BLE is left out, and a note says how many. Wi-Fi sightings
with zero, broadcast or multicast addresses are also left out and counted in a warning. They
remain in the store and count towards the upload cutoff, but not the totals above. Locally
administered unicast Wi-Fi and BLE random addresses remain exportable. `export`, `analyze` and
`upload` also warn about channel-zero Wi-Fi sightings, including those with invalid addresses:
the warning counts can overlap and count sightings, not networks or rows. Channel zero alone
does not exclude a sighting, and BLE channel zero is not counted. Advertised channels outside
the scan pool remain exportable. Wi-Fi sightings of an `excluded-ssids` entry are left out too, and
a note says how many; see "Config file".

`RCOIs` holds a Passpoint access point's roaming consortium identifiers and `MfgrId` a BLE
advertiser's manufacturer identifier, both blank (NULL in the store) when none was offered. A BLE
row's `Frequency` is blank on purpose: that column is a Bluetooth "device type" code only an active
inquiry produces, and the nodes never transmit while scanning.

### Analyzing a capture

`analyze` prints what `export` would report for a capture, then what it lost on the way in. It
takes `--db`, `--recapture` and `--config` as `export` does. Reports go to standard output and
quality warnings to standard error; no output file is written. The tail of its output, after the
export summary:

```
  bridge     1,203,551 frames received  1,300 dropped (0.1%)  1 reboot  host read 1,202,240  11 lost on USB
  host       147 not stored: 140 retries  7 foreign  0 store dropped  behind ≥100 ms 31 times (worst 412 ms)  slowest commit 58 ms  queue peak 1,204
  health     under-voltage in 3 samples  throttled in 0  since boot: under-voltage  temp max 71.2 °C  battery min 3.62 V
  batches    212 lost between node and host
  heartbeats 41 missed of 3,497 expected (1.2%)  12 unsent
  buffer full wifi 18,220  ble 1,203
  loss by node
    1C:5A  batches 12  heartbeats 3/702 (0.4%)  buffer full ble 1,203
    57:84  batches 200  heartbeats 38/2,795 (1.4%)  12 unsent  buffer full wifi 18,220
```

| Line          | What it counts                                                                  |
|---------------|---------------------------------------------------------------------------------|
| `bridge`      | Frames the bridge received, dropped as the host fell behind, and lost on USB    |
| `host`        | Frames the host read but did not store, and how the host and its store held up  |
| `health`      | Under-voltage and throttling the Pi firmware reported, temperature, battery     |
| `batches`     | Sighting batches lost between node and host; the fleet table's `lost`           |
| `heartbeats`  | Heartbeats lost between node and host, and how many of them were `unsent`       |
| `buffer full` | Sightings a full pending buffer refused; most reappear in a later dwell or scan |

Counts follow the assumptions below; `lost on USB` is approximate. The capture's first bridge
reply and first heartbeat per node are baselines, so what was dropped before the capture began
is not counted. `batches` and `bridge` print zeros, since "0 lost" is the answer. So do
`store dropped` and, on a Pi, the `health` counts. Left out:

- the `bridge` line, when the capture holds no status reply
- the `unsent` clause, total or per node, when it is zero
- `buffer full` figures that are zero, and the `buffer full` line when all are
- the reboot clause, when the bridge never restarted
- the `lost on USB` clause, when it is zero
- the `oversize (not a node's)` clause, when it is zero. It counts ESP-NOW frames too long to be
  a wartui node's, which the bridge dropped and left out of `frames received`
- `host` clauses that are zero, and the `host` and `health` lines when the capture holds no host row
- the `health` line, when nothing could be read; off a Pi it holds only the temperature
- the `battery min` clause, when the host has no hwmon device named `battery`

The host records a `host_status` row when the capture starts, every 5 s, and at shutdown. Each
`bridge_status` row records the frames the host had read when the reply arrived. `frames
received` less `dropped` less `host read` is `lost on USB`, to within the frames queued in the
bridge at the first and last reply, since a reply overtakes them. On the `host` line:

| Clause                 | What it counts                                                            |
|------------------------|---------------------------------------------------------------------------|
| `not stored`           | Frames read and deliberately dropped: radio retries, undecodable, foreign |
| `store dropped`        | Rows lost because the store's queue was full or a write failed            |
| `garbled`              | USB frames that failed their checksum                                     |
| `behind ≥100 ms`       | Samples in which the host fell far enough behind the air to miss windows  |
| `admin windows missed` | Assignments held back because the heartbeat behind them was stale         |
| `slowest commit`       | The slowest batch the store wrote                                         |
| `queue peak`           | The deepest the store's queue got                                         |

`under-voltage` and `throttled` count samples with that bit of `vcgencmd get_throttled` set now;
`since boot` lists what the last sample says happened at any time since the Pi booted.
`battery min` is the lowest voltage the battery's fuel gauge read; its current draw is stored as
`battery_ma` for queries but not printed.

Compare the per-node heartbeat percentages to spot disproportionate loss. Each is missed beats
divided by distinct received plus missed beats, including the received baseline. The totals cover
the whole capture; `--recapture` selects export rows, not heartbeat intervals.

`unsent` is the missed beats the node's own radio could not send: a full send queue or a failed
send callback. Each heartbeat carries the node's count of them since boot. The rest were lost on
the air or past the bridge. A large `unsent` share points at the node, not the air.

Each heartbeat carries a 16-bit sequence number that starts at 1 when the node boots and wraps.
The 32-bit sweep counter wraps too. Forward modulo deltas below half-range are progress, including
wraps; a delta at least half-range in either counter indicates a restart. This assumes fewer than
32,768 beats and 2^31 sweeps between observations. A restart whose counters both look forward is
undetectable. A repeated heartbeat counts once. Across a detected restart, only missing beats
since boot are counted, not any loss before that boot. A gap after a heartbeat replayed from the
bridge's backlog is not counted, including the replay-to-live handoff.

Add `--heartbeat-windows` to list each counted gap after the normal summary:

```sh
wartui analyze --db tonight.db --heartbeat-windows
```

Each line gives the node, two UTC arrival timestamps with milliseconds, heartbeat row IDs, and
missed count, with `(N unsent)` when the node's radio could not send some of them. These observed
arrivals bracket missing beats; they are not exact transmission times. Rows stay in arrival order
even when the wall clock steps back; a reversed clock is labeled.
Repeats add no gap, and the first heartbeat per node is only a baseline. A `restart-associated,
uncertain interval` counts only missing beats since boot: neither the boot time nor pre-boot loss
is known. Window counts sum to the lifetime missed total. With no counted gaps, the flag prints
`no counted gaps`; without the flag, no windows are printed.

### Uploading to WDGWars

`upload` sends a capture to the [WDGWars](https://wdgwars.pl) leaderboard. It builds the same CSV
`export` would, gzips it in memory, and writes nothing to disk. Paste the site's API key in settings
(`c`), or set `wdgwars` under `[api-keys]` in `wartui.toml`; see "Config file".

```sh
wartui upload                     # the newest capture in this directory
wartui upload --db tonight.db -y  # another capture, without asking
```

| Flag                  | Default                  | What it is                    |
|-----------------------|--------------------------|-------------------------------|
| `--db PATH`           | newest in this directory | The capture to read           |
| `--recapture SECONDS` | `3600`                   | How wide each row's window is |
| `--config PATH`       | per-OS path              | Where to read the settings    |
| `--yes`, `-y`         | off                      | Upload without asking         |
| `--resend`            | off                      | Send everything, not just new |

It prints the same counts as `export`, plus the compressed size, then asks `Upload to WDGWars?
[y/N]`. Without a terminal to ask on, it refuses before building the upload unless given
`--yes`. A capture with no positioned rows is not sent, and a capture made with `--sim` or
`--lat`/`--lon` is never uploaded; `export` warns about such a capture too. The site takes at most
40 MB (40,000,000 bytes) compressed, and a larger capture is refused before sending. Sizes are
printed in decimal units.

A repeat upload sends only sightings stored since the last upload the site queued. The cutoff is the
last sighting that upload covered, in the order the capture stored them, so uploading a capture that
is still running loses nothing. A job the site reports failed does not count. `upload` needs to
write the capture to record the upload, and refuses before sending if it cannot. The confirm summary
names the last job (`previously`), and `--resend` sends everything again, which a decoder fix needs.
A network heard within the recapture width on both sides of the last upload gets a second row, which
WDGWars skips when scoring. A sighting with no position counts as sent once an upload has covered
it.

The site queues the file and imports it later. `upload` polls until the import ends, saying each
change of state, then prints the site's counts (`imported`, `captured`, `updated`, and any others).

Once the site has queued the job, the upload has happened, so `upload` exits zero unless the site
reports the import failed. Rerunning it would send a duplicate.

| After queuing                                          | What `upload` does                                 |
|--------------------------------------------------------|----------------------------------------------------|
| The import fails                                       | Exits non-zero with the site's reason              |
| A poll errors (network, timeout, 5xx)                  | Says so once per distinct error, and keeps polling |
| A poll redirects to the login page, or answers 401/403 | Stops polling, prints the job's URL, exits zero    |
| 30 seconds pass                                        | Stops polling, prints the job's URL, exits zero    |

When it stops early the import carries on; the job's URL or your profile on wdgwars.pl shows the
result. The wait is short because someone sits at the terminal for it and nothing needs the result
to finish. Each poll gets 10 seconds; sending the file has no time limit. Before queuing, a refused key (with the site's reason), an oversized file and any other
error status are reported in plain words and exit non-zero. Redirects are not followed.

#### From the fleet view

`u` uploads the capture `run` is writing, with the key in settings when `u` is pressed. It shows
the same summary in a box; `y` sends and any other key cancels. The capture keeps running
throughout. The footer follows the upload on a line of its own (`upload: sending 1.2 MB`,
`upload: job 42 queued`, the site's counts or why it stopped), which stays until the next `u`. A
second `u` while one is under way is refused. Quitting abandons an upload still being sent, and the
site queues nothing; one the site already queued is recorded in the capture.

### Config file

`wartui.toml` holds settings an operator wants to stop typing every run:

```toml
pool = "us"
excluded-ssids = ["HomeNet", "Pixel 9"]

[tx-power]
fleet = 10
bridge = 15

[bluetooth]
remember = true
node = "AA:BB:CC:DD:EE:FF"

[bridge]
remember = true

[api-keys]
wdgwars = ""
```

| Key                    | Default | What it is                                                                     |
|------------------------|---------|--------------------------------------------------------------------------------|
| `pool`                 | `all`   | The channel pool: `us`, `eu`, or `all`; see "Channel pools"                    |
| `excluded-ssids`       | `[]`    | Wi-Fi networks never captured, matched exactly; see below                      |
| `[tx-power] fleet`     | `2`     | Node transmit power (heartbeats, sightings), whole dBm 2–20                    |
| `[tx-power] bridge`    | `2`     | Bridge transmit power (assignments), whole dBm 2–20                            |
| `[bluetooth] remember` | on      | Whether `b` remembers the Bluetooth node; see "Bluetooth"                      |
| `[bluetooth] node`     | —       | The MAC `b` last gave the scan to, written as `wartui ports` prints it         |
| `[bridge] remember`    | on      | Whether `run` remembers the bridge; see "When nothing arrives"                 |
| `[api-keys] wdgwars`   | `""`    | WDGWars API key; settings writes the empty key so there is a place to paste it |

Each power defaults on its own; neither falls back to the other. The firmware accepts 21 dBm, but
nothing above 20 is verified to work. wartui refuses to start on an unknown key or table, naming the
file and line. An out-of-range value, an unknown `pool`, a malformed `node`, a `node` beside
`remember = false`, or an `excluded-ssids` entry that is empty, over 32 bytes, or listed twice is
refused too, naming the file and key. Only `run` and `upload` read the file,
so a broken one cannot stop `ports`, `status`, or `reset`. Settings (`c`) rewrites the whole file
on save, and `b` writes the Bluetooth node; see "Keyboard commands". When the file is missing, `run`
creates it holding only `wdgwars = ""`, except under `--sim`. A new file is readable only by its
owner.

A Wi-Fi sighting whose SSID equals an `excluded-ssids` entry is dropped before wartui counts or
records it, so it never reaches the capture, the live stream, an export, or an upload. An SSID
excluded mid-run is dropped from its next sighting on. Sightings recorded before then stay in the
capture, but `export`, `analyze` and `upload` leave out every sighting of an SSID excluded when they
run. The match is exact and case-sensitive: `homenet` does not exclude `HomeNet`. Bluetooth names
are never matched. `--record-raw` keeps whole frames, so those still hold an excluded network's
bytes.

| OS               | Default path                                                                                                     |
|------------------|------------------------------------------------------------------------------------------------------------------|
| macOS            | `~/Library/Application Support/wartui/wartui.toml`                                                               |
| Linux and others | `$XDG_CONFIG_HOME/wartui/wartui.toml`, or `~/.config/wartui/wartui.toml` when that variable is unset or relative |

`--config PATH` (a `run`, `export`, `analyze` and `upload` flag) reads another file, for testing
and debugging. A named file that is missing is refused; a missing default file silently means the
built-in defaults.

### Benchmarking the store

The hidden `bench` judges a store change on the card it will run from. It drives the simulator
through the engine into a fresh database, drawing nothing, and reports rows, commit times, and, on
Linux, what the kernel and block device wrote.

```sh
wartui bench --db /path/on/the/card/bench.db --fresh --duration 300 --json
```

`--profile drive` (default) is ten nodes at real time; `burst` is twenty. Every node reports every
sweep in the simulated neighborhood, measuring starts once the fleet holds its assignments, and a
run counts only if `idle_node_windows` reads 0. `--interval` (default 60 s) adds a timeline showing
when the card slowed. Each SQLite and batching setting has a flag, so two settings compare on one
binary. The store checkpoints on its own thread after each commit; `--checkpoint-every MS` spaces
those out, and `--inline-checkpoint` puts SQLite's own back in the commit.
[`docs/store-io-findings.md`](../../docs/store-io-findings.md) has the method and numbers.

### Flashing the fleet

`flash-fleet` puts one node image on every attached board of the chip `--features` names, several at
once, never on the bridge. It uses `espflash`, which must be on `PATH`.

```sh
wartui flash-fleet --features esp32c5 --dry-run     # what would be flashed, and from where
wartui flash-fleet --features esp32c5
wartui flash-fleet --features esp32c6,xiao-external-antenna --bridge 10:BD:A3:EC:44:C0
```

| Flag                    | Default        | What it is                                                                 |
|-------------------------|----------------|----------------------------------------------------------------------------|
| `--features LIST`       | —              | The node firmware's features; exactly one of `esp32c5` or `esp32c6`        |
| `--no-default-features` | off            | Build without the firmware's default features                              |
| `--bridge PATH\|MAC`    | remembered     | The bridge, which must be attached and is never flashed; `00:08` works too |
| `--no-bridge`           | off            | No bridge is attached; a remembered one is still spared                    |
| `--skip MAC`            | —              | Leave this board alone too; repeatable                                     |
| `--image BIN`           | —              | Flash this merged image rather than building or fetching one               |
| `--firmware-dir DIR`    | the checkout's | Build the image from the node firmware here                                |
| `--jobs N`              | `4`            | How many boards to probe or flash at once                                  |
| `--dry-run`             | off            | Probe and report; build, fetch and flash nothing                           |

**The bridge has to be known.** It shares the nodes' vendor, product, and often chip, so only its
address tells it apart. The run spares the board `--bridge` names and the bridge `run` remembered,
whichever are known; a remembered bridge that is not attached is unplugged, not an error. The run is
refused when:

- Neither is known, unless `--no-bridge` says none is attached.
- `--bridge` matches no attached board, since sparing it would spare nothing. Typical causes: a
  typo, a device node that moved on replug, or a macOS `/dev/tty.*` where `wartui ports` lists
  `/dev/cu.*`. Naming the bridge by address avoids all three.
- A short `--bridge` matches several boards.

A board is flashed only when all five hold; otherwise it is skipped and listed with the reason:

1. It is native USB Serial/JTAG (`303a:1001`) and reports its address as its serial number.
2. It is not the bridge, and not named with `--skip`.
3. `espflash board-info` reads the chip `--features` names.
4. `espflash board-info` reads the same address from the chip that the OS reported for the port.
5. `espflash board-info` reads at least 4 MB of flash, which the node's two app slots need.

The image is merged (bootloader, `firmware/node/partitions.csv`, app) and written at `0x0` with
`espflash write-bin`. Then `espflash erase-region` erases `otadata` (`0xd000`, `0x2000`), so the
board boots the app just written whatever slot an earlier over-the-air update left selected. The
image comes from the first source that applies:

- `--image BIN`, as given.
- `--firmware-dir DIR`, or the checkout's `firmware/node` when this binary was built from one:
  `cargo build --release` there with the same features, merged with `espflash save-image`.
- The release this binary was built for, checked against its `SHA256SUMS` and kept under the state
  directory (`images/<tag>/`) so an offline host can reflash. A release publishes `esp32c5`,
  `esp32c6`, and `esp32c6,xiao-external-antenna`, all with default features; anything else needs
  `--image` or `--firmware-dir`.

The chip in the image's header must match `--features`, or nothing is touched. After flashing, each
node's port is watched for up to 10 s for a line only a boot prints; a node not seen to boot is reset
once more with `espflash reset` and watched again, since espflash's reset does not always take and
the old image runs on until a power cycle. Each board prints a line by its last two octets:

| Line                                                                    | Meaning                                         |
|-------------------------------------------------------------------------|-------------------------------------------------|
| `flashed`                                                               | Written, and seen to boot                       |
| `flashed, but not seen to reboot: power-cycle it to run the new image`  | Written, but the old image may still be running |
| `flashed, but not seen to reboot, and espflash reset failed: …`         | As above, with the reset's full output beneath  |
| `failed: …`                                                             | espflash's last line, full output beneath       |
| `skipped: …`                                                            | Not touched, with the reason                    |

A failure or a board not seen to reboot fails the run; skips do not. `--dry-run` still resets each
candidate once, since `board-info` does.

### Flashing the bridge

`flash-bridge` puts one bridge image on exactly one board, the bridge, through `espflash`.

```sh
wartui flash-bridge --features esp32c6 --dry-run    # which board, and the image's source
wartui flash-bridge --features esp32c5,t-dongle-c5
wartui flash-bridge --features esp32c6 --bridge 10:BD:A3:EC:44:C0
```

| Flag                 | Default        | What it is                                                            |
|----------------------|----------------|-----------------------------------------------------------------------|
| `--features LIST`    | —              | The bridge firmware's features; exactly one of `esp32c5` or `esp32c6` |
| `--bridge PATH\|MAC` | see below      | The board to flash, which must be attached; `00:08` works too         |
| `--image BIN`        | —              | Flash this merged image rather than building or fetching one          |
| `--firmware-dir DIR` | the checkout's | Build the image from the bridge firmware here                         |
| `--dry-run`          | off            | Probe and report; build, fetch and flash nothing                      |

**The board is chosen before anything is reset**, from the first that applies:

1. The board `--bridge` names.
2. The bridge `run` remembered, which must be attached.
3. With none remembered, the only Espressif board attached.

The run is refused when:

- The remembered bridge is not attached, because the board present is known not to be it.
- `--bridge` matches no attached board, or several.
- Several boards are attached and none is known. The refusal lists their addresses; pick one with
  `--bridge`.
- No board is attached.

The chosen board must be native USB Serial/JTAG (`303a:1001`) reporting its address, and
`espflash board-info` must read the `--features` chip and that address, or nothing is flashed.

The image comes from the same three sources as `flash-fleet`'s, with `firmware/bridge` for
`firmware/node`. A release publishes `esp32c5`, `esp32c5,t-dongle-c5`, `esp32c6`, and
`esp32c6,xiao-external-antenna`, under the same `images/<tag>/`.

The board prints one line by its last two octets: would flash, flashed, or failed with espflash's
full output. A board flashed without `--bridge` is remembered as the bridge, so `flash-fleet` spares
it and `run` opens it first; one named with `--bridge` is not. `--dry-run` resets it once, for
`board-info`, and remembers nothing.

### Updating over the air

`ota` updates the nodes' firmware through the bridge, with no cable to them. It listens for
heartbeats, shows what it would do with each node, asks, then updates the chosen nodes one at a
time. Nothing else may hold the bridge meanwhile, so stop `run` first.

```sh
wartui ota                                    # every node heard
wartui ota --node A1:B0 --node 57:84          # just these two
wartui ota --image wartui-node-app-esp32c5.bin --yes
```

| Flag                 | Default          | What it is                                                     |
|----------------------|------------------|----------------------------------------------------------------|
| `--bridge PATH\|MAC` | detected         | The bridge, by path, address or `00:08`                        |
| `--node MAC`         | every node heard | A node to update, by address or the end of one; repeatable     |
| `--image BIN`        | —                | Send this app-only image, to the nodes of the board it is for  |
| `--firmware-dir DIR` | the checkout's   | Build each board's image from the node firmware here           |
| `--listen SECS`      | `6`              | How long to gather heartbeats before choosing                  |
| `--force`            | off              | Also update a node on the image's version, or a newer one      |
| `--yes`, `-y`        | off              | Update without asking                                          |
| `--sim N`            | —                | Update a fake fleet, running an older version, instead         |

**The image is app-only**: what one app slot holds, not the merged image `flash-fleet` writes.
Its board and version come from the app descriptor inside it, never from its file name. A merged
image, one built for no node board, or one over 1920 KiB is refused. So is a corrupt one: its
segment checksum and appended SHA-256 are checked as the bootloader checks them, before anything
is sent. Each board gets its own image, from the first source that applies:

- `--image BIN`, sent only to nodes of the board its descriptor names.
- `--firmware-dir DIR`, or the checkout's `firmware/node` when this binary was built from one:
  `cargo build --release` with that board's features, saved with `espflash save-image` without
  `--merge`.
- The release this binary was built for: `wartui-node-app-<variant>.bin`, checked against its
  `SHA256SUMS` and kept under `images/<tag>/` like `flash-fleet`'s.

**The table comes first.** Each node heard, or each one `--node` names, gets a line:

```
node     board           version   image     action
A1:B0    esp32c5         0.1.0     0.1.1     update
57:84    esp32c6-xiao    0.1.1     0.1.1     skip: already 0.1.1
```

| Action                            | Why                                                                       |
|-----------------------------------|---------------------------------------------------------------------------|
| `update`                          | The image is newer than what the node runs, or `--force`                  |
| `confirm`                         | The node runs the image's version on trial; it is confirmed, not resent   |
| `skip: already 0.1.1`             | The node runs the image's version                                         |
| `skip: older image (use --force)` | The node runs a newer version than the image                              |
| `refuse: no image for <board>`    | `--image` is for another board, or no node firmware is built for this one |

Only the heartbeat's version, board and flags are read, so a node `run` shows as `needs update` is
one `ota` reaches. Without `--yes`, `ota` asks `[y/N]` before sending anything.

**Each node then gets a progress line**: how much of the image it holds, the rate, and the phase
(`waiting for the node`, `sending`, `verifying`, `rebooting`, `confirming`). It ends `done 0.1.1
(confirmed)` or `failed:` with the reason. A node that reboots mid-transfer is picked up again
on its next heartbeat, and the image starts over from the beginning. Any failure makes the run
exit non-zero, and ctrl-c aborts the node being updated and stops. With `--log-file` and
`RUST_LOG=wartui_core=debug`, the log holds every frame sent, every send result, every status
from the node and every phase change, as `key=value` fields.

**How long it takes.** The first frame waits for the node's next heartbeat: up to 5 s on an
assigned node, a second on a parked one. Each 1 KB chunk then costs about 3.5 ms on the link, and
at each 4 KB sector boundary the host waits for the node to erase and write the sector: about
29 ms on a C6 and 68 ms on a C5 when the slot holds an old image. An image of 850 to 880 KB takes
about 10 s to transfer to a C6 and 18 s to a C5, and the reboot and confirmation add a few seconds
more. A node's first update after a USB flash writes into a blank slot and runs about twice as
fast ([`docs/ota-findings.md`](../../docs/ota-findings.md)).

**Trial, confirmation and rollback.** A verified image is switched to at once, and the node
reboots into it on trial: the fleet table shows `;trial` after its version. The first frame it
hears from the host confirms it, and `ota` sends one, so `done` means the node heartbeated the new
version with the trial over. A node that boots the new image three times without hearing the host
goes back to the old one by itself. A node with no USB cable is never left without a working
image.

A node still on trial is updated like any other: the update's first frame confirms the image it
runs, and the new one goes into its other slot. One on trial on the image's own version, as an
update whose confirmation went unheard leaves it, is only confirmed (`confirm`).

| Failure                                       | What to do                                                                       |
|-----------------------------------------------|----------------------------------------------------------------------------------|
| `no answer to the update request in 30 s`     | Check `wartui sniff` hears it, and run again; persisting, `flash-fleet` it       |
| `the node refused the update: …`              | Its partition table has no `trial` sector, or its flash failed: `flash-fleet` it |
| `the image is larger than the node's …`       | Build a smaller image; the slot holds 1920 KiB                                   |
| `the node could not write its flash`          | Run again; if it repeats, `flash-fleet` it                                       |
| `the image arrived corrupted …`               | Run again                                                                        |
| `node not answering`                          | It kept dropping the update; move it nearer the bridge, check its power, rerun   |
| `the node could not switch to the new image`  | `flash-fleet` it                                                                 |
| `no heartbeat from the new image within 60 s` | Power-cycle it; after three boots without the host it returns to the old image   |
| `still running …`, `rolled back to …`         | The new image did not run on that node, which is back on the old one             |
| `the new image was never confirmed; …`        | It returns to the old image after three boots; `run` confirms it if it can       |

## Keyboard commands

| Key                    | What it does                                                                 |
|------------------------|------------------------------------------------------------------------------|
| `↑` `↓` / `k` `j`      | Move the cursor through the fleet table                                      |
| `b`                    | Toggle Bluetooth scanning on the selected node                               |
| `c`                    | Open config (settings)                                                       |
| `g`                    | Show GPS diagnostics: fix and satellites                                     |
| `r` / `R`              | Clear the selected node's dedup ring, or every assignable node's             |
| `u`                    | Upload this capture to WDGWars, after asking                                 |
| `q` / `Esc` / `ctrl-c` | Stop, committing the last batch                                              |

Only `b`, `c`, `r`, and `R` reach the air, and no key sets channels. `b` picks _which_ node scans
Bluetooth; the planner still authors every share. With `remember bt node` on, `b` also writes its
choice, or its absence, to `[bluetooth]` in `wartui.toml`, leaving the rest of the file alone. `r`
and `R` make a node forget every address it has reported, on its next heartbeat.

`u` asks before it sends, sends only what is new since the last upload, and leaves the capture
running; see "Uploading to WDGWars".

### In settings (`c`)

| Key                 | What it does                                                       |
|---------------------|--------------------------------------------------------------------|
| `↑` `↓` / `k` `j`   | Move between rows                                                  |
| `←` `→` / `h` `l`   | Change the selected row; values wrap at their ends                 |
| Type / paste        | On the `wdgwars` row: enter the key (`Backspace`, `ctrl-u` clears) |
| `Enter` / `→` / `l` | On the `excluded ssids` row: open its page                         |
| `Enter`             | Apply every row and save it to `wartui.toml`                       |
| `Esc` / `q`         | Close without changing anything                                    |
| `ctrl-c`            | Quit                                                               |

On the `excluded ssids` page:

| Key                    | What it does                                                      |
|------------------------|-------------------------------------------------------------------|
| `↑` `↓` / `k` `j`      | Move through the list                                             |
| `a`                    | Start typing an SSID; type or paste it, then `Enter` adds it      |
| `Backspace` / `ctrl-u` | While typing: delete a character, or clear the line               |
| `Esc`                  | While typing: stop without adding                                 |
| `d` / `Delete`         | Remove the selected SSID                                          |
| `Enter`                | Apply and save the list to `wartui.toml`, then return to the rows |
| `Esc` / `q`            | Exit this menu, dropping this page's edits                        |

The rows are the pool (`all → eu → us`), the nodes' transmit power and the bridge's (1 dBm steps),
`remember bt node` (`off`, `on`), `remember bridge` (`off`, `on`), `excluded ssids`, and the
`wdgwars` API key. `excluded ssids` shows how many are listed, or `none`, and opens a page that
lists them. While an SSID is being typed there, every letter types, spaces included. `Enter` refuses
one that is empty, over 32 bytes, or already listed, and says why. `Enter` in the list sends it to
the engine, writes only `excluded-ssids`, and returns to the rows; edits to the rows stay pending.
On the key row, letters type, so `h` `j` `k` `l` `q` neither move nor close; `↑` leaves it, and the
footer shows the `ctrl-u` tip while a key is entered. The key shows its last three characters, up to
six bullets before them, and from ten characters its first one to three. `Enter` sends the rows to
the engine and writes every row shown to `pool`, `excluded-ssids`, `[tx-power]`, `[bluetooth]`,
`[bridge]`, and `[api-keys]`, whatever was there. The bridge takes its power on its next status
poll, and settings closes. If the engine is not accepting commands, nothing is sent or written, and
settings stays open with its edits. A moved pool re-cuts the fleet. Each node takes its new share
and power on its next heartbeat, emptying its dedup ring when its share changes.

## How channels are assigned

**The planner cuts the pool, and nothing else does.** It deals the pool across every heartbeating,
sniffing node. It re-cuts when that set changes, the Bluetooth scan moves (§ "Bluetooth"), or the
pool changes in settings. The header shows its inputs: `auto — 4 of 5` is four heartbeating nodes of
five seen.

**A changed share waits for the node's admin window.** The radio is away scanning except for the
100 ms it holds open every 5 s, so an assignment takes up to one heartbeat interval, not a sweep.
That delay is the protocol, not lag. The `channels` column leads with a count (`1: 1…`) because a
round-robin share is a dozen scattered channels, too many to fit.

**An assignment counts once the node's radio acknowledges it** at the MAC layer, never on the
bridge's report of a successful enqueue. An ack does not prove the node took it. Adoption does:
every heartbeat carries the epoch the node holds, usually the new one within 5 s of the window. A
heartbeat reporting some *other* epoch makes wartui re-send in the window it opened. The footer
counts those as "assignments acknowledged but not adopted", apart from `no admin ack`'s
never-acknowledged ones.

**Repeated `no admin ack` is nearly always Bluetooth**, sharing the antenna through the admin window
(§ "Bluetooth"). Press `b` on the node. The scanning node itself rarely shows this: its Wi-Fi radio
never leaves the control channel.

## Bluetooth

**At most one node scans Bluetooth, none by default, and it is that node's whole job.** `b` on a
node gives it the scan; `b` on the node holding it takes it off the fleet. With `remember bt node`
on (the default), `b` saves the choice. The remembered node takes the scan back whenever it is
heartbeating and nothing holds it: at startup, or on returning to the fleet. Taking the scan off
with `b` forgets it. Turning `remember bt node` off forgets the node and leaves the scan; turning it
on remembers whoever holds it then. That node's `channels` reads `bluetooth`, since it is dealt no
channels, or `bluetooth…` while the change waits for its admin window. Taking the scan off shows as
its new share, pending.

**It costs a whole node because Wi-Fi and Bluetooth share one 2.4 GHz antenna.** A node that also
sweeps must hand the antenna back for every admin window; a stock node failed to, acknowledging none
of thirty-two assignments ([`docs/phase-2-findings.md`](../../docs/phase-2-findings.md)). With
nothing else to do, the scan runs **back to back**, competing only with the node's own transmits.

The fleet is one sniffer short, and the pool is re-cut the moment the scan moves: giving it away
grows every other share, taking it back shrinks them. A fleet of **one** node holding the scan
sweeps no Wi-Fi at all, and the fault box says so.

## Channel pools

`pool = "all"` is the default: everything a node can tune, 2.4 GHz 1–13 and all of 5 GHz including
UNII-4 (169, 173, 177). A node only parks and reads beacons, so a pool sets where it listens, never
what it emits: the choice is coverage, not legality.

| Pool  | 2.4 GHz | 5 GHz  | Channels |
|-------|---------|--------|----------|
| `all` | 1–13    | 36–177 | 41       |
| `us`  | 1–11    | 36–165 | 36       |
| `eu`  | 1–13    | 36–140 | 32       |

`us` is what the FCC permits: no 12, 13, or UNII-4. `eu` is what ETSI permits: 5 GHz stops at 140,
because 144's twenty megahertz cross the 5725 MHz edge and 149 up is another band. Channel 14 is in
no pool; `esp-radio` cannot reach it.

An assignment's forty-two-bit mask can name any subset. The planner deals round-robin, pool index
_k_ to node _k mod n_, so every node carries both bands. A block split would put one node on all of
2.4 GHz, and losing it would blind the fleet to that band until the next re-cut landed.

**An ESP32-C6 is never dealt a 5 GHz channel.** It has no 5 GHz radio and says so in every
heartbeat. A mixed fleet is dealt 5 GHz first; in pool order the C5s would take 2.4 GHz and then all
of 5 GHz, a block split by another route. Shares can then differ by more than one channel: a C6
beside a C5 holding 5 GHz sweeps faster however the rest is dealt. The deal minimizes the _largest_
share, which sets how stale the slowest node's observations get. A fleet of alike radios has no
constrained channels. With no 5 GHz radio at all, 5 GHz is left out of every assignment, and the
footer counts the unscanned channels.

Every fleet change re-cuts the _whole_ fleet, and each node takes its share in its next admin
window, so the fleet converges within one heartbeat interval. A node adopts only an epoch that
differs from its own, so an unchanged fleet is left alone. Two consequences:

- **A node can report a channel outside its share.** 2.4 GHz channels are interleaved (one node 1,
  3, 5; the next 2, 4, 6), 5 MHz apart but 20 MHz wide, so a node on 2 hears 1 and 3. A strong
  access point is heard well beyond its own channel, and an access point may send copies of its
  beacon across its width. A node reports the channel the beacon names when it is a scan-table
  channel the node could have heard: any 2.4 GHz channel when parked on 2.4 GHz, or one within
  160 MHz on 5 GHz, so a 5 GHz node can report a nearby 5 GHz channel too. Otherwise it reports the
  channel it was parked on, which covers vendor gear that names a number off the channel list.
  About a quarter of access points reach more than one node. `export` keeps the strongest per
  recapture window, so this costs only store rows.
- **A fleet with no 5 GHz radio covers only 2.4 GHz**: 13 channels on `all` and `eu`, 11 on `us`.
  From fourteen such nodes (twelve on `us`), nodes outnumber channels. Surplus nodes keep what they
  last held and double up, because the only channel-less frame means "Bluetooth is the whole job",
  not "stop". A node with nothing to keep, such as the former Bluetooth scanner, is handed every
  channel its radio can reach, and its `channels` shows the whole pool once adopted. So is a surplus
  node whose channels a newly chosen pool leaves out.

## Positions

Each observation gets the best position available, resolved fresh: GPS, then `--lat`/`--lon`, then
none. Rows record which tier answered, so a garage-to-road capture is honest about both. No record
is dropped for lack of a position, but **WiGLE rejects rows without coordinates**: a capture with
none exports nothing and says how many networks it left out.

- **`--lat`/`--lon` is for testing only.** WDGWars and WiGLE ban fixed positions, so the capture is
  marked as test data: `upload` refuses it, and `export` warns.
- **A receiver is found without being named.** Each serial port that is not a fleet board is tried
  at 9600, 38400, 4800, then 115200 baud. The first with two checksum-valid sentences is kept.
- **Self-naming ports (`u-blox`, `GPS`, `GNSS`) only go first.** Common pucks sit behind a generic
  USB-to-UART chip that names nothing, so reading the port settles it.
- **The search writes nothing and never opens an Espressif port.** That keeps it clear of the bridge
  search, which transmits into whatever it opens.
- **`--gps PATH` pins one receiver**, when several are attached or the search picks wrong. Baud
  rates are still tried unless `--gps-baud` names one, and one valid sentence settles a named port.
- **`--gps` with `--gps-baud` skips the probe.** Nothing is left to detect, and a terse receiver
  could otherwise be refused for saying too little in one window.
- **`--no-gps` turns the search off; `--sim` implies it unless `--gps` is given.** A simulated fleet
  is for working with nothing plugged in, not for opening every serial port.
- **Any NMEA 0183 serial receiver works.** `GGA` and `RMC` give the position. Altitude, satellite
  count, and an accuracy estimated from HDOP reach the export. `GSA` and `GSV` are read for `g` only.
- **`g` shows why a receiver will not lock.** The left side has the fix: 2D or 3D, satellites used,
  time, position, speed, the HDOP accuracy estimate (not a measurement), and the port. The right side
  lists each satellite in view with its signal strength and whether the fix uses it. `Esc`, `q` or
  `g` closes it.
- **A named receiver that is missing is a fault; an unfound one is not.** Most GPS-less captures
  never had one, so the header stays quiet; `--gps` asks for a port, so its absence is reported.
- **A fix must be recent.** Past `--gps-max-age` seconds the position falls back a tier and the
  header says `gps fix is stale`. At driving speed a minute-old fix is another neighborhood, and
  falling back beats quietly claiming it. The reader logs that age limit too, even when the receiver
  is silent. The last fix is retained; a new fix logs recovery.
- **The receiver runs on its own thread; nothing waits for it.** Capture starts at once, and the
  header shows `gps scanning /dev/… @38400`, `gps searching`, `gps ok, 8 sats`, `gps fix is stale`,
  or the port's error. The search keeps running there, so a puck replugged into a *different* socket
  is found under its new name.

## Reading the fleet table

Rows name nodes by chip and last two address octets: `C5 57:84`, `C6 9D:24`. The chip comes from the
board its heartbeats announce, so a node not yet heartbeating shows `—`, a XIAO C6 shows `C6`, and a
board this build has no name for shows `??`.

| State          | Meaning                                                                                                                                     |
|----------------|---------------------------------------------------------------------------------------------------------------------------------------------|
| `alive`        | Heartbeating, so it can be given channels                                                                                                   |
| `stale`        | Still being heard, but not heartbeating — most often Bluetooth coexistence on the node holding the radio through its admin window           |
| `no heartbeat` | Seen, but has never completed a sweep                                                                                                       |
| `no admin ack` | An assignment went out and its radio did not answer — nearly always Bluetooth, see [How channels are assigned](#how-channels-are-assigned)  |
| `refused`      | The bridge's peer table is full                                                                                                             |
| `needs update` | Runs an older major or minor of the node firmware, so wartui cannot read its frames; update it with `wartui ota`                            |
| `host behind`  | Runs a newer major or minor of the node firmware than this wartui drives; update wartui                                                     |
| `rebooted xN`  | Its heartbeat counter went backwards, or its epoch went back to 0, so it has forgotten any assignment; wartui re-issues under a fresh epoch |

The `channels` column shows where an assignment stands
([How channels are assigned](#how-channels-are-assigned)):

| `channels`       | Meaning                                           |
|------------------|---------------------------------------------------|
| yellow, with `…` | Waiting for the node's admin window               |
| blue, with `…`   | Acknowledged by the node's radio, not yet adopted |
| plain            | Adopted                                           |
| cyan             | The Bluetooth node                                |

The line under the table shows the selected node's firmware and its latest supply and die readings:
`C5 57:84  0.1.0;5g  3.30 V  41 °C`, with `—` for a node that has sent none or whose read failed.

The firmware is its version, then `;5g` for a radio that reaches 5 GHz. A node on an older version
than this build's adds an arrow and this build's version: `0.1.2;5g ↑0.1.3`. A different patch is
still driven; an older major or minor is `needs update` and a newer one `host behind`.

The voltage is the chip's 3.3 V rail, not the board's 5 V input, and the die runs well above the
air around it. A reading out of range is yellow on that line, and turns an `alive` node's `state`
yellow too; any other state keeps its own color:

| Reading | Warns at       | Why                                                                                 |
|---------|----------------|-------------------------------------------------------------------------------------|
| Voltage | below 3.00 V   | The C5's and C6's minimum recommended supply; the chip resets at 2.7 V              |
| Die     | 70 °C or above | Short of 85 °C, the XIAO C6's rated ambient limit; the die runs hotter than ambient |

A `stale`, `refused`, `no heartbeat`, `needs update`, or `host behind` node is out of the plan:
nothing sent would reach it, its band is unknown, or its frames are not this build's, so its share
might be nobody's. `b` on one says which state it is, because the fix differs: wait for a
heartbeat, find out why heartbeats stopped, update the node with `ota`, or update wartui. Heartbeats from a
`needs update` or `host behind` node are not counted among the footer's frames from an older
firmware. Once its heartbeat has named its version, its sighting batches are, since none of them
is recorded.

`stale` and `no heartbeat` are deliberately distinct from silence and from each other. wartui keeps
two clocks, so a node that streams observations but loses heartbeats doesn't age out and churn the
whole fleet's topology.

**An `alive` node gone quiet is usually fine.** A node reports each address once and holds it back,
so a stationary node falls silent after reporting everything in range. That memory lives on the node
and survives into your next session. A held address is reported again five minutes after it was last
reported, or sooner if heard at least 10 dB louder. `r`/`R` clear it without a reboot, and a change
of share or Bluetooth role empties it. `crates/wartui-proto/src/dedup.rs` explains why.

| Header says            | Meaning                                                                  |
|------------------------|--------------------------------------------------------------------------|
| `waiting for nodes`    | No node has heartbeated yet this session                                 |
| `N need update`        | Heard only from nodes on an older minor; `N host behind` for newer ones  |
| `all nodes silent`     | Nodes were seen, but none is heartbeating now                            |
| `N alive, none usable` | Heartbeating, but none can be assigned channels; the node table says why |

The footer shows faults only once they happen, so a clean run has a clean footer:

- **`lost`** (a column, and `lost N` summed in the footer): whole batches missing between node and
  host, from gaps in each batch's sequence number. The column reads `—` before a node's first batch,
  then a running count; both appear once any node loses one. A sequence advances only on a MAC-layer
  ack, so a gap is lost after the bridge's radio took it, in its receive queue or on USB, not on the
  air. A batch is up to about a dozen access points, hidden until the node's five-minute refresh.
  Each gap is a `batch_gap` row; summing its `lost` per node gives the column's final value. A gap
  after a batch replayed from the bridge's backlog is not counted, because those batches were
  dropped while no host was reading.
- **`dup N`**: batches dropped as identical to the previous one (same `seq`, same bytes, within
  100 ms), a radio retransmitting after the bridge's ack was lost. The first copy was recorded, so
  nothing is lost and `dup` never overlaps `lost`. A repeat 100 ms or more later is the node's own
  re-send after a failed send, recorded normally.
- **`buffer full wifi N  ble M`**: a line below the totals, counting distinct networks and
  advertisers a node heard but had no room for in that dwell or scan, fleet-wide, this session. It
  appears when a count passes zero, omits a kind still at zero, and stays. Not a fault: an address
  turned away is not held back, so the next pass reports it; only one never heard again is lost.
  A steadily growing count means a buffer too small for the area.
- **`N frames from a vendor fleet`**, **`N admin frames from another core`**: another fleet
  transmitting on the nodes' channel.
- **`N frames from an older firmware — reflash`**: a fleet mid-upgrade, speaking a wire format this
  host does not, so it appears only here.
- **`bridge dropped`**: frames lost since this host attached. `wartui status` reports the total
  since the bridge booted, which is large and harmless on a dongle left powered with nothing
  listening. Every status reply is kept as a `bridge_status` row, with its since-boot counts.
- **`57:84 at 2 dBm, assigned 6 dBm`**: that node's radio reports a transmit power other than the
  assignment it holds. It refused the power and kept its previous one. `57:84 cannot read its
  transmit power` means it could not report one at all. Power-cycle that node; if it persists, it
  is that board. The line goes once the node reports its assigned power.
- **`4F:08 restarted: brownout`**, with `(×3)` after repeats: a node restarted during the run for a
  fault, and the line stays for the run. `brownout` means check that node's supply; `software`,
  `watchdog` or `lockup` is the firmware; `unknown` is a cause this build cannot name. A power-on
  or a reset over USB is not shown.

## The bridge panel

A LilyGO T-Dongle-C5 has a screen, and a bridge flashed for it shows five lines of the capture. It
needs no setup: the bridge reports a panel when it announces itself, `wartui status` prints that,
and a bridge without one is sent nothing.

| Line              | What it says                                                         |
|-------------------|----------------------------------------------------------------------|
| `gps ok, 9 sats`  | Where the position is coming from                                    |
| `nodes 3`         | How many nodes are heartbeating, and how many of those can be driven |
| `APs ~12.3k`      | Distinct Wi-Fi access points this session, estimated                 |
| `BLE ~840`        | Distinct BLE addresses, estimated the same way                       |
| `avg -55 min -72` | How strongly the *bridge* is hearing the fleet                       |

The figures are the view's, from the same snapshot, about once a second. The screen is seventeen
characters wide, hence the terse wording. `nodes 2 of 5` means 2 drivable of 5 alive. The RSSI line
drops its label for two figures and reads `rssi -55` when average and weakest match. −100 dBm or
worse prints `BAD`: it does not fit, and the exact figure means nothing well before that.

**Each line is colored by its own state**, readable across a car: green fine, amber working but not
ideal, red a fault.

- **GPS is green only on a live, current fix**, stricter than the view. Connecting, scanning,
  searching, or a stale fix is amber: attached and trying. No receiver, an unreadable port, or a
  pinned `--lat`/`--lon` is red, however deliberate: none will produce a fix, and a constant
  position is what most needs noticing from a distance.
- **Nodes** are amber when something heartbeating cannot be driven (so both counts print), red when
  nothing is alive or the link is down. Nodes on another major or minor are named instead,
  `2 need update` or `1 host behind` (`3 undrivable` for a mix): red when nothing can be driven,
  amber otherwise.
- **RSSI** is amber when the average falls below −65 dBm or any node below −70, both above the
  −74 dBm a 24 Mbps ESP-NOW link needs. So it warns while there is time to act: close a window, move
  the dongle off the floor, walk a node back. `rssi: none heard` is red, for live nodes the bridge
  has not measured; `rssi n/a` means nothing is alive.
- **AP and BLE counts** have no bad state and stay green.

With no host, and for about ten seconds after one leaves, the bridge shows its own chip, address,
channel, and uptime in amber, since no host is exactly "working but not ideal". A capture takes the
panel back within a second, with no replug or reflash.

## When nothing arrives

**wartui finds the bridge by asking.** It opens each attached Espressif board in turn (a USB-plugged
node is one) and keeps the first that answers the link protocol. It saves that address to
`~/.local/state/wartui/bridge`, so later runs open only that port. A capture given `--bridge` saves
the named board once it answers, since naming it for a capture is choosing it. `status`, `sniff`,
and `reset` with `--bridge` save nothing, so questioning one board changes nothing later. An
answering board is held for the run: unplug the bridge to reflash a node, and wartui waits rather
than transmit into it.

That file is state, not settings. Deleting it is always safe, costs one slower start, and fixes
wartui opening the wrong board. It also self-corrects: an answering board overwrites it, and the
remembered board is dropped if, opened with no other to try, it stays silent, as a reflashed bridge
does. A board merely passed over in a sweep is kept; being slower than its neighbor proves nothing.
With `remember bridge` off (settings, or `[bridge] remember = false`), `run` deletes the file, saves
nothing, and sweeps on every start.

The header's `waiting for a bridge to announce itself` has three causes; the fault box names it:

- **`link down: could not open …`**: the port is not ours. Nearly always another `wartui`, `screen`,
  an IDE's serial monitor, or ModemManager holds it. ModemManager clears on its own: where it runs,
  it opens each new CDC-ACM device for a few seconds to ask whether it is a modem.
- **No fault**: the port opened and the dongle is not answering.
- **`swept N boards and none answered`**: no attached Espressif board is a bridge.

With no fault, the bridge is usually deaf in one direction: its USB transmit endpoint stopped
draining while it keeps reading. The firmware notices within three seconds and reboots, showing
`bridge rebooted itself: USB transmit had stalled`. Otherwise `wartui reset` reboots it, which works
because the receive half still runs, and keeps the device path. `espflash reset --port …`
re-enumerates and can move `ttyACM0` to `ttyACM1`; `--bridge 10:BD:A3:EC:44:C0` names the board
either way. `espflash` is the fallback for a bridge that answers nothing; unplugging is last.

```sh
wartui status                          # exits in 5 s with the reason
wartui reset                           # reboot a bridge that stopped answering
wartui --log-file wartui.log run
rm ~/.local/state/wartui/bridge        # forget which board was the bridge
```

`wartui reset` takes about thirty seconds to fail on a board that is not a bridge. The board it is
for answers nothing, so it transmits before anything has identified itself. A board that is not
reading takes one packet and queues the rest, and closing the port waits for them.

`run` never pays this cost. It asks with one frame, however long it then waits. That one frame is
enough for a wedged bridge, which reboots three seconds after a frame it cannot answer.

`wartui reset` never sweeps, because a `Reset` reboots a node and costs it the addresses it held
back. It goes to the `--bridge` board, else the remembered one, else the only one attached. With
several and none known, it asks for `--bridge` rather than guessing.

**`--log-file` is the only way to see the transport's own account of a run**: the view owns the
terminal, so without it nothing is logged. It records which port was resolved, whether it opened,
and why a link went down, once per reason, since somebody else's port is retried every 750 ms all
capture. The GPS reader logs searching, its first fix, stale fixes, recovery, and unreadable or
removed receivers on transitions, not every sentence or satellite-count change. Reopening a failed
port does not count as recovery until a valid fix or no-fix report arrives. The log also records each
time the host falls 100 ms or more behind the bridge and when it catches up, with both frames' bridge
stamps. A reconnect while frames are arriving can log one spell of about 100 ms, because the lag after
a connect starts from the 100 ms the engine assumes rather than from a measurement. Each node's
membership changes are logged once each:

| Message               | When                                              |
| --------------------- | ------------------------------------------------- |
| `node joined`         | its first heartbeat this run                      |
| `node left the fleet` | its peer is removed after a minute of silence     |
| `node returned`       | its first heartbeat after `node left the fleet`   |
| `node restarted`      | its heartbeat counter fell, or its epoch reset    |

`RUST_LOG=debug` adds GPS probe attempts, each retry, undecodable frames, and dropped bulk
commands.

### Telling the boards apart

A USB-plugged node has the bridge's vendor and product ID and an adjacent device node, so a path
says nothing about which board it reaches. `wartui ports` names them:

```
$ wartui ports
/dev/serial/by-id/usb-Espressif_USB_JTAG_serial_debug_unit_10:BD:A3:EC:44:C0-if00
  10:BD:A3:EC:44:C0     USB JTAG/serial debug unit  (303a:1001)
/dev/serial/by-id/usb-Espressif_USB_JTAG_serial_debug_unit_02:00:5E:10:9D:24-if00
  02:00:5E:10:9D:24     USB JTAG/serial debug unit  (303a:1001)
```

**An ESP32's USB serial number is its MAC**, so the OS already pairs each device node with its
radio's address: nothing opened, no `esp` tool, no reflash. The command rests on that, on Linux and
macOS alike. It is also why each board has its own `by-id` name, built by udev from the serial.
Other USB devices carry a manufacturing serial; one reporting none reads `address not reported` and
is named by its socket.

The bridge is the row with the address the fleet table shows for the bridge; a node is a row whose
heartbeats `wartui sniff` attributes to its address. Across board generations the OUI differs too.
Either name works as `--bridge`, as does an address's last one to five octets (two hex digits each)
when exactly one board matches. `--bridge 44:C0` is how a board is read off the fleet table. Several
matches are refused with a list, never guessed. Keep the address: a device node moves when the board
re-enumerates and a `by-path` name moves with the cable, but an address moves only with the board.

```sh
wartui --bridge 10:BD:A3:EC:44:C0
wartui status --bridge 44:C0
```
