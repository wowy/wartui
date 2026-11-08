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
| `sniff`        | Print every record the bridge's nodes send, decoded, a line per sighting            |
| `status`       | Ask the bridge for its counters and uptime                                          |
| `reset`        | Reboot a bridge that has stopped answering                                          |
| `power-cycle`  | Switch one node off and on through the carrier board; see "Power-cycling a node"    |
| `fan`          | Read or set the carrier's lid fan duty; see "The lid fan"                           |
| `pool`         | Read or set the bridge's channel pool; see "The channel pool"                       |
| `bluetooth`    | Read or set the slot whose node scans Bluetooth; see "Bluetooth"                    |
| `ports`        | List the Espressif boards and CH343s attached, and the address of each              |
| `flash-fleet`  | Flash node firmware onto every attached board of one chip; see "Flashing the fleet" |
| `flash-bridge` | Update the P4 bridge's firmware over its link; see "Flashing the bridge"            |
| `ota`          | Update node firmware through the bridge; see "Updating over the air"                |

`sniff` names each record by the slot it came from, as the fleet table does, and each heartbeat by
the node's address. It also prints a `# status` line every 10 s: a connection that has sent nothing for 10 s asks
for one, since the P4 stops writing to a host it has not heard from in 15 s.
A node is reached through the slot its latest heartbeat came from, so moving a node to another
slot moves its assignment with it on its next heartbeat.

`run` takes:

| Flag                    | Default     | What it is                                             |
|-------------------------|-------------|--------------------------------------------------------|
| `--db PATH`             | dated       | New file for the capture; an existing one is refused   |
| `--bridge PATH\|MAC`    | detected    | Which board the bridge is, by path, address or `00:08` |
| `--lat` `--lon` `--alt` | —           | A fixed test position; the capture is never uploaded   |
| `--gps PATH\|bridge`    | detected    | An NMEA port, or `bridge` for the bridge's alone       |
| `--no-gps`              | off         | Read no receiver, the bridge's included                |
| `--gps-baud N`          | detected    | Line rate of that receiver                             |
| `--gps-max-age S`       | `5`         | How old a fix may be before falling back               |
| `--sim N`               | —           | Run a fake fleet instead of hardware                   |
| `--record-raw`          | off         | Also keep the undecoded bytes of every frame           |
| `--notes TEXT`          | —           | A note about this run, stored with the capture         |
| `--config PATH`         | per-OS path | See "Config file"                                      |

The bridge GPS rate has no flag; see "Config file". The channel pool has a command, `wartui pool`,
and a settings row; see "The channel pool". `--log-file PATH` is global; see "When nothing arrives".

### Capture metadata

The capture records the host build and its settings in SQLite metadata:

| Key | Meaning |
|-----|---------|
| `capture.host_version` | Host crate version |
| `capture.host_build` | Content-derived host source identity, including uncommitted edits |
| `capture.core_version` | Core crate version |
| `capture.release_tag` | Release tag, when the host was built with one |
| `capture.bus_version` | The node-bus version the host understands |
| `capture.settings.initial` | Initial host settings, normalized by the engine |
| `capture.settings.NNN` | Runtime's first settings snapshot and subsequent changes, in capture order |

Settings include the channel pool, raw recording, positioning configuration, the Bluetooth slot
(`ble_slot`, by its bus name) and the node holding the scan (`ble_node`). The pool and the slot are
the ones the bridge reports, `none` before its first status; the rest are desired host settings.
None of them says a node adopted anything.
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

`analyze` prints what `export` would report for a capture, then how fast sightings arrived, then
what it lost on the way in. It takes `--db`, `--recapture` and `--config` as `export` does. Reports
go to standard output and quality warnings to standard error; no output file is written. The tail
of its output, after the export summary:

```
  peak       1,842 sightings/s  41,210/min
  peak wifi  1,790/s  39,880/min
  peak ble   120/s  2,310/min
  peak by node
    1C:5A  120/s  2,310/min
    57:84  1,790/s  39,880/min
  bridge     1,203,551 frames received  1,300 dropped (0.1%)  1 reboot  host read 1,202,240  11 lost on USB
  queue      host-bulk  peak 245,760/245,760 bytes  1,300 dropped
  queue      node-down  peak 3,102/4,160 bytes  0 dropped
  radio link 48,210 packets  2 crc_fail  1 uart_errors  4 radio_seq_gap
  host link  0 host_uart_errors
  host       147 not stored: 140 retries  7 incompatible  0 store dropped  behind ≥100 ms 31 times (worst 412 ms)  slowest commit 58 ms  queue peak 1,204
  health     under-voltage in 3 samples  throttled in 0  since boot: under-voltage  temp max 71.2 °C  battery min 3.62 V
  batches    212 lost between node and host
  buffer full wifi 18,220  ble 1,203
  loss by node
    1C:5A  batches 12  heartbeats 699  buffer full ble 1,203
    57:84  batches 200  heartbeats 2,757  buffer full wifi 18,220
```

| Line           | What it counts                                                                  |
|----------------|---------------------------------------------------------------------------------|
| `peak`         | The most sightings in any one second and any one minute, across the fleet       |
| `peak wifi`    | The same for Wi-Fi sightings alone; `peak ble` for BLE                          |
| `peak by node` | The same for each node, by address                                              |
| `bridge`       | Frames the bridge received, dropped as the host fell behind, and lost on USB    |
| `queue`        | One of the bridge's queues: the most it held against its size, and its drops    |
| `radio link`   | Packets on the P4–C6 link, and its link errors that are not zero                |
| `host link`    | Host-link reads on the P4 that saw line errors or overflows                     |
| `host`         | Frames the host read but did not store, and how the host and its store held up  |
| `health`       | Under-voltage and throttling the Pi firmware reported, temperature, battery     |
| `batches`      | Sighting batches lost between node and host; the fleet table's `lost`           |
| `buffer full`  | Sightings a full pending buffer refused; most reappear in a later dwell or scan |

Peak windows slide rather than fall on clock seconds, so a burst astride a second counts once.
They are half-open: a sighting and one exactly a second earlier share no one-second window, so a
steady 1 Hz source reads `1/s`. Every stored sighting counts, including those an export leaves out,
since the figure is what came over the link. Time is the host's arrival time, so a backlog the
bridge replays after a reconnect counts as the burst it was on the link. Where the wall clock steps
back, every window restarts and the `peak` line says how many times `wall clock reversed`; a peak
around the step is a lower bound.

Counts follow the assumptions below; `lost on USB` is approximate. The capture's first bridge
reply and first heartbeat per node are baselines, so what was dropped before the capture began
is not counted. `batches` and `bridge` print zeros, since "0 lost" is the answer. So do
`store dropped`, `queue` drops, `host link` and, on a Pi, the `health` counts. A `queue` peak is
the most the queue held since the bridge booted, so it can predate the capture. Left out:

