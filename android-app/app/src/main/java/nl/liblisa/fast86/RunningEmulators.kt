package nl.liblisa.sem86

class RunningEmulators {
    private val runningEmulators: HashMap<String, RunningEmulator> = hashMapOf()

    fun startEmulator(file: String, config: EmulatorConfig): RunningEmulator {
        TODO()
    }

    fun getRunningEmulator(filename: String): RunningEmulator? {
        TODO()
    }

    fun stopEmulator(filename: String): RunningEmulator? {
        TODO()
    }
}