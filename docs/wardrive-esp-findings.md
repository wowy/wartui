# The WardriveESP service — bench on D0:1C

Measured on 2026-11-17 on D0:1C, the carrier-10 P4 with its ten C5 nodes, from the bench PC
(Fedora 44, BlueZ 5.88) and an Android phone running the Wardrive Go app. The two checks made in
the app against the production firmware are from 2026-11-18. The service is described in
[`wardrive-esp.md`](wardrive-esp.md).

Two builds ran. The first was a C6-only bench build at `c5462c6`, which admitted a phone and
answered it from the C6 with no access points, to try the service against the app before the P4
served it. The second was the production firmware at `e865fcf`, with the setting on and the
fleet's pool at US.

The PC stood in for a phone through BlueZ: a script that subscribes to TX, writes framed and
unframed commands to RX, and prints every frame. The PC held a bond with the bridge throughout,
so its link was encrypted, which the service does not need. The phone linked unencrypted.

On 2026-11-18 the bridge and its radio were updated to `e6907dc`, which changes how a scan's
period and counts start, how a channel list is read, and when a CMD is acknowledged. The stand-in
ran once more against it; those rows name the build.

## Results

| Check | Result |
| --- | --- |
| The app finds the bridge under a `wartui` name | Yes. It lists `wartui D0:1C`, links by itself and stays linked. No pairing prompt, no complaint |
| The app's link | Unencrypted, unbonded, MTU 182, 2M PHY, 30 ms interval |
| The app relinks after a bridge restart | Yes, by itself |
| What the app sends on linking | Framed CMDs: `wdstream start -wifi`, then `wdstream start -wifi -ch 1,2,…,11,36,40,…`. No `caps`, `iam` or `hb` in the 75 s watched |
| Setting off: the wartui link | Works over USB and over Bluetooth with the bond the PC held |
| Setting on: a bonded wartui host over Bluetooth | Works. It gets no Security Request and encrypts with its key |
| Setting on: a fresh `wartui bluetooth pair` | Paired and bonded in 3 of 4 tries. See below |
| Header on subscribing | `WD:CAPS`, `WD:MODE`, `WD:BEGIN`, `WD:PLAN`, in order. `WD:CAPS` spans two DATA frames, 167 and 66 bytes |
| A framed CMD | Its ACK arrives ahead of its reply, and the reply's frames carry its id |
| `-ch 1,6,11,36,200` | `WD:PLAN chans=1,6,11,36`. Before it, access points came on eleven channels; after it, only on 1, 6, 11 and 36 |
| `reboot` | `unknown command 'reboot'` and the help list. The bridge does not restart |
| `stop`, then `start` | `WD:STATUS` and `WD:END`, then the header again |
| The fleet's sightings | About 170 `WD:AP` lines in 20 s from ten nodes on the bench, none dropped |
| The app against the production firmware | It shows access points arriving |
| `wartui ports` on the PC beside the linked app | The app keeps its link and its access points keep arriving, as seen in the app |
| `WD:STATUS` | Every 5 s: 73, then 83 after a restart of the scan, then 10 as the nodes ran out of new access points |
| `e6907dc`: the first `WD:STATUS` of a scan | 4.98 s after `WD:BEGIN`, on subscribing and again after `stop` and `start` |
| `e6907dc`: `stop`, then `start` | `status` answers `aps 0 scans 0`, and the next `WD:STATUS` says `scans=1` |
| `e6907dc`: `ch 1, 6, 11` | `WD:PLAN chans=1,6,11` and `scanning 1,6,11` |
| `e6907dc`: a framed CMD | Its ACK still arrives ahead of its reply |
| A central reading TX subscribes to the wartui stream too | Refused, ATT error `0xFC` |
| A USB host takes the bridge | The phone is dropped, and a TX subscription is refused, ATT error `0xFC`, while USB holds the bridge |
| The setting turned off with the app still running | The phone keeps connecting and is dropped each time: `unbonded_kicks` rose from 1 to 4 in 75 s. See below |

## A pairing that did not connect

With the setting on, the C6 sends no Security Request, so a first pairing relies on the host
starting it. `wartui bluetooth pair` does, through BlueZ. Of four fresh pairings on the bench
build, three paired and bonded on the first try. The first of the four, straight after a
`bluetooth forget`, ended in BlueZ's `ConnectionAttemptFailed: Page Timeout` before any pairing
began. The cause was not found, and it did not repeat.