- the `peak` lines, when the capture holds no sighting
- `peak wifi` or `peak ble`, when the capture holds no sighting of that kind
- the `bridge` line, when the capture holds no status reply
- `queue` lines, when the bridge reports no queues
- `buffer full` figures that are zero, and the `buffer full` line when all are
- the reboot clause, when the bridge never restarted
- the `lost on USB` clause, when it is zero
- the `oversize (not a node's)` clause, when it is zero. It counts node records longer than any
  node of this build sends, which the bridge dropped and left out of `frames received`
- `host` clauses that are zero, and the `host` and `health` lines when the capture holds no host row
- the `health` line, when nothing could be read; off a Pi it holds only the temperature
- the `battery min` clause, when the host has no hwmon device named `battery`

The host records a `host_status` row when the capture starts, every 5 s, and at shutdown. Each
`bridge_status` row records the frames the host had read when the reply arrived. `frames
received` less `dropped` less `host read` is `lost on USB`, to within the frames queued in the
bridge at the first and last reply, since a reply overtakes them. On the `host` line:

| Clause                 | What it counts                                                                 |
|------------------------|--------------------------------------------------------------------------------|
| `not stored`           | Frames read and deliberately dropped: bus resends, undecodable, incompatible   |
| `store dropped`        | Rows lost because the store's queue was full or a write failed                 |
| `garbled`              | USB frames that failed their checksum                                          |
| `behind ≥100 ms`       | Samples in which the host read the link 100 ms or more behind the bridge       |
| `slowest commit`       | The slowest batch the store wrote                                              |
| `queue peak`           | The deepest the store's queue got                                              |

`under-voltage` and `throttled` count samples with that bit of `vcgencmd get_throttled` set now;
`since boot` lists what the last sample says happened at any time since the Pi booted.
`battery min` is the lowest voltage the battery's fuel gauge read; its current draw is stored as
`battery_ma` for queries but not printed.

`heartbeats` per node is the heartbeats the capture holds. A heartbeat carries no sequence number
of its own, so a lost one is not counted. A restart is the node's sweep counter falling by half its
range or more, which resets the `buffer full` baseline.

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
excluded-ssids = ["HomeNet", "Pixel 9"]

[bridge]
remember = true
mac = "AA:BB:CC:DD:EE:FF"
serial = "5CF7153520"

[gps]
bridge-rate-hz = 5

