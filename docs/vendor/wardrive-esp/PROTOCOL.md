# WardriveESP protocol v1.6

The board speaks one text protocol over two transports: Bluetooth LE (below) and USB (1.6+, see "USB").

## GATT

No encryption or authentication flags. One phone connection per board.

| Role | UUID | Properties |
|---|---|---|
| Service | `47686F73-7445-5350-4272-696467655376` | primary, advertised |
| RX (phone → board) | `0147686F-7374-4553-5042-726964675852` | write, write without response |
| TX (board → phone) | `0247686F-7374-4553-5042-726964675854` | notify |
| CTRL (phone → board) | `0347686F-7374-4553-5042-72694C525443` | write, write without response |
| OTA (phone → board) | `0447686F-7374-4553-5042-72694F544144` | write without response (firmware image bytes, 1.2+) |

The advertisement carries the service UUID. The scan response carries the name `WardriveESP-XXXX` (last two bytes
of the BLE address). Firmware before 1.6.1 advertised `iosESP-XXXX` (a bare `iosESP` before 1.1); apps should find
boards by the service UUID and accept both names.

### Status beacon (1.6.5+)

Every phone nearby can see every board, linked or not, and who has it.

- **Free board**: advertises the service UUID, connectable, exactly as before. The scan response carries the name and
  bytes 0-8 of the status below.
- **Linked board**: a board only takes one phone, so while linked it advertises non-connectable (scannable, about once
  a second) with the **status UUID** `0547686F-7374-4553-5042-726964675354` instead of the service UUID. Apps that
  predate 1.6.5 look for the service UUID only, so they never try to connect to a board that is taken. The scan
  response carries the status below, including the holder's nickname.

Status = manufacturer-specific data, company id `0xFFFF`, little-endian:

| Byte | Meaning |
|---|---|
| 0 | `W` (0x57) |
| 1 | flags: bit 0 linked, bit 1 scanning, bits 2-3 mode (0 Wi-Fi, 1 Bluetooth, 2 capture, 3 Wi-Fi radar (CSI, dev builds)), bit 4 a USB host has a session, bit 5 GPS, bit 6 updating, bit 7 the holder is an iPhone |
| 2 | chip temperature, °C, signed (-128 = unknown) |
| 3-4 | Wi-Fi: APs in the last sweep. Bluetooth: devices in the last window. Capture: APs this session. CSI: packets (rx), frames (tx) or links (mesh) in the last second |
| 5 | firmware major << 4 \| minor |
| 6 | firmware patch |
| 7-8 | the two address bytes in the name (`WardriveESP-%02X%02X`), so a linked board can be named without its name in the advert |
| 9+ | linked boards only: the holder's nickname, UTF-8, up to 18 bytes (whole characters), empty when the app didn't say |

The status refreshes every 5 s when anything in it changes.

### Advert extras (1.7.3+)

Both scan responses are full, so the rest rides in the advert packet itself (free and linked boards alike): service
data under the 16-bit UUID `0x5744`, 6 bytes, in two frames the board swaps every 5 s.

| Byte | Frame A (bit 0 of byte 0 = 0) | Frame B (bit 0 of byte 0 = 1) |
|---|---|---|
| 0 | bit 0 frame, bits 1-2 how the last link ended (0 no link since boot, 1 released by its host on purpose, 2 lost), bit 3 a screen is fitted, bits 4-5 co-processor radio (0 none, 1 off, 2 BT Classic, 3 Wi-Fi) | same |
| 1 | board: 0 generic, 1 Biscuit Crumb, 2 Rabbit Labs Ghost Board, 3 Rabbit Labs Poltergeist, 4 Biscuit Pro, 5 Biscuit Ultra, 6 Biscuit Pro or Ultra, 7 Koko Marauder V8, 8 Marauder Mini v3, 9 AWOK Dual C5 (its screen chip), 10 Waveshare ESP32-C6-LCD-1.47, 11 Waveshare ESP32-C5-LCD-1.47, 12 Waveshare ESP32-S3-Touch-LCD-3.5 | channel bits 24-31 |
| 2 | battery %, 0xFF = no gauge or unknown | channel bits 32-39 |
| 3-5 | channel bits 0-23 | bits 40-41, then 0 |

Channel bit i set = that channel is in the board's plan, in this order: 2.4 GHz 1-14, then 5 GHz 36, 40, 44, 48, 52,
56, 60, 64, 100, 104, 108, 112, 116, 120, 124, 128, 132, 136, 140, 144, 149, 153, 157, 161, 165, 169, 173, 177.

"Released" means the app sent `bye` before it disconnected (or a USB host sent `wdusb bye`): another phone can link
at once. "Lost" (and "no link since boot", after a reboot or an update) means the holder will probably be back: other
phones leave it to that holder for 60 s.

## Frames

12-byte header, then payload. Little-endian.

| Offset | Size | Field |
|---|---|---|
| 0 | 1 | `0x47` |
| 1 | 1 | `0x42` |
| 2 | 1 | version `0x01` |
| 3 | 1 | type: 1 CMD, 2 ACK, 3 DATA, 4 END, 5 ERR |
| 4 | 1 | status |
| 5 | 1 | flags (CMD: `0x01` first, `0x02` more) |
| 6 | 4 | command id |
| 10 | 2 | payload length |
| 12 | n | payload |

- Each command is one CMD frame. The board replies with an ACK carrying the same id.
- Output is text in DATA frames sized to the MTU. Join payloads in order and split on `\n`.
- Unframed text written to RX or CTRL is also accepted as a command.

## Commands

The first word picks the command. Replies to terminal commands are plain text lines.

