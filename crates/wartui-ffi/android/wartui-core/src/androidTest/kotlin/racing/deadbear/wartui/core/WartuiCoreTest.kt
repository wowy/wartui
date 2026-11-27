package racing.deadbear.wartui.core

import android.system.Os
import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.platform.app.InstrumentationRegistry
import java.io.File
import java.util.concurrent.ConcurrentLinkedQueue
import kotlinx.coroutines.runBlocking
import kotlinx.coroutines.withTimeout
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertTrue
import org.junit.Test
import org.junit.runner.RunWith

/** The longest any wait takes. */
const val DEADLINE_MS = 30_000L

/** The shipped binding on ART, through JNA, with no `testing` feature. */
@RunWith(AndroidJUnit4::class)
class WartuiCoreTest {
    private val context = InstrumentationRegistry.getInstrumentation().targetContext

    /** Settings kept in memory, where an app would use its own storage. */
    class MemoryStore : SettingsStore {
        @Volatile var text: String? = null

        override fun load(): String? = text

        override fun save(text: String, secret: Boolean) {
            this.text = text
        }

        override fun place(): String = "memory"
    }

    /** Keeps every log line the library sends. */
    class Lines : LogSink {
        val lines = ConcurrentLinkedQueue<String>()

        override fun log(level: LogLevel, target: String, message: String) {
            lines.add("$level $target $message")
        }
    }

    private fun capture(db: File) = CaptureConfig(
        dbPath = db.absolutePath,
        notes = null,
        recordRaw = false,
        build = HostBuild(hostVersion = "android-test", hostCommit = null, releaseTag = null),
    )

    /** A capture's file under the app's files, none there before. */
    private fun freshDb(name: String): File =
        File(context.filesDir, name).also { db ->
            listOf("", "-wal", "-shm").forEach { File(db.path + it).delete() }
        }

    /** Follow [session]'s views until [done] holds for one, within the deadline. */
    private suspend fun until(session: Session, what: String, done: (View) -> Boolean): View =
        withTimeout(DEADLINE_MS) {
            val ended = "the capture ended waiting for $what"
            var viewed = session.nextView(0UL) ?: error(ended)
            while (!done(viewed.view)) {
                viewed = session.nextView(viewed.seq) ?: error(ended)
            }
            viewed.view
        }

    @Test
    fun library_loads_when_run_on_art() {
        assertEquals("f2410001-d870-4eb6-a5a0-ed645732a877", linkUuids().service)
    }

    @Test
    fun settings_keep_an_edit_when_store_is_kotlin() {
        val store = MemoryStore()
        val settings = Settings.open(store, null)
        val saved = settings.saved()
        val edit = SettingsEdit(
            rememberBridge = saved.rememberBridge,
            waitForFix = saved.waitForFix,
            apiKeys = saved.apiKeys,
            excludedSsids = listOf("example-network"),
        )
        val outcome = settings.save(edit)
        assertTrue("the save came to $outcome", outcome is Saved.At)
        assertTrue("the store holds ${store.text}", store.text?.contains("example-network") == true)
        // Read back through the store's `load`.
        assertEquals(listOf("example-network"), Settings.open(store, null).saved().excludedSsids)
    }

    @Test
    fun capture_exports_when_app_names_a_temporary_directory() = runBlocking {
        // An export sorts through temporary files, so it refuses until the app names their
        // directory.
        Os.unsetenv("SQLITE_TMPDIR")
        Os.unsetenv("TMPDIR")
        val db = freshDb("simulated.db")
        val location = LocationFeed()
        val session = Session.start(
            capture(db),
            Link.simulated(3u),
            Settings.unsaved(),
            PositionSources(bridgeReceiver = false, location = location, fixed = null),
            null,
        )
        until(session, "three nodes") { it.nodes.size == 3 }
        // Synthetic and mid-ocean, taken now on this device's clock.
        val fix = LocationFix(
            lat = 10.0,
            lon = -40.0,
            altMsl = null,
            accuracyM = 5.0,
            atMs = System.currentTimeMillis(),
            mock = false,
        )
        assertEquals(Pushed.TAKEN, location.push(fix))
        until(session, "three nodes heard, placed") { view ->
            view.position.located && view.nodes.size == 3 && view.nodes.all { it.sightings > 0UL }
        }
        val end = withTimeout(DEADLINE_MS) { session.stop() }
        assertTrue("no rows were written", end.written > 0UL)

        val csv = File(context.cacheDir, "simulated.csv").also { it.delete() }
        suspend fun export() = withTimeout(DEADLINE_MS) {
            exportWigleCsv(
                Settings.unsaved(),
                db.absolutePath,
                csv.absolutePath,
                ExportOptions(),
                "android-test",
            )
        }
        val refused = try {
            export()
            null
        } catch (error: ExportException.NoTemporaryDirectory) {
            error
        }
        assertNotNull("the export ran with no temporary directory", refused)
        assertFalse("the refused export wrote $csv", csv.exists())

        Os.setenv("SQLITE_TMPDIR", context.cacheDir.absolutePath, true)
        val exported = export()
        assertTrue("no export at $csv", csv.isFile)
        assertTrue("the export wrote no rows", exported.rows > 0UL)
        assertEquals(3, exported.nodes.size)
    }

    @Test
    fun log_sink_hears_the_library_when_a_capture_runs() = runBlocking {
        val sink = Lines()
        assertTrue("a log sink was already set", logTo(sink, LogLevel.DEBUG))
        val session = Session.start(
            capture(freshDb("logged.db")),
            Link.simulated(1u),
            Settings.unsaved(),
            PositionSources(bridgeReceiver = false, location = null, fixed = null),
            null,
        )
        try {
            withTimeout(DEADLINE_MS) {
                var seq = 0UL
                while (sink.lines.isEmpty()) {
                    seq = session.nextView(seq)?.seq ?: error("the capture ended unheard")
                }
            }
        } finally {
            withTimeout(DEADLINE_MS) { session.stop() }
        }
        Unit
    }
}