[api-keys]
wdgwars = ""
```

| Key                    | Default | What it is                                                                     |
|------------------------|---------|--------------------------------------------------------------------------------|
| `excluded-ssids`       | `[]`    | Wi-Fi networks never captured, matched exactly; see below                      |
| `[bridge] remember`    | on      | Whether wartui remembers the bridge; see "When nothing arrives"                |
| `[bridge] mac`         | —       | The bridge last connected to or flashed, written as `wartui ports` prints it   |
| `[bridge] serial`      | —       | The USB serial number of the CH343 that bridge answered on                     |
| `[gps] bridge-rate-hz` | `5`     | Fixes a second from a bridge's GPS receiver, 1–5; see "Positions"              |
| `[api-keys] wdgwars`   | `""`    | WDGWars API key; settings writes the empty key so there is a place to paste it |

wartui refuses to start on an unknown key or table, naming the file and line. An out-of-range value,
a malformed `[bridge] mac`, a `mac` or `serial` beside `remember = false`, or an `excluded-ssids`
entry that is empty, over 32 bytes, or listed twice is refused too, naming the file and key.
`status`, `sniff`, `reset`, `power-cycle`, `fan`, `pool`, `bluetooth` and `ota` read only `[bridge]
mac` and `serial`, and treat a broken file as no bridge remembered, so it cannot stop them.
`flash-bridge` reads only `[bridge] mac`, but refuses a broken file, since it decides which board
gets firmware. `flash-fleet` never reads the file. Settings (`c`) rewrites the whole file on save.
`run` writes `[bridge] mac` and `serial` when the bridge it remembers changes; see "Keyboard
commands" and "When nothing arrives". `flash-bridge` writes both for a board it found and flashed;
see "Flashing the bridge". When the file is missing, `run` creates it holding only `wdgwars = ""`,
except under `--sim`. A new file is readable only by its owner.

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

`--config PATH` reads another file, for testing and debugging. Every command that reads the file
takes it: `run`, `export`, `analyze`, `upload`, `status`, `sniff`, `reset`, `power-cycle`, `fan`,
`pool`, `bluetooth`, `ota` and `flash-bridge`. A named file that is missing is refused; a missing
default file silently means the built-in defaults.

### Benchmarking the store

The hidden `bench` judges a store change on the card it will run from. It drives the simulator
through the engine into a fresh database, drawing nothing, and reports rows, commit times, and, on
Linux, what the kernel and block device wrote.

```sh
wartui bench --db /path/on/the/card/bench.db --fresh --duration 300 --json
```

`--profile drive` (default) is ten nodes at real time; `burst` is fourteen (`plan::MAX_NODES`), the
most a bridge takes. Every node reports every sweep in the simulated neighborhood, measuring starts
once the fleet holds its assignments, and a run counts only if `idle_node_windows` reads 0.
`--interval` (default 60 s) adds a timeline showing when the card slowed. Each SQLite and batching
setting has a flag, so two settings compare on one binary. The store checkpoints on its own thread
after each commit; `--checkpoint-every MS` spaces those out, and `--inline-checkpoint` puts SQLite's
own back in the commit. [`docs/store-io-findings.md`](../../docs/store-io-findings.md) has the
method and numbers.

### Flashing the fleet

`flash-fleet` puts one node image on every attached ESP32-C5, several at once, never on the bridge.
It uses `espflash`, which must be on `PATH`.

```sh
wartui flash-fleet --dry-run     # what would be flashed, and from where
wartui flash-fleet
wartui flash-fleet --skip 10:BD:A3:EC:44:C0
```

| Flag                    | Default        | What it is                                                                 |
|-------------------------|----------------|----------------------------------------------------------------------------|
| `--skip MAC`            | —              | Leave this board alone; repeatable                                         |
| `--image BIN`           | —              | Flash this merged image rather than building or fetching one               |
| `--firmware-dir DIR`    | the checkout's | Build the image from the node firmware here                                |
| `--jobs N`              | `4`            | How many boards to probe or flash at once                                  |
| `--dry-run`             | off            | Probe and report; build, fetch and flash nothing                           |

**No bridge has to be known.** A P4 bridge's CH343 is told apart by its USB IDs and skipped, so
the run reads no `wartui.toml`. Every attached Espressif native-USB ESP32-C5 that isn't named with `--skip` gets
node firmware, including any other C5 board on the host.

A board is flashed only when all five hold; otherwise it is skipped and listed with the reason:

1. It is native USB Serial/JTAG (`303a:1001`) and reports its address as its serial number. A
   CH343 is skipped with its own reason: it is the P4 bridge's port, which `flash-bridge` updates.
2. It is not named with `--skip`.
3. `espflash board-info` reads an ESP32-C5.
4. `espflash board-info` reads the same address from the chip that the OS reported for the port.
5. `espflash board-info` reads at least 4 MB of flash, which the node's two app slots need.

The image is merged (bootloader, `firmware/node/partitions.csv`, app) and written at `0x0` with
`espflash write-bin`. Then `espflash erase-region` erases `otadata` (`0xd000`, `0x2000`), so the
board boots the app just written whatever slot an earlier over-the-air update left selected. The
image comes from the first source that applies:

- `--image BIN`, as given.
- `--firmware-dir DIR`, or the checkout's `firmware/node` when this binary was built from one:
  `cargo build --release` there, merged with `espflash save-image`.
- The release this binary was built for: `wartui-node-fw-esp32c5.bin`, checked against its
  `SHA256SUMS` and kept under the state directory (`images/<tag>/`) so an offline host can reflash.

The chip in the image's header must be the C5, or nothing is touched. After flashing, each
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

`flash-bridge` updates the P4 bridge over the link it bridges on: the Type-C cable to its CH343,
the one its first flash used (`firmware/p4-bridge/README.md` § "First flash").

```sh
wartui flash-bridge --dry-run                       # which board, and the image's source
wartui flash-bridge
wartui flash-bridge --bridge 71:F4
```

| Flag                 | Default        | What it is                                                            |
|----------------------|----------------|-----------------------------------------------------------------------|
| `--bridge PATH\|MAC` | see below      | The P4 bridge to update, which must answer; `71:F4` works too         |
| `--config PATH`      | per-OS path    | Where to read and save the remembered bridge; see "Config file"       |
| `--image BIN`        | —              | Send this app-only image rather than building or fetching one         |
| `--firmware-dir DIR` | the checkout's | Build the image from the P4 bridge firmware here                      |
| `--dry-run`          | off            | Report the board and the image's source; build, fetch and send no image |

**The board is chosen before any image is sent.** Each attached CH343 is asked which bridge it is,
as `wartui ports` asks, and the choice is made among those that answered, from the first that
applies. A node beside the bridge changes nothing.

1. The board `--bridge` names.
2. The bridge remembered in `wartui.toml`, which must answer.
3. With none remembered, the only P4 bridge that answered.

The run is refused when:

- The remembered bridge does not answer, because the bridge present is known not to be it.
- The remembered address is a node's. Name the P4 bridge with `--bridge`.
- `--bridge` matches no P4 bridge that answered, or several. Naming a node is the first of these.
- Several P4 bridges answer and none is known. The refusal lists their addresses; pick one with
  `--bridge`.
- No P4 bridge answers. A blank P4 is a CH343 that does not answer, and takes its first flash by
  hand.

A board found without `--bridge` is saved to `wartui.toml` as the bridge once updated, so `run` and
the next `flash-bridge` open it first; with `remember bridge` off, nothing is saved. A board
named with `--bridge` is not saved, since one flashed by name is often some other device. When the
save fails, the update still succeeds and a line says why.

The image comes from the same three sources as `flash-fleet`'s, with `firmware/p4-bridge` for
`firmware/node`; a release publishes one under the same
`images/<tag>/`. Nothing is sent unless the image is an app-only image whose header names the
`esp32p4` and whose descriptor names `wartui-p4-bridge-fw`, its checksum and appended SHA-256 hold,
and it fits the P4's 1920 KiB app slot.

The run opens the bridge, checks it answers as a P4 with the chosen address, and sends the image.
The P4 checks it, switches slots and reboots into it on trial. The connection holds through the
reboot, and the run asks for a status every second, since a rebooted P4 says nothing until it hears
a host. It waits up to 120 s for the P4 to come back, and the first frame after the C6 answers
confirms the image. When the new image carries a new C6
image, the P4 then updates its C6, and the run prints that progress for up to 30 s. A P4 already
running the image is left alone. The board prints, by its last two octets:

| Line                                              | Meaning                                                  |
|---------------------------------------------------|----------------------------------------------------------|
| `n%  sending`                                     | The image is on its way                                  |
| `installing; waiting for the bridge to come back` | The P4 holds the whole image and is rebooting into it    |
| `installed and confirmed`                         | The P4 came back on the new image and confirmed it       |
| `fault  …`, `Info  …`                             | A line the bridge sent, such as its C6's update progress |
| `already runs this image`                         | Nothing was sent                                         |

The run fails, and the P4 keeps the image it ran, when:

- The P4 came back on its old image: the new one did not boot, and its trial watchdog rolled it
  back on its third boot.
- The link dropped mid-transfer, the P4 restarted into another image, or the P4 could not
  switch to the image it received. Run `flash-bridge` again. A restart of the C6 alone does not
  stop the transfer.
- The P4 does not announce itself within 10 s: another program, such as a running `wartui run`,
  holds the port, or the bridge is still starting. Try again.
- The P4 answers in a form this build cannot read, or does not answer the update: it runs
  firmware from another tree. Flash it once by hand.
- The P4 refuses the update: it has no second app slot, as on espflash's default partition table.
  Flash it once by hand with its own table.

The run also fails when the P4 came back on the new image but did not report it confirmed within
3 s. The image is then still on trial and rolls back unless a host confirms it: run `flash-bridge`
again, or any `wartui` command that opens the bridge.

### Updating over the air

`ota` updates the nodes' firmware through the bridge, with no USB cable to them. It listens for
heartbeats, shows what it would do with each node, asks, then updates every chosen node at once
while it uploads the image into the bridge's node image cache. Nothing else may hold the bridge
meanwhile, so stop `run` first.

```sh
wartui ota                                    # every node heard
wartui ota --node N3 --node N7                # just these two
wartui ota --image wartui-node-app-esp32c5.bin --yes
```

| Flag                 | Default          | What it is                                                     |
|----------------------|------------------|----------------------------------------------------------------|
| `--bridge PATH\|MAC` | detected         | The bridge, by path, address or `00:08`                        |
| `--node SLOT`        | every node heard | A node to update, by the name the fleet table shows (`N3`)     |
| `--image BIN`        | —                | Send this app-only image, to the nodes of the board it is for  |
| `--firmware-dir DIR` | the checkout's   | Build the image from the node firmware here                    |
| `--listen SECS`      | `6`              | How long to gather heartbeats before choosing                  |
| `--force`            | off              | Also update a node on the image's version, or a newer one      |
| `--yes`, `-y`        | off              | Update without asking                                          |
| `--sim N`            | —                | Update a fake fleet, running an older version, instead         |
| `--config PATH`      | per-OS path      | Where to read the remembered bridge; see "Config file"         |

**The image is app-only**: what one app slot holds, not the merged image `flash-fleet` writes.
Its board and version come from the app descriptor inside it, never from its file name. A merged
image, one built for no node board, or one over 1920 KiB is refused. So is a corrupt one: its
segment checksum and appended SHA-256 are checked as the bootloader checks them, before anything
is sent. Each board gets its own image, from the first source that applies:

- `--image BIN`, sent only to nodes of the board its descriptor names.
- `--firmware-dir DIR`, or the checkout's `firmware/node` when this binary was built from one:
  `cargo build --release`, saved with `espflash save-image` without `--merge`.
- The release this binary was built for: `wartui-node-app-esp32c5.bin`, checked against its
  `SHA256SUMS` and kept under `images/<tag>/` like `flash-fleet`'s.

**The table comes first.** Each node heard, or each one `--node` names, gets a line, by its slot:

```
node     board      version   image     action
N3       esp32c5    0.1.0     0.1.1     update
N7       esp32c5    0.1.1     0.1.1     skip: already 0.1.1
```

A `--node` slot no node heartbeated from during the listen is refused, and so is any `--node` on a
bridge with no carrier board, whose slots have no names. The progress lines name nodes the same
way.

| Action                            | Why                                                                       |
|-----------------------------------|---------------------------------------------------------------------------|
| `update`                          | The image is newer than what the node runs, or `--force`                  |
| `confirm`                         | The node runs the image's version on trial; it is confirmed, not resent   |
| `skip: already 0.1.1`             | The node runs the image's version                                         |
| `skip: older image (use --force)` | The node runs a newer version than the image                              |
| `refuse: no image for <board>`    | `--image` is for another board, or no node firmware is built for this one |

Only the heartbeat's version, board and flags are read, so a node `run` shows as `needs update` is
one `ota` reaches. `ota` and `flash-fleet` update only an ESP32-C5: `ota` refuses any other board,
and `flash-fleet` skips it. Without `--yes`, `ota` asks `[y/N]` before sending anything.

**The image goes to the bridge once**, however many nodes take it, and the `cache` line says how
long that took. It goes beside each node's update request, and no node gets a byte of it before
the bridge has checked its SHA-256.

**Each node then gets a progress line**: how much of the image it holds, the rate, and the phase
(`waiting for the node`, `sending`, `verifying`, `rebooting`, `confirming`). On a terminal the lines
are redrawn in place; otherwise each node gets a line at each phase change. Each ends `done 0.1.1
(confirmed)` or `failed:` with the reason. A node that fails leaves the others updating, and keeps
its old image. A node that reboots mid-transfer is picked up again on its next heartbeat, and the
image starts over from the beginning. Any failure makes the run exit non-zero, and ctrl-c aborts
every node still updating and stops. So does a bridge that loses the image mid-run, by rebooting
or by not hearing the host for 15 s. With `--log-file` and `RUST_LOG=wartui_core=debug`, the log
holds every frame sent, every send result, every status from a node and every phase change, as
`key=value` fields.

**How long it takes.** The upload to the bridge is bounded by its link, about 386 KB/s, so it takes
a few seconds.
Each node's first frame goes at once to the slot the listen heard it in, and one that has moved
since waits for its next heartbeat: up to 5 s on an assigned node, a second on a
parked one. Then every node streams at once, the two buses side by side. A node waits to write
each 4 KB sector, and at each 64 KB block to erase the block first: a fraction of a second when the
slot holds an old image, during which it takes nothing over the bus.
The reboot and confirmation add a few seconds more. A node's first update after a USB flash writes
into a blank slot, so its erases cost less ([`docs/ota-findings.md`](../../docs/ota-findings.md)).

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
| `the bridge did not take the node image (…)`  | That board's nodes were not updated. `no PSRAM`: PSRAM did not map; else rerun   |
| `the bridge lost the node image; …`           | Run again: nodes it confirmed keep the new image, and the rest start over        |
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

### Power-cycling a node

On the carrier board, the bridge switches each node's power, so a hung node comes back without
anyone reaching for it. `p` on a fleet-table row asks first, then switches that node off for a
second and on again. From the shell:

```sh
wartui power-cycle --slot N3              # the name the fleet table shows
wartui power-cycle --slot N3 --bridge 44:C0
```

It resolves one bridge as `wartui reset` does, and takes `--config PATH` as `status` does. `--slot`
takes the carrier's silk names, `N1` to `N6`, `N10` or `N14`; another name is refused with the
range. A bridge with no carrier is refused before anything is sent. A node with its own USB
plugged in stays powered through a cycle. When the bridge takes the node supply to be lost, it
switches every node off and powers them again one at a time
([`docs/node-bus.md`](../../docs/node-bus.md#on-the-carrier-board)). Otherwise nothing
power-cycles a node on its own.

The bridge answers once the node is powered again, or says why not:

- `N3 power-cycled`: done.
- `this bridge has no carrier board, so it cannot switch node power`: the bridge found no
  wartui-carrier at boot.
- `A4 is not a slot on this carrier-6`: the carrier has no socket there.
- `N3 was not power-cycled: the carrier's I/O expander did not answer`: the bridge could not reach
  the switch. One that fails at boot is a `fault` in `wartui status` too.