| Command | Effect |
|---|---|
| `wdstream start -wifi [-ch 1,6,11,36]` | start scanning; `-ch` limits this board to those channels from the next sweep |
| `wdstream stop`, `stop` | stop scanning, send `WD:END`; from 1.6.3 records still queued on the board are sent before it |
| `start` | start scanning in the board's current mode |
| `bt on`, `bt off`, `bt` | scan Bluetooth instead of Wi-Fi, back to Wi-Fi, or show the mode (1.3+) |
| `cap on`, `cap off`, `cap` | passive capture (handshakes, PMKIDs, clients) instead of Wi-Fi, back to Wi-Fi, or show the mode (1.5+) |
| `caps` | send `WD:CAPS` |
| `help` | list the terminal commands |
| `info` | firmware, chip, MAC, uptime, last reset, memory, flash, slot |
| `status` | scanning state, sweeps, APs last sweep, channels, MTU |
| `link` | what the board is connected to: phone address, how long, signal, link timing, last drop (1.6.4+) |
| `drop` | disconnect the phone so another one can link (1.6.4+) |
| `bye` | the app is letting the board go on purpose (bridge off, board turned off in the app); it disconnects right after. No reply. Shown to other phones as "released" in the advert extras (1.7.3+) |
| `iam a <nickname>`, `iam i <nickname>` | the app says who it is (`a` Android, `i` iPhone), once after linking. No reply. Shown by `link` and in the status beacon (1.6.5+) |
| `ch`, `ch 1,6,11`, `ch all` | show, set or reset this board's channels |
| `led`, `led alert <pattern> <rrggbb> [seconds] [label]`, `led test`, `led off` | LED ring (boards with `led=` in `WD:CAPS`, 1.6.6+): plays a pattern in a color for a few seconds, then returns to the idle breath; `off` returns to idle now, `led` alone shows the state. Replies `led <pattern> <rrggbb> <seconds>s`. Patterns (default color, seconds): `police` red/blue chase, color ignored (8); `heartbeat` lub-dub (FF4FA3, 6); `strobe` hard flashes (FF3B30, 6); `spin` a circling comet (00C8FF, 6); `pulse` a fast breath (FFC800, 6); `sparkle` flares that fade (FFAA00, 3); `rainbow` the swirl the ring plays when a phone or USB host links (2). A new alert replaces the one playing; alerts take priority over the link swirl. Older names still work: `led police|drone|notable|capture|storm [rrggbb] [seconds]`. On a board with a screen (`disp=` in `WD:CAPS`) the alert also takes over the screen; `label` is one word of up to 12 characters shown there (`_` for a space, shown in capitals), defaulting to the pattern name |
| `led bright <0-100>`, `led stealth on\|off` | ring brightness (default 40), stealth keeps the ring dark; both saved on the board (1.6.6+) |
| `screen` | boards with `disp=`: the frame on the screen as `WD:SCREEN page=<0-3> x=<0\|64> <128 hex digits>` lines, 64 column bytes each, bit 0 the top pixel of the 8-pixel page (1.6.6+) |
| `csi start ch=<n> ...`, `csi tx ch=<n> ...`, `csi mesh ch=<n> id=<n> ...`, `csi stop`, `csi` | Wi-Fi radar (boards with `csi=1` in `WD:CAPS`, dev builds): capture CSI from one transmitter, be a transmitter, or run a multi-board mesh; `stop` returns to the mode the board had before. See "Wi-Fi radar (CSI)" |
| `coproc`, `coproc mode btc\|wifi\|off` | boards with `coproc=` in `WD:CAPS` (1.6.8+): the co-processor radio's state as `WD:COPROC`, and from then on its results for the host that sent it; `mode` switches it between Bluetooth Classic, Wi-Fi 2.4 GHz and off (see "Co-processor radio") |
| `reboot` | restart the board; from 1.6.3 it stops scanning and sends queued records first |
| `ota begin <size>`, `ota end`, `ota abort` | firmware update (below) |

Anything else gets `unknown command '<word>'` followed by the help list.

- Without `-ch` the board scans every channel it supports.
- Unsupported channels are dropped. If none remain the board replies `WD:ERR reason=no_supported_channels`.
- The channel list resets to all supported channels on every connect and disconnect, unless another host is
  still linked (1.6+).
- Scanning also starts when the phone subscribes to TX. If a scan is already running for another host, the phone
  gets `WD:CAPS`, `WD:BOOT`, `WD:CRASH`, `WD:LINK`, `WD:MODE`, `WD:BEGIN` and `WD:PLAN` instead.

## Output lines

Space-separated `key=value` pairs. Ignore unknown keys.

| Line | Sent |
|---|---|
| `WD:CAPS fw=1.6.0 chip=esp32c5 bands=2g,5g ota=1 bt=1 gps=<0\|1> cap=1 usb=1 [csi=<0\|1>] [board=<id> board_name=<name>] [led=ring<n>] [batt=1] [disp=128x32] link=<ble\|usb\|uart> chans=1,6,11,36,...` | at scan start, on `caps`, and when a host joins a running scan; `link` names the transport that line was sent on (1.6+). 1.6.6+, each key only when it applies: `board=` the board the firmware identified from its hardware (`crumb` Biscuit Crumb, `poltergeist` Rabbit Labs Poltergeist; 1.6.7+ `biscuit_pro`, `biscuit_ultra`, `biscuit_dual` from the co-processor, see "Co-processor (WDCP)"; absent on generic boards) with `board_name=` its display name, spaces as `_` (boards added after the apps in the stores, the Waveshare ESP32-C6-LCD-1.47, ESP32-C5-LCD-1.47 and ESP32-S3-Touch-LCD-3.5, send `board=generic model=waveshare_c6_lcd147` / `waveshare_c5_lcd147` / `waveshare_s3_touch_lcd35`: older apps call an unknown `board=` partner hardware and offer it no firmware updates, and apps read `model=` only without a `board=`); `led=ring<n>` the board drives an n-pixel LED ring (1 = a single LED) and takes `led` commands; `batt=1` it has a battery gauge, reported in `WD:HEALTH`; `disp=128x32` it has a status screen (see "Screen"; color screens report their size, `disp=240x320`, `128x128`, `172x320`, `320x480`); 1.6.8+ `coproc=<btc\|wifi\|off>` a co-processor with a second radio and the mode it is in (see "Co-processor radio"). CSI dev builds: `csi=1` the board takes `csi` commands (ESP32-C5 and ESP32-C6), `csi=0` it doesn't; firmware without the key has no CSI |
| `WD:MODE mode=wifi\|bt\|cap\|csi` | at scan start and when the mode changes (1.3+; `cap` 1.5+; `csi` CSI dev builds) |
| `WD:BOOT fw= slot= image= reset= boots= prev_fw= prev_stage= rolled_back=` | at scan start and in `info` (1.2.2+) |
| `WD:CRASH kind=panic\|stack_overflow reason= task= pc=0x… core= up_ms= last= ch= heap_min=` | at scan start and in `info` after a crash or watchdog reset (1.5.3+); `reason=unrecorded` when no record survived |
| `WD:LINK conns=<n> mtu=<n> prev_disc=<0x… \| none> prev_disc_ago=<s>` | at scan start (1.5.3+): links since boot and why the last one ended |
| `WD:CONN itvl_ms=<n> latency=<n> timeout_ms=<n> status=<n>` | BLE only, when the link timing changes (1.6.2+). After connecting, the board asks for a 30-50 ms interval with a 5 s supervision timeout (15-30 ms during a firmware update); the phone may keep its own |
| `WD:HEALTH up=<s> heap= heap_min= largest= stack_scan= stack_bt= stack_cap= stack_gps= stack_usb= stack_health= low_stack= mtu= last= [txdrop=<n>] [batt=<%> batt_mv=<mV> [batt_rate=<%/h>]] [temp=<°C>]` | every 15 s while a host is linked (1.5.3+; `stack_usb` 1.6+; `batt` on boards with a gauge, 1.6.6+, left out while the level is unknown); `stack_*` = least bytes ever left (-1 = not running). 1.6.8+: `batt_rate` the gauge's charge rate in percent per hour, negative while discharging (co-processor 0.2.0+ only); `temp` the chip's own temperature. CSI dev builds: `stack_csi`, the CSI task's least free stack, only while it runs. After 1.6.8: `txdrop` = BLE output frames dropped since boot because the link couldn't keep up (the board keeps a few buffers free for GATT answers and waits briefly before dropping). Commands written over BLE run just after their write response is sent, so either write kind works under load; apps use write without response |
| `WD:BEGIN type=wifi interval=0 channel=auto` | scan started |
| `WD:PLAN chans=1,6,11` | at scan start and after `-ch` |
| `WD:AP ts=<ms> bssid=AA:BB:CC:DD:EE:FF ssid_hex=<hex> rssi=<dBm> ch=<n> auth=<token> hidden=<0\|1> [src=coproc]` | per AP; `src=coproc` (1.6.8+) when the co-processor's 2.4 GHz scan heard it rather than this chip |
| `WD:STATUS aps=<n> scans=<n>` | after each Wi-Fi sweep; `aps` = APs heard in that sweep |
| `WD:BT ts=<ms> mac=AA:BB:CC:DD:EE:FF type=<public\|random> rssi=<dBm> name_hex=<hex> mfg=<id> conn=<0\|1> [kind=classic cod=<6 hex> src=coproc]` | per Bluetooth device, about once per window (1.3+); `kind=classic` (1.6.8+) a Bluetooth Classic (BR/EDR) device from the co-processor's inquiry, with its Class of Device. WiGLE type `BT`; everything else is `BLE` |
| `WD:COPROC mode=<btc\|wifi\|off> state=<scan\|idle\|off\|err\|boot> n=<n> total=<n> dropped=<n> fw=<ver>` | the co-processor radio (1.6.8+), to hosts that sent `coproc`: on `coproc`, after each of its windows, and when its mode changes; `n` results in its last window, `total` since it started, `dropped` lost to the link's pacing |
| `WD:STATUS bt=<n> scans=<n> dropped=<n>` | after each Bluetooth window; `bt` = devices reported in it |
| `WD:GPS ts=<ms> lat=<deg> lon=<deg> alt=<m> fix=1 sats=<n>` | per GPS fix, at most once a second (1.4+); `alt` omitted when the sentence carried none |
| `WD:HS ts=<ms> bssid=… sta=… ssid_hex=<hex> ch=<n> rssi=<dBm> hc22000=WPA*02*…*00` | per captured WPA 4-way handshake (1.5+) |
| `WD:PMKID ts=<ms> bssid=… sta=… ssid_hex=<hex> ch=<n> rssi=<dBm> pmkid=<32 hex>` | per captured RSN PMKID (1.5+) |
| `WD:CLIENT ts=<ms> mac=… bssid=… rssi=<dBm> ch=<n>` | per station seen in capture mode, once per session (1.5+); `bssid` empty when unknown (probe-request) |
| `WD:STATUS cap=1 aps=<n> hs=<n> pmkid=<n> clients=<n> data=<n> scans=<n> dropped=<n>` | after each capture sweep (1.5+) |
| `WD:CSI`, `WD:CSIMESH`, `WD:CSICFG`, `WD:CSISRC`, `WD:STATUS csi=1 ...` | Wi-Fi radar (CSI dev builds), see "Wi-Fi radar (CSI)" |
| `WD:END reason=stop` | scan stopped |
| `WD:USB pong`, `WD:USB bye reason=<host\|idle>` | USB session replies (1.6+, see "USB") |
| `WD:ERR reason=<text>` | command refused |

