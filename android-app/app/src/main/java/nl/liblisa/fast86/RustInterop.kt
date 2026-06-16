package nl.liblisa.sem86

import android.content.res.AssetManager
import android.graphics.SurfaceTexture
import android.os.Build
import android.view.Surface

object RustInterop {
    init {
        println("Supported ABIs: ${Build.SUPPORTED_ABIS.joinToString()}")
        System.loadLibrary("android_bridge")

        init()
    }

    @JvmStatic
    external fun test(): Long

    @JvmStatic
    external fun init()

    @JvmStatic
    external fun setAssetManager(am: AssetManager)

    @JvmStatic
    external fun startEmulation(ide0_0_fd: Int, ide1_0_fd: Int, ide1_0_is_cd: Boolean, memorySizeMb: Int, resumeFromFd: Int): Long

    @JvmStatic
    external fun stopEmulation(emulatorIndex: Long, snapshotFd: Int)

    @JvmStatic
    external fun connectSurface(emulatorIndex: Long, surface: Surface)

    @JvmStatic
    external fun resizeSurface(emulatorIndex: Long, width: Int, height: Int)

    @JvmStatic
    external fun dropSurface(emulatorIndex: Long)

    @JvmStatic
    external fun mouseMove(emulatorIndex: Long, dx: Double, dy: Double)

    @JvmStatic
    external fun mouseScroll(emulatorIndex: Long, dz: Double)

    @JvmStatic
    external fun mouseButtonState(emulatorIndex: Long, left: Boolean, right: Boolean)

    @JvmStatic
    external fun render(emulatorIndex: Long)

    @JvmStatic
    external fun keyboardInput(emulatorIndex: Long, isDown: Boolean, keyCode: Int)
}