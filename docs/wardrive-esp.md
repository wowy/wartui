# The WardriveESP service

The bridge can present its fleet to a phone app as one WardriveESP board. The app links to the
bridge's C6 over Bluetooth, with no pairing, and reads the access points or Bluetooth devices the
nodes hear as text lines. The service is off until the operator turns it on.

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

**Advertising.** A legacy advertisement holds 31 bytes and cannot carry two 128-bit UUIDs and the
name, so what it carries depends on the setting and on whether a central is connected.

| State | Advertisement | Scan response |
| --- | --- | --- |
| on, no central | connectable, every 100 ms: flags, the WardriveESP service UUID, the advert extras | the name, `wartui XX:YY`, and status bytes 0-8 |
| on, a central connected | not connectable, about once a second: flags, the status UUID, the advert extras | the status, with the nickname |
| off, no central | connectable, every 100 ms: flags, the name | the wartui link's service UUID |
| off, a central connected | nothing | nothing |

"On" is the setting on and USB not holding the bridge. The name is `wartui XX:YY`, the bridge's
last two octets, which is how a wartui host finds the bridge. The status UUID is
`0547686F-7374-4553-5042-726964675354`. It replaces the service UUID while a central is connected,
whether a phone or a wartui host, so an app does not try a bridge that is taken.

The status is manufacturer data under company id `0xFFFF`:

| Byte | The bridge sends |
| --- | --- |
| 0 | `W` |
| 1 | bit 0 a central is connected, bit 1 the phone's session scans, bits 2-3 its mode (`0` Wi-Fi, `1` Bluetooth), bit 7 the connected app said it is an iPhone. The USB, GPS and updating bits are clear |
| 2 | `0x80`: the temperature is unknown |
| 3-4 | the `WD:AP` or `WD:BT` lines in the last finished 5 s period, little-endian, at most `0xFFFF`; 0 with no phone |
| 5-6 | `0x17`, `0x03`: protocol level 1.7.3, as `WD:CAPS` names it |
| 7-8 | the bridge's last two octets, as in the name |
| 9+ | while a central is connected, the nickname from `iam`; nothing without one |

The advert extras are 6 bytes of service data under `0x5744`, frame A and frame B swapped every
5 s:

| Byte | Frame A | Frame B |
| --- | --- | --- |
| 0 | bit 0 `0`; bits 1-2 how the last phone link ended | bit 0 `1`; bits 1-2 the same |
| 1 | `0`: a generic board | channel bits 24-31 |
| 2 | `0xFF`: no battery gauge | channel bits 32-39 |
| 3-5 | channel bits 0-23 | channel bits 40-41, then `0` |

- How the last phone link ended: `0` no phone has linked since the P4 booted, `1` the app sent
  `bye` before it let go, `2` any other end, USB taking the bridge and the setting turned off
  among them.
- The channel bits are the phone's channel list, or the pool with no list or no phone, in
  the document's order. The bridge has no screen and no co-processor, so those bits are clear.
- Both refresh every 5 s and at once when what they say changes. The connectable advertisement
  keeps running through a refresh.

**GATT.** The service sits beside the wartui link's, with no encryption or bonding required.

| Role | UUID | Properties |
| --- | --- | --- |
| Service | `47686F73-7445-5350-4272-696467655376` | primary |
| RX, phone to bridge | `0147686F-7374-4553-5042-726964675852` | write, write without response |
| TX, bridge to phone | `0247686F-7374-4553-5042-726964675854` | notify |
| CTRL, phone to bridge | `0347686F-7374-4553-5042-72694C525443` | write, write without response |

RX and CTRL are read alike. There is no OTA characteristic. The C6 serves an ATT MTU of 182.

**Linking.** Subscribing to TX links the phone and starts the scan, in Wi-Fi. A subscription lasts
for its connection only: a phone subscribes again each time it connects. A central reads one stream:
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

A command is cut to 240 bytes and trimmed. Its first word picks it, and the spaces after the
word are skipped.

