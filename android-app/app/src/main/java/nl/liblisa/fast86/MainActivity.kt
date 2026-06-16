package nl.liblisa.sem86

import android.content.Context
import android.content.Intent
import android.net.Uri
import android.os.Bundle
import android.provider.OpenableColumns
import android.widget.Toast
import androidx.activity.ComponentActivity
import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.activity.compose.setContent
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.foundation.layout.*
import androidx.compose.material3.Button
import androidx.compose.material3.Text
import androidx.compose.runtime.*
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.unit.dp

fun Context.saveUri(key: String, uri: Uri?) {
    val prefs = getSharedPreferences("uris", Context.MODE_PRIVATE)
    prefs.edit().putString(key, uri?.toString()).apply()
}

fun Context.loadUri(key: String): Uri? {
    val prefs = getSharedPreferences("uris", Context.MODE_PRIVATE)
    val str = prefs.getString(key, null) ?: return null
    return Uri.parse(str)
}

class MainActivity : ComponentActivity() {
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        setContent {
            FilePickerScreen()
        }
    }

    @Composable
    fun FilePickerScreen() {
        val context = LocalContext.current
        var ide0FileUri by remember { mutableStateOf<Uri?>(null) }
        var ide1FileUri by remember { mutableStateOf<Uri?>(null) }

        LaunchedEffect(Unit) {
            ide0FileUri = context.loadUri("ide0")
            ide1FileUri = context.loadUri("ide1")
        }

        // Launcher for IDE0:0
        val ide0Launcher = rememberLauncherForActivityResult(
            contract = ActivityResultContracts.OpenDocument(),
            onResult = { uri ->
                uri?.let {
                    contentResolver.takePersistableUriPermission(
                        uri,
                        Intent.FLAG_GRANT_READ_URI_PERMISSION or
                                Intent.FLAG_GRANT_WRITE_URI_PERMISSION
                    )

                    ide0FileUri = uri
                    context.saveUri("ide0", it)
                }
            }
        )

        // Launcher for IDE1:0
        val ide1Launcher = rememberLauncherForActivityResult(
            contract = ActivityResultContracts.OpenDocument(),
            onResult = { uri ->
                uri?.let {
                    contentResolver.takePersistableUriPermission(
                        uri,
                        Intent.FLAG_GRANT_READ_URI_PERMISSION or
                                Intent.FLAG_GRANT_WRITE_URI_PERMISSION
                    )

                    ide1FileUri = uri
                    context.saveUri("ide1", it)
                }
            }
        )

        // UI
        Box(
            modifier = Modifier.fillMaxSize(),
            contentAlignment = Alignment.Center
        ) {
            Column(horizontalAlignment = Alignment.CenterHorizontally) {
                Button(onClick = { ide0Launcher.launch(arrayOf("*/*")) }) {
                    Text("Pick file for IDE0:0")
                }

                Spacer(modifier = Modifier.height(16.dp))

                Button(onClick = { ide1Launcher.launch(arrayOf("*/*")) }) {
                    Text("Pick file for IDE1:0")
                }

                Spacer(modifier = Modifier.height(32.dp))

                ide0FileUri?.let {
                    val fileName = it.getFileName(LocalContext.current)
                    Text("IDE0:0 selected: $fileName")
                }
                ide1FileUri?.let {
                    val fileName = it.getFileName(LocalContext.current)
                    Text("IDE1:0 selected: $fileName")
                }

                Button(
                    onClick = {
                        val intent = Intent(this@MainActivity, EmulatorActivity::class.java)
                        intent.putExtra("ide0_0", ide0FileUri)
                        intent.putExtra("ide1_0", ide1FileUri)
                        startActivity(intent)
                    },
                    modifier = Modifier.fillMaxWidth()
                ) {
                    Text("Run")
                }
            }
        }
    }
}