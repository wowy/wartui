// The binding end to end from Kotlin: a simulated capture steered and exported, its nodes updated
// between captures, then one over a radio written here that loops back to a simulated bridge, and
// the first capture refused for upload as test data. Also the first example a phone developer
// reads: the loopback driver is shaped as an Android `BluetoothGattCallback` adapter is.

import racing.deadbear.wartui.core.Board
import racing.deadbear.wartui.core.CaptureConfig
import racing.deadbear.wartui.core.ChannelPool
import racing.deadbear.wartui.core.Completion
import racing.deadbear.wartui.core.ExportOptions
import racing.deadbear.wartui.core.HealthFeed
import racing.deadbear.wartui.core.Heard
import racing.deadbear.wartui.core.HostBuild
import racing.deadbear.wartui.core.Link
import racing.deadbear.wartui.core.LocationFeed
import racing.deadbear.wartui.core.LocationFix
import racing.deadbear.wartui.core.LogLevel
import racing.deadbear.wartui.core.LogSink
import racing.deadbear.wartui.core.NodeImage
import racing.deadbear.wartui.core.NodeOutcome
import racing.deadbear.wartui.core.NodeUpdateListener
import racing.deadbear.wartui.core.NodeUpdateProgress
import racing.deadbear.wartui.core.PositionSource
import racing.deadbear.wartui.core.PositionSources
import racing.deadbear.wartui.core.Pushed
import racing.deadbear.wartui.core.Radio
import racing.deadbear.wartui.core.RadioDriver
import racing.deadbear.wartui.core.Saved
import racing.deadbear.wartui.core.SecretStore
import racing.deadbear.wartui.core.Session
import racing.deadbear.wartui.core.Settings
import racing.deadbear.wartui.core.SettingsEdit
import racing.deadbear.wartui.core.SettingsStore
import racing.deadbear.wartui.core.UploadOptions
import racing.deadbear.wartui.core.UploadRefusal
import racing.deadbear.wartui.core.View
import racing.deadbear.wartui.core.WireSim
import racing.deadbear.wartui.core.bridgesNearby
import racing.deadbear.wartui.core.exportWigleCsv
import racing.deadbear.wartui.core.logTo
import racing.deadbear.wartui.core.prepareUpload
import racing.deadbear.wartui.core.testingNodeImage
import java.nio.file.Files
import java.nio.file.Path
import java.util.concurrent.ConcurrentLinkedQueue
import java.util.concurrent.Executors
import java.util.concurrent.atomic.AtomicInteger
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

/** API keys kept in memory, where an app would use the Keystore. */
class MemorySecrets : SecretStore {
    @Volatile var text: String? = null

    override fun load(): String? = text

    override fun save(text: String) {
        this.text = text
    }
}

/**
 * A radio whose one bridge is [sim]. Each request returns at once and is answered later on the
 * `gatt` thread, as a `BluetoothGattCallback` answers on its binder thread.
 */
class LoopbackDriver(private val sim: WireSim, private val pumps: CoroutineScope) : RadioDriver {
    lateinit var radio: Radio
    private val gatt =
        Executors.newSingleThreadExecutor { Thread(it, "gatt").apply { isDaemon = true } }
    private var pumping = false

    private fun answer(request: ULong, outcome: Completion) =
        gatt.execute { radio.completed(request, outcome) }

    override fun pairsOnConnect() = true

    override fun recall(request: ULong, key: String) =
        answer(request, Completion.Recalled(known = false))

    override fun known(request: ULong) = answer(request, Completion.Known(bridges = emptyList()))

    override fun startScan(request: ULong) {
        answer(request, Completion.Done)
        gatt.execute { radio.heard(Heard(peer = PEER, id = sim.id(), name = null, live = true)) }
    }

    override fun stopScan() {}

    override fun connect(request: ULong, peer: String) =
        answer(request, Completion.Connected(made = true))

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

/** Counts the steps a node update reports. */
class Counting : NodeUpdateListener {
    val calls = AtomicInteger()

    override fun progress(progress: NodeUpdateProgress) {
        calls.incrementAndGet()
    }
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

/** What a settings screen holds before the operator moves a row: nothing to send. */
fun unmoved(settings: Settings, view: View?): SettingsEdit {
    val saved = settings.saved()
    return SettingsEdit(
        rememberBridge = saved.rememberBridge,
        waitForFix = saved.waitForFix,
        apiKeys = saved.apiKeys,
        excludedSsids = view?.settings?.excludedSsids ?: saved.excludedSsids,
    )
}

fun capture(db: Path) = CaptureConfig(
    dbPath = db.toString(),
    notes = null,
    recordRaw = false,
    build = HostBuild(hostVersion = "smoke", hostCommit = null, releaseTag = null),
)

/**
 * A simulated capture, placed by a fix the platform pushes, steered, then exported, and its nodes
 * updated over the link once it has stopped. Returns the capture's path.
 */
suspend fun simulated(dir: Path): Path {
    val db = dir.resolve("simulated.db")
    val location = LocationFeed()
    val settings = Settings.unsaved()
    val link = Link.simulated(3u)
    val session = Session.start(
        capture(db),
        link,
        settings,
        PositionSources(bridgeReceiver = false, location = location, fixed = null),
        HealthFeed(),
    )
    val shown = until(session, "three nodes, the link up and the pool") {
        it.nodes.size == 3 && it.link.up && it.settings.pool != null
    }
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
    // The operator moves the pool; the view shows it once the bridge holds it.
    val pool = if (shown.settings.pool == ChannelPool.EU) ChannelPool.US else ChannelPool.EU
    val applied = session.apply(unmoved(settings, shown).copy(pool = pool))
    check(applied.saved == Saved.Nowhere) { "unsaved settings were saved: ${applied.saved}" }
    until(session, "the pool moved") { it.settings.pool == pool }
    until(session, "every node heartbeating and heard") { view ->
        view.nodes.size == 3 && view.nodes.all { it.firmware != null && it.sightings > 0UL }
    }
    val end = withTimeout(DEADLINE_MS) { session.stop() }
    check(end.written > 0UL) { "no rows were written" }
    check(Files.exists(db)) { "no capture at $db" }

    val csv = dir.resolve("simulated.csv")
    val exported = withTimeout(DEADLINE_MS) {
        exportWigleCsv(settings, db.toString(), csv.toString(), ExportOptions(), "smoke")
    }
    check(Files.exists(csv)) { "no export at $csv" }
    check(exported.nodes.size == 3) { "the export names ${exported.nodes.map { it.name }}" }

    updateNodes(link)
    return db
}

/** Every node [link] hears updated, forced, to an image of the version they run. */
suspend fun updateNodes(link: Link) {
    val heard = withTimeout(DEADLINE_MS) { link.hearNodes(3_000u) }
    // No slot named: every node heard.
    val version = heard.targets(listOf()).first().capabilities.version
    val image = NodeImage.parse(testingNodeImage(Board.Esp32c5, version))
    val plan = heard.plan(listOf(), listOf(image), true)
    check(plan.chosen() == 3u) { "the plan chooses ${plan.rows()}" }
    val counting = Counting()
    val updated = withTimeout(DEADLINE_MS) { link.updateNodes(plan, null, counting) }
    check(updated.failed == null && updated.outcomes.size == 3) { "updated: $updated" }
    check(updated.outcomes.all { it.outcome is NodeOutcome.Done }) { "updated: $updated" }
    check(counting.calls.get() > 0) { "the listener heard nothing" }
}

/** A simulated capture is test data, which is never uploaded. This contacts nothing. */
suspend fun refusedAsTestData(settings: Settings, db: Path) {
    try {
        prepareUpload(settings, db.toString(), UploadOptions(appVersion = "smoke"))
        error("a simulated capture was prepared for upload")
    } catch (refused: UploadRefusal.TestData) {
        // As it should be.
    }
}

/**
 * A capture over the loopback radio: the bridge picked from those nearby and announced before the
 * capture, remembered in the settings, and an API key typed in afterwards kept in the secret store.
 * Returns the settings, which hold the key.
 */
suspend fun loopback(dir: Path): Settings {
    val pumps = CoroutineScope(Dispatchers.Default + SupervisorJob())
    val driver = LoopbackDriver(WireSim.start(3u, 185u), pumps)
    val radio = Radio(driver)
    driver.radio = radio
    val nearby = withTimeout(DEADLINE_MS) { bridgesNearby(radio, 500u) }
    check(nearby.size == 1 && nearby[0].peer == PEER && nearby[0].live) { "nearby: $nearby" }
    val bridge = nearby[0].tail

    val store = MemoryStore()
    val secrets = MemorySecrets()
    val settings = Settings.open(store, secrets)
    val link = Link.bluetooth(radio, settings, bridge)
    val announced = withTimeout(DEADLINE_MS) { link.ready(15_000u, null) }
    check(announced.bridge.macTail == bridge) { "announced ${announced.bridge}, not $bridge" }
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

    // Obviously fake.
    val key = "AAAAAAAAAA"
    val typed = unmoved(settings, null).let { it.copy(apiKeys = it.apiKeys.copy(wdgwars = key)) }
    val keyed = settings.save(typed)
    check(keyed is Saved.At) { "the key was not saved: $keyed" }
    check(secrets.text?.contains(key) == true) { "the secret store holds ${secrets.text}" }
    check(store.text?.contains(key) == false) { "the key is in the settings: ${store.text}" }
    return settings
}

fun main() {
    try {
        runBlocking {
            val sink = Lines()
            check(logTo(sink, LogLevel.INFO)) { "a log sink was already set" }
            val dir = Files.createTempDirectory("wartui-smoke")
            val simulatedDb = simulated(dir)
            val keyed = loopback(dir)
            refusedAsTestData(keyed, simulatedDb)
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