## A phone that remembers the bridge

The app connects to the bridge by its address whenever it is in range, whatever the bridge
advertises. With the setting off, or while USB holds the bridge, the C6 refuses its subscription
and drops it as it drops any central that holds no bond: 10 s after it connects with the pairing
window closed, 30 s with it open. The app then connects again. In 75 s with the setting off,
`unbonded_kicks` rose from 1 to 4, and two of four `status` reads showed a central connected,
`not encrypted or bonded`.

The bridge takes one connection, so a wartui host over Bluetooth can find it taken meanwhile.
Tried on 2026-11-18 at `9926c16`, with the app running on the phone and the setting off:

| Check | Result |
| --- | --- |
| The PC's stored bond, pairing window closed | `status --link bluetooth` was served on the first try, encrypted and bonded |
| A fresh `wartui bluetooth pair` after `forget`, window open | Paired and bonded in 2 of 12 tries. The other ten ended in `nothing advertised as wartui D0:1C over Bluetooth in 10s` |

Eight of those tries, one of the two that paired among them, ran after the app was closed on the
phone. The bridge's log shows why they fail. An unbonded central connects within about 0.1 s of
each disconnect, under a new random address each time, and does nothing until the C6 drops it 30 s
later, so the bridge advertises for about 0.1 s in every 30 s. A `bluetoothctl scan on` beside the pairing made no difference: one try paired with it running
and three failed.

The central is the phone, app closed or not. With the phone's Bluetooth turned off, the bridge
logged no connection in 45 s, `unbonded_kicks` stayed at 31, and a fresh pairing after `forget`
paired and bonded in 3 of 3 tries.

With the setting on and the phone linked, the PC is not served at all, which is the rule: the
phone is the host. Turning the board off in the app, or the phone's Bluetooth, frees the bridge.

## A pairing request on the phone

Up to `e6907dc` the C6 sent a Security Request to an unbonded central whenever phones were not
admitted. A phone that remembers the bridge connects then too, and Android answers the request
with a pairing prompt: the Pixel 8 showed one each time `wartui status` ran over USB, which holds
phones out for 15 s. From `9926c16` the C6 sends no Security Request. On that build the PC's
stored bond resumed and a fresh pairing bonded (the table above), and through 14
`unbonded_kicks` of the phone the bridge counted no pairing begun or failed. The phone
showed no prompt on that build.

## Health and link lines (2026-11-18)

D0:1C, both chips on the build that adds `WD:BOOT`, `WD:LINK`, `WD:HEALTH`, `link`, `drop` and
`iam`, with the PC stand-in as the phone.

| Check | Result |
| --- | --- |
| Scan header | `WD:CAPS`, `WD:BOOT fw=1.6.0 slot=ota_0 image=confirmed reset=software`, `WD:LINK conns=1 mtu=182 prev_disc=none`, `WD:MODE`, `WD:BEGIN`, `WD:PLAN`, in that order |
| `WD:HEALTH` | arrived inside 15 s of linking: `up=30 heap=67524 heap_min=63452 mtu=182 txdrop=0` |
| `link`, 2 s after linking | `connected to … for 0h 00m 02s`, `signal -51 dBm, MTU 182, interval 30 ms, timeout 420 ms`, `links since boot 1, no drops` |
| `iam a <name>`, then `link` | `held by <name> (Android)`, a two-byte character intact |
| `info` | ends with the `WD:BOOT` line |
| `drop` | `disconnecting the phone` arrived, and the link ended 0.5 s later |
| Link again after `drop`, twice | `WD:LINK conns=2 … prev_disc=0x216 prev_disc_ago=5s`, then `conns=3`; `link` named the drop as `0x16` |
| `wartui status --link bluetooth` afterwards | answered, encrypted and bonded |

**A bonded central's subscription outlives its connection.** The stand-in is bonded, for the
wartui link, and TrouBLE keeps a bonded central's subscriptions across connections. On a build
that read TX's as it stood, `drop` left it set: the next connection got the phone's stream before
it asked for anything, its own requests then went unanswered, and BlueZ failed its subscribe 30 s
later, on 4 connections of 4. The C6 counts a TX subscription only once the central writes it on
the connection it holds, and the same sequence linked on both tries. A phone holds no bond, so the
app never met this. Why the central's requests went unanswered was not traced.

