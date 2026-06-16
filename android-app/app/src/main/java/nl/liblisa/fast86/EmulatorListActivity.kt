package nl.liblisa.sem86

import android.content.Intent
import android.net.Uri
import android.os.Bundle
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.foundation.layout.*
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.itemsIndexed
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.filled.Edit
import androidx.compose.material.icons.filled.PlayArrow
import androidx.compose.material3.*
import androidx.compose.runtime.*
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.input.nestedscroll.nestedScroll
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.platform.LocalLayoutDirection
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import androidx.core.view.WindowInsetsControllerCompat
import nl.liblisa.sem86.ui.theme.sem86Theme
import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.compose.animation.ExperimentalAnimationApi
import androidx.compose.animation.slideInVertically
import androidx.compose.animation.slideOutVertically
import androidx.compose.foundation.clickable
import androidx.compose.material.icons.filled.AddCircle
import androidx.compose.material.icons.filled.Clear
import androidx.compose.material.icons.filled.Delete
import androidx.navigation.NavController
import androidx.navigation.compose.NavHost
import androidx.navigation.compose.composable
import androidx.navigation.compose.rememberNavController
import androidx.navigation.NavType
import androidx.navigation.navArgument
import androidx.core.net.toUri
import kotlin.math.roundToInt

class EmulatorListActivity : ComponentActivity() {
    private lateinit var repository: EmulatorRepository

    @OptIn(ExperimentalAnimationApi::class)
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        repository = EmulatorRepository.getDefault(this)

        val wic = WindowInsetsControllerCompat(window, window.decorView)
        wic.isAppearanceLightStatusBars = true

        setContent {
            sem86Theme {
                Surface(modifier = Modifier.fillMaxSize()) {
                    val navController = rememberNavController()
                    NavHost(
                        navController = navController,
                        startDestination = "configList",
                        enterTransition = { slideInVertically(initialOffsetY = { it }) },
                        exitTransition = { slideOutVertically(targetOffsetY = { it }) }
                    ) {
                        composable("configList") {
                            EmulatorListScreen(navController, repository)
                        }

                        composable(
                            "editConfig/{emulatorIndex}",
                            arguments = listOf(navArgument("emulatorIndex") { type = NavType.IntType })
                        ) { backStackEntry ->
                            val emulatorIndex = backStackEntry.arguments?.getInt("emulatorIndex") ?: -1
                            val emulator = if (emulatorIndex >= 0) {
                                repository.getEmulatorAt(emulatorIndex)
                            } else {
                                null
                            }
                            val config = emulator?.config ?: EmulatorConfig(name = "")
                            AddEditEmulatorConfigScreen(
                                emulator = emulator,
                                config = config,
                                onSave = { updatedConfig ->
                                    navController.popBackStack()
                                    if (emulatorIndex != -1) {
                                        repository.getEmulatorAt(emulatorIndex).save()
                                    } else {
                                        repository.add(updatedConfig)
                                    }
                                },
                                onDelete = {
                                    navController.popBackStack()
                                    repository.removeAt(emulatorIndex)
                                },
                                onCancel = {
                                    navController.popBackStack() // Go back without saving
                                }
                            )
                        }
                    }
                }
            }
        }
    }
}

