# The WardriveESP service

The bridge can present its fleet to a phone app as one WardriveESP board. The app links to the
bridge's C6 over Bluetooth, with no pairing, and reads the access points the nodes hear as text
lines. The service is off until the operator turns it on.

WardriveESP is by RocketGod and MIT licensed. Its protocol document is vendored, unmodified, at
[`vendor/wardrive-esp/PROTOCOL.md`](vendor/wardrive-esp/PROTOCOL.md), with its license and
provenance beside it. This page says which part of that document the bridge implements and how
the fleet maps onto it. The code is `wartui_proto::wardrive_esp`
([`crates/wartui-proto/src/wardrive_esp/`](../crates/wartui-proto/src/wardrive_esp/)), written
from the document; it copies none of WardriveESP's source.

## Turning it on

```sh
wartui bluetooth wardrive-esp        # print the setting
wartui bluetooth wardrive-esp on
wartui bluetooth wardrive-esp off
```

The settings modal's `wardrive esp` row sets it too. The bridge keeps the setting in its flash
and boots into it, so a bridge set once serves phones with no wartui host attached.

**The service is open.** While it is on, any phone in range can link, hold the bridge's one
Bluetooth connection, and start or stop the scan. Off, the bridge advertises and behaves as it
does without the service.

## One host at a time

A linked phone counts as the bridge's host and holds it alone.

- The C6 takes one Bluetooth connection. While a phone is linked, a wartui host cannot connect
  over Bluetooth, and the reverse.
- A wartui host on USB takes the bridge from a phone, as it does from a Bluetooth host. The C6
  drops the phone and admits none while USB holds the bridge. Once USB has been silent for 15 s,
  the bridge advertises the service again and the app relinks by itself.
- `wartui ports`, and any other `Identify`, is answered beside a linked phone and does not
  disturb it.
- No capture runs for a phone: the bridge sends it lines, and wartui's store sees none of them.

## What a phone sees

**Advertising.** A legacy advertisement cannot hold two 128-bit UUIDs and the name, so the pair
depends on the setting.

| Setting | Advertisement | Scan response |
| --- | --- | --- |
| on, and USB does not hold the bridge | flags, the WardriveESP service UUID | the name, `wartui XX:YY` |
| off, or USB holds the bridge | flags, the name | the wartui link's service UUID |

The name is `wartui XX:YY`, the bridge's last two octets, under either pair, which is how a
wartui host finds the bridge. The bridge sends no status beacon and no advert extras, and
advertises nothing while a central is connected.

**GATT.** The service sits beside the wartui link's, with no encryption or bonding required.

| Role | UUID | Properties |
| --- | --- | --- |
| Service | `47686F73-7445-5350-4272-696467655376` | primary |
| RX, phone to bridge | `0147686F-7374-4553-5042-726964675852` | write, write without response |
| TX, bridge to phone | `0247686F-7374-4553-5042-726964675854` | notify |
| CTRL, phone to bridge | `0347686F-7374-4553-5042-72694C525443` | write, write without response |

RX and CTRL are read alike. There is no OTA characteristic. The C6 serves an ATT MTU of 182.

**Linking.** Subscribing to TX links the phone and starts the scan. A subscription lasts for its
connection only: a phone subscribes again each time it connects. A central reads one stream:
the first subscription decides whether it is a phone or a wartui host, and the other stream's
subscription is refused. A central that has subscribed to neither stream 10 s after it connects is
disconnected, 30 s while the bridge's pairing window is open, and so is a phone that stays
unsubscribed for 30 s.

## Frames

The frame is the document's: a 12-byte header (`47 42`, version `01`, type, status, flags, a
32-bit command id and a 16-bit payload length, little-endian), then the payload.

| Direction | Frame | What the bridge does |
| --- | --- | --- |
| phone → bridge | CMD (type 1) | runs its payload as one command, acknowledged when it is empty or queued for the P4 |
| phone → bridge | unframed text | runs the whole write as one command, unacknowledged |
| phone → bridge | any other framed type | takes its id and runs nothing |
| bridge → phone | ACK (type 2) | 12 bytes, the command's id, no payload, ahead of the reply |
| bridge → phone | DATA (type 3) | text; one line a frame, or part of one longer than the frame's room |