## Status beacon and advert extras (2026-11-18)

D0:1C on the build that adds them, pool US, read on the bench PC through BlueZ, one short scan
per sample. BlueZ gives the manufacturer data and the service data; the bytes below follow the
company id and the `0x5744` UUID.

| State | Status | Extras, frame A / frame B |
| --- | --- | --- |
| free, no phone since boot | `57 00 80 00 00 16 00 d0 1c` | `00 00 ff ff c7 ff` / `01 ff 7f 00 00 00` |
| stand-in linked, `iam i Bench`, `ch 1,6,11,36`, scanning | `57 83 80 54 00 16 00 d0 1c` and `Bench` | `00 00 ff 21 44 00` / `01 00 00 00 00 00` |
| free, after `bye` and a disconnect | `57 00 80 00 00 16 00 d0 1c` | `02 00 ff ff c7 ff` / `03 ff 7f 00 00 00` |
| free, after `drop` | the same | `04 00 ff ff c7 ff` / `05 ff 7f 00 00 00` |

- The frames swapped every 5 s, with the connectable advertisement running through each swap.
- The count, `54 00`, was the `WD:STATUS aps=84` of the period before it, and moved to `08 00`
  with the next.
- The C6's controller took the beacon beside a connection: the PC read it while it was itself the
  linked central.
- `wartui status` answered over Bluetooth and over USB afterwards, on the stored bond.

## Bluetooth mode (2026-11-18)

D0:1C with its ten nodes on 0.6.0, updated over the bus by `wartui ota`, all confirmed. N10 held
the Bluetooth scan.

**The node's record.** `wartui sniff` beside `wartui scan start --bluetooth`, 6.5 minutes: 172
Bluetooth records, none undecodable, 12 with a name. The bridge's own advertisement was among
them as `wartui D0:1C` at -19 dBm, read by a node on the same board. Complete names and shortened
ones both arrived. Most advertisers in the room sent none in the advertisement itself, and a
passive scan hears no scan response.

**The phone's mode**, with the PC stand-in:

| Check | Result |
| --- | --- |
| `bt on` while scanning Wi-Fi | `WD:MODE mode=bt`, `WD:STATUS aps=80 scans=0`, `WD:END reason=stop`, `WD:MODE mode=bt`, `WD:BEGIN type=bt interval=0 channel=auto`, in that order |
| `WD:BT` lines | 101 in 15 s, the first 0.6 s after the switch; 9 named; `type=public` and `type=random`, `conn=0` and `conn=1`, `mfg=` empty and filled all seen |
| `WD:STATUS` | `bt=90 scans=1 dropped=0`, then `bt=5 scans=2 dropped=0`, 5 s apart |
| `status`, `bt` | `mode Bluetooth`, `bt 5 scans 2` |
| `wdstream start -bt` in Bluetooth mode | `WD:MODE mode=bt`, nothing else |
| `bt off` | `WD:MODE mode=wifi`, the last Bluetooth `WD:STATUS`, `WD:END`, then the six Wi-Fi header lines; `WD:STATUS aps=102 scans=1` 5 s later |
| No `WD:AP` in Bluetooth mode, no `WD:BT` in Wi-Fi mode | held |
| Status beacon while linked in Bluetooth mode | `57 07 80 59 00 …`: linked, scanning, mode 1, 89 devices |

## Protocol level 1.7.3 (2026-11-18)

D0:1C on the build that names level 1.7.3 and adds `WD:CONN`, with the PC stand-in: `WD:CAPS
fw=1.7.3`, `WD:BOOT fw=1.7.3`, `info`'s `WardriveESP protocol 1.7.3`, and the free board's status
`57 00 80 00 00 17 03 d0 1c`. Linking sent no `WD:CONN`, since the timing at the subscribe is the
baseline, and the stand-in's link kept its 30 ms interval throughout, so none followed.