@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun EmulatorListScreen(navController: NavController, repository: EmulatorRepository) {
    // Observe configs as state
    var configs by remember { mutableStateOf(repository.getEmulators()) }
    val layoutDirection = LocalLayoutDirection.current

    val scrollBehavior = TopAppBarDefaults.enterAlwaysScrollBehavior()

    Scaffold(
        topBar = {
            TopAppBar(
                title = { Text("Emulators") },
                scrollBehavior = scrollBehavior
            )
        },
        floatingActionButton = {
            FloatingActionButton(onClick = {
                navController.navigate("editConfig/-1")
            }) {
                Text("+", fontSize = 24.sp)
            }
        }
    ) { innerPadding ->
        val context = LocalContext.current
        LazyColumn(
            contentPadding = PaddingValues(
                start = innerPadding.calculateLeftPadding(layoutDirection),
                top = innerPadding.calculateTopPadding(),
                end = innerPadding.calculateRightPadding(layoutDirection),
                bottom = innerPadding.calculateBottomPadding() + 72.dp // To make sure the + button doesn't obscure the bottom list entry
            ),
            modifier = Modifier
                .fillMaxSize()
                .padding(16.dp)
                .nestedScroll(scrollBehavior.nestedScrollConnection),
        ) {
            itemsIndexed(configs) { index, emulator ->
                EmulatorItem(config = emulator.config, onRunClick = {
                    val intent = Intent(context, EmulatorActivity::class.java)
                    intent.putExtra("emulator", emulator.configDir.name)
                    context.startActivity(intent)
                }, onEditClick = {
                    navController.navigate("editConfig/${index}") // pass the config ID
                })
                HorizontalDivider(modifier = Modifier.padding(vertical = 8.dp))
            }
        }
    }
}

@Composable
fun EmulatorItem(config: EmulatorConfig, onRunClick: () -> Unit, onEditClick: () -> Unit) {
    Row(
        modifier = Modifier
            .fillMaxWidth()
            .padding(vertical = 8.dp)
            .clickable {
                onEditClick()
            },
        verticalAlignment = Alignment.CenterVertically
    ) {
        Column(
            modifier = Modifier.weight(1f)
        ) {
            Text(text = config.name, style = MaterialTheme.typography.titleMedium)
            Text(
                text = "${config.memoryInMb} MiB RAM",
                style = MaterialTheme.typography.bodySmall
            )
        }

        // Edit button
        IconButton(onClick = onEditClick) {
            Icon(
                imageVector = Icons.Default.Edit,
                contentDescription = "Edit"
            )
        }

        // Run button
        IconButton(onClick = onRunClick) {
            Icon(
                imageVector = Icons.Default.PlayArrow,
                contentDescription = "Run"
            )
        }
    }
}