The document leaves these open, and the bridge settles them so:

- A write is framed when its magic matches and its length field fits inside the write. The
  version, status and flags bytes are not read.
- One write is one command. A command is never joined from several frames.
- Every DATA frame carries the id of the latest framed command, stream lines included. Before
  any, it is 1.
- A DATA frame carries at most the MTU less 15 bytes: 167 at the served MTU, 8 at the minimum
  of 23. A reader joins payloads and splits on `\n`.
- END and ERR frames (types 4 and 5) are never sent. `WD:END` and `WD:ERR` are text lines.

## Commands

A command is cut to 240 bytes and trimmed. Its first word picks it.

| Command | Effect |
| --- | --- |
| `wdstream start -wifi [-ch 1,6,11,36]` | sets the channel list when given, then starts the scan, or answers `WD:MODE` when it runs already |
| `wdstream stop`, `stop` | stops the scan: a last `WD:STATUS`, then `WD:END`. Nothing when it is stopped |
| `start` | starts the scan with the list as it is. Nothing when it runs already |
| `caps` | `WD:CAPS` |
| `ch`, `ch 1,6,11`, `ch all` | shows, sets or resets the channel list |
| `status`, `info`, `help` | plain text lines, in wartui's own wording; `info` ends with `WD:BOOT` |
| `link` | plain text lines about the Bluetooth link: the phone's address and time linked, its `iam` name, signal, MTU, interval and timeout, and the links and last drop since the C6 booted |
| `drop` | `disconnecting the phone`, then the bridge disconnects the phone, which stops the scan |
| `iam a <nickname>`, `iam i <nickname>` | kept until the phone links again or lets go, and shown by `link`: Android or iPhone, and the nickname cut to 18 bytes on a character boundary. Answered with nothing |
| `bye`, `hb`, `client`, `time …`, and any other `iam` | taken, and answered with nothing |
| `bt`, `cap`, `csi`, `coproc`, `led`, `screen`, `sd`, `ota`, and `wdstream` with `-bt`, `-cap` or `-promisc` | `WD:ERR reason=unsupported`, and nothing changes |
| anything else | `unknown command '<word>'`, then the help list |

`wdstream` reads its arguments as the WardriveESP firmware does: `stop`, `-ch `, `-bt`, `-cap`
and `-promisc` count anywhere after the word, and `start` is optional. A channel list is the text
after `-ch ` or `ch `: numbers between commas or spaces, ending at the first other byte.

`reboot` is unknown on purpose: the service takes commands from any phone in range, so none of
them restarts the bridge. `drop` and `link` touch only the phone that sends them.

## Lines

| Line | Sent |
| --- | --- |
| `WD:CAPS fw=1.6.0 chip=esp32c5 bands=2g,5g ota=0 bt=0 gps=0 cap=0 usb=0 board=wartui board_name=wartui link=ble chans=…` | at scan start and on `caps` |
| `WD:BOOT fw=1.6.0 slot=<ota_0\|ota_1\|?> image=<trial\|confirmed> reset=<key>` | at scan start, and at the end of `info` |
| `WD:LINK conns=<n> mtu=<n> prev_disc=<0x2NN\|none> [prev_disc_ago=<n>s]` | at scan start |
| `WD:MODE mode=wifi` | at scan start, and to a `wdstream start` while scanning |
| `WD:BEGIN type=wifi interval=0 channel=auto` | at scan start |
| `WD:PLAN chans=…` | at scan start, and when the channel list changes while scanning |
| `WD:AP ts=<ms> bssid=AA:BB:CC:DD:EE:FF ssid_hex=<hex> rssi=<dBm> ch=<n> auth=<token> hidden=<0\|1>` | per sighting |
| `WD:STATUS aps=<n> scans=<n>` | every 5 s while scanning, and before `WD:END` |
| `WD:END reason=stop` | when the scan stops |
| `WD:ERR reason=<unsupported\|no_supported_channels>` | a command refused |
| `WD:HEALTH up=<s> heap=<bytes> heap_min=<bytes> mtu=<n> txdrop=<n>` | every 15 s while the phone is linked, scanning or not |