- `ts`: milliseconds since boot.
- `ssid_hex`: SSID bytes as lower-case hex; empty with `hidden=1`.
- `ch` above 14 is 5 GHz (5000 + 5 × ch MHz).
- `auth`: `OPEN`, `WEP`, `WPA`, `WPA2`, `WPA3`, `WPA2_WPA3`, `UNKNOWN`.
- Firmware before 1.1 sends no `WD:CAPS` and ignores `-ch`.

## Firmware update (1.2+)

Boards that send `ota=1` in `WD:CAPS` accept a new app image over BLE, and from 1.6 over a USB session (see "USB").

1. Phone sends `ota begin <size>` (bytes of the app image, e.g. `wardrive_bridge.bin`). The board stops scanning
   and replies `WD:OTA ready window=<bytes> size=<bytes>`, or `WD:OTA error=<reason>`. `size` (1.6.3+) is the
   receive buffer the board could allocate; `window` is sized to fit it, so always use the `window` sent. More
   fields may follow in later versions.
2. Phone writes the image in order to the OTA characteristic, never more than `window` bytes past the last ack.
3. The board replies `WD:OTA ack=<bytes written>` every 4 KB (every `window / 2` when `window` is under 8 KB) and
   when the whole image is written.
4. After `ack=<size>`, phone sends `ota end`. The board checks the image, replies `WD:OTA done` and restarts into it.
5. `ota abort` or a disconnect cancels. The running firmware is untouched until `WD:OTA done`.

`WD:OTA` lines go only to the host that sent `ota begin`. One update runs at a time; a second `ota begin` gets
`WD:OTA error=busy`.

From 1.6.3 the board sends the records it still holds (`WD:AP`, `WD:BT`, `WD:CLIENT`, `WD:HS`, `WD:PMKID`) and
`WD:END` before `WD:OTA ready`. A handshake still waiting for its second message is not reported. Phones should send
`stop` and read until `WD:END` (or about 2 s) before `ota begin` or `reboot`, which also covers older firmware.

`WD:OTA error=no_memory`: the board couldn't allocate a receive buffer. Before 1.6.3 this happens on a board that has
been scanning for hours (the 24 KB buffer needs one contiguous block). Send `reboot`, wait for the board to link
again, and retry.

New firmware that fails to bring up Wi-Fi and BLE is rolled back on the next boot. From 1.2.2 a board on trial
firmware that isn't up within 30 s restarts itself, so the rollback happens without a power-cycle.

`WD:BOOT` fields: `slot` running app slot; `image` its state at boot (`trial` = first boot after an update,
`confirmed`, …); `reset` last reset cause (`power_on`, `software`, `crash`, `watchdog`, `brownout`, …); `boots` boot
count; `prev_fw` / `prev_stage` how far the previous boot got (`storage`, `wifi`, `bluetooth`, `ready`,
`boot_watchdog`); `rolled_back` the slot of an update that was rolled back, or `none`.

## USB (1.6+)

Boards with `usb=1` in `WD:CAPS` speak the same protocol over their USB port. Two ports are served, and a host can
use whichever one the board exposes:

| Port | Boards | Host sees |
|---|---|---|
| USB-Serial-JTAG | native USB (ESP32-C3/C5/C6/S3 wired straight to the connector) | CDC-ACM, `303a:1001` |
| UART0 | boards with a USB-UART bridge | the bridge (CP210x `10c4:ea60`, CH34x `1a86:7523` / `1a86:55d4`, ...) |

UART0 runs at 115200 baud, 8N1, no flow control (the console rate). The USB-Serial-JTAG port ignores the baud rate.

### Lines

- Text only, no binary frames: one command per line from the host, one output line per line from the board.
  Lines end in `\n`; a `\r` before it is ignored. Command lines are at most 240 bytes.
- The board sends exactly the lines it sends over BLE (every `WD:` line and every terminal reply), with no 12-byte
  frame header, no CMD/ACK ids and no MTU chunking.
- The only binary data is a firmware image during an update (below), and only after `WD:OTA ready`.

### Session

The port starts in log mode: ESP-IDF log text and boot messages, no protocol output. In log mode the board only
reacts to `wdusb hello`.

| Host sends | Board does |
|---|---|
| `wdusb hello` | opens a session on that port and replies with `WD:CAPS ... link=usb` (or `link=uart`), `WD:BOOT`, `WD:CRASH`, `WD:LINK`, `WD:MODE`, `WD:BEGIN`, `WD:PLAN`; starts scanning if it wasn't |
| `wdusb ping` | `WD:USB pong` |
| `wdusb bye` | `WD:USB bye reason=host`, then back to log mode |
| any other line | runs it as a command (same table as BLE) |

- From the hello on, that port carries only protocol lines; log output is held off it (and still goes to the other
  port when that one is in log mode).
- The session ends and the port returns to log mode after 60 s with nothing received (`WD:USB bye reason=idle`),
  on `wdusb bye`, when a native USB host disconnects or stops reading for 5 s, and on reboot. Send `wdusb ping`
  (or any line) at least every 20 s to keep it open.
- Until the session opens, expect boot and log text on the port. Send `wdusb hello` about once a second until a
  `WD:CAPS` line comes back, and skip any line that doesn't parse (a rare log line can still appear mid-session).
- `WD:CAPS` and the other start lines can arrive more than once; treat them as current state.

Everything a BLE host gets also arrives over a USB session: every `WD:` line (`WD:AP`, `WD:BT`, `WD:CLIENT`,
`WD:HS`, `WD:PMKID`, `WD:GPS`, `WD:STATUS`, `WD:HEALTH` every 15 s, `WD:MODE`/`BEGIN`/`PLAN`/`END`, `WD:CONN` excepted
as it is BLE-only) and every reply to a command.

### USB holds the board (1.6.6+)

Opening a session takes the board off Bluetooth: a linked phone is disconnected, then Bluetooth is switched off
entirely (host stopped, controller freed), so Wi-Fi has the radio to itself with no Wi-Fi/Bluetooth coexistence.
With Bluetooth off there is no advertising and no
status beacon: phones nearby simply stop seeing the board. Choosing the Bluetooth scan (`bt on`) during a session
brings Bluetooth back for the scan, and while it runs the board sends the linked-board status beacon (`STATUS_UUID`,
the USB flag set, the last holder's nickname) but takes no phone. Scanning, mode and channels carry on across the
switch. When the last session ends (`wdusb bye`, idle, unplug) Bluetooth comes back, the board advertises normally
again and phones relink by themselves.

### Several hosts

Before 1.6.6 a BLE phone and USB sessions could be active at the same time; from 1.6.6 a USB session ends the
Bluetooth link (above), and several USB ports can still be open together. Every host gets the full output stream, including
replies to commands another host sent. Commands from any host control the board and the last one wins (mode,
channels, start/stop). Connecting or disconnecting a host resets mode and channels only when no other host is
linked. Scanning stops when the last host goes away.

### Firmware update over USB

Same flow as BLE, with the image sent on the session port instead of the OTA characteristic:

1. Host sends `ota begin <size>`. On `WD:OTA ready window=<bytes> size=<bytes>` the next `<size>` bytes on the port are image
   data, not lines.
2. Host writes the raw image, never more than `window` bytes past the last `WD:OTA ack`. UART0 has no flow control,
   so the window is required there.
3. After `WD:OTA ack=<size>`, host sends `ota end` as a line. `WD:OTA done`, then the board restarts (a native USB
   port re-enumerates).

If no image byte arrives for 10 s the update is aborted and the port goes back to reading lines. Ending the
session aborts an update that port started.

### Download mode

The USB protocol never touches the reset lines. Flashing tools reset into the ROM bootloader with DTR/RTS exactly
as before. To talk to the firmware without resetting it, open the port with DTR and RTS both deasserted and leave
RTS deasserted (on native USB, RTS asserted with DTR deasserted resets the chip; on bridge boards RTS drives EN).

### Telling boards apart

`303a:1001` and the common UART bridges are shared by every ESP32 firmware. A board is WardriveESP when it answers
`wdusb hello` with a `WD:CAPS` line carrying `usb=1`.

## Bluetooth (1.3+)

Boards with `bt=1` in `WD:CAPS` can scan Bluetooth LE instead of Wi-Fi. `bt on` stops Wi-Fi scanning and starts
BLE discovery in 10 s windows with duplicate filtering, so each device is reported about once per window. The board
keeps its link to the phone while it scans. The mode resets to Wi-Fi on every connect and disconnect.

- `mac`: the address as broadcast, most significant byte first. `type` says whether it is a public or random address.
- `name_hex`: the advertised name as lower-case hex, empty when none was heard.
- `mfg`: the Bluetooth SIG company id from manufacturer data, as 4 hex digits, or empty.
- `conn`: 1 when the advertisement is connectable.

## GPS (1.4+)

Boards with `gps=1` in `WD:CAPS` have a UART NMEA receiver and stream fixes as `WD:GPS` lines. The phone uses
the board's position as a location source when its own fix is stale or was never granted, so a wardrive is never
left without coordinates. The capability is runtime-detected: a board only advertises `gps=1`, and only sends
`WD:GPS`, once it has parsed a valid fix — so a board with no GPS wired up never claims it.

- `lat` / `lon`: signed decimal degrees (south and west negative).
- `alt`: meters above sea level; omitted when the NMEA sentence carried no altitude.
- `fix`: always `1` (the board only emits a line once it has a valid fix).
- `sats`: satellites in use, when reported.
- Emitted at most once a second while connected. The board reads `$??GGA` and `$??RMC` sentences (any talker id).
- UART pins and baud are set in `menuconfig` under *WardriveESP* (default UART1, RX GPIO 4, 9600 baud, TX disabled).
- From 1.6.6, when the configured pin stays silent for the first 2.5 s, an ESP32-C6 also listens (input only) on
  GPIO 2 at 9600 baud and uses it if a module streams NMEA there. A pin the board's own hardware uses is skipped.

## Offline runs (microSD, dev build `-sd`)

Boards with `sd=1 sd_free_mb=<MB>` in `WD:CAPS` have a mounted microSD card. Only boards with both a card slot and a GPS use it: AWOK Dual C5, Koko V8, Marauder Mini
v3 and the Rabbit Labs Ghost Board (the Biscuit Ultra and the Poltergeist have no GPS).
While no host is linked, the board saves its Wi-Fi scan to the card: one WiGLE CSV 1.4 file per offline stretch,
`run_<n>.csv`, located by the board's own GPS. A run starts when the card mounts after power-up with no host linked,
and whenever the last host lets go. It closes when a host links. Run numbers only go up on a board, so a name is never
reused after a delete. Only the Wi-Fi scan is saved (no Bluetooth, no capture).

On every link the header lines carry the runs waiting for the app:

| Line | Sent |
|---|---|
| `WD:RUNS n=<count> bytes=<total>` | after `WD:LINK` when a card is mounted; closed runs only |

The app takes them with `sd` commands (replies go to the host that asked):

| Command | Reply |
|---|---|
| `sd list` | `WD:SD file name=run_3.csv size=<bytes> open=<0\|1>` per file, then `WD:SD list n=<count>`; `open=1` is the run still being written (never take it) |
| `sd read <name> <offset> <len>` | `WD:SD data name= off= len= size= crc=<crc32 hex> b64=<base64>`, at most 768 bytes; `len=0` at the end of the file. `crc` is the standard CRC-32 (zlib) of the decoded bytes |
| `sd del <name>` | `WD:SD del name= ok=1`, or `ok=0 reason=open_run\|no_file` |
| `sd status` | `WD:SD mounted=1 total_mb= free_mb= run=<open\|closed\|none> file= rows= ...`, or `WD:SD mounted=0 slot=<0\|1>` |
| `time <epoch>` | sets the board's clock when its GPS hasn't (rows written before either show time since boot from 1970) |
| any error | `WD:SD err reason=not_mounted\|no_file\|no_memory\|usage name=` |

Taking a run (the app drives it; the board never deletes on its own):

1. `sd list`; skip `open=1`.
2. `sd read` from offset 0 (or from the bytes already held for this board + name + size, after a drop), next offset =
   offset + `len`. Check each chunk's `crc`; ask again for a chunk that fails or never answers. Up to 6 reads may be
   outstanding (the board queues 6 commands; reads past that are answered from its Bluetooth task, which drops them
   when its buffers are busy). The apps adapt: 4 to start, one more after a clean stretch, half after a loss. Over
   Bluetooth ask for 768 bytes (the most a read returns); USB readers with a 1024-byte line limit ask for 640.
   From 1.7.8 a read over Bluetooth also switches the link to the fast timing (15-30 ms, like a firmware update)
   until 3 s after the last one.
3. When the bytes held equal `size`, import the file as its own closed run, then `sd del <name>`.

A drop at any point loses nothing: the file stays on the card until `sd del`.

Boards with a color screen show the take from the reads and deletes alone (nothing new on the wire): SENDING RUNS,
run n of N (N = closed runs on the card when the first read came), the file, a bar from the furthest byte read over the
file's size, KB and KB/s, then SENT for 2.5 s once `sd del` leaves no closed run on the card. It gives up after 8 s with
no read (30 s once every byte was read, while the phone saves the file).

## Capture (1.5+)

Boards with `cap=1` in `WD:CAPS` can do **passive** 802.11 promiscuous capture instead of Wi-Fi scanning. `cap on`
puts the radio in monitor mode and camps each planned channel ~300 ms, reporting three kinds of artifact. It is
**receive-only** — the board never transmits, deauthenticates or injects, so an artifact appears only when a client
associates (or probes) on its own. For authorized security research / your own networks. The mode resets to Wi-Fi on
every connect and disconnect, same as Bluetooth.

Capture is a **superset of a Wi-Fi scan** (1.5.1+): it also emits `WD:AP` from the beacons and probe-responses it
hears (BSSID, SSID, RSSI, channel, auth parsed from the RSN/WPA IEs, hidden flag), reporting an AP the first time it
is seen and again when a later sighting is notably stronger. So a board left in capture mode still fills the wardrive
map and WiGLE CSV with APs — it just adds handshakes, PMKIDs and clients on top. (It is passive beacon listening, not
active probing, so a plain scan still edges it slightly on probe-only APs and channel-cycle speed.)

- `WD:HS` carries a full hashcat-22000 line in `hc22000` (`WPA*02*MIC*AP*STA*ESSID*ANONCE*EAPOL*00`): the board pairs
  the handshake's M1 ANonce with the M2 by replay counter and zeroes the MIC field in the EAPOL copy, so the phone can
  append the line to a `.hc22000` hashlist as-is.
- `WD:PMKID` carries the RSN PMKID an AP puts in EAPOL-Key M1 (only some APs do). Build `WPA*01*PMKID*AP*STA*ESSID***`.
- `WD:CLIENT` is a station MAC seen associating, probing or in a data frame, with its BSSID when known. Reported once
  per device per capture session.
- `bssid`/`sta`: most significant byte first. `ssid_hex`: the ESSID the board learned from a beacon/probe-response for
  that BSSID, as lower-case hex; empty when it hasn't heard one yet.
- Each artifact is deduped for the session on the board (handshake by MIC, PMKID by value, client by MAC); the phone
  should dedup again by `pmkid`/MIC and by client MAC.
- Longer per-channel dwell than a scan, because handshakes are opportunistic. Set `ch 1,6,11` to camp fewer channels.

## Scanning

- 2.4 GHz: channels 1–14, active, driver default dwell.
- 5 GHz (ESP32-C5): passive, 120 ms per channel. From 1.5.5 `chans` lists every 5 GHz channel the radio tunes —
  36–64, 100–144 and 149–177 — receive-only: passive scan in Wi-Fi mode, promiscuous RX in capture mode, no probes.
- The board requests its maximum allowed TX power at start (1.5+) so active-scan probe requests reach farther APs;
  the driver clamps to the chip and country ceiling. RX sensitivity is fixed hardware — there is no RX-power setting.

## Wi-Fi radar (CSI, experimental)

Dev builds only (branch `fw-csi-radar`, `info` shows `-csi-dev`); not in a release and not verified on hardware yet.
Boards with `csi=1` in `WD:CAPS` (ESP32-C5, ESP32-C6) capture Wi-Fi Channel State Information: the per-subcarrier
channel estimate of each frame the radio decodes. People moving between a transmitter and a receiver change it, which
the app turns into motion and presence through walls. With 4-8 boards around a building (`csi mesh`), every pair of
boards is a link, and the app can image where the motion is (radio tomographic imaging). It is a role like `bt` and
`cap`, and only ever starts on a `csi` command.

Three ways to run it:

- **rx** (`csi start`): one board listens to one transmitter (a nearby AP, or a board in `csi tx`) and streams the
  amplitude vector of every packet.
- **tx** (`csi tx`): the board is a transmitter only.
- **mesh** (`csi mesh`): every board transmits and listens to the others, and reports each link as a compact summary
  ten times a second.

### Commands

| Command | Effect |
|---|---|
| `csi start ch=<n> [mac=<AA:BB:CC:DD:EE:FF>] [rate=<1-100>] [sc=<8-64>]` | rx on channel `n` (any channel in `chans`). `mac` keeps frames from that transmitter only: a nearby AP's BSSID, or the `self` address of a board in `csi tx`. Without it every transmitter is reported. `rate`: the most packets per second sent on (default 30). `sc`: amplitudes per packet (default 32) |
| `csi tx ch=<n> [rate=<1-100>] [id=<0-255>] [via=espnow\|raw]` | tx: a small frame `rate` times a second (default 50) at HT20 MCS0, carrying `id` (default 0). `via`: broadcast ESP-NOW (default) or a raw 802.11 data frame, see "Frames" |
| `csi mesh ch=<n> id=<0-255> [rate=<1-100>] [slot=<i> slots=<n>] [report=<50-1000>] [via=espnow\|raw]` | mesh: transmit like `tx` with this board's `id`, and listen to every other board on the channel. `report`: ms per `WD:CSIMESH` line (default 100). `slots` (1-16, default 1 = none) and `slot` (default `id` mod `slots`): see "Slots" |
| `csi stop` | end it and go back to the mode the board had before |
| `csi` | `WD:CSICFG` and the status, or the usage when it is off |

- Any of the first three sent while the role runs restarts it with the new settings.
- Refusals: `WD:ERR reason=csi_unsupported chip=<chip>` (a board without `csi=1`), `csi_bad_channel`, `csi_bad_mac`,
  `csi_bad_id`, `csi_bad_slot`, `csi_bad_via`, `csi_no_channel`, `csi_usage`, `busy` (updating). `WD:ERR reason=csi_failed step=<step>
  err=<error>`: the radio refused a step; Wi-Fi is restored and the board goes back to its previous mode.
  `WD:ERR reason=csi_wifi_restore err=<error>`: Wi-Fi could not be restored after the role, and the board reboots
  rather than stay without it.
- `stop`, `start`, `wdstream start -wifi|-bt|-cap`, `bt on` and `cap on` work as for any role. Like every mode it resets
  to Wi-Fi on every connect and disconnect, so a lost link ends the radar, Wi-Fi is restored, and the board comes back
  as usual.
- No `WD:AP` or `WD:BT` while it runs. `WD:HEALTH` (with `stack_csi`), `WD:GPS` and command replies carry on.

### Lines

- `WD:MODE mode=csi`, `WD:BEGIN type=csi interval=0 channel=<n>`, then `WD:CSICFG` once it runs.
- `WD:CSICFG mode=<rx|tx|mesh> ch=<n> mac=<AA:BB:CC:DD:EE:FF|any> rate=<n> sc=<k> id=<n> slot=<i> slots=<n>
  report=<ms> via=<espnow|raw|none> txrate=<ok|ofdm6|error|none> self=<AA:BB:CC:DD:EE:FF>`: `sc` is the amplitudes per
  packet (rx) or bins per link (mesh, 8), 0 in tx; `report` is 0 outside mesh. `via` is how this board transmits
  (`none` in rx). `txrate` is whether the frame rate took: `ok` HT20 MCS0, `ofdm6` the driver refused HT20 and the
  board fell back to legacy OFDM 6 Mbps (still CSI), or the driver's error name when both were refused (the frames then
  go at the 1 Mbps default and receivers get no CSI from them). `self` is the board's Wi-Fi address, the transmitter
  address of its frames.
- `WD:STATUS ...` every second, see below.
- `WD:END reason=stop` when it ends, after Wi-Fi is restored.

#### rx: `WD:CSI`

`WD:CSI ts=<ms> ch=<n> sc=<k> p=<packet>;<packet>;...`: one batch, up to 8 packets and at most 100 ms of them. Each
packet is `dt,rssi,nf,src,fmt,nsc,amps`:

| Field | Meaning |
|---|---|
| `dt` | ms after `ts` (the first packet is 0) |
| `rssi` | dBm |
| `nf` | noise floor, as the driver reports it |
| `src` | the transmitter as a 16-bit hash, 4 hex digits; `WD:CSISRC` names it |
| `fmt` | frame format: 1 11a/g (L-LTF), 2 HT, 3 VHT, 4 HE SU (others possible) |
| `nsc` | subcarriers in the driver's estimate (64 for a 20 MHz L-LTF or HT-LTF; HT40 and HE carry more) |
| `amps` | `sc` amplitudes, 2 hex digits each (0-255): the driver's subcarriers split into `sc` equal groups in the driver's order, each the RMS magnitude of its nonzero I/Q values. Not gain-compensated: the radio's gain control moves the whole vector in steps, so normalize per packet (or by `rssi`) before comparing over time |

- `WD:CSISRC src=<hash> mac=<AA:BB:CC:DD:EE:FF> [id=<n>]`: the first time a transmitter shows up in a session, and
  again after a host joins. `id` when it is a WardriveESP board in `tx` or `mesh`.
- `WD:STATUS csi=1 mode=rx ch=<n> pps=<n> heard=<n> skipped=<n> dropped=<n>`: packets sent in the last second, CSI
  frames heard in the last second (after the `mac` filter), left out by `rate` in the last second, and lost this
  session because the link was slower.

```
WD:CSICFG mode=rx ch=6 mac=AA:BB:CC:11:22:33 rate=30 sc=32 id=0 slot=0 slots=1 report=0 self=58:8C:81:00:12:34
WD:CSISRC src=4f1a mac=AA:BB:CC:11:22:33
WD:CSI ts=81234 ch=6 sc=32 p=0,-47,-96,4f1a,2,64,<64 hex digits>;33,-48,-96,4f1a,2,64,<64 hex digits>
WD:STATUS csi=1 mode=rx ch=6 pps=30 heard=212 skipped=182 dropped=0
```

#### tx

- `WD:STATUS csi=1 mode=tx ch=<n> id=<n> pps=<n> txfail=<n>`: frames sent in the last second, frames the driver
  refused this session.

#### mesh: `WD:CSIMESH`

`WD:CSIMESH ts=<ms> id=<this board> ch=<n> win=<ms> l=<link>;<link>;...`: every `report` ms, one entry per board heard
in that window (boards not heard are left out; no line when none was heard). `win` is the window's length. Each link
is `tx,n,rssi,rsd,motion,bins`:

| Field | Meaning |
|---|---|
| `tx` | the transmitting board's `id` (the receiver is this line's `id`) |
| `n` | frames heard from it in the window |
| `rssi` | mean RSSI, dBm |
| `rsd` | RSSI standard deviation in tenths of a dB |
| `motion` | the mean over the 8 bins of (standard deviation / mean) of the normalized amplitudes in the window, x1000: low on a still link, higher while something moves through it (values need the bench) |
| `bins` | 8 amplitude bins, 2 hex digits each: the subcarriers in 8 equal groups, each frame normalized so 128 (`80`) is its own mean, then averaged over the window. The shape of the channel, with the radio's gain steps divided out |

- Each pair of boards is heard both ways (A in B's line, B in A's), so 8 boards give 56 directed links for 28 pairs.
  Average the two directions per pair, or keep both.
- `WD:STATUS csi=1 mode=mesh ch=<n> id=<n> links=<n> heard=<n> pps=<n> txfail=<n> synced=<0|1> dropped=<n>`: the most
  links in one window in the last second, frames heard from other boards in the last second, frames sent in the last
  second, frames the driver refused this session, whether this board's slot timing is locked to slot 0 (1 for slot 0
  itself; 0 without slots), and frames from boards past the 16 it keeps windows for.
- A board keeps windows for 16 transmitters; frames from more are counted in `dropped`.

```
WD:CSICFG mode=mesh ch=6 mac=any rate=50 sc=8 id=3 slot=3 slots=8 report=100 self=58:8C:81:00:12:34
WD:CSIMESH ts=95120 id=3 ch=6 win=100 l=0,5,-58,12,9,7a858e807d7f8487;1,4,-71,25,64,6c90a1785e8a9a70;5,5,-66,8,11,82807b8384807e7f
WD:STATUS csi=1 mode=mesh ch=6 id=3 links=3 heard=148 pps=50 txfail=0 synced=1 dropped=0
```

### Slots (mesh)

The radio already waits for a clear channel before each frame (CSMA), and the frames are short (about 0.1 ms at HT20
MCS0), so 8 boards at 50 frames/s use about 4% of the channel and rarely collide. Slots are there for the case where
boards can't hear each other well (hidden boards) and collide at a receiver between them.

- The app assigns each board a slot: `slots=<number of boards>` and `slot=<0..n-1>` (or leaves `slot` out to use
  `id` mod `slots`). Use the same `rate` and `slots` on every board.
- The period (1/`rate`) is split into `slots` equal slots. The board in slot 0 free-runs. Every other board times its
  frames from slot 0's frames: `slot` x period / `slots` after the last frame it heard from slot 0. Every frame names
  its sender's slot in its address, so no clock sync over the phone link is needed.
- A board that hasn't heard slot 0 for 2 s free-runs at its period, so a missing slot 0 never stops the mesh; it shows
  `synced=0` in its status.
- At 50 frames/s and 8 slots a slot is 2.5 ms, about 25 frame lengths: plenty of guard for the receive-time jitter.

### Frames (tx and mesh)