### The lid fan

The carrier has a header for a 4-pin PWM fan, a Noctua NF-A6x15 5V PWM. The bridge keeps its duty,
0 to 100 %, in its own flash and boots into it, so the duty holds across its reboots and with no
host attached; `wartui.toml` holds none. A bridge with nothing stored runs the fan at 100 %, as it
does while it resets. The fan stops at low duty: on the bench it needed 15 % to start from rest,
kept turning down to 10 % once spinning, and stopped at 5 % and 0 %.

```sh
wartui fan                  # the duty and speed now
wartui fan 40               # set 40 %, wait 2 s, then print the duty and the speed at it
wartui fan 40 --bridge 44:C0
```

It resolves one bridge and takes `--config PATH` as `power-cycle` does. A bridge with no carrier
has no fan and is refused. The bridge writes a duty only when it differs from the one it
stored. One it could not save holds until its next reboot, and `wartui fan` prints the bridge's
`fault` line saying so. Setting the same duty again retries the save.

In `run`, settings' `fan` row shows the duty the bridge last reported and steps it by 5 %. `Enter`
sends it only when the row moved; `no fan` means the bridge reported none, and `no bridge` that the
link is down. While the link is up, the header shows the fan after the bridge, as
`fan 40% 1830 rpm`, or `fan 40%` with no reading: in the bridge's first second, and for a second or
two after a duty change, since no reading spans two duties. `wartui status` prints a `fan` line too.

### The channel pool

The bridge keeps the channel pool in its own flash and boots into it, so the pool holds across its
reboots and with no host attached; `wartui.toml` holds none. Every bridge holds one, carrier or
not, and one with nothing stored holds `all`. "Channel pools" has what each pool covers.

```sh
wartui pool                 # the pool the bridge holds
wartui pool eu              # set EU, then print the pool the bridge reports
wartui pool us --bridge 44:C0
```

It resolves one bridge and takes `--config PATH` as `fan` does. The bridge writes a pool only when
it differs from the one it stored. One it could not save holds until its next reboot, and
`wartui pool` prints the bridge's `fault` line saying so. Setting the same pool again retries the
save.

In `run`, settings' `pool` row shows the pool the bridge last reported. `Enter` sends it only when
the row moved; `no bridge` means the link is down or no status has reported a pool yet, so there is
nothing to send it to. The header shows the
pool once the bridge has reported one, and `wartui status` prints a `pool` line too.