A scan starts with `WD:CAPS`, `WD:BOOT`, `WD:LINK`, `WD:MODE`, `WD:BEGIN` and `WD:PLAN`, in that
order. `WD:HEALTH` is queued as a command's answer is, so a backlog of stream lines never holds
it.

- `fw=` names the level of the protocol document, not the bridge's firmware version, which
  `info` gives. `board=wartui` and `ota=0` tell an app the bridge is not a board it updates.
- `chans=` in `WD:CAPS` is the bridge's channel pool. `WD:PLAN` is the phone's channel list.
- `ts` is milliseconds since the bridge booted, when the line was written.
- `auth` is `OPEN`, `WEP`, `WPA`, `WPA2`, `WPA3`, `WPA2_WPA3` or `UNKNOWN`. WPA2 Enterprise and
  mixed WPA/WPA2 read as `WPA2`; WAPI, OWE and anything unnamed read as `UNKNOWN`.
- `hidden=1` goes with an empty `ssid_hex=`.
- `WD:BOOT` describes the P4's image: `slot` is the running app slot, `image=trial` until a
  wartui host confirms an update, and `reset` is `power_on`, `software`, `watchdog`, `brownout`,
  `reset_pin` or `other`. The document's `boots`, `prev_fw`, `prev_stage` and `rolled_back` are
  left out: the bridge keeps none of them.
- `WD:LINK` describes the C6's Bluetooth link: `conns` counts the connections since the C6
  booted, this one included. `prev_disc` is `0x200` plus the HCI reason the connection before
  this one ended for, the form the WardriveESP firmware reports, and `prev_disc_ago` the seconds
  since. With no earlier disconnection since the C6 booted, it is `prev_disc=none`.
- `WD:HEALTH`: `up` is seconds since the P4 booted, `heap` and `heap_min` the C6's free and
  least free heap (the P4 has no heap), and `txdrop` the lines dropped from the phone's queue
  since it linked. The document's `largest`, `stack_*`, `low_stack`, `last`, `batt` and `temp`
  are left out: the bridge has no figure for them.

## How the fleet maps onto one board

- **The planner is untouched.** The fleet scans its whole pool, split across its nodes, as it
  does for a wartui host. A phone's channel list only filters what it is sent: a `WD:AP` goes out
  when its channel is on the list. A list naming no channel of the pool is refused with
  `WD:ERR reason=no_supported_channels`, and the list stays as it was. The list resets when the
  phone links or lets go.
- **Starting a scan stops the fleet first**, so every node starts with nothing reported and the
  phone hears each access point afresh. The scan is Wi-Fi only.
- **A node reports an access point once**, then again after 5 minutes or when it is 5 dB
  stronger (`wartui_proto::dedup`). A WardriveESP board reports every access point on every
  sweep, so a phone sees fewer lines for the same neighborhood from the bridge.
- **There is no sweep to count.** `WD:STATUS` goes every 5 s from the scan's start: `aps` is the
  `WD:AP` lines since the last one, and `scans` counts those periods.
- **The phone lets go, the fleet stops.** An unsubscribe, a disconnect, a `drop`, the setting
  turned off or USB taking the bridge each stop the scan.
- **A slow phone loses the oldest lines.** The bridge holds 16 KB of stream lines for the phone
  and evicts the oldest past that. An app stamps an access point with the phone's position when
  its line arrives, so a deep backlog would put access points in the wrong place. `status` gives
  the count dropped.

## What is not implemented

Everything the document marks as a later version or another transport: Bluetooth scanning
(`WD:BT`), capture, GPS, firmware updates, the USB protocol, the status beacon and advert
extras, `WD:CRASH` and `WD:CONN`, CSI, the LED ring, the screen, microSD runs and the
co-processor.

What the bench measured is in [`wardrive-esp-findings.md`](wardrive-esp-findings.md).
