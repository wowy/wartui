// The shipped binding in an iOS simulator, with no `testing` feature: the static library loads,
// a store written in Swift is called back, a simulated capture exports, and a log sink hears it.

import Foundation
import Testing
import WartuiCore

/// The longest any wait takes.
let deadline: Duration = .seconds(30)

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

struct TimedOut: Error {
    let what: String
}

/// Run `work`, failing if it takes longer than the deadline.
func within<T: Sendable>(
    _ what: String, _ work: @escaping @Sendable () async throws -> T
) async throws -> T {
    try await withThrowingTaskGroup(of: T.self) { group in
        group.addTask { try await work() }
        group.addTask {
            try await Task.sleep(for: deadline)
            throw TimedOut(what: what)
        }
        defer { group.cancelAll() }
        return try await group.next()!
    }
}

/// Follow `session`'s views until `done` holds for one.
func until(
    _ session: Session, _ what: String, _ done: @escaping @Sendable (View) -> Bool
) async throws -> View {
    try await within(what) {
        var viewed = await session.nextView(after: 0)
        while let shown = viewed, !done(shown.view) {
            viewed = await session.nextView(after: shown.seq)
        }
        guard let shown = viewed else { throw TimedOut(what: "\(what): the capture ended") }
        return shown.view
    }
}

func capture(_ db: URL) -> CaptureConfig {
    CaptureConfig(
        dbPath: db.path,
        notes: nil,
        recordRaw: false,
        build: HostBuild(hostVersion: "ios-test", hostCommit: nil, releaseTag: nil)
    )
}

/// A directory of its own under the test's temporary directory.
func freshDirectory() throws -> URL {
    let dir = FileManager.default.temporaryDirectory
        .appendingPathComponent("wartui-\(UUID().uuidString)")
    try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
    return dir
}

@Test func libraryLoadsWhenRunInASimulator() {
    #expect(linkUuids().service == "f2410001-d870-4eb6-a5a0-ed645732a877")
}

@Test func settingsKeepAnEditWhenStoreIsSwift() throws {
    let store = MemoryStore()
    let settings = try Settings.open(store: store, secrets: nil)
    let saved = settings.saved()
    let edit = SettingsEdit(
        rememberBridge: saved.rememberBridge,
        waitForFix: saved.waitForFix,
        apiKeys: saved.apiKeys,
        excludedSsids: ["example-network"]
    )
    guard case .at = try settings.save(edit: edit) else {
        Issue.record("the settings were not saved")
        return
    }
    #expect(store.text?.contains("example-network") == true)
    // Read back through the store's `load`.
    let reopened = try Settings.open(store: store, secrets: nil)
    #expect(reopened.saved().excludedSsids == ["example-network"])
}

@Test func captureExportsWhenRunOverTheSimulator() async throws {
    let dir = try freshDirectory()
    let db = dir.appendingPathComponent("simulated.db")
    let location = LocationFeed()
    let session = try await Session.start(
        config: capture(db),
        link: Link.simulated(nodes: 3),
        settings: Settings.unsaved(),
        position: PositionSources(bridgeReceiver: false, location: location, fixed: nil),
        health: nil
    )
    _ = try await until(session, "three nodes") { $0.nodes.count == 3 }
    // Synthetic and mid-ocean, taken now on this device's clock.
    let fix = LocationFix(
        lat: 10.0,
        lon: -40.0,
        altMsl: nil,
        accuracyM: 5.0,
        atMs: Int64(Date().timeIntervalSince1970 * 1000),
        mock: false
    )
    #expect(location.push(fix: fix) == .taken)
    _ = try await until(session, "three nodes heard, placed") { view in
        view.position.located && view.nodes.count == 3 && view.nodes.allSatisfy { $0.sightings > 0 }
    }
    let end = try await within("stop") { try await session.stop() }
    #expect(end.written > 0)

    let csv = dir.appendingPathComponent("simulated.csv")
    let exported = try await within("export") {
        try await exportWigleCsv(
            settings: Settings.unsaved(), dbPath: db.path, outPath: csv.path,
            options: ExportOptions(), appVersion: "ios-test"
        )
    }
    #expect(FileManager.default.fileExists(atPath: csv.path))
    #expect(exported.rows > 0)
    #expect(exported.nodes.count == 3)
}

@Test func logSinkHearsTheLibraryWhenACaptureRuns() async throws {
    let lines = Lines()
    #expect(logTo(sink: lines, level: .debug), "a log sink was already set")
    let dir = try freshDirectory()
    let session = try await Session.start(
        config: capture(dir.appendingPathComponent("logged.db")),
        link: Link.simulated(nodes: 1),
        settings: Settings.unsaved(),
        position: PositionSources(bridgeReceiver: false, location: nil, fixed: nil),
        health: nil
    )
    _ = try await until(session, "a log line") { _ in lines.count > 0 }
    _ = try await within("stop") { try await session.stop() }
    #expect(lines.count > 0)
}
