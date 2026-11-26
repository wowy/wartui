# wartui-ffi

The library an Android or iOS app loads: `wartui-app`'s captures, settings and Bluetooth link,
through [UniFFI](https://mozilla.github.io/uniffi-rs/)-generated Kotlin and Swift. The app's own
types cross unchanged, so a phone shows a capture by the same rules as the terminal
(`crates/wartui-app/src/view/mod.rs`).

## The object model

| The phone implements | The phone calls |
|---|---|
| `RadioDriver` | `Radio`: `heard`, `completed`, `notified`, `dropped` |
| `SettingsStore` | `Settings`: `open`, `unsaved`, `place` |
| `LogSink` | `logTo` |
| | `Link`: `simulated`, `bluetooth`, `isSimulated`, `close` (`disconnect` in Kotlin) |
| | `LocationFeed`: `push`, `lost`, `wanted`, `nextWanted`, `state` |
| | `HealthFeed`: `set` |
| | `Session`: `start`, `view`, `nextView`, `showSky`, `startWithoutFix`, `stop` |

`linkUuids()` gives the link service's UUIDs. Kotlin calls `Link.close` `disconnect`, since every
Kotlin object's `close()` frees its handle (`uniffi.toml`).

A capture: open `Settings` over the app's store, make a `Radio` over the app's driver, open
`Link.bluetooth`, `Session.start` over it (a `suspend` function in Kotlin, `async` in Swift),
follow `nextView`, and `stop`. The link stays open for the next capture, read meanwhile so the
bridge's stream never backs up into it. `smoke/kotlin/Smoke.kt` and `smoke/swift/main.swift` do
exactly that. Under `Link.simulated` the view's `link.host` shows the simulator as a bridge on
USB, as the terminal does: it describes no real link.

## What a phone's adapter keeps to

- **A request returns at once and is answered once.** A `RadioDriver` method that takes a
  `request` id starts the work and returns; its result goes to `Radio.completed` with that id,
  later, from any thread. Throwing fails that request. `src/radio.rs` has the rest of the
  contract, which is `wartui_bridge::pipe`'s (`crates/wartui-bridge/src/pipe.rs`).
- **Report for the current link only.** `Radio.notified` and `Radio.dropped` name a peer, not a
  link, so ignore the callbacks of a link closed or replaced: on Android an old `BluetoothGatt`'s,
  on iOS those for a peripheral since reconnected. Report a link's drop before answering a later
  `connect` to the same peer.
- **Calls arrive on any thread.** Every method the phone implements may be called from any of the
  library's threads; on Kotlin, JNA may use a new one for each call. Keep the methods short and
  thread-safe, and hand work to the stack's own queue.
- **On Android, ask for a larger MTU before completing `connect`.** Android starts a link at 23;
  the bridge serves 182, and the transport cuts its writes to the MTU it reads from `mtu`.
- **A fix is stamped on this device's clock.** `LocationFix.atMs` is when it was taken, in Unix
  milliseconds; where the platform stamps satellite time, give the wall clock less the fix's age.
  Mock fixes, and fixes without an accuracy or worse than `maxFixAccuracyM()`, are refused
  (`crates/wartui-core/src/feed.rs`).
- **Errors are typed.** Every error and state is an enum for the app to word; a `detail` string
  is the failure's own account from below, for a log.

`src/lib.rs` has the threading rules, and each module's `//!` the rules it keeps.

## Generating the bindings

Build the library, then run the crate's own `uniffi-bindgen` on it in library mode:

```sh
cargo build -p wartui-ffi --features cli
target/debug/uniffi-bindgen generate --library target/debug/libwartui_ffi.dylib \
  --language kotlin --out-dir out/kotlin       # or --language swift; .so on Linux
```

Kotlin lands in package `racing.deadbear.wartui.core` and needs JNA and kotlinx-coroutines; Swift
is module `WartuiCore` over the C module `WartuiCoreFFI` (`uniffi.toml`).

## Smoke tests

```sh
crates/wartui-ffi/smoke/run.sh [kotlin|swift|all]    # default all; prints OK <language>
```

It builds the library with the `testing` feature, which adds `WireSim`, a simulated bridge at the
byte level, generates both bindings under `target/ffi-smoke/`, and runs a program in each
language: a simulated capture placed by a pushed fix, then a capture over a loopback
`RadioDriver` to a `WireSim` that remembers its bridge through an in-memory `SettingsStore`. It
needs `kotlinc` and `java`, or `swiftc`, on `PATH`, and fetches JNA and kotlinx-coroutines from
Maven Central against pinned digests. CI runs it on macOS (`.github/workflows/bindings.yml`).

Swift written against the binding needs Swift 6.4 or later, and the smoke program compiles in the
Swift 6 language mode. The generated file itself keeps UniFFI's checks for older compilers.

The Rust tests (`cargo test -p wartui-ffi`) drive the same paths through the crate's API.