Both kinds carry the same body: `WDCSI`, version 3, id, slot, slots and a 32-bit sequence number. Receivers recognize
a WardriveESP transmitter by that body (or the raw frame's group address) and read its id and slot from it. Frames are
sent on a timer, not a task loop. No beacon, no SSID: nothing shows up in a Wi-Fi list.

- `via=espnow` (default): broadcast ESP-NOW (a vendor action frame from `self`), with the broadcast peer's rate set to
  HT20 MCS0. This is how Espressif's own CSI sender (esp-csi `csi_send`) feeds its receivers. The body is 24 bytes.
- `via=raw`: a data frame with no DS bits to the locally administered group address `03:57:44:43:<slot>:<id>` ("WDC",
  then the sender's slot and id), from and BSSID `self`, LLC/SNAP with EtherType 0x88B5 (IEEE local experimental), then
  the body (45 bytes). The raw-frame rate is set to HT20 MCS0 before and after Wi-Fi starts.
- The rate matters: the driver's default for both is 1 Mbps DSSS, which carries no CSI. If HT20 is refused the board
  falls back to OFDM 6 Mbps; `txrate=` in `WD:CSICFG` says what took. The transmitting board also sets its bandwidth to
  20 MHz.
- Bench 2026-10-28 (the first dev build, raw frames, rate set only before Wi-Fi started): a C6 in `csi tx` reported
  50 frames/s and no failures, and a C5 receiver got no CSI at all from it while hearing 8 other transmitters, so those
  frames went at 1 Mbps. ESP-NOW became the default after that.

### Rates and link budget

- rx: a packet is about 30 bytes plus 2 per amplitude, about 95 bytes at `sc=32`. The default 30/s at `sc=32` is about
  3 KB/s in about 10 lines a second; 50/s at `sc=64` about 8 KB/s. Keep the defaults over Bluetooth; USB carries much
  more.
- mesh: a link is at most about 40 bytes, so a board hearing 7 others sends about 320 bytes per window: about 3 KB/s
  per board at the default `report=100` (10 Hz), 25 KB/s for 8 boards on one phone. Each board has its own Bluetooth
  link, which carries 3 KB/s easily; if the phone's total is the limit, `report=200` halves it (5 Hz). The frames per
  link per window are `rate` x `report` / 1000 (5 at the defaults); more frames give steadier statistics.
- rx `rate` is a ceiling; the transmitter sets the floor. A busy AP sends data frames many times a second, an idle one
  only beacons (10/s). 2.4 GHz beacons sent at 1 Mbps (11b) carry no CSI at all; 5 GHz beacons (OFDM) do. A board in
  `csi tx` gives a steady, known rate.
- When the link can't keep up, rx packets are dropped on the board and counted (`dropped`), never queued without
  bound. Mesh windows don't queue at all: a slow link only delays the next line.

### How the board does it

- The Wi-Fi driver only produces CSI when it was initialized for it, which costs about 10 KB of RAM. The board still
  boots without it, exactly as before. Entering the role restarts Wi-Fi with CSI on (and the HT20 MCS0 frame rate for
  raw tx and mesh); leaving it restarts Wi-Fi exactly as booted (both bands, every channel, max TX power), then the next
  mode starts.
- rx and mesh: promiscuous receive of management and data frames on `ch`, 20 MHz. Estimates from L-LTF (11a/g),
  HT20/HT40, VHT (C5) and HE SU frames. Mesh counts only frames from WardriveESP transmitters (the `WDCSI` body).
- Over USB, Bluetooth is off as in every USB session, so Wi-Fi has the radio to itself. Over Bluetooth the radio is
  shared with the link and the stream has small gaps.

## Co-processor (WDCP, 1.6.7+)

The Biscuit Pro and Biscuit Ultra have two chips behind one USB plug: an ESP32-C5 (the scanner, where WardriveESP
runs; native USB `303a:1001`) and a classic ESP32 WROOM (behind a CH340 `1a86:7523`) that holds the battery gauge
(MAX17048 on WROOM SDA 21 / SCL 22). The chips share one internal UART at 115200 8N1: WROOM TX 17 / RX 16 to C5
RX 12 / TX 11, which are the C5's UART0 pins. Neither chip can reset the other.

The WardriveESP co-processor (`firmware/wardrive-coproc`, ESP32) runs on the WROOM, reads the gauge and talks to the
C5 in WDCP lines. From 0.2.0 it is also a second radio (Wi-Fi by default from 0.2.1, see "Co-processor radio" below); 0.1.0 keeps
Wi-Fi and Bluetooth off.

### Lines

Text, one per line, `\n` terminated (`\r` ignored), always starting `WDCP `. Each side sends a newline before every
line, because the C5's console log shares the C5 → WROOM wire: a reader looks for `WDCP ` anywhere in a line and
ignores everything else (log text, the stock Biscuit firmware's `CMD:` lines, unknown verbs).

| Co-processor → C5 | When |
|---|---|
| `WDCP hello proto=1 board=<biscuit_ultra\|biscuit_pro\|biscuit_dual> fw=<ver> chip=esp32 gauge=<0\|1> scan=<0\|1> [mode=<off\|btc\|wifi>] mac=<wroom mac>` | every 2 s for 30 s after boot, for 20 s after it hears the C5's ROM banner (`ESP-ROM:` / `rst:0x`, a C5 reboot), then every 10 s while no `ping` came for 90 s; on `hello?` |
| `WDCP batt <pct> <mV> [<rate>]` | every 30 s, after a hello, and on `batt?`; `-1 0` without a gauge. 0.2.0+ adds the gauge's charge rate (MAX17048 CRATE) in hundredths of a percent per hour, negative while discharging |
| `WDCP pong` | on `ping` |
| `WDCP err setboard <id>`, `WDCP err mode <m>` | `setboard` with an unknown id, `mode` with an unknown mode |
| `WDCP bt <mac> <rssi> <cod> [<name hex>]` | 0.2.0+, Bluetooth Classic mode: a device that answered the inquiry, once per window. `mac` 12 hex digits, most significant first; `cod` the 24-bit Class of Device as 6 hex digits; the name (from its inquiry response or a remote name request) as hex, absent when none |
| `WDCP ap <bssid> <ch> <rssi> <auth> <hidden> [<ssid hex>]` | 0.2.0+, Wi-Fi mode: an AP heard on 2.4 GHz, when new, every 60 s while heard, or when 6 dB stronger. `auth` as in `WD:AP` |
| `WDCP stat <mode> <state> <n> <total> <dropped>` | 0.2.0+: after each window (Bluetooth inquiry or Wi-Fi sweep), when scanning starts or pauses, and on `stat?`. `state` `scan`, `idle` (not asked to scan), `off`, `err` (the radio failed to start) |
| `WDCP mode <m> [restart]` | 0.2.0+: the reply to `mode`; `restart` when it changed, and the co-processor restarts into the new mode about 0.3 s later (a hello follows) |

| C5 (or the CH340 console) → co-processor | Does |
|---|---|
| `WDCP ping` | `WDCP pong`; from the C5 it also ends the hello repeats |
| `WDCP hello?` | sends the hello again |
| `WDCP batt?` | reads the gauge now and sends `batt` |
| `WDCP setboard <biscuit_ultra\|biscuit_pro\|biscuit_dual>` | stores the model in NVS, then sends a hello (to the C5 too) |
| `WDCP mode <off\|btc\|wifi>` | 0.2.0+: stores the scan mode in NVS and restarts into it (see the reply above) |
| `WDCP run <1\|0>` | 0.2.0+: scan, or stop. From the C5 it is a lease: `run 1` lasts 95 s and the C5 renews it with every ping, so a C5 that reboots, loses its host or runs older firmware never leaves the radio on. From the console it holds until `run 0` or a restart (bench) |
| `WDCP stat?` | 0.2.0+: sends `stat` |

- `board`: nothing on the WROOM side tells a Pro from an Ultra, so the co-processor says `biscuit_dual` until it is
  told the model with `setboard`. The app's flasher reads the stock WROOM banner before converting the board
  (`Device name: Biscuit Ultra` / `Biscuit Pro`) and stores it.
- `scan=1` (0.2.0+): the co-processor has a second radio, and `mode` is the one it runs. `scan=0` (0.1.0): battery only.
- The CH340 port (WROOM UART0, 115200) carries the co-processor's log, including every line it sends to the C5
  (`to C5: WDCP batt 87 4012`) and receives (`from C5: WDCP ping`), and takes the same commands.

### WardriveESP on the C5

- ESP32-C5 builds only. UART0 is already up on a C5 (console log out on GPIO 11, the UART link in on GPIO 12), so
  listening costs nothing. WardriveESP **sends nothing WDCP until it has heard a valid hello** (`proto` ≥ 1, a known
  `board`, a `fw`), so a generic C5 board, or any one of 20 boards on a hub, never sees a WDCP line.
- On a hello: answers `WDCP ping` and `WDCP batt?`, sets its board from `board` (`board=biscuit_pro board_name=Biscuit_Pro`,
  `biscuit_ultra` / `Biscuit_Ultra`, `biscuit_dual` / `Biscuit_Pro_or_Ultra`), reports `batt=1` when `gauge=1`, and
  sends `WD:CAPS` again to every linked host. A board already identified another way (Crumb gauge, Poltergeist
  screen) keeps that identity.
- The battery is reported exactly like the Crumb's: `batt=1` in `WD:CAPS`, `batt=<%> batt_mv=<mV>` in `WD:HEALTH`,
  and `info`. It pings every 30 s; after 120 s with no line from the co-processor the level reads unknown (no `batt=` in `WD:HEALTH`)
  until the next `batt`.
- LED: the Pro's single NeoPixel on C5 GPIO 10 (`led=ring1`), also on `biscuit_dual`. `biscuit_ultra` drives no LED
  (the bench Ultra has none fitted). A hello that names another model takes effect at once: the ring starts or
  stops and `led=` in the next `WD:CAPS` follows it, no reboot (1.6.8+).

### Co-processor radio (co-processor 0.2.0+, WardriveESP 1.6.8)

The WROOM's own radio, used for what the C5 can't do or as a second 2.4 GHz receiver. One device, one radio card in
the app: the co-processor is shown on the board's card as its second radio, never as a board of its own, and the
channel planner does not count it.

| Mode | What the WROOM does |
|---|---|
| `btc` | Bluetooth Classic (BR/EDR) general inquiry, 10.24 s, at full transmit power; then a remote name request to at most 2 devices that answered without a name (stronger than -82 dBm, each one asked at most every 10 min); then 5 s with the radio quiet. Never discoverable or connectable, never pairs |
| `wifi` | passive scan of 2.4 GHz channels 1-13, 220 ms each, receive only (no probe requests); an AP is reported when new, every 60 s while heard, or when 6 dB stronger. The default from 0.2.1 |
| `off` | no radio started at all (the default in 0.2.0) |

- The mode is saved on the co-processor and survives restarts; a co-processor that was never told starts in its
  built-in default (`COPROC_DEFAULT_MODE` in `wardrive_coproc.c`: `wifi` from 0.2.1, `off` in 0.2.0). On the bench `btc`
  cost the C5 about 25% of its 2.4 GHz finds and `wifi` cost nothing measurable, so `btc` is opt-in. Changing it restarts the WROOM into the new
  mode, so only one radio stack ever runs. Apps show the mode the board reports (`coproc=` in `WD:CAPS`, `WD:COPROC`).
- It scans only while WardriveESP asks: a host linked that sent `coproc`, the board scanning, and a mode other than
  `off` (`WDCP run`). With no host, or a host that never asked, the WROOM's radio stays idle.
- Results reach only the hosts that sent `coproc` on their current link, so an app that predates this never gets a
  Bluetooth Classic device it would store as BLE. `coproc mode <m>` also counts as asking.
- Link budget: results are paced on the WROOM to 4 KB/s on average (bursts to 2 KB) of the 115200 link's ~11.5 KB/s,
  de-duplicated per window (Bluetooth) or per 60 s (Wi-Fi); past that they are dropped and counted (`dropped`), never
  queued without bound. A Bluetooth line is about 40-100 bytes and a Wi-Fi line about 45-110, so a few hundred
  devices or APs a minute use well under half the link.
- Forwarded as `WD:BT ... kind=classic cod=<cod> src=coproc` (type `public`, `conn=0`, no `mfg`) and
  `WD:AP ... src=coproc`. Apps store Bluetooth Classic with WiGLE type `BT` and the WiGLE AuthMode for its Class of
  Device (`Smartphone;10`, `Headphones;10`, ...), and count co-processor results on the card's second-radio row, not
  in the board's own counts.

### Flashing

| Chip | Port | Image |
|---|---|---|
| C5 | native USB, `--before usb-reset` | `WardriveESP-esp32c5-merged.bin` at `0x0` |
| WROOM | CH340, default DTR/RTS reset, 460800 baud | Update: the separate parts, `bootloader.bin` 0x1000, `partition-table.bin` 0x8000, `wardrive_coproc.bin` 0x10000 (published as `bins/esp32/wardrive-coproc-bootloader.bin`, `wardrive-coproc-partition-table.bin`, `wardrive_coproc.bin`). They leave NVS (0x9000-0xF000) alone, so the stored model and mode survive. `wardrive-coproc-merged.bin` at `0x0` covers NVS with 0xFF and **erases the stored model and mode** (send `WDCP setboard <model>` after it); use it only for a first flash over the stock firmware. From 0.2.0 the factory app is 3 MB (Bluetooth Classic and Wi-Fi do not fit 1 MB) |

Leave about 10 s between ROM sessions on the C5, and end a failed one with a plain reset (RTS pulse, DTR low): a
session started while the C5 app is still booting can leave it in the ROM loader, where the WROOM loses it.

## LED alerts (1.6.6+)

The app picks a pattern per kind of alert and passes the alert banner's category color:

| App alert | Pattern |
|---|---|
| Police / fire / EMS responders | `police` |
| Adult toys, health devices | `heartbeat` |
| Attack tools (Flipper, RF and hacking gadgets), gunshot sensors | `strobe` |
| Trackers and Find My, drones, Remote ID | `spin` |
| Cameras (Flock, ALPR, CCTV, body cams, spy cams, glasses, home cams) | `pulse` |
| Captured handshake / PMKID (gold), cracked password (green) | `sparkle` |
| Celebrations | `rainbow` |

Example: `led alert heartbeat FF4FA3 5`, or with a screen label `led alert spin 00C8FF 6 DRONE` /
`led alert heartbeat FF4FA3 5 ADULT_TOY`. Boards without `led=` in `WD:CAPS` reply `no LED ring on this board`, so the
app only sends these to boards that report a ring.

## Screen (1.6.6+)

Boards with `disp=128x32` in `WD:CAPS` (the Rabbit Labs Poltergeist) run a status screen on their own; the app sends
nothing for it beyond what it already sends:

- Boot: the WardriveESP wordmark, firmware version and board name.
- Dashboard: who holds the board (`BT` or `USB` plus the `iam` nickname) and uptime; the number of different APs seen
  since boot in large digits with a `+N new` count for the last 10 s; mode and channel (or Bluetooth devices); the
  capture count; a strip of per-channel activity (2.4 GHz left, 5 GHz right) with a mark under the channel being scanned.
- `led alert ...` (and `led <pattern> ...`) also shows a full-screen banner with the label, flashing in step with the
  LED, for the alert's seconds; `led off` ends it. A phone or USB host linking shows `LINKED` for 1.5 s.
- `led stealth on` turns the screen off as well. The picture moves by a pixel every 3 minutes and dims after 30 s with
  no host, so it doesn't burn in.
- `screen` returns the frame being shown (see "Commands").
