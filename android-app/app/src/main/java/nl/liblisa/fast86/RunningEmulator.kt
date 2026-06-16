package nl.liblisa.sem86

import android.content.Context
import android.net.Uri
import android.provider.OpenableColumns

fun Uri.getFileName(context: Context): String? {
    try {
        val cursor = context.contentResolver.query(
            this,
            arrayOf(OpenableColumns.DISPLAY_NAME),
            null,
            null,
            null
        )

        cursor?.use {
            if (it.moveToFirst()) {
                return it.getString(it.getColumnIndexOrThrow(OpenableColumns.DISPLAY_NAME))
            }
        }
    } catch (e: Exception) {
        println("Failed to access file: ${e}")
    }

    return null
}

class RunningEmulator(config: EmulatorConfig, context: Context, resumeFromFd: Int) {
    val emulatorIndex: Long
    init {
        RustInterop.setAssetManager(context.assets)

        val contentResolver = context.contentResolver
        val fd1IsCd = config.getIde1FileUri()?.let { uri -> uri.getFileName(context)?.endsWith(".iso") } ?: false

        val pfd0 = config.getIde0FileUri()?.let { uri -> contentResolver.openFileDescriptor(uri, "rw") }
        val pfd1 = config.getIde1FileUri()?.let { uri -> contentResolver.openFileDescriptor(uri, if (fd1IsCd) "r" else "rw") }

        val fd0 = pfd0?.detachFd()
        val fd1 = pfd1?.detachFd()

        println("fd0=$fd0, fd1=$fd1, fd1IsCd=$fd1IsCd, resumeFromFd=$resumeFromFd")

        emulatorIndex = RustInterop.startEmulation(fd0 ?: -1, fd1 ?: -1, fd1IsCd, config.memoryInMb, resumeFromFd)
    }

    fun stopAndSnapshot(snapshotFd: Int) {
        RustInterop.stopEmulation(emulatorIndex, snapshotFd)
    }
}