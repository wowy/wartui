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
later, so the bridge advertises for about 0.1 s in every 30 s. Its 48.75 ms interval is the
Pixel's; whether it is the phone was not settled. A `bluetoothctl scan on` beside the pairing made
no difference: one try paired with it running and three failed.

With the setting on and the phone linked, the PC is not served at all, which is the rule: the
phone is the host. Turning the board off in the app, or the phone's Bluetooth, frees the bridge.

## A pairing request on the phone

Up to `e6907dc` the C6 sent a Security Request to an unbonded central whenever phones were not
admitted. A phone that remembers the bridge connects then too, and Android answers the request
with a pairing prompt: the Pixel 8 showed one each time `wartui status` ran over USB, which holds
phones out for 15 s. From `9926c16` the C6 sends no Security Request. On that build the PC's
stored bond resumed and a fresh pairing bonded (the table above), and through 14
`unbonded_kicks` of the phone the bridge counted no pairing begun or failed. Whether the prompt
is gone was not seen from the bench: it shows only on the phone.

## Not measured

- The advertisement's raw bytes: `btmon` needs root on the bench PC. BlueZ listed the WardriveESP
  service UUID and the name while the setting was on.
- iOS.
- `wartui ports` beside a linked phone, by the bridge's own counters. It was judged in the app,
  which showed no break.
- A command dropped for a full queue toward the P4, which goes unacknowledged. The bench never
  filled that queue.
- A drive: the app's access point counts beside a wartui capture, a slow link, and the backlog's
  evictions.
- A first pairing from a host other than Linux while the setting is on.