@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun AddEditEmulatorConfigScreen(
    emulator: Emulator?,
    config: EmulatorConfig,
    onSave: (EmulatorConfig) -> Unit,
    onDelete: () -> Unit,
    onCancel: () -> Unit
) {
    var snapshotExists by remember(emulator) {
        mutableStateOf(emulator?.hasSnapshot() == true)
    }

    var name by remember { mutableStateOf(config.name) }
    var ide0Uri by remember { mutableStateOf(config.ide0FileUriStr) }
    var ide1Uri by remember { mutableStateOf(config.ide1FileUriStr) }
    var memory by remember { mutableIntStateOf(config.memoryInMb) }

    val layoutDirection = LocalLayoutDirection.current
    val scrollBehavior = TopAppBarDefaults.enterAlwaysScrollBehavior()
    Scaffold(
        topBar = {
            TopAppBar(
                title = { Text("Edit emulator") },
                scrollBehavior = scrollBehavior
            )
        },
        floatingActionButton = {
            FloatingActionButton(onClick = {
                onDelete()
            },
                containerColor = MaterialTheme.colorScheme.error,
                contentColor = MaterialTheme.colorScheme.onError
            ) {
                Icon(Icons.Default.Delete, contentDescription = "Delete")
            }
        }
    ) { innerPadding ->
        LazyColumn(
            contentPadding = PaddingValues(
                start = innerPadding.calculateLeftPadding(layoutDirection),
                top = innerPadding.calculateTopPadding(),
                end = innerPadding.calculateRightPadding(layoutDirection),
                bottom = innerPadding.calculateBottomPadding() + 72.dp // To make sure the + button doesn't obscure the bottom list entry
            ),
            modifier = Modifier
                .fillMaxSize()
                .padding(16.dp)
                .nestedScroll(scrollBehavior.nestedScrollConnection),
        ) {
            item {
                OutlinedTextField(
                    value = name,
                    onValueChange = { name = it },
                    label = { Text("Name") },
                    modifier = Modifier.fillMaxWidth()
                )
            }

            item {
                FilePickerRow("IDE0", ide0Uri?.toUri()) { ide0Uri = it?.toString() }
            }

            item {
                FilePickerRow("IDE1", ide1Uri?.toUri()) { ide1Uri = it?.toString() }
            }

            item {
                val allowedMemoryValues =
                    listOf(1, 2, 4, 8, 16, 32, 64, 128, 256, 512, 1024, 2048)
                var memoryIndex by remember { mutableIntStateOf(allowedMemoryValues.indexOfFirst { v -> memory == v }) }
                Text("Memory: ${memory.toInt()} MB")
                Slider(
                    value = memoryIndex.toFloat(),
                    onValueChange = {
                        memoryIndex = it.roundToInt()
                        memory = allowedMemoryValues[memoryIndex.toInt()]
                    },
                    valueRange = 0f..(allowedMemoryValues.size - 1).toFloat(),
                    steps = allowedMemoryValues.size - 2
                )
            }

            item {
                Spacer(modifier = Modifier.height(16.dp))
            }

            item {
                Row(
                    modifier = Modifier.fillMaxWidth(),
                    horizontalArrangement = Arrangement.SpaceEvenly
                ) {
                    Button(onClick = {
                        config.name = name
                        config.ide0FileUriStr = ide0Uri
                        config.ide1FileUriStr = ide1Uri
                        config.memoryInMb = memory.toInt()
                        onSave(config)
                    }) {
                        Text("Save")
                    }

                    Button(onClick = { onCancel() }) {
                        Text("Cancel")
                    }
                }
            }

            if (snapshotExists) {
                item {
                    Spacer(modifier = Modifier.height(24.dp))
                    TextButton(
                        onClick = {
                            emulator?.removeSnapshot()
                            snapshotExists = false
                        },
                        modifier = Modifier.fillMaxWidth(),
                        colors = ButtonDefaults.textButtonColors(contentColor = MaterialTheme.colorScheme.error)
                    ) {
                        Icon(imageVector = Icons.Default.Delete, contentDescription = "Delete")
                        Spacer(modifier = Modifier.width(8.dp))
                        Text("Delete Snapshot")
                    }
                }
            }
        }
    }
}


@Composable
fun FilePickerRow(
    label: String,
    fileUri: Uri?,
    onFilePicked: (Uri?) -> Unit
) {
    val contentResolver = LocalContext.current.contentResolver
    val launcher = rememberLauncherForActivityResult(
        contract = ActivityResultContracts.OpenDocument(),
        onResult = { uri ->
            uri?.let {
                contentResolver.takePersistableUriPermission(
                    uri,
                    Intent.FLAG_GRANT_READ_URI_PERMISSION or
                            Intent.FLAG_GRANT_WRITE_URI_PERMISSION
                )

                onFilePicked(uri)
            }
        }
    )

    Row(
        modifier = Modifier
            .fillMaxWidth()
            .padding(vertical = 4.dp),
        verticalAlignment = Alignment.CenterVertically
    ) {
        Text(
            text = label,
            modifier = Modifier.width(80.dp)
        )

        Text(
            text = fileUri?.let {
                it.getFileName(LocalContext.current)
            } ?: "No file selected",
            modifier = Modifier
                .weight(1f)
                .padding(horizontal = 8.dp)
                .clickable(enabled = fileUri == null) {
                    launcher.launch(arrayOf("*/*"))
                }
        )

        IconButton(onClick = {
            if (fileUri != null) {
                onFilePicked(null)
            } else {
                launcher.launch(arrayOf("*/*"))
            }
        }) {
            if (fileUri != null) {
                Icon(Icons.Default.Clear, contentDescription = "Remove file")
            } else {
                Icon(Icons.Default.AddCircle, contentDescription = "Select file")
            }
        }
    }
}