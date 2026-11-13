# C6 Bluetooth pairing and bonding — Just Works on TrouBLE

Measured on 2026-11-12 and 2026-11-13 on D0:1C, the carrier-10 P4 with its ten C5 nodes idle on
the bus. Issue #309 asked whether TrouBLE on the C6 can pair and bond with the PC and the two
baseline devices, and what that costs against the unencrypted link of
[`c6-ble-host-findings.md`](c6-ble-host-findings.md).

The code is on the branch `spike/c6-ble-pairing`, from `spike/c6-ble-host` at `7670856`, and is
never merged: `64d0c2d` to `a8203d5`. Every build had the 1 MB PSRAM host ring of run 15.

## What the spike built

- **Just Works, LE Secure Connections, bonding.** The bridge has no display and no input, and a
  phone must pair from its own prompt with no cable. trouble-host 0.8.0's `security` feature, with
  `legacy-pairing` off.
- **A pairing window.** It is open while no bond is stored, and for 120 s after the P4 boots. A
  host command reopens it for 120 s, from USB or from a bonded central. TrouBLE accepts every
  pairing, so the C6 enforces the window:
  - a connection made inside the window is bondable; one made outside it is not;
  - a central that pairs without a bond, or encrypts without a stored one, outside the window is
    disconnected;
  - a central not bonded and encrypted 30 s after connecting inside the window, or 10 s outside
    it, is disconnected, so none holds the only connection.
- **The gate.** Up's CCCD and Down's write need an encrypted link, which TrouBLE checks before
  a write reaches the C6's handler. The C6 also forwards a Down write, and counts a subscription,
  only for a central that holds a stored bond, since Just Works gives an unbonded link the same
  encryption level.
- **Bonds live on the P4,** in the second sector of its `nvs` partition: append-only add and
  remove records with a CRC, four bonds at most, the oldest evicted. The P4 sends them to the C6
  after every hello, so a C6 restart, a P4 restart and a C6 update all reload them. The C6 keeps
  no flash state.
- **Spike host commands:** open the window, clear the bonds, and reset the C6 through its enable
  pin.

| | |
| --- | --- |
| Random source | TrouBLE seeds its RNG from the controller's `LeRand`; nothing to supply |
| Controller commands | `LeRand`, the LTK replies and the resolving list all answered OK at start; the resolving list holds 4 |
| Patches | none to esp-radio, esp-hal, bt-hci or trouble-host |
| New `unsafe` | none |

## Hosts

| Host | OS | Paired | Time to pair | Time to resume |
| --- | --- | --- | --- | --- |
| Bench PC, BlueZ 5.88 on the USB adapter (`0489:e10d`), `tools/ble-standin` | Fedora 44, kernel 7.3.3 | by hand, once per bond: `bluetoothctl`, agent `NoInputNoOutput`, `pair` | 254–439 ms, four pairings | 225–420 ms at the interval BlueZ picks; 205 ms on the build that requests 15 ms |
| iPad mini (A17 Pro), nRF Connect | iPadOS 27.2.1 | its own prompt, first tap | 2.4 s | 403–407 ms (30 ms interval) |
| Pixel 8, nRF Connect | Android 17 | its own prompt | 6.3 s | 637–685 ms (48.75 ms interval) |

- Times run from the connection to the pairing's end, or to encryption on a stored bond.
- A phone's pairing time is mostly a person reading the prompt and tapping it.
- Only Just Works was run. Out-of-band data and a passkey were not tried on any host.
- **BlueZ never started a first pairing on its own.** Neither the Insufficient Authentication
  answer nor the C6's bonding Security Request started one, so the stand-in's subscribe timed out.
  The PC pairs once by hand inside the window, then resumes on its own. A Security Request without
  the bonding flag did make an already bonded BlueZ pair again, which is the first fault below.

## Against the pass criteria