**The app at 1.7.3** (Android, Pixel 8): it linked, showed the board as v1.7.3 and offered no
firmware update. The bridge's beacon, read on the PC while the phone was linked, was `57 03 80 04
00 17 03 d0 1c` and the six bytes of `Pixel8`: linked, scanning, Wi-Fi, an Android app, and a
nickname. So the app sends `iam` at this level; it sent none in the 75 s watched at 1.6.0. The
extras named every channel of the pool.

With the bridge switched off in the app, the beacon read `57 00 …`, a free board, and the extras
`02 00 ff ff c7 ff` / `03 ff 7f 00 00 00`: the last link released. So the app sends `bye` before
it lets go. Over the 12 s read, no central connected.

## A channel list that drives the planner (2026-11-18)

D0:1C, ten nodes, pool `us`, the PC stand-in as the phone, before and after the planner took the
phone's list. Each row is the `WD:AP` lines after `ch 1,6,11`, `stop` and `start`, over 20 s. One
run of each, about an hour apart.

| Build | Lines | BSSIDs | Lines per BSSID |
| --- | --- | --- | --- |
| the list filters, the pool is scanned | 72 | 52 | 36 once, 13 twice, 3 more often |
| the list is dealt | 288 | 78 | 20 once, 9 twice, 49 three to ten times |

- **Half again as many access points in the same 20 s**, from ten nodes parked on three channels
  in place of one node visiting each. The first 5 s period counted 217 lines, against 46.
- **One-channel shares hold.** This is their first run on hardware: lines kept arriving through
  the 20 s, and `ch all` afterward brought 5 GHz lines back within 12 s.
- **Lines name channels beside the list**: 8 and 10 under a list of 1, 6 and 11. One line named
  36 in the second after the list was set, from a node that had not yet taken its new share.
- **A list outside the pool is taken**: `ch 12,13` answered `scanning 12,13` under the US pool.
  The five access points heard in its 4 s all named channel 11, each four to six times: nothing
  near the bench sits on 12 or 13.
- **`WD:CAPS` and `ch` name 41 channels** under the US pool, 12, 13, 169, 173 and 177 among them,
  and `ch all` answered `scanning all 41 channels`.
- **Nothing was dropped**: `txdrop=0` and `dropped 0` through the run.
- **The host's cut**: `wartui scan start` over USB dealt 1, 6 and 11 to N1, N2 and N3, 36
  channels in shares of four and three. After the stand-in unlinked, `wartui status` showed the
  scan stopped and the pool still US.

## Advertising sets and the bridge's id (2026-11-19)

D0:1C, carrier-10, ten nodes, with the bench PC as central through BlueZ. The PC was bonded for
the wartui link, and two other LE devices were connected to its adapter throughout.

**The controller and the encoder.** `LeReadNumberOfSupportedAdvSets` answered 2, with
`multi_adv_instances` at 1. The first `advertise_ext` of one legacy-PDU set failed with Invalid
HCI Command Parameters (0x12). A probe sent each command alone:

| Command | Answer |
| --- | --- |
| `LeSetExtAdvParams` at 10 dBm | accepted; the controller answered 10 dBm |
| `LeSetExtAdvData` | refused with 0x12, with either fragment preference |
| `LeSetExtScanResponseData` | refused with 0x12, with either fragment preference |
| `LeSetExtAdvEnable`, on and off | accepted |

With every command encoded by bt-hci's sync encoder, all five were accepted and the set started.
The cause is bt-hci 0.10.1's async encoder, which writes a byte slice's length plus one. bt-hci
0.11 writes the length itself, and the C6 has built on it since 2026-11-21 with no sync encoder
in the path. On D0:1C that build advertised, a central connected and a Mac's scan read the
bridge's id from the advertising data; the five commands were not counted again
([`c6-ble-host-findings.md`](c6-ble-host-findings.md#upstream-status-checked-2026-11-12)).

**Two sets**, on a throwaway build that counted each call:

| Check | Result |
| --- | --- |
| Starts of two sets | 14, none refused |
| Starts of one set | 11, none refused |
| Stops of every set on a connection | 14, none refused |
| The status beacon started beside a connection it was due on | 9 of 9 |
| In-place refreshes of the WardriveESP set, both sets running | 70, none refused |
| Least free heap | 61,352 bytes; 68.9 KB free at rest |
| Signal at the PC, same bench position | -70 to -52 dBm on the build before; -68 to -55 dBm with the sets at 10 dBm |

**What BlueZ makes of two sets under one address:**

- `UUIDs` accumulated both services' UUIDs, and the 16-bit UUIDs of the service data.
- With `DuplicateData` on, as btleplug scans, each advertisement reported replaced `ServiceData`
  and raised a change. With it off, the entries merged.
- Reports came at a scan's start and again about every 10.5 s, BlueZ's discovery cycle, not per
  advertisement. A service-UUID discovery filter did not change that.

| Where the id was | Scans | Heard |
| --- | --- | --- |
| the wartui pair only | 8 | the wartui set in the first cycle in 4; only the WardriveESP set until the next in 4 |
| both pairs, 12 s scans | 12 | the id within 0.3 s in 10; at 10.4 s and 10.8 s in 2 |
| both pairs, `wardrive esp` off | 3 | the id within 0.2 s in 3 |

**The build as committed** (firmware `fad42cb`, host `b40b53c`):

| Check | Result |
| --- | --- |
| PC stand-in on WardriveESP, phones admitted | linked 3 of 3; 104 to 115 `WD:AP` lines in 8 s; a subscription to the wartui link's Up refused with ATT `0xFC` |
| Bonded PC on the wartui link, phones admitted | `wartui status --link bluetooth --bridge D0:1C` 6 of 6, about 3 s each; with no bridge named 3 of 3, about 5 s |
| `wartui bluetooth pair --fresh`, then a bonded resume | passed |
| `wartui bluetooth forget`, then `pair` | passed; `forget` removed the PC's BlueZ entry by the id |
| USB host while the stand-in was linked | the stand-in was dropped at once; both sets ran again once USB fell silent |
| `wardrive esp` off | only the wartui set on air; the bonded PC linked |
| `flash-bridge --link bluetooth`, twice, no bridge remembered | `installed and confirmed` both times, the link dropping and reconnecting by the id; 125 s each, the build included |
| Faults | no host stop, no failed pairing, no kick, no refused start, stop or refresh |

On the throwaway build, one `pair` straight after `forget` failed with BlueZ's
`ConnectionAttemptFailed: Page Timeout` and passed on the next try, as on 2026-11-17
([A pairing that did not connect](#a-pairing-that-did-not-connect)).

**The app beside two sets.** Wardrive Go on a Pixel 8 found the bridge at once on that build and
received data. On the build with the id in the status beacon (`8060bb7`), a day later, it
connected and listed access points.

**A scan beside the status beacon.** On that build the beacon's scan response carried no id. With
the PC's BlueZ connected to the bridge and the beacon heard, `wartui status --link bluetooth
--bridge D0:1C` failed 3 of 3 with `bridge D0:1C did not advertise over Bluetooth in 12s`: BlueZ
held the beacon's service data in place of the id's. With the empty id in the beacon's scan
response, the same sequence linked 3 of 3, in 2.4 s, 10.4 s and 15.6 s from the command's start:
the two slow ones first heard the beacon's advertisement without its scan response. The Pixel 8's
Bluetooth was on through both runs, with the bridge remembered by the app.

## Not measured

- The advertisement's raw bytes: `btmon` needs root on the bench PC. BlueZ listed the WardriveESP
  service UUID and the name while the setting was on.
- The status UUID in the linked board's advertisement, for the same reason, and its 1 s interval.
- A first pairing beside the beacon's advertisement. The stored bond resumed.
- The beacon read by a phone app: a second phone beside a linked one.
- `WD:CONN` on a link whose timing changes: the stand-in's never did.
- The app in Bluetooth mode: whether it sends `bt on` or `wdstream start -bt`, and what it shows.
- `bt on` on a fleet of one node, refused in the session's tests only.
- iOS.
- `wartui ports` beside a linked phone, by the bridge's own counters. It was judged in the app,
  which showed no break.
- A command dropped for a full queue toward the P4, which goes unacknowledged. The bench never
  filled that queue.
- A drive: the app's access point counts beside a wartui capture, a slow link, and the backlog's
  evictions.
- A first pairing from a host other than Linux while the setting is on.
- The app beside `WD:BOOT`, `WD:LINK` and `WD:HEALTH`, and whether it sends `iam` or `bye`: it sent
  neither in the 75 s watched on 2026-11-17.
- The app beside the wider `chans=`: whether its channel picker offers 12, 13 and 169 to 177, and
  what list it sends.
- A list shorter than the fleet in a drive, where the doubled nodes' extra lines meet a slow link.
- The Wardrive Go app beside two advertising sets on iOS.
- A bonded central on a resolvable address resuming beside two sets.
- A scan for the id from a PC whose adapter holds no other connection.
- The two sets' raw advertisement bytes.
