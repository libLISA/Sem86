package nl.liblisa.sem86

import android.content.Context
import java.io.File

class EmulatorRepository private constructor(context: Context) {
    companion object {
        private var current: EmulatorRepository? = null

        fun getDefault(context: Context): EmulatorRepository {
            if (current == null) {
                current = EmulatorRepository(context)
            }

            return current!!
        }
    }

    private val configDir: File = File(context.filesDir, "configurations")
    private val configs: MutableList<Emulator> = mutableListOf()

    init {
        if (!configDir.exists()) {
            configDir.mkdirs()
        }

        loadAllConfigs()
    }

    private fun loadAllConfigs() {
        configs.clear()
        configDir.listFiles { dir -> dir.isDirectory }?.forEach { dir ->
            try {
                configs.add(Emulator(dir))
            } catch (e: Exception) {
                e.printStackTrace()
                // Optionally handle malformed files
            }
        }
    }

    fun getEmulators(): List<Emulator> {
        return configs.toList()
    }

    fun getEmulator(name: String): Emulator? {
        return configs.find { e -> e.configDir.name == name }
    }

    fun add(config: EmulatorConfig) {
        var baseName = config.name
            .lowercase()
            .replace(Regex("[^a-z0-9_-]"), "_")
        var dirname = baseName

        // Ensure filename is unique
        var counter = 1
        while (File(configDir, dirname).exists()) {
            dirname = "${baseName}_$counter"
            counter++
        }

        val configDir = File(configDir, dirname)
        configDir.mkdir()

        configs.add(Emulator(configDir, config))
    }

    fun removeAt(index: Int) {
        val removed = configs.removeAt(index)
        removed.delete()
    }

    fun getEmulatorAt(index: Int): Emulator {
        return configs[index]
    }
}