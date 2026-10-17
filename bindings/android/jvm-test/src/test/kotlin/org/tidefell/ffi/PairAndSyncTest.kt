package org.tidefell.ffi

import java.io.File as JFile
import java.net.ServerSocket
import java.nio.file.Files
import java.util.concurrent.CopyOnWriteArrayList
import java.util.concurrent.TimeUnit
import kotlin.test.Test
import kotlin.test.assertContentEquals
import kotlin.test.assertEquals
import kotlin.test.assertFailsWith
import kotlin.test.assertFalse
import kotlin.test.assertIs
import kotlin.test.assertTrue
import kotlinx.coroutines.async
import kotlinx.coroutines.delay
import kotlinx.coroutines.runBlocking

/**
 * The Android flow on the JVM: pair with a real `tidefelld` that holds the user key, join its
 * folder as a partial replica, open a placeholder and save a file back.
 */
class PairAndSyncTest {
    private val bin = JFile(System.getProperty("tidefell.bin"))
    private val network =
        Network(relay = Relay.Disabled, addressLookup = false, localDiscovery = false, bind = "127.0.0.1:0")

    private fun run(vararg args: String): String {
        val process = ProcessBuilder(*args).redirectErrorStream(true).start()
        val output = process.inputStream.bufferedReader().readText()
        assertTrue(process.waitFor(60, TimeUnit.SECONDS), "${args.toList()} timed out")
        assertEquals(0, process.exitValue(), "${args.toList()} failed:\n$output")
        return output
    }

    private fun field(output: String, name: String): String =
        output.lines().first { it.startsWith("$name:") }.substringAfter(":").trim()

    private fun freePort(): Int = ServerSocket(0).use { it.localPort }

    /** Printed when a wait times out. */
    private var context: () -> String = { "" }

    private suspend fun eventually(what: String, check: () -> Boolean) {
        repeat(1500) {
            if (check()) return
            delay(20)
        }
        throw AssertionError("timed out waiting for $what\n${context()}")
    }

    @Test
    fun pairJoinOpenAndSave() = runBlocking {
        val root = Files.createTempDirectory("tidefell-jvm").toFile()
        try {
            // The laptop: tidefelld with the user key and a folder holding one file.
            val laptopData = root.resolve("laptop/data")
            val laptopFolder = root.resolve("laptop/Photos").apply { mkdirs() }
            laptopFolder.resolve("beach.jpg").writeText("sand and sea")
            run("$bin/tidefelld", "init", "--data-dir", laptopData.path, "--name", "laptop")
            val user = field(run("$bin/tidefelld", "id", "--data-dir", laptopData.path), "user")
            val folderId = run("$bin/tidefelld", "new-folder-id").trim()
            val config = root.resolve("laptop/tidefelld.toml")
            config.writeText(
                """
                data_dir = "${laptopData.path}"

                [network]
                relay = { mode = "disabled" }
                address_lookup = false
                local_discovery = false
                bind = "127.0.0.1:${freePort()}"

                [[folders]]
                id = "$folderId"
                path = "${laptopFolder.path}"
                members = ["$user"]
                retention_days = 30
                scan_interval_secs = 1
                """.trimIndent(),
            )
            val daemon = ProcessBuilder("$bin/tidefelld", "run", "--config", config.path)
                .apply { environment()["RUST_LOG"] = "info,tidefell_net=debug,tidefell_engine=debug" }
                .redirectErrorStream(true)
                .redirectOutput(root.resolve("laptop/daemon.log"))
                .start()
            try {
                eventually("the control socket") { laptopData.resolve("control.sock").exists() }

                // The phone.
                val phoneData = root.resolve("phone").path
                assertTrue(initDiagnostics(phoneData, "info,tidefell_net=debug,tidefell_engine=debug"))
                log(LogLevel.INFO, "PairAndSyncTest", "the app's own message")
                val logFile = JFile(phoneData, "logs/tidefell.log")
                assertTrue(logFile.readText().contains("the app's own message"))
                val device = deviceId(phoneData)
                assertFalse(isPaired(phoneData))
                assertFailsWith<TidefellException.NotPaired> { Node.start(phoneData, network) }

                val pairing = Pairing.start(phoneData, network, "JVM phone")
                assertTrue(pairing.code().startsWith("TFP1-"))
                val approve = async(kotlinx.coroutines.Dispatchers.IO) {
                    run("$bin/tidefell", "--data-dir", laptopData.path, "pair", "--yes", pairing.code())
                }
                val confirming = assertIs<PairingUpdate.Confirming>(pairing.nextUpdate())
                assertEquals("laptop", confirming.approverName)
                val paired = assertIs<PairingUpdate.Paired>(pairing.nextUpdate())
                val approved = approve.await()
                assertTrue(approved.contains("${confirming.confirmationCode.take(3)} ${confirming.confirmationCode.drop(3)}"))
                assertEquals("JVM phone", paired.name)
                assertEquals(user, paired.user)
                assertTrue(isPaired(phoneData))
                pairing.close()

                val node = Node.start(phoneData, network)
                context = {
                    "phone peers: ${node.peers()}\nphone folders: ${node.folders()}\n" +
                        "daemon log:\n" + root.resolve("laptop/daemon.log").readText()
                }
                val events = CopyOnWriteArrayList<NodeEvent>()
                node.subscribe(object : EventListener {
                    override fun onEvent(event: NodeEvent) {
                        events.add(event)
                    }
                })
                assertEquals(device, node.status().device)
                assertFalse(node.status().revoked)
                val offer = paired.folders.single()
                assertEquals(folderId, offer.id)
                assertEquals("Photos", offer.name)
                val phoneFolder = root.resolve("phone-files/Photos")
                node.joinFolder(
                    offer.id,
                    phoneFolder.path,
                    10_000_000uL,
                    offer.members,
                    offer.retentionDays,
                    SyncOptions(Direction.BOTH, LocalDeletes.EVERYWHERE),
                )

                eventually("the phone lists the photo") { node.files(offer.id).size == 1 }
                val placeholder = node.files(offer.id).single()
                assertEquals("beach.jpg", placeholder.path)
                assertFalse(placeholder.onDisk)
                val local = node.open(offer.id, "beach.jpg")
                assertContentEquals("sand and sea".toByteArray(), JFile(local).readBytes())
                node.close(offer.id, "beach.jpg")
                assertFailsWith<TidefellException.NotFound> { node.open(offer.id, "nope.jpg") }

                // A photo taken on the phone reaches the laptop.
                val source = root.resolve("camera.tmp").apply { writeText("new photo") }
                node.write(offer.id, "camera/new.jpg", source.path)
                eventually("the laptop has the phone's photo") {
                    laptopFolder.resolve("camera/new.jpg").let { it.exists() && it.readText() == "new photo" }
                }
                // The phone's activity, newest first: its own photo, then the laptop's (ADR 0030).
                val activity = node.activity(offer.id, 10u)
                assertEquals(listOf("camera/new.jpg", "beach.jpg"), activity.map { it.path })
                assertEquals(listOf(true, false), activity.map { it.local })
                assertEquals(listOf(ChangeKind.ADDED, ChangeKind.ADDED), activity.map { it.kind })
                assertEquals(placeholder.author, activity.last().device)
                eventually("events") {
                    events.any { it is NodeEvent.FileUpdated && it.path == "beach.jpg" }
                }

                node.shutdown()
                node.close()
            } finally {
                daemon.destroy()
                daemon.waitFor(10, TimeUnit.SECONDS)
            }
        } finally {
            root.deleteRecursively()
        }
    }
}