| Criterion | PC | iPad mini | Pixel 8 |
| --- | --- | --- | --- |
| Resumes after a disconnect | 3 of 3 | yes | yes |
| Resumes after a reconnect within 2 s (esp-rs/esp-hal#5877) | 4 of 4, gaps ~0.1–1.2 s (inferred from timing) | yes | not testable (below) |
| Resumes after a C6 restart | yes | yes | yes |
| Resumes after a P4 restart | yes | yes, power cycle | yes, power cycle |
| Resumes after its own Bluetooth off and on | yes | yes | yes |
| At least 16 KB/s | 112.7 KB/s | 38.2 KB/s | 67.9 KB/s |
| An unbonded central stays out | yes | — | yes |

- **No resume paired again,** and no link dropped with a MIC failure (0x3D). Every disconnect
  on the encrypted build was 0x13, 0x15 or 0x16. The C6 shows none of esp-rs/esp-hal#5877's S3 failure.
- **The unbonded Pixel, with the window closed,** tried to subscribe to Up and write to Down. The
  C6 disconnected it, its write was refused before the handler, it never took the host queue, and
  no bond was stored. The bonded PC connected straight after and resumed in 225 ms, with no record
  lost.
- **The Pixel cannot reconnect within 2 s from nRF Connect.** Android keeps the link up for a few
  seconds after the app disconnects, so a fast reconnect reuses the encrypted link, and the C6's
  connection count did not move. The PC's four fast reconnects and the iPad's one cover the case.
- **A bond survives everything tested:** a C6 restart, a P4 reset, a bridge power cycle, and a
  flash of another image. A fifth bond's eviction ran only in the bond store's host tests.

## Cost against the unencrypted link

**The stand-in on the PC,** at 2M, MTU 247 and a 15 ms interval the C6 requests. The encrypted
and the unencrypted build (`7670856`) were interleaved, four encrypted arms and two unencrypted.
Each arm ran a 500-batch burst, then `saturate`. The first encrypted arm's burst is left out: its
board already ran the image, so the ring's high-water mark held an earlier `saturate`'s.
The adapter had no other LE link.

| | Encrypted | Unencrypted |
| --- | --- | --- |
| `saturate`, bytes credited over Bluetooth-owned time | 113.5, 112.4, 113.2, 111.8 KB/s | 113.5, 111.7 KB/s |
| 500-batch burst: peak backlog | 424, 418, 419 KB | 426, 423 KB |
| 500-batch burst: back to steady | ≤6 s | ≤4 s |
| Records lost | 0 | 0 |

- **Encryption costs no throughput the stand-in can measure.** The two builds' means differ by
  0.1 KB/s.
- The stand-in's own receive rate reads about 4% lower encrypted. It waits 1.0 to 2.0 s for the
  link to be secured before it starts the load, inside its 40 s window, and the bytes it missed
  match that wait.
- Stats arrive every 2 s, so the burst's 4 s and 6 s are one sample apart.
- One `saturate` arm evicted one record, the generator's own: it holds the ring within two frames
  of full, and a real frame evicts (run 10). The stand-in matched it.

**The phones,** each in nRF Connect, `saturate` at MTU 247 and 2M. Each rate is the bytes
notified between two reads in mid-run, over the time the device held the queue.

| Device | Interval | Encrypted | Unencrypted (run 14, 96:C4) |
| --- | --- | --- | --- |
| iPad mini | 30 ms | 38.2 KB/s | 39.1 KB/s |
| Pixel 8 | 48.75 ms | 67.9 KB/s | 66.5 KB/s |

Run 14 was on 96:C4 and its internal ring, so the phones' figures compare across boards.

**The C6:**

| | Encrypted | Unencrypted (`7670856`) |
| --- | --- | --- |
| Static RAM | 137,968 B | 135,488 B |
| Image | 637,360 B of 1,966,080 B | 523,792 B |
| Heap high-water | 37,712–37,976 B, arms that paired included | 37,712–37,820 B |

`security` adds 2,480 B of static RAM and 113,568 B of image, P-256 and AES-CMAC. The heap
high-water marks show no cost: the unencrypted spike reached 37,976 B too
([`c6-ble-host-findings.md`](c6-ble-host-findings.md)). That doc's 134,796 B and 462,992 B are an
earlier spike build; `7670856` is the build this branch starts from.

## Two faults the bench found, both fixed on the branch

**A Security Request that says no bonding makes BlueZ pair again.** The C6 sent one on every
connection. Outside the window its AuthReq carried no bonding flag, and BlueZ 5.88 answered by
pairing again instead of encrypting with the key it held: one LTK request per connection, the
pairing's own. The C6 then disconnected it as unbonded. The C6 now sends no Security Request to a
central it holds a bond for (`1569d20`). That central encrypts on its own, or on the Insufficient
Authentication answer.

**A 10 s deadline is shorter than a person.** The Pixel's first pairing needed two taps. The C6
had disconnected it 10 s after it connected, the window open, before the first tap landed. That is
the only path that disconnects a central inside the window, and the pairing that worked took 6.3 s.
The deadline is now 30 s inside the window, the pairing protocol's own timeout, and 10 s outside
it (`a8203d5`). The iPad then paired on its first tap.

## What a host has to know

- **Clearing the bridge's bonds strands every host.** With its bond cleared on the bridge, the PC
  failed every connect, and BlueZ never paired again on its own. Forgetting the bridge on the host
  and pairing again recovered it. A production build that clears bonds must tell the operator to
  forget the bridge on each host.
- **A host bonded to the bridge cannot connect to an unencrypted build.** BlueZ timed out until
  the bond was removed. That matters only when a spike build goes back on.
- **A Down write counts only while the central holds the host queue.** The P4 drops Bluetooth
  bytes until a central subscribes to Up, and a USB frame takes the queue back. A reconnected
  Pixel's command, written before it re-enabled notifications, was forwarded by the C6 and then
  dropped.
- **The iPad's first reset command did nothing,** and a second, with a leading `00`, worked. Either
  nRF Connect on iOS dropped the frame's trailing `00`, as it once did in the earlier spike, or the
  iPad did not hold the queue. Which one was not found.

## What this settles

- TrouBLE pairs, bonds and resumes on the C6 with the PC, an iPad and an Android phone, through
  every restart the issue named, with no patch and no new `unsafe`.
- Just Works with a pairing window lets a phone pair from its own prompt with no cable. The PC
  pairs once with `bluetoothctl`.
- Bonds can live on the P4, and the C6 can stay stateless.
- Encryption costs no throughput the stand-in can measure, on the same board with the builds
  interleaved. The phones' encrypted rates sit within 2.3% of run 14's unencrypted ones, which
  ran on another board. It costs 2,480 B of the C6's static RAM and 113,568 B of its image.
- Unbonded centrals stay out: they cannot write, subscribe or take the queue, and a bonded host
  connects straight after.

It does not settle:

- **Whether a phone accepts out-of-band data or a passkey.** Only Just Works was run.
- **Why BlueZ does not start a first pairing on its own,** when Android and iOS do on the same
  answers.
- **The iPad's first reset,** above.
- **A fifth bond on hardware.** Eviction ran only in the bond store's tests.