The bridge keeps the Bluetooth slot beside the pool, the same way; see "Bluetooth".

## Keyboard commands

| Key                    | What it does                                                                 |
|------------------------|------------------------------------------------------------------------------|
| `↑` `↓` / `k` `j`      | Move the cursor through the fleet table                                      |
| `b`                    | Toggle Bluetooth scanning on the selected node                               |
| `c`                    | Open config (settings)                                                       |
| `g`                    | Show GPS diagnostics: fix and satellites                                     |
| `p`                    | Power-cycle the selected node through the carrier board, after asking        |
| `r` / `R`              | Clear the selected node's dedup ring, or every assignable node's             |
| `u`                    | Upload this capture to WDGWars, after asking                                 |
| `q` / `Esc` / `ctrl-c` | Stop, committing the last batch                                              |

Only `b`, `c`, `p`, `r`, and `R` reach the engine, and no key sets channels. `b` picks _which_
node scans Bluetooth, which the bridge keeps; the planner still authors every share. `r` and `R`
make a node forget every address it has reported, on its next heartbeat.

`u` asks before it sends, sends only what is new since the last upload, and leaves the capture
running; see "Uploading to WDGWars".

`p` asks before it power-cycles the selected node, and works on a node that has stopped
heartbeating, as long as it has a slot. A bridge with no carrier refuses it, and so does a
bridge that is not connected, even with the confirm already open. The bridge's answer replaces the
key help for a few seconds; see "Power-cycling a node".

### In settings (`c`)

| Key                 | What it does                                                        |
|---------------------|---------------------------------------------------------------------|
| `↑` `↓` / `k` `j`   | Move between rows                                                   |
| `←` `→` / `h` `l`   | Change the selected row; the pool wraps, the fan stops at 0 and 100 |
| Type / paste        | On the `wdgwars` row: enter the key (`Backspace`, `ctrl-u` clears)  |
| `Enter` / `→` / `l` | On the `excluded ssids` row: open its page                          |
| `Enter`             | Apply every row; pool and fan to the bridge, rest to `wartui.toml`  |
| `Esc` / `q`         | Close without changing anything                                     |
| `ctrl-c`            | Quit                                                                |

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

The rows are the `pool` (`all → eu → us`; see "The channel pool"), the `fan` (0 to 100 %, by 5;
see "The lid fan"), `remember bridge` (`off`, `on`),
`excluded ssids`, and the `wdgwars` API key. `excluded ssids` shows how many are
listed, or `none`, and opens a page that lists them. While an SSID is being typed there, every
letter types, spaces included. `Enter` refuses one that is empty, over 32 bytes, or already listed,
and says why. `Enter` in the list sends it to the engine, writes `excluded-ssids` and the remembered
bridge but none of the rows, and returns to the rows; edits to the rows stay pending.
On the key row, letters type, so `h` `j` `k` `l` `q` neither move nor close; `↑` leaves it, and the
footer shows the `ctrl-u` tip while a key is entered. The key shows its last three characters, up to
six bullets before them, and from ten characters its first one to three. `Enter` sends the rows to
the engine and writes every row shown but the pool and the fan to `excluded-ssids`, `[bridge]`, and
`[api-keys]`, whatever was there, and settings closes. The pool and the fan go to
the bridge, which keeps them, and only when their rows moved; neither is written to `wartui.toml`.
If the engine is not accepting commands, nothing is sent or written, and settings stays open with
its edits. A pool the bridge takes re-cuts the fleet once the status asked for behind it reports
the pool. Each node takes its new share on its next heartbeat, emptying its dedup ring when its
share changes.

## How channels are assigned

**The planner cuts the pool, and nothing else does.** It deals the pool across every heartbeating,
sniffing node. It re-cuts when that set changes, the Bluetooth scan moves (§ "Bluetooth"), or the
bridge reports another pool (§ "Channel pools"). The header shows its inputs: `4 of 5` is four
nodes in the plan of five heard.

**A changed share waits for the node's next heartbeat.** An assignment answers a heartbeat, through
the node-bus slot that heartbeat came from, and an assigned node heartbeats every 5 s, so an
assignment takes up to one heartbeat interval, not a sweep. That delay is the protocol, not lag.
The `channels` column leads with a count (`1: 1…`) because a round-robin share is a dozen
scattered channels, too many to fit.

**An assignment counts once the bridge says the node acknowledged it** on the node bus, never on the
bridge's report of a successful enqueue. An ack does not prove the node took it. Adoption does:
every heartbeat carries the epoch the node holds, usually the new one on the heartbeat after the
send. A heartbeat reporting some *other* epoch makes wartui re-send in answer to it. The footer
counts those as "assignments acknowledged but not adopted", apart from `no admin ack`'s
never-acknowledged ones.

**Repeated `no admin ack` means the bridge cannot deliver to that node.** The P4 resent the
transfer on every try and the node acknowledged none, or the bridge never answered.

## Bluetooth

**At most one node scans Bluetooth, none by default, and it is that node's whole job.** The scan
belongs to a slot, and the bridge holds it: it keeps the slot in its own flash beside the pool,
boots into it, and reports it in every status, so it survives restarts of the host and the bridge.
`b` on a node asks the bridge to move the scan to its slot; `b` on the node holding it takes it off
the fleet. `wartui bluetooth` sets the same slot from the command line. Whichever node heartbeats
from that slot holds the scan, so a node swapped into the socket takes it over. That node's
`channels` reads `bluetooth`, since it is dealt no channels, or `bluetooth…` while the change waits
for the node's next heartbeat. Taking the scan off shows as its new share, pending. The header
shows `bt N3` while the bridge holds a slot, and `b` on that slot's row takes the scan off even
when its node is not heartbeating. `wartui status` prints a `bluetooth` line naming the slot, or
`none`.

```sh
wartui bluetooth                # the slot the bridge holds
wartui bluetooth N3             # give the scan to N3, then print the slot the bridge reports
wartui bluetooth none           # take the scan off the fleet
wartui bluetooth N3 --bridge 44:C0
```

It resolves one bridge and takes `--config PATH` as `pool` does. A slot is named as the fleet table
names it, either case, and a bridge with no carrier takes only `none`. A slot the bridge could not
save prints its `fault` line, and the command fails when the slot read back is not the one sent.

**It costs a whole node because Wi-Fi and Bluetooth share one 2.4 GHz antenna.** A Bluetooth
controller left free takes it whenever the node is otherwise idle
([`docs/phase-2-findings.md`](../../docs/phase-2-findings.md)), so the node holding the scan sniffs
no Wi-Fi. With nothing else to do, the scan runs **back to back**.

The fleet is one sniffer short, and the pool is re-cut the moment the scan moves: giving it away
grows every other share, taking it back shrinks them. A fleet of **one** node holding the scan
sweeps no Wi-Fi at all, and the fault box says so.

## Channel pools

