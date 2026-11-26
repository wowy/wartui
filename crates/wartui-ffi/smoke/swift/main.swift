// The binding end to end from Swift: a simulated capture, then one over a radio written here that
// loops back to a simulated bridge. Also the first example a phone developer reads: the loopback
// driver is shaped as a `CBPeripheralDelegate` adapter is.

import Foundation

/// The longest any wait takes, in seconds.
let deadline: Double = 30

/// The key the loopback radio knows its one bridge by.
let peer = "loopback"

func fail(_ why: String) -> Never {
    FileHandle.standardError.write(Data("FAILED: \(why)\n".utf8))
    exit(1)
}

/// Run `work`, failing the program if it takes longer than the deadline.
func within<T>(_ what: String, _ work: () async throws -> T) async rethrows -> T {
    let watchdog = DispatchWorkItem { fail("no \(what) within \(deadline) s") }
    DispatchQueue.global().asyncAfter(deadline: .now() + deadline, execute: watchdog)
    defer { watchdog.cancel() }
    return try await work()
}

/// Keeps every log line the library sends.
final class Lines: LogSink, @unchecked Sendable {
    private let lock = NSLock()
    private var kept: [String] = []

    func log(level: LogLevel, target: String, message: String) {
        lock.lock()
        kept.append("\(level) \(target) \(message)")
        lock.unlock()
    }

    var count: Int {
        lock.lock()
        defer { lock.unlock() }
        return kept.count
    }
}

/// Settings kept in memory, where an app would use its own storage.
final class MemoryStore: SettingsStore, @unchecked Sendable {
    private let lock = NSLock()
    private var stored: String?

    var text: String? {
        lock.lock()
        defer { lock.unlock() }
        return stored
    }

    func load() -> String? { text }

    func save(text: String, secret: Bool) {
        lock.lock()
        stored = text
        lock.unlock()
    }

    func place() -> String { "memory" }
}

/// A radio whose one bridge is `sim`. Each request returns at once and is answered later on the
/// `gatt` queue, as a `CBPeripheralDelegate` answers on its own queue.
final class LoopbackDriver: RadioDriver, @unchecked Sendable {
    private let sim: WireSim
    private let gatt = DispatchQueue(label: "gatt")
    private let lock = NSLock()
    private var radio: Radio?
    private var pump: Task<Void, Never>?

    init(sim: WireSim) { self.sim = sim }

    func attach(_ radio: Radio) {
        lock.lock()
        self.radio = radio
        lock.unlock()
    }

    /// Stop pumping notifications.
    func detach() {
        lock.lock()
        pump?.cancel()
        radio = nil
        lock.unlock()
    }

    private var attached: Radio? {
        lock.lock()
        defer { lock.unlock() }
        return radio
    }

    private func answer(_ request: UInt64, _ outcome: Completion) {
        gatt.async { self.attached?.completed(request: request, outcome: outcome) }
    }

    func pairsOnConnect() -> Bool { true }

    func recall(request: UInt64, key: String) { answer(request, .recalled(known: false)) }

    func known(request: UInt64) { answer(request, .known(bridges: [])) }

    func startScan(request: UInt64) {
        answer(request, .done)
        let id = sim.id()
        let heard = Heard(peer: peer, id: id, name: nil, live: true)
        gatt.async { self.attached?.heard(device: heard) }
    }

    func stopScan() {}

    func connect(request: UInt64, peer: String) { answer(request, .connected(made: true)) }

    func abandon(peer: String) {}

    func discover(request: UInt64, peer: String) { answer(request, .done) }

    func subscribe(request: UInt64, peer: String) {
        answer(request, .done)
        lock.lock()
        defer { lock.unlock() }
        guard pump == nil else { return }
        // Each notification's value, as `peripheral(_:didUpdateValueFor:error:)` would hand it
        // over.
        pump = Task { [sim] in
            while !Task.isCancelled, let value = await sim.notified() {
                self.attached?.notified(peer: peer, bytes: value)
            }
        }
    }

    func mtu(peer: String) -> UInt16 { 185 }

    func write(request: UInt64, peer: String, bytes: Data, withResponse: Bool) {
        sim.write(bytes: bytes)
        answer(request, .done)
    }

    func isConnected(peer: String) -> Bool { true }

    func close(peer: String, unsubscribe: Bool) {}
}

/// Follow `session`'s views until `done` holds for one, within the deadline.
func until(_ session: Session, _ what: String, _ done: (View) -> Bool) async -> View {
    await within(what) {
        var viewed = await session.nextView(after: 0)
        while let shown = viewed, !done(shown.view) {
            viewed = await session.nextView(after: shown.seq)
        }
        guard let shown = viewed else { fail("the capture ended waiting for \(what)") }
        return shown.view
    }
}

func capture(_ db: URL) -> CaptureConfig {
    CaptureConfig(
        dbPath: db.path,
        notes: nil,
        recordRaw: false,
        build: HostBuild(hostVersion: "smoke", hostCommit: nil, releaseTag: nil)
    )
}

/// A simulated capture, placed by a fix the platform pushes.
func simulated(_ dir: URL) async throws {
    let db = dir.appendingPathComponent("simulated.db")
    let location = LocationFeed()
    let session = try await Session.start(
        config: capture(db),
        link: Link.simulated(nodes: 3),
        settings: Settings.unsaved(),
        position: PositionSources(bridgeReceiver: false, location: location, fixed: nil),
        health: HealthFeed()
    )
    _ = await until(session, "three nodes and the link up") { $0.nodes.count == 3 && $0.link.up }
    // Synthetic and mid-ocean, taken now on this device's clock.
    let fix = LocationFix(
        lat: 10.0,
        lon: -40.0,
        altMsl: nil,
        accuracyM: 5.0,
        atMs: Int64(Date().timeIntervalSince1970 * 1000),
        mock: false
    )
    guard location.push(fix: fix) == .taken else { fail("the fix was refused") }
    _ = await until(session, "rows placed by the platform") {
        $0.position.located && $0.position.source == .platform
    }
    let end = try await within("stop") { try await session.stop() }
    guard end.written > 0 else { fail("no rows were written") }
    guard FileManager.default.fileExists(atPath: db.path) else { fail("no capture at \(db.path)") }
}

/// A capture over the loopback radio, which remembers its bridge in the settings.
func loopback(_ dir: URL) async throws {
    let driver = LoopbackDriver(sim: WireSim.start(nodes: 3, mtu: 185))
    let radio = Radio(driver: driver)
    driver.attach(radio)
    let store = MemoryStore()
    let settings = try Settings.open(store: store)
    let link = try Link.bluetooth(radio: radio, settings: settings, bridge: nil)
    let session = try await Session.start(
        config: capture(dir.appendingPathComponent("loopback.db")),
        link: link,
        settings: settings,
        position: PositionSources(bridgeReceiver: false, location: nil, fixed: nil),
        health: nil
    )
    _ = await until(session, "three nodes and a bridge") { $0.nodes.count == 3 && $0.bridge != nil }
    try await within("stop") {
        _ = try await session.stop()
        try await link.close()
    }
    driver.detach()
    guard let saved = store.text, saved.contains("\"\(peer)\"") else {
        fail("the bridge was not remembered: \(store.text ?? "nothing")")
    }
}

let lines = Lines()
guard logTo(sink: lines, level: .info) else { fail("a log sink was already set") }
let dir = FileManager.default.temporaryDirectory
    .appendingPathComponent("wartui-smoke-\(ProcessInfo.processInfo.globallyUniqueString)")
do {
    try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
    try await simulated(dir)
    try await loopback(dir)
} catch {
    fail("\(error)")
}
guard lines.count > 0 else { fail("the log sink saw nothing") }
print("OK")
