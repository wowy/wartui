# wartui-ffi

The library an Android or iOS app loads: `wartui-app`'s captures, settings, Bluetooth link,
uploads and firmware updates, through
[UniFFI](https://mozilla.github.io/uniffi-rs/)-generated Kotlin and Swift. The app's own types
cross unchanged, so a phone shows a capture by the same rules as the terminal
(`crates/wartui-app/src/view/mod.rs`).

## The object model

| The phone implements | The phone calls |
|---|---|
| `RadioDriver` | `Radio`: `heard`, `completed`, `notified`, `dropped` |
| | `bridgesNearby`: every bridge the radio hears or the OS lists, for a picker |
| `SettingsStore` | `Settings`: `open`, `unsaved`, `place`, `saved`, `save` |
| `SecretStore` | `settingsLimits`, `checkExcludedSsid` |
| `LogSink` | `logTo` |
| `ReadyListener` | `Link`: `simulated`, `bluetooth`, `isSimulated`, `ready`, `close` |
| | `LocationFeed`: `push`, `lost`, `wanted`, `nextWanted`, `state` |
| | `HealthFeed`: `set` |
| | `Session`: `start`, `view`, `nextView`, `showSky`, `stop` |
| | `Session`'s steering: `apply`, `applyExcluded`, `startWithoutFix`, `powerCycle`, `formatSd` |
| | `exportWigleCsv` |
| `UploadListener` | `prepareUpload`, then `PreparedUpload`: `shown`, `send` |
| | `NodeImage`, `BridgeImage`: `parse`; `fetchNodeImage`, `fetchBridgeImage` |
| `NodeUpdateListener` | `Link`'s node update: `hearNodes`, then `updateNodes` |
| | `NodesHeard`: `targets`, `boards`, `plan`; `NodePlan`: `rows`, `chosen` |
| `BridgeUpdateListener` | `Link`'s bridge update: `updateBridge` |
| | `Stopper`: `stop`, for either update |

`linkUuids()` gives the link service's UUIDs. Kotlin calls `Link.close` `disconnect`, since every
Kotlin object's `close()` frees its handle. Both languages call the rule enum `Invalid`
`BrokenRule`, since inside `SettingsException.Invalid` Kotlin would read `Invalid` as that variant,
and for the same reason the upload's `Missing` `MissingCredentials` and its `Unfollowed`
`JobUnfollowed` (`uniffi.toml`).

A capture: open `Settings` over the app's stores, make a `Radio` over the app's driver, pick a
bridge from `bridgesNearby`, open `Link.bluetooth` to it, wait for `link.ready` to show it,
`Session.start` over it (a `suspend` function in Kotlin, `async` in Swift), follow `nextView`,
and `stop`. The link stays open for the next capture, read meanwhile so the bridge's stream never
backs up into it, with the bridge it announced. `exportWigleCsv` writes the capture out
afterwards, leaving out the networks the settings exclude, as `wartui export` does.
`smoke/kotlin/Smoke.kt` and `smoke/swift/main.swift` do exactly that. Under `Link.simulated` the
view's `link.host` shows the simulator as a bridge on USB, as the terminal does: it describes no
real link.

`powerCycle` and `formatSd` are answered by the bridge in the view's `answers`, each answer
numbered so a new one is told from the last.

## Settings

The settings text is the TOML a desktop keeps in `wartui.toml`, in the app's `SettingsStore`.
Given a `SecretStore` too, `Settings.open` keeps the `[api-keys]` table there, in the Keychain or
the Keystore, and the rest in the settings store. A save writes both, the keys first.

A settings screen builds a `SettingsEdit`: the rows the bridge holds only when moved, and the
host's rows as shown. In a capture it seeds its rows from the view's `settings`, `Session.apply`
sends the moved rows and saves the host's, and `Session.applyExcluded` sends and saves the
excluded list alone.
With none running, `Settings.save` saves the host's rows and ignores the bridge's. Turning
remember on there remembers the bridge an idle `Link.bluetooth` opened with those settings has
announced, in the same save. `settingsLimits` gives the ranges an edit must keep to, and a value
outside them, or an excluded list `checkExcludedSsid` refuses, is a `SteerError` with nothing sent
or saved. What a save came to is a `Saved`, its rule typed when the settings break one.

## Uploads

`prepareUpload` reads a capture and builds each site's upload, with the API keys the settings
hold and without the networks they exclude, and contacts nothing. A site ready to send is a
`PreparedUpload`, whose `shown` is what a confirm screen shows. A capture of test data, simulated
or placed by a typed-in position, is refused for every site.

- **`send` is the operator's yes.** An upload publishes to the operator's profile and cannot be
  taken back, so call it only from the operator's confirmation of what `shown` showed. It sends
  once; a second call is refused.
- **Run it in a background task.** It can take minutes: the uplink, then WDGWars' job followed to
  its end.
- **A lost answer reads as a failed send.** The site may have taken the file all the same, so a
  retry may send the rows again. `UploadOutcome.failed` says when a retry is called for.

## Firmware updates

`hearNodes` listens for the fleet's heartbeats, and its `plan`, made from checked `NodeImage`s,
is what a confirm screen shows and what `updateNodes` runs. `updateBridge` sends a checked
`BridgeImage` and reads the outcome across the bridge's reboot from the bridge's announcements,
which name the image it runs and whether it is still on trial. `fetchNodeImage` and
`fetchBridgeImage` take a release's images, checked against its `SHA256SUMS`, for the release tag
the app is built against.

- **Name the bridge, or remember it.** `Link.bluetooth` pins the link to the bridge named, else
  to the one the settings remember when it opens, and its transport refuses every other, so the
  app moves to another bridge by naming it. With neither it takes the only bridge heard, whose
  fleet would take the image, and an update over it is refused with `NotPinned`.
- **There is no USB fallback from a phone.** A bridge update that fails over Bluetooth is
  recovered from a computer, so the app says so before it starts.
- **Run either in a background task the OS does not suspend.** A bridge update can run several
  minutes and reads its outcome across reconnects. A `Stopper`, or an await the app cancels,
  cancels a node update, telling each node under way. It stops a bridge update only while the
  image is sent, ending the update's session on the bridge, which keeps the image it runs, with
  `Stopped`; from `Installing` on, the update runs to its outcome.
- **A bridge update needs the bridge's `Ready`**, from `link.ready`. The link keeps the latest
  one it reads, so after an install it names the new image.
- **A bridge on another major, or another build of this one, takes an update and nothing else.**
  The two share only the frozen core of the link, which names, updates and restarts the bridge.
  `link.ready` refuses it with `ReadyError.Incompatible`, carrying both versions and how they
  stand (`Standing`: `Apart` names the older end, `OtherBuild` the same major read as another
  build); `Session.start` refuses it with `StartError.Incompatible` and creates no capture, and
  `hearNodes` and `updateNodes` with `UpdateError.Incompatible`. `updateBridge` updates it, after a
  drop of the link too, which is the fix when the bridge is older; when the library is older, the
  app is. A bridge a minor
  apart is captured over, and the view says so as a `Note.BridgeMinorApart`. A capture whose
  bridge turns incompatible under it shows `Fault.BridgeIncompatible` first, in place of a link
  fault.

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
- **On Android, `altMsl` is `Location.getMslAltitudeMeters()`, API 34 and later.** Below it,
  leave `altMsl` out: `getAltitude()` is a height above the ellipsoid, not above sea level.
- **Errors are typed.** Every error and state is an enum for the app to word; a `detail` string
  is the failure's own account from below, for a log.

`src/lib.rs` has the threading rules, and each module's `//!` the rules it keeps.

## Generating the bindings

Build the library and `uniffi-bindgen` (`crates/wartui-bindgen`, on the same `uniffi`), then run
the generator on the library in library mode:

```sh
cargo build -p wartui-ffi -p wartui-bindgen
target/debug/uniffi-bindgen generate --library target/debug/libwartui_ffi.dylib \
  --language kotlin --out-dir out/kotlin       # or --language swift; .so on Linux
```

Kotlin lands in package `racing.deadbear.wartui.core` and needs JNA and kotlinx-coroutines; Swift
is module `WartuiCore` over the C module `WartuiCoreFFI` (`uniffi.toml`).

## Packaging

```sh
crates/wartui-ffi/package/build.sh [android|ios|all]    # default all
```

| It makes | Where, under `crates/wartui-ffi/` unless `target/` |
|---|---|
| `libwartui_ffi.so` for `arm64-v8a` and `x86_64` | `android/wartui-core/generated/jniLibs/` |
| The Kotlin binding, with `android = true` | `android/wartui-core/generated/kotlin/` |
| The release AAR | `target/phone/android/wartui-core.aar` |
| `libwartui_ffi.a` for the device and the arm64 simulator | `target/<triple>/release/` |
| `WartuiCoreFFI.xcframework`: both libraries, the C header and module map | `swift/` |
| `WartuiCore.swift` | `swift/Sources/WartuiCore/` |

All are release builds, and git ignores them. Neither ships the `testing` feature, and the script
fails a binding that exports it. Android needs an SDK (`ANDROID_HOME`, else
`~/Library/Android/sdk`) and an NDK (`ANDROID_NDK_HOME`, else `ANDROID_NDK_LATEST_HOME`, else
the newest in the SDK); iOS needs Xcode.

| Minimum | |
|---|---|
| Android | API 31 (`minSdk`, and the NDK clang the library links with) |
| iOS | 26.0 (the package's platform, and `IPHONEOS_DEPLOYMENT_TARGET`) |
| Swift | 6.4, in the Swift 6 language mode |

**An Android app** depends on the AAR. A file carries no dependency list, so the app also names
JNA and kotlinx-coroutines, which the binding calls through:

```kotlin
dependencies {
    implementation(files("libs/wartui-core.aar"))
    implementation("net.java.dev.jna:jna:5.19.1@aar")
    implementation("org.jetbrains.kotlinx:kotlinx-coroutines-core:1.11.0")
}
```

The AAR carries the R8 rules JNA and the binding need, and asks for no permission: the app
declares Bluetooth and location. Google Play requires native libraries aligned to 16 KB pages:
the script links with `-z max-page-size=16384` and fails a library with a `LOAD` segment
aligned to less. JNA 5.19.1's `libjnidispatch.so` is aligned too.

`android/` is the Gradle project that builds the AAR, with its wrapper and dependency
verification (`gradle/verification-metadata.xml`); `gradle/libs.versions.toml` pins every version
it builds with. Its `./gradlew connectedAndroidTest` runs the shipped binding on ART in a running
emulator; the test is JUnit 5, which the `de.mannodermaus.android-junit` plugin runs on a device.

**An iOS app** adds `swift/` as a local package (Xcode's "Add Package Dependencies", "Add
Local", or `.package(path: "…/crates/wartui-ffi/swift")`) and imports `WartuiCore`. Its test runs
on a simulator: `xcodebuild test -scheme WartuiCore -destination 'platform=iOS Simulator,…'`. CI
builds the package and runs that test on `main` only; a branch asks for it with
`gh workflow run bindings.yml --ref <branch>`.

**SQLite's temporary storage.** An export and an upload sort through temporary files. An Android
app names their directory once, before its first export or upload:

```kotlin
Os.setenv("SQLITE_TMPDIR", context.cacheDir.absolutePath, true)
```

Without it, `exportWigleCsv` throws `ExportException.NoTemporaryDirectory` and `prepareUpload`
throws `UploadRefusal.NoTemporaryDirectory`, before any work. The script fails a library whose
SQLite is not built to keep temporary storage in files. On iOS SQLite uses the app's `TMPDIR`
(`src/temp.rs`, `crates/wartui-core/src/export.rs`).

## Smoke tests

```sh
crates/wartui-ffi/smoke/run.sh [kotlin|swift|all]    # default all; prints OK <language>
```

It builds the library with the `testing` feature, which adds `WireSim`, a simulated bridge at the
byte level, `testingNodeImage`, and uploads and release fetches sent to a local listener. It
generates both bindings under `target/ffi-smoke/`, and runs a program in each language: a
simulated capture placed by a pushed fix, its pool moved and its rows exported, and its nodes
updated over the link once it stops, to an image the `testing` feature builds; then a capture over
a loopback `RadioDriver` to a `WireSim` found by `bridgesNearby` and announced by `link.ready`,
which remembers its bridge through an in-memory `SettingsStore` and keeps a typed-in API key in an
in-memory `SecretStore`; then, with that key, the simulated capture refused for upload as test
data, which contacts nothing. It needs `kotlinc` and `java`, or `swiftc`, on `PATH`, and fetches
JNA and kotlinx-coroutines from Maven Central against pinned digests. CI runs it on macOS
(`.github/workflows/bindings.yml`).

Swift written against the binding needs Swift 6.4 or later, and the smoke program compiles in the
Swift 6 language mode. The generated file itself keeps UniFFI's checks for older compilers.

The Rust tests (`cargo test -p wartui-ffi`) drive the same paths through the crate's API.