| Command | Effect |
| --- | --- |
| `wdstream start -wifi [-ch 1,6,11,36]` | sets the channel list when given, then switches to Wi-Fi |
| `wdstream start -bt` | switches to Bluetooth |
| `wdstream start` | starts the scan in the session's mode, or answers `WD:MODE` when it runs already |
| `wdstream stop`, `stop` | stops the scan: a last `WD:STATUS`, then `WD:END`. Nothing when it is stopped |
| `start` | starts the scan in the session's mode, with the list as it is. Nothing when it runs already |
| `bt on`, `bt off` | switches to Bluetooth or to Wi-Fi |
| `bt`, or `bt` with anything but `on` or `off` | `mode Wi-Fi` or `mode Bluetooth` |
| `caps` | `WD:CAPS` |
| `ch` | `supported …`, every channel a node can tune, then `scanning …`, what the fleet scans |
| `ch 1,6,11` | sets the channel list |
| `ch all` | sets the channel list to every channel a node can tune, and answers `scanning all 41 channels` |
| `status`, `info`, `help` | plain text lines, in wartui's own wording; `status` starts with the mode, and `info` ends with `WD:BOOT` |
| `link` | plain text lines about the Bluetooth link: the phone's address and time linked, its `iam` name, signal, MTU, interval and timeout, and the links and last drop since the C6 booted |
| `drop` | `disconnecting the phone`, then the bridge disconnects the phone, which stops the scan |
| `iam a <nickname>`, `iam i <nickname>` | kept until the phone links again or lets go, and shown by `link`: Android or iPhone, and the nickname cut to 18 bytes on a character boundary. Answered with nothing |
| `bye` | answered with nothing; the advert extras then say the link was released when the phone lets go |
| `hb`, `client`, `time …`, and any other `iam` | taken, and answered with nothing |
| `cap`, `csi`, `coproc`, `led`, `screen`, `sd`, `ota`, and `wdstream` with `-cap` or `-promisc` | `WD:ERR reason=unsupported`, and nothing changes |
| anything else | `unknown command '<word>'`, then the help list |

`wdstream` reads its arguments as the WardriveESP firmware does: each counts anywhere after the
word, and `start` is optional. They are read in this order:

1. `stop` stops the scan, and nothing else is read.
2. `-cap` or `-promisc` is refused as unsupported, and nothing changes.
3. `-ch ` sets the channel list, or answers `WD:ERR reason=no_supported_channels` when it names no
   channel a node can tune.
4. `-bt` switches to Bluetooth.
5. Else `-wifi` switches to Wi-Fi.
6. Else the scan starts in the session's mode.

A channel list is the text after `-ch ` or `ch `: numbers between commas or spaces, ending at the
first other byte.

Switching to a mode, by `bt on`, `bt off`, `-bt` or `-wifi`:

- Bluetooth with fewer than two fleet members is refused with `WD:ERR reason=bt_needs_two_nodes`,
  and nothing changes.
- The mode the session is in answers `WD:MODE` while scanning, and starts the scan while stopped.
- Another mode answers `WD:MODE` with the new mode while scanning, ends the scan as `stop` does,
  and starts the new mode's. A stopped session starts in the new mode.

The mode resets to Wi-Fi when the phone links or lets go.

`reboot` is unknown on purpose: the service takes commands from any phone in range, so none of
them restarts the bridge. `drop` and `link` touch only the phone that sends them.

## Lines

| Line | Sent |
| --- | --- |
| `WD:CAPS fw=1.7.3 chip=esp32c5 bands=2g,5g ota=0 bt=1 gps=0 cap=0 usb=0 board=wartui board_name=wartui link=ble chans=…` | at a Wi-Fi scan's start and on `caps` |
| `WD:BOOT fw=1.7.3 slot=<ota_0\|ota_1\|?> image=<trial\|confirmed> reset=<key>` | at a Wi-Fi scan's start, and at the end of `info` |
| `WD:LINK conns=<n> mtu=<n> prev_disc=<0x2NN\|none> [prev_disc_ago=<n>s]` | at a Wi-Fi scan's start |
| `WD:MODE mode=<wifi\|bt>` | at scan start, and to a switch while scanning |
| `WD:BEGIN type=<wifi\|bt> interval=0 channel=auto` | at scan start |
| `WD:PLAN chans=…` | at a Wi-Fi scan's start, and when the channel list changes while scanning |
| `WD:AP ts=<ms> bssid=AA:BB:CC:DD:EE:FF ssid_hex=<hex> rssi=<dBm> ch=<n> auth=<token> hidden=<0\|1>` | per sighting |
| `WD:BT ts=<ms> mac=AA:BB:CC:DD:EE:FF type=<public\|random> rssi=<dBm> name_hex=<hex> mfg=<id> conn=<0\|1>` | per Bluetooth sighting, in Bluetooth mode |
| `WD:STATUS aps=<n> scans=<n>` | in Wi-Fi mode, every 5 s while scanning, and before `WD:END` |
| `WD:STATUS bt=<n> scans=<n> dropped=<n>` | in Bluetooth mode, every 5 s while scanning, and before `WD:END` |
| `WD:END reason=stop` | when the scan stops |
| `WD:ERR reason=<unsupported\|no_supported_channels\|bt_needs_two_nodes>` | a command refused |
| `WD:HEALTH up=<s> heap=<bytes> heap_min=<bytes> mtu=<n> txdrop=<n>` | every 15 s while the phone is linked, scanning or not |
| `WD:CONN itvl_ms=<n> latency=<n> timeout_ms=<n> status=0` | when the link's timing changes while the phone is linked, scanning or not |

A Wi-Fi scan starts with `WD:CAPS`, `WD:BOOT`, `WD:LINK`, `WD:MODE`, `WD:BEGIN` and `WD:PLAN`, in
that order; a Bluetooth scan with `WD:MODE` and `WD:BEGIN`. `WD:HEALTH` and `WD:CONN` are
queued as a command's answer is, so a backlog of stream lines never holds them.

- `fw=` names the highest level the protocol document describes, not the bridge's firmware
  version, which `info` gives. The bridge implements that level's Bluetooth side, or says in
  `WD:CAPS` that it lacks a capability. `board=wartui` and `ota=0` tell an app the bridge is not a board it updates.
- `chans=` in `WD:CAPS` names every channel a node can tune, whatever the pool. `WD:PLAN` names
  what the fleet scans: the phone's channel list, or the pool with no list.
- `ts` is milliseconds since the bridge booted, when the line was written.
- `auth` is `OPEN`, `WEP`, `WPA`, `WPA2`, `WPA3`, `WPA2_WPA3` or `UNKNOWN`. WPA2 Enterprise and
  mixed WPA/WPA2 read as `WPA2`; WAPI, OWE and anything unnamed read as `UNKNOWN`.
- `hidden=1` goes with an empty `ssid_hex=`.
- `WD:BT`: `mac` is the advertiser's address, most significant byte first, and `type` whether it
  is random. `name_hex` is the advertised name as lower-case hex, empty when the advertisement
  carried none. `mfg` is the company identifier from its manufacturer data, as 4 upper-case hex
  digits, or empty. `conn=1` when the advertisement is connectable.
- `WD:STATUS` in Bluetooth mode: `bt` is the `WD:BT` lines since the last one, `scans` counts the
  periods, and `dropped` the lines dropped from the phone's queue since the last one.
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
- `WD:CONN`: `itvl_ms` is the connection interval in whole milliseconds, rounded down,
  `latency` the peripheral latency in connection events, and `timeout_ms` the supervision
  timeout. `status=0`, since the C6 reports only a change that took. None is sent at linking:
  the timing in force when the phone subscribes is the baseline.

## How the fleet maps onto one board

- **The fleet scans the phone's channel list**, split across its nodes. With no list it scans the
  pool, as it does for a wartui host. A list outranks the pool and may name any channel a node
  can tune: 2.4 GHz 1-13 and all of 5 GHz. A list naming none is refused with
  `WD:ERR reason=no_supported_channels`, and the list stays as it was. A list changed while
  scanning re-cuts the fleet at once, with no stop. The list resets when the phone links or lets
  go.
- **Fewer channels than nodes means nodes double up**, on 1, 6 and 11 first. The planner keeps 1,
  6 and 11 on different nodes (`crates/wartui/README.md` § "Channel pools").
- **The phone is sent everything the nodes hear.** A node reports the channel a beacon names,
  which can be beside its share, so a phone that asked for 1 also hears what its node picks up
  from 2 and 3.
- **Starting a scan stops the fleet first**, so every node starts with nothing reported and the
  phone hears each device afresh.
- **In Bluetooth mode one node scans Bluetooth**, the planner's pick, and the others keep
  scanning Wi-Fi, which the phone is not sent. A fleet of one refuses the mode, since the planner
  gives Bluetooth to no node below two members. The channel list sets what those Wi-Fi nodes
  scan.
- **The Bluetooth scan is passive**, so a `WD:BT` carries a name only when the advertisement
  itself carried one.
- **A node reports a device once**, then again after 5 minutes or when it is 5 dB stronger
  (`wartui_proto::dedup`). A WardriveESP board reports every access point on every sweep, and
  every Bluetooth device once per 10 s window, so a phone sees fewer lines for the same
  neighborhood from the bridge.
- **There is no sweep or window to count.** `WD:STATUS` goes every 5 s from the scan's start, in
  either mode: `aps` or `bt` is the lines since the last one, and `scans` counts those periods.
- **The phone lets go, the fleet stops.** An unsubscribe, a disconnect, a `drop`, the setting
  turned off or USB taking the bridge each stop the scan.
- **A slow phone loses the oldest lines.** The bridge holds 16 KB of stream lines for the phone
  and evicts the oldest past that. An app stamps an access point with the phone's position when
  its line arrives, so a deep backlog would put access points in the wrong place. `status` gives
  the count dropped.

## What is not implemented

Everything the document marks as a later version or another transport: capture, GPS, firmware updates, the USB protocol, `WD:CRASH`, CSI, the
LED ring, the screen, microSD runs and the co-processor.

What the bench measured is in [`wardrive-esp-findings.md`](wardrive-esp-findings.md).
