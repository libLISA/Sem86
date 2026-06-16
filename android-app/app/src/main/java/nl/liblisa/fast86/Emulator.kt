package nl.liblisa.sem86

import android.content.Context
import android.os.ParcelFileDescriptor
import android.system.Os
import android.system.OsConstants
import kotlinx.serialization.encodeToString
import kotlinx.serialization.json.Json
import java.io.File

class Emulator(val configDir: File, newConfig: EmulatorConfig? = null) {
    val config: EmulatorConfig
    var runningEmulator: RunningEmulator? = null

    init {
        if (newConfig != null) {
            config = newConfig
            save()
        } else {
            val configFile = configDir.resolve("config.json");
            val json = configFile.readText()
            config = Json.decodeFromString<EmulatorConfig>(json)
        }
    }

    fun save() {
        val file = File(configDir, "config.json")
        val json = Json.encodeToString(config)
        file.writeText(json)
    }

    private fun snapshotFile(): File {
        return File(configDir, "save.snapshot")
    }

    fun hasSnapshot(): Boolean {
        return snapshotFile().exists()
    }

    fun removeSnapshot() {
        snapshotFile().delete()
    }

    fun start(context: Context) {
        val snapshotPath = snapshotFile()
        var resumeFromFd = -1
        if (snapshotPath.exists()) {
            resumeFromFd = ParcelFileDescriptor.dup(Os.open(snapshotPath.canonicalPath, OsConstants.O_RDONLY, 0)).detachFd()
        }

        runningEmulator = RunningEmulator(config, context, resumeFromFd)
    }

    fun stop() {
        println("Stopping emulator and saving snapshot...")
        runningEmulator?.let {
            val snapshotPath = snapshotFile()
            val fd = Os.open(
                snapshotPath.canonicalPath,
                OsConstants.O_CREAT or OsConstants.O_TRUNC or OsConstants.O_WRONLY,
                420
            )
            val snapshotFd = ParcelFileDescriptor.dup(fd).detachFd()

            println("snapshotFd = $snapshotFd")

            it.stopAndSnapshot(snapshotFd)
        }

        runningEmulator = null
    }

    fun delete() {
        configDir.deleteRecursively()
    }

    fun mouseMove(dx: kotlin.Double, dy: kotlin.Double) {
        runningEmulator?.let {
            RustInterop.mouseMove(it.emulatorIndex, dx, dy)
        }
    }

    fun mouseScroll(dz: Double) {
        runningEmulator?.let {
            RustInterop.mouseScroll(it.emulatorIndex, dz)
        }
    }

    fun mouseButtonState(left: Boolean, right: Boolean) {
        runningEmulator?.let {
            RustInterop.mouseButtonState(it.emulatorIndex, left, right)
        }
    }

    fun render() {
        runningEmulator?.let {
            RustInterop.render(it.emulatorIndex)
        }
    }

    fun dropSurface() {
        runningEmulator?.let {
            RustInterop.dropSurface(it.emulatorIndex)
        }
    }
}