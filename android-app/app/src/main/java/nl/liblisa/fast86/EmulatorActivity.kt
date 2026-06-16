package nl.liblisa.sem86

import android.app.Activity
import android.content.ComponentCallbacks2
import android.content.res.Configuration
import android.os.Bundle
import android.view.WindowManager
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.WindowInsets
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.ime
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.systemBars
import androidx.compose.foundation.layout.union
import androidx.compose.material3.Scaffold
import androidx.compose.runtime.Composable
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalConfiguration
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.platform.LocalView
import androidx.compose.ui.viewinterop.AndroidView
import androidx.core.view.WindowCompat
import androidx.core.view.WindowInsetsCompat
import androidx.core.view.WindowInsetsControllerCompat
import androidx.lifecycle.Lifecycle
import androidx.lifecycle.LifecycleEventObserver
import androidx.lifecycle.compose.LocalLifecycleOwner
import nl.liblisa.sem86.ui.theme.sem86Theme

class EmulatorActivity : ComponentActivity() {
    lateinit var emulator: Emulator

    override fun onLowMemory() {
        super.onLowMemory()

        println("!! LOW MEMORY !!")
    }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)

        RustInterop.setAssetManager(assets)

        val emulatorName = intent.getStringExtra("emulator")
        emulator = EmulatorRepository.getDefault(this).getEmulator(emulatorName!!)!!

        window.addFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON)
        window.setSoftInputMode(
            WindowManager.LayoutParams.SOFT_INPUT_ADJUST_RESIZE
        )

        val context = this
        setContent {
            sem86Theme {
                var isEmulatorRunning = remember { mutableStateOf(true) }

                val lifecycleOwner = LocalLifecycleOwner.current

                DisposableEffect(lifecycleOwner) {
                    val observer = LifecycleEventObserver { _, event ->
                        when (event) {
                            Lifecycle.Event.ON_PAUSE -> {
                                emulator.stop()
                                isEmulatorRunning.value = false
                            }
                            Lifecycle.Event.ON_RESUME -> {
                                emulator.start(context)
                                isEmulatorRunning.value = true
                            }
                            else -> {}
                        }
                    }

                    lifecycleOwner.lifecycle.addObserver(observer)

                    onDispose {
                        lifecycleOwner.lifecycle.removeObserver(observer)
                    }
                }

                HideSystemBarsInPortrait()

                DisposableEffect(Unit) {
                    onDispose {
                        emulator.stop()
                    }
                }

                Scaffold(modifier = Modifier.fillMaxSize(),
                    contentWindowInsets = WindowInsets
                        .systemBars
                        .union(WindowInsets.ime)
                ) { innerPadding ->
                    Column (
                        modifier = Modifier
                            .fillMaxSize()
                            .padding(innerPadding)
                    ) {
                        AndroidView(
                            factory = { context ->
                                EmulatorView(context, emulator)
                            }, modifier = Modifier
                                .fillMaxSize()
                        )
                    }
                }
            }
        }
    }
}

@Composable
fun HideSystemBarsInPortrait() {
    val context = LocalContext.current
    val activity = context as Activity
    val view = LocalView.current

    val configuration = LocalConfiguration.current
    val isLandscape = configuration.orientation == Configuration.ORIENTATION_LANDSCAPE

    DisposableEffect(isLandscape) {
        val window = activity.window
        val controller = WindowCompat.getInsetsController(window, view)

        if (isLandscape) {
            controller.systemBarsBehavior =
                WindowInsetsControllerCompat.BEHAVIOR_SHOW_TRANSIENT_BARS_BY_SWIPE
            controller.hide(WindowInsetsCompat.Type.systemBars())
        } else {
            controller.show(WindowInsetsCompat.Type.systemBars())
        }

        onDispose {
            // Always restore when leaving composition
            controller.show(WindowInsetsCompat.Type.systemBars())
        }
    }
}