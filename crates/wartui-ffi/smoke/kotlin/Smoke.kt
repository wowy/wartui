// The binding end to end from Kotlin: a simulated capture, then one over a radio written here that
// loops back to a simulated bridge. Also the first example a phone developer reads: the loopback
// driver is shaped as an Android `BluetoothGattCallback` adapter is.

import io.github.wowy.wartui.CaptureConfig
import io.github.wowy.wartui.Completion
import io.github.wowy.wartui.HealthFeed
import io.github.wowy.wartui.Heard
import io.github.wowy.wartui.HostBuild
import io.github.wowy.wartui.Link
import io.github.wowy.wartui.LocationFeed
import io.github.wowy.wartui.LocationFix
import io.github.wowy.wartui.LogLevel
import io.github.wowy.wartui.LogSink
import io.github.wowy.wartui.PositionSource
import io.github.wowy.wartui.PositionSources
import io.github.wowy.wartui.Pushed
import io.github.wowy.wartui.Radio
import io.github.wowy.wartui.RadioDriver
import io.github.wowy.wartui.Session
import io.github.wowy.wartui.Settings
import io.github.wowy.wartui.SettingsStore
import io.github.wowy.wartui.View
import io.github.wowy.wartui.WireSim
import io.github.wowy.wartui.logTo
import java.nio.file.Files
import java.nio.file.Path
import java.util.concurrent.ConcurrentLinkedQueue
import java.util.concurrent.Executors
import kotlin.system.exitProcess
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.cancel
import kotlinx.coroutines.launch
import kotlinx.coroutines.runBlocking
import kotlinx.coroutines.withTimeout

/** The longest any wait takes. */
const val DEADLINE_MS = 30_000L

/** The key the loopback radio knows its one bridge by. */
const val PEER = "loopback"

/** Keeps every log line the library sends. */
class Lines : LogSink {
    val lines = ConcurrentLinkedQueue<String>()

    override fun log(level: LogLevel, target: String, message: String) {
        lines.add("$level $target $message")
    }
}

/** Settings kept in memory, where an app would use its own storage. */
class MemoryStore : SettingsStore {
    @Volatile var text: String? = null

    override fun load(): String? = text

    override fun save(text: String, secret: Boolean) {
        this.text = text
    }

    override fun place(): String = "memory"
}

/**
 * A radio whose one bridge is [sim]. Each request returns at once and is answered later on the
 * `gatt` thread, as a `BluetoothGattCallback` answers on its binder thread.
 */
class LoopbackDriver(private val sim: WireSim, private val pumps: CoroutineScope) : RadioDriver {
    lateinit var radio: Radio
    private val gatt = Executors.newSingleThreadExecutor { Thread(it, "gatt").apply { isDaemon = true } }
    private var pumping = false

    private fun answer(request: ULong, outcome: Completion) =
        gatt.execute { radio.completed(request, outcome) }

    override fun pairsOnConnect() = true

    override fun recall(request: ULong, key: String) = answer(request, Completion.Recalled(known = false))

    override fun known(request: ULong) = answer(request, Completion.Known(bridges = emptyList()))

    override fun startScan(request: ULong) {
        answer(request, Completion.Done)
        gatt.execute { radio.heard(Heard(peer = PEER, id = sim.id(), name = null, live = true)) }
    }

    override fun stopScan() {}

    override fun connect(request: ULong, peer: String) = answer(request, Completion.Connected(made = true))

    override fun abandon(peer: String) {}

    override fun discover(request: ULong, peer: String) = answer(request, Completion.Done)

    @Synchronized
    override fun subscribe(request: ULong, peer: String) {
        answer(request, Completion.Done)
        if (pumping) return
        pumping = true
        // Each notification's value, as `onCharacteristicChanged` would hand it over.
        pumps.launch {
            while (true) {
                val value = sim.notified() ?: break
                radio.notified(peer, value)
            }
        }
    }

    override fun mtu(peer: String): UShort = 185u

    override fun write(request: ULong, peer: String, bytes: ByteArray, withResponse: Boolean) {
        sim.write(bytes)
        answer(request, Completion.Done)
    }

    override fun isConnected(peer: String) = true

    override fun close(peer: String, unsubscribe: Boolean) {}
}

/** Follow [session]'s views until [done] holds for one, within the deadline. */
suspend fun until(session: Session, what: String, done: (View) -> Boolean): View =
    withTimeout(DEADLINE_MS) {
        var viewed = session.nextView(0UL) ?: error("the capture ended waiting for $what")
        while (!done(viewed.view)) {
            viewed = session.nextView(viewed.seq) ?: error("the capture ended waiting for $what")
        }
        viewed.view
    }

fun capture(db: Path) = CaptureConfig(
    dbPath = db.toString(),
    notes = null,
    recordRaw = false,
    build = HostBuild(hostVersion = "smoke", hostCommit = null, releaseTag = null),
)

/** A simulated capture, placed by a fix the platform pushes. */
suspend fun simulated(dir: Path) {
    val db = dir.resolve("simulated.db")
    val location = LocationFeed()
    val session = Session.start(
        capture(db),
        Link.simulated(3u),
        Settings.unsaved(),
        PositionSources(bridgeReceiver = false, location = location, fixed = null),
        HealthFeed(),
    )
    until(session, "three nodes and the link up") { it.nodes.size == 3 && it.link.up }
    // Synthetic and mid-ocean, taken now on this device's clock.
    val fix = LocationFix(
        lat = 10.0,
        lon = -40.0,
        altMsl = null,
        accuracyM = 5.0,
        atMs = System.currentTimeMillis(),
        mock = false,
    )
    check(location.push(fix) == Pushed.TAKEN) { "the fix was refused" }
    until(session, "rows placed by the platform") {
        it.position.located && it.position.source == PositionSource.PLATFORM
    }
    val end = withTimeout(DEADLINE_MS) { session.stop() }
    check(end.written > 0UL) { "no rows were written" }
    check(Files.exists(db)) { "no capture at $db" }
}

/** A capture over the loopback radio, which remembers its bridge in the settings. */
suspend fun loopback(dir: Path) {
    val pumps = CoroutineScope(Dispatchers.Default + SupervisorJob())
    val driver = LoopbackDriver(WireSim.start(3u, 185u), pumps)
    val radio = Radio(driver)
    driver.radio = radio
    val store = MemoryStore()
    val settings = Settings.open(store)
    val link = Link.bluetooth(radio, settings, null)
    val session = Session.start(
        capture(dir.resolve("loopback.db")),
        link,
        settings,
        PositionSources(bridgeReceiver = false, location = null, fixed = null),
        null,
    )
    until(session, "three nodes and a bridge") { it.nodes.size == 3 && it.bridge != null }
    withTimeout(DEADLINE_MS) {
        session.stop()
        link.disconnect()
    }
    pumps.cancel()
    val saved = store.text
    check(saved != null && saved.contains("\"$PEER\"")) { "the bridge was not remembered: $saved" }
}

fun main() {
    try {
        runBlocking {
            val sink = Lines()
            check(logTo(sink, LogLevel.INFO)) { "a log sink was already set" }
            val dir = Files.createTempDirectory("wartui-smoke")
            simulated(dir)
            loopback(dir)
            check(sink.lines.isNotEmpty()) { "the log sink saw nothing" }
        }
    } catch (failure: Throwable) {
        System.err.println("FAILED: $failure")
        exitProcess(1)
    }
    println("OK")
    // The library's runtime threads outlive `main`.
    exitProcess(0)
}