The bridge holds the pool: it keeps it in its own flash, boots into it, and reports it in every
status. `run`'s planner deals what the bridge reports, and cuts nothing before the first status
arrives. A bridge with nothing stored holds `all`, the default: everything a node can tune, 2.4 GHz
1–13 and all of 5 GHz including UNII-4 (169, 173, 177). A node only parks and reads beacons, so a
pool sets where it listens, never what it emits: the choice is coverage, not legality.

| Pool  | 2.4 GHz | 5 GHz  | Channels |
|-------|---------|--------|----------|
| `all` | 1–13    | 36–177 | 41       |
| `us`  | 1–11    | 36–165 | 36       |
| `eu`  | 1–13    | 36–140 | 32       |

`us` is what the FCC permits: no 12, 13, or UNII-4. `eu` is what ETSI permits: 5 GHz stops at 140,
because 144's twenty megahertz cross the 5725 MHz edge and 149 up is another band. Channel 14 is in
no pool; `esp-radio` cannot reach it.

Set it from settings (`c`) or with `wartui pool`; see "The channel pool".

An assignment's forty-two-bit mask can name any subset. The planner deals round-robin, pool index
_k_ to node _k mod n_, so every node carries both bands. A block split would put one node on all of
2.4 GHz, and losing it would blind the fleet to that band until the next re-cut landed.

Every fleet change re-cuts the _whole_ fleet, and each node takes its share on its next
heartbeat, so the fleet converges within one heartbeat interval. A node adopts only an epoch that
differs from its own, so an unchanged fleet is left alone.

**A node can report a channel outside its share.** 2.4 GHz channels are interleaved (one node 1, 3,
5; the next 2, 4, 6), 5 MHz apart but 20 MHz wide, so a node on 2 hears 1 and 3. A strong access
point is heard well beyond its own channel, and an access point may send copies of its beacon across
its width. A node reports the channel the beacon names when it is a scan-table channel the node
could have heard: any 2.4 GHz channel when parked on 2.4 GHz, or one within 160 MHz on 5 GHz, so a 5
GHz node can report a nearby 5 GHz channel too. Otherwise it reports the channel it was parked on,
which covers vendor gear that names a number off the channel list. About a quarter of access points
reach more than one node. `export` keeps the strongest per recapture window, so this costs only
store rows.

## Positions

Each observation gets the best position available, resolved fresh: GPS, then `--lat`/`--lon`, then
none. The GPS is a receiver named with `--gps PATH`, then the bridge's receiver, then one the host
found, each while its fix is recent. Rows
record which tier answered, so a garage-to-road capture is honest about both. No record is dropped
for lack of a position, but **WiGLE rejects rows without coordinates**: a capture with none exports
nothing and says how many networks it left out.

- **A bridge with a GPS receiver is the position source while it has a fix.** The P4 bridge reads a
  u-blox receiver on its I2C (GPIO7/GPIO8) and forwards its bytes; see
  [`firmware/p4-bridge/README.md`](../../firmware/p4-bridge/README.md) § "GPS". The host's own search
  stands down while the bridge's receiver is sending sentences, with or without a fix, letting go
  of any port, so `g` shows it finding satellites. The search resumes once the last sentence is
  older than `--gps-max-age`, or the bridge disconnects. After a reconnect it stands down again on
  the first new sentence. A port named with `--gps` is always read, and its fix goes ahead of the
  bridge's. wartui sets the receiver to `[gps] bridge-rate-hz` fixes a second (default 5) on every
  bridge status poll. `g` reports on a port named with `--gps` when there is one. Otherwise it
  reports on the bridge's receiver while it is sending sentences, or when a bridge is connected and
  no host receiver is read or found, and shows `bridge` as its port.
  `wartui sniff --verbose` prints the receiver's raw stream.
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
- **`--gps bridge` reads the bridge's receiver only.** No host port is opened or searched, so
  `--gps-baud` is refused with it.
- **`--gps` with `--gps-baud` skips the probe.** Nothing is left to detect, and a terse receiver
  could otherwise be refused for saying too little in one window.
- **`--no-gps` turns the search and the bridge's receiver off; `--sim` turns the search off unless
  `--gps` is given.** A simulated fleet is for working with nothing plugged in, not for opening every
  serial port.
- **Any NMEA 0183 serial receiver works.** `GGA` and `RMC` give the position. Altitude, satellite
  count, and an accuracy estimated from HDOP reach the export. `GSA` and `GSV` are read for `g` only.
- **`g` shows why a receiver will not lock.** The left side has the fix: 2D or 3D, satellites used,
  time, position, speed, the HDOP accuracy estimate (not a measurement), and the port. The right side
  lists each satellite in view with its signal strength and whether the fix uses it. `Esc`, `q` or
  `g` closes it.
- **A named receiver that is missing is a fault; an unfound one is not.** `--gps` asks for a port,
  so its absence is reported as a fault. When neither the host nor the bridge has a receiver, the
  header reads `gps no receiver`. Until the bridge says whether its port has one, it reads
  `gps connecting`.
- **A fix must be recent.** Past `--gps-max-age` seconds the position falls back a tier and the
  header says `gps fix is stale`. At driving speed a minute-old fix is another neighborhood, and
  falling back beats quietly claiming it. The reader logs that age limit too, even when the receiver
  is silent. The last fix is retained; a new fix logs recovery.
- **The receiver runs on its own thread; nothing waits for it.** Capture starts at once, and the
  header shows `gps scanning /dev/… @38400`, `gps searching`, `gps ok, 8 sats`, `gps fix is stale`,
  `gps no receiver`, or the port's error. The search keeps running there, so a puck replugged into
  a *different* socket is found under its new name.

## Reading the fleet table

Rows name nodes by slot and chip: `N1 C5`. The slot is the node-bus slot its latest heartbeat came
from, by the carrier's silk name, `N1` up. `B1` is `N4` on the 6, `N6` on the 10 and `N8` on the 14,
and with no carrier a slot keeps its bus name. A capture stores the bus name. Rows run in slot
order, and a node with no slot (`—`) comes last. The chip comes from the board its heartbeats
announce, so a node not yet heartbeating shows `—`, and a board this build has no name for shows
`??`.

| State          | Meaning                                                                                                                                     |
|----------------|---------------------------------------------------------------------------------------------------------------------------------------------|
| `alive`        | Heartbeating, so it can be given channels                                                                                                   |
| `stale`        | Still being heard, but not heartbeating, so it cannot be given channels                                                                     |
| `off bus`      | The bridge no longer hears its slot on the node bus: it lost power, crashed or was unplugged                                                |
| `no heartbeat` | Seen, but has never completed a sweep                                                                                                       |
| `no admin ack` | The bridge could not deliver its assignment, or never answered; see [How channels are assigned](#how-channels-are-assigned)                 |
| `no slot`      | Another node's heartbeat came from its node-bus slot, so it cannot be reached until its own heartbeat names a slot                          |
| `needs update` | Runs an older major or minor of the node firmware, so wartui cannot read its frames; update it with `wartui ota`                            |
| `host behind`  | Runs a newer major or minor of the node firmware than this wartui drives; update wartui                                                     |
| `rebooted xN`  | Its heartbeat counter went backwards, or its epoch went back to 0, so it has forgotten any assignment; wartui re-issues under a fresh epoch |

The `channels` column shows where an assignment stands
([How channels are assigned](#how-channels-are-assigned)):

| `channels`       | Meaning                                           |
|------------------|---------------------------------------------------|
| yellow, with `…` | Waiting for the node's next heartbeat             |
| blue, with `…`   | Acknowledged by the node, not yet adopted         |
| plain            | Adopted                                           |
| cyan             | The Bluetooth node                                |

The line under the table shows the selected node's last two address octets, its firmware and its
latest supply and die readings: `N1 C5 57:84  0.1.0;5g  3.30 V  41 °C`, with `—` for a node that
has sent none or whose read failed. It is the one place the main screen shows a node's address,
which `flash-fleet --skip` and the OS's device names go by.

The firmware is its version, then `;5g` for a radio that reaches 5 GHz. A node on an older version
than this build's adds an arrow and this build's version: `0.1.2;5g ↑0.1.3`. A different patch is
still driven; an older major or minor is `needs update` and a newer one `host behind`.

The voltage is the chip's 3.3 V rail, not the board's 5 V input, and the die runs well above the
air around it. A reading out of range is yellow on that line, and turns an `alive` node's `state`
yellow too; any other state keeps its own color:

| Reading | Warns at       | Why                                                           |
|---------|----------------|---------------------------------------------------------------|
| Voltage | below 3.00 V   | The C5's minimum recommended supply; the chip resets at 2.7 V |
| Die     | 70 °C or above | A warning; the die runs hotter than ambient                   |

A `stale`, `off bus`, `no slot`, `no heartbeat`, `needs update`, or `host behind` node is out of
the plan: nothing sent would reach it, it has not yet said what it is, or its frames are not this
build's, so its share might be nobody's. `b` on one says which state it is, because the fix
differs: wait for a heartbeat, find out why heartbeats stopped, check the node's power and seat,
update the node with `ota`, or update wartui. Heartbeats from a `needs update` or `host behind`
node are not counted among the footer's frames from an older firmware. Once its heartbeat has
named its version, its sighting batches are, since none of them is recorded.

`stale` and `no heartbeat` are deliberately distinct from silence and from each other. wartui keeps
two clocks, so a node that streams observations but loses heartbeats doesn't age out and churn the
whole fleet's topology.

**An `alive` node gone quiet is usually fine.** A node reports each address once and holds it back,
so a stationary node falls silent after reporting everything in range. That memory lives on the node
and survives into your next session. A held address is reported again five minutes after it was last
reported, or sooner if heard at least 5 dB louder (10 dB for a Bluetooth device). `r`/`R` clear it
without a reboot, and a change of share or Bluetooth role empties it.
`crates/wartui-proto/src/dedup.rs` explains why.

| Header says            | Meaning                                                                  |
|------------------------|--------------------------------------------------------------------------|
| `waiting for nodes`    | No node has heartbeated yet this session                                 |
| `N need update`        | Heard only from nodes on an older minor; `N host behind` for newer ones  |
| `all nodes silent`     | Nodes were seen, but none is heartbeating now                            |
| `N alive, none usable` | Heartbeating, but none can be assigned channels; the node table says why |

The footer shows faults only once they happen, so a clean run has a clean footer:

- **`lost`** (a column, and `lost N` summed in the footer): whole batches missing between node and
  host, from gaps in each batch's sequence number. The column reads `—` before a node's first batch,
  then a running count; both appear once any node loses one. A sequence advances only when the P4
  acknowledges a batch, so a gap is lost after the P4 took it, in its queue or on USB, not on the
  node bus. A batch is up to about a dozen access points, hidden until the node's five-minute
  refresh. Each gap is a `batch_gap` row; summing its `lost` per node gives the column's final
  value. A gap after a batch replayed from the bridge's backlog is not counted, because those
  batches were dropped while no host was reading.
- **`dup N`**: batches dropped as identical to the previous one (same `seq`, same bytes, within
  2 s), the node bus resending a batch whose ack the node did not see. The first copy was
  recorded, so nothing is lost and `dup` never overlaps `lost`. A repeat 2 s or more later is
  recorded normally.
- **`buffer full wifi N  ble M`**: a line below the totals, counting distinct networks and
  advertisers a node heard but had no room for in that dwell or scan, fleet-wide, this session. It
  appears when a count passes zero, omits a kind still at zero, and stays. Not a fault: an address
  turned away is not held back, so the next pass reports it; only one never heard again is lost.
  A steadily growing count means a buffer too small for the area.
- **`N frames from an older firmware — update`**: a fleet mid-upgrade, speaking a wire format this
  host does not, so it appears only here.
- **`bridge dropped`**: frames lost since this host attached. `wartui status` reports the total
  since the bridge booted, which is large and harmless on a bridge left powered with nothing
  listening. Every status reply is kept as a `bridge_status` row, with its since-boot counts and
  radio link counters, and a `bridge_queue` row per queue it reported.
- **`fan stopped at 40%: unplugged or stalled`**: the bridge read no tach pulse over a whole second
  at 20 % or more, where the fan always turns. Check the fan header. Below 20 % nothing is raised,
  since the fan may stop there by design.
- **`N3 restarted: brownout`**, with `(×3)` after repeats: a node restarted during the run for a
  fault, and the line stays for the run. A node with no slot is named by its last two address
  octets. `brownout` means check that node's supply; `software`, `watchdog` or `lockup` is the
  firmware; `unknown` is a cause this build cannot name. A power-on or a reset over USB is not
  shown.

## When nothing arrives

**wartui finds the bridge by asking.** It opens each attached CH343 in turn, and never a node, and
keeps the first that answers the link protocol. `run` saves that address to `wartui.toml` as
`[bridge] mac`, and the CH343's serial number as `serial`, so later runs open only that port. A
capture given `--bridge` saves the named board once it answers, since naming it for a capture is
choosing it. `flash-bridge` saves the board it found and flashed. `status`, `sniff`, `reset`,
`power-cycle`, `fan`, `pool`, `bluetooth` and `ota` read it and save nothing, so questioning a board
changes nothing later, and `flash-fleet` never reads it. An answering board is held for the run:
unplug the bridge to reflash a node, and wartui waits rather than transmit into it.

Deleting the `mac` and `serial` lines is always safe, costs one slower start, and fixes wartui
opening the wrong board. It also self-corrects: an answering board replaces it, and the remembered board is dropped
if, opened with no other to try, it stays silent, as a reflashed bridge does. A board merely passed
over in a sweep is kept; being slower than its neighbor proves nothing. With `remember bridge` off
(settings, or `[bridge] remember = false`), `run` drops both, saves no bridge, and sweeps on every
start.

The header's `waiting for bridge` has three causes; the fault box names it:

- **`link down: could not open …`**: the port is not ours. Nearly always another `wartui`, `screen`,
  an IDE's serial monitor, or ModemManager holds it. ModemManager clears on its own: where it runs,
  it opens each new CDC-ACM device for a few seconds to ask whether it is a modem.
- **No fault**: the port opened and the bridge is not answering, or a program that took no lock on
  the port, such as `cat`, is reading its answers. A P4 that is still booting misses the one
  question; the next attempt asks again.
- **`swept N boards and none answered`**: no attached CH343 answered.

With no fault, `wartui reset` reboots a bridge that still reads what it is sent. A power cycle is
the fallback for a bridge that answers nothing.

```sh
wartui status                          # exits in 5 s with the reason
wartui reset                           # reboot a bridge that stopped answering
wartui --log-file wartui.log run
```

Beyond the counters, `wartui status` prints what the bridge reports about its insides:

- **`queue NAME  HIGH/CAPACITY UNIT  dropped N`**, one per queue: the high-water mark since boot
  and what the queue threw away. The bridge reports `radio-up` (the C6's lines and status for the
  P4), `radio-down` (the P4's records for the C6), `node-down` (the P4's queues of records for its
  nodes: one slot's size, the deepest any slot has been, and every slot's refusals), and
  `host-priority` and `host-bulk` (its two queues of frames for the host).
- **`radio link  …`**: the UART link's packets from the C6 and its error counters at each end;
  the `radio_` counters are the C6's. Every error count means corrupted or lost packets, and one
  bad packet can move more than one count. A C6 restart reads as one `seq_gap`.
- **`host link  uart_errors N`**: host-link reads since boot that saw one or more line errors or
  overflows on the P4's UART from the host. Each is host bytes lost or corrupted, as while the P4
  writes its own flash during `flash-bridge`; the update rewinds over them. Outside an update it
  should stay 0.
- **`board  …`**: what carries the nodes, `carrier-6`, `carrier-10` or `carrier-14`, and the slot
  names it takes, or `no carrier`, beside the fault saying no node is driven.
- **`radio  …`**: the C6's address, firmware, reset cause and last phase. After a
  C6 restart under a running P4, `last reset` names the C6's cause too.
- **`fault  …`**: a fault the bridge reports while `status` waits for its counters, such as the
  P4 updating its radio. One line per report, and the counters follow.
- **`pool  …`**: the channel pool the bridge holds, which `run` deals; see "Channel pools".
- **`bluetooth  …`**: the slot whose node scans Bluetooth, by its silk name, or `none`; see
  "Bluetooth".
- **`slots  …`**: the slots the bridge hears a node in, by the carrier's silk names (`N1 N2 N8`),
  or `none`.

`wartui reset` takes about thirty seconds to fail on a node named by path. The board it is for
answers nothing, so it transmits before anything has identified itself. A node that is not reading
takes one packet and queues the rest, and closing the port waits for them.

`run` never pays this cost. It asks a CH343 every 500 ms until the bridge answers, since a CH343
drains whatever it is sent, and any other port with one frame. Once the bridge has answered, it
sends a status poll whenever 10 s pass without a frame, since the P4 stops writing to a host it has
not heard from in 15 s.

`wartui reset` never sweeps, because a `Reset` sent down the wrong port reboots the wrong board. It
goes to the `--bridge` board, else the CH343 the bridge was remembered on, else the only CH343
attached. With several and none known, it asks for `--bridge` rather than guessing. An address is
matched by asking each CH343, except the remembered bridge's own, which goes to its remembered port
unasked; one no CH343 answers with is refused before anything is opened. The P4 comes back on the
same port, since its CH343 stays on the bus.

**`--log-file` is the only way to see the transport's own account of a run**: the view owns the
terminal, so without it nothing is logged. It records which port was resolved, whether it opened,
and why a link went down, once per reason, since somebody else's port is retried every 750 ms all
capture. The GPS reader logs searching, its first fix, stale fixes, recovery, and unreadable or
removed receivers on transitions, not every sentence or satellite-count change. Reopening a failed
port does not count as recovery until a valid fix or no-fix report arrives. The log also records each
time the host falls 100 ms or more behind the bridge and when it catches up, with both frames' bridge
stamps. A reconnect while frames are arriving can log one spell of about 100 ms, because the lag after
a connect starts from the 100 ms the engine assumes rather than from a measurement. Every line the
bridge sends is logged as `bridge: …`, faults at `warn`, such as `the radio has stopped answering`.
Each node's membership changes are logged once each:

| Message               | When                                                                                                 |
| --------------------- | ---------------------------------------------------------------------------------------------------- |
| `node joined`         | its first heartbeat this run                                                                         |
| `node left the fleet` | its slot goes off the bus (`reason="off bus"`), or 20 s after its last heartbeat (`reason="silent"`) |
| `node returned`       | its first heartbeat after `node left the fleet`                                                      |
| `node restarted`      | its heartbeat counter fell, or its epoch reset                                                       |

`RUST_LOG=debug` adds GPS probe attempts, each retry, undecodable frames, and dropped bulk
commands.

### Telling the boards apart

USB-plugged nodes share a vendor and product ID and sit on adjacent device nodes, so a path says
nothing about which board it reaches. `wartui ports` names them:

```
$ wartui ports
/dev/serial/by-id/usb-1a86_USB_Single_Serial_5CF7153520-if00
  98:A3:16:81:96:C4     P4 bridge on a CH343  (1a86:55d3)
/dev/serial/by-id/usb-Espressif_USB_JTAG_serial_debug_unit_02:00:5E:10:9D:24-if00
  02:00:5E:10:9D:24     USB JTAG/serial debug unit  (303a:1001)
```

**A node's USB serial number is its MAC**, so the OS already pairs each node's device node with
its radio's address: nothing opened, no `esp` tool, no reflash. It is also why each board has its
own `by-id` name, built by udev from the serial. Other USB devices carry a manufacturing serial;
one reporting none reads `address not reported` and is named by its socket.

**The P4 bridge is reached through its board's CH343** (`1a86:55d3`), whose serial number is the
CH343's own. So `wartui ports` asks each CH343, every 500 ms for up to 3 s, and prints the address
its `Ready` names: the C6's MAC, or the P4's own MAC when the C6 never answered. A CH343 that does
not answer reads `not answering`, as a blank P4 does, and one another program holds reads
`busy or refused`.

The bridge is the `P4 bridge on a CH343` row, with the address the header shows for the bridge. A
node is a row whose heartbeats `wartui sniff` attributes to its address. Either name works as
`--bridge`, as does an address's last one to five octets (two hex digits each) when exactly one
bridge answers with it. An address names only a bridge that answers, and a path
is opened as given. `--bridge 44:C0` is how a board is read off the fleet table. Several matches
are refused with a list, never guessed. Keep the address: a device node moves when the board
re-enumerates and a `by-path` name moves with the cable, but an address moves only with the board.

```sh
wartui --bridge 10:BD:A3:EC:44:C0
wartui status --bridge 44:C0
```
