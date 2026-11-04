// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.nexoryx.pasteget

import android.Manifest
import android.content.ClipboardManager
import android.content.Intent
import android.content.pm.PackageManager
import android.net.Uri
import android.os.Build
import android.os.Bundle
import android.provider.Settings
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.activity.enableEdgeToEdge
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.foundation.isSystemInDarkTheme
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.safeDrawingPadding
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.Button
import androidx.compose.material3.Card
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.Surface
import androidx.compose.material3.Switch
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.material3.darkColorScheme
import androidx.compose.material3.dynamicDarkColorScheme
import androidx.compose.material3.dynamicLightColorScheme
import androidx.compose.material3.lightColorScheme
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.res.pluralStringResource
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.unit.dp
import androidx.core.content.ContextCompat
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import androidx.lifecycle.lifecycleScope
import com.journeyapps.barcodescanner.ScanContract
import com.journeyapps.barcodescanner.ScanOptions
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext
import uniffi.pasteget_ffi.PairedComputer
import uniffi.pasteget_ffi.PastegetException

class MainActivity : ComponentActivity() {
    private var computers by mutableStateOf(emptyList<PairedComputer>())
    private var message by mutableStateOf<String?>(null)
    private var screenshots by mutableStateOf(ScreenshotSetting(wanted = false, access = PhotoAccess.NONE))
    private var photoRequestDenied by mutableStateOf(false)

    private val photoPermission =
        registerForActivityResult(ActivityResultContracts.RequestMultiplePermissions()) {
            val granted = PhotoAccess.of(this) == PhotoAccess.FULL
            photoRequestDenied = !granted
            setSendScreenshots(granted)
        }

    private val scanner = registerForActivityResult(ScanContract()) { result ->
        result.contents?.let(::pair)
    }

    private val notificationPermission =
        registerForActivityResult(ActivityResultContracts.RequestPermission()) {
            // The service runs either way; without the permission its notification is hidden.
            SyncService.start(this)
        }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        enableEdgeToEdge()
        computers = pasteGet.client.pairedComputers()
        setContent {
            PasteGetTheme {
                val connected by pasteGet.connected.collectAsStateWithLifecycle()
                HomeScreen(
                    deviceId = pasteGet.client.deviceId(),
                    computers = computers,
                    connected = connected,
                    message = message,
                    onPair = ::scan,
                    onSendClipboard = ::sendClipboard,
                    onForget = ::forget,
                    screenshots = screenshots,
                    photoRequestDenied = photoRequestDenied,
                    onScreenshotsChange = ::toggleScreenshots,
                    onOpenSettings = ::openAppSettings,
                )
            }
        }
        startSyncService()
        handlePairingLink(intent)
    }

    /** Photo access may have changed in the system settings meanwhile. */
    override fun onResume() {
        super.onResume()
        refreshScreenshots()
    }

    override fun onNewIntent(intent: Intent) {
        super.onNewIntent(intent)
        handlePairingLink(intent)
    }

    /** A pairing QR code scanned with the system camera opens the app with a pasteget:// link. */
    private fun handlePairingLink(intent: Intent?) {
        PairingCode.extract(intent?.dataString)?.let(::pair)
    }

    private fun startSyncService() {
        val needsPermission = Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU &&
            ContextCompat.checkSelfPermission(this, Manifest.permission.POST_NOTIFICATIONS) !=
            PackageManager.PERMISSION_GRANTED
        if (needsPermission) {
            notificationPermission.launch(Manifest.permission.POST_NOTIFICATIONS)
        } else {
            SyncService.start(this)
        }
    }

    private fun scan() {
        scanner.launch(
            ScanOptions()
                .setDesiredBarcodeFormats(ScanOptions.QR_CODE)
                .setPrompt(getString(R.string.scan_prompt))
                .setBeepEnabled(false)
                .setOrientationLocked(false),
        )
    }

    private fun pair(scanned: String) {
        val code = PairingCode.extract(scanned)
        if (code == null) {
            message = getString(R.string.not_a_pairing_code)
            return
        }
        message = getString(R.string.pairing)
        lifecycleScope.launch {
            message = try {
                val computer = withContext(Dispatchers.IO) { pasteGet.client.pair(code) }
                computers = pasteGet.client.pairedComputers()
                SyncService.start(this@MainActivity)
                getString(R.string.paired_with, computer.name)
            } catch (e: PastegetException) {
                e.message
            }
        }
    }

    private fun forget(computer: PairedComputer) {
        try {
            pasteGet.client.forget(computer.deviceId)
            pasteGet.setConnected(computer.deviceId, false)
        } catch (e: PastegetException) {
            message = e.message
        }
        computers = pasteGet.client.pairedComputers()
    }

    private fun refreshScreenshots() {
        screenshots = ScreenshotSetting(Preferences.sendScreenshots(this), PhotoAccess.of(this))
    }

    private fun toggleScreenshots(on: Boolean) {
        if (on && PhotoAccess.of(this) != PhotoAccess.FULL) {
            photoPermission.launch(PhotoAccess.permissions())
        } else {
            setSendScreenshots(on)
        }
    }

    private fun setSendScreenshots(on: Boolean) {
        Preferences.setSendScreenshots(this, on)
        refreshScreenshots()
        // The service picks up the change.
        SyncService.start(this)
    }

    private fun openAppSettings() {
        startActivity(Intent(Settings.ACTION_APPLICATION_DETAILS_SETTINGS, Uri.fromParts("package", packageName, null)))
    }

    /** This activity has focus, so it may read the clipboard directly. */
    private fun sendClipboard() {
        val clip = getSystemService(ClipboardManager::class.java).primaryClip
        lifecycleScope.launch { message = Sender.send(this@MainActivity, clip) }
    }
}

@Composable
private fun PasteGetTheme(content: @Composable () -> Unit) {
    val context = LocalContext.current
    val dark = isSystemInDarkTheme()
    // Material You colors exist from Android 12.
    val colors = when {
        Build.VERSION.SDK_INT >= Build.VERSION_CODES.S && dark -> dynamicDarkColorScheme(context)
        Build.VERSION.SDK_INT >= Build.VERSION_CODES.S -> dynamicLightColorScheme(context)
        dark -> darkColorScheme()
        else -> lightColorScheme()
    }
    MaterialTheme(colorScheme = colors) {
        Surface(modifier = Modifier.fillMaxSize(), content = content)
    }
}

/** Whether the user wants screenshots sent, and whether the app can see them. */
private data class ScreenshotSetting(val wanted: Boolean, val access: PhotoAccess) {
    val on: Boolean
        get() = wanted && access == PhotoAccess.FULL
}

@Composable
private fun HomeScreen(
    deviceId: String,
    computers: List<PairedComputer>,
    connected: Set<String>,
    message: String?,
    onPair: () -> Unit,
    onSendClipboard: () -> Unit,
    onForget: (PairedComputer) -> Unit,
    screenshots: ScreenshotSetting,
    photoRequestDenied: Boolean,
    onScreenshotsChange: (Boolean) -> Unit,
    onOpenSettings: () -> Unit,
) {
    Column(
        modifier = Modifier
            .safeDrawingPadding()
            .verticalScroll(rememberScrollState())
            .padding(24.dp),
        verticalArrangement = Arrangement.spacedBy(16.dp),
    ) {
        Text(stringResource(R.string.app_name), style = MaterialTheme.typography.headlineLarge)
        Text(
            text = when {
                computers.isEmpty() -> stringResource(R.string.status_no_computer)
                connected.isEmpty() -> stringResource(R.string.notification_connecting)
                else -> pluralStringResource(
                    R.plurals.notification_connected,
                    connected.size,
                    connected.size,
                )
            },
            style = MaterialTheme.typography.titleMedium,
        )

        Button(onClick = onPair, modifier = Modifier.fillMaxWidth()) {
            Text(stringResource(R.string.pair_button))
        }
        OutlinedButton(
            onClick = onSendClipboard,
            enabled = connected.isNotEmpty(),
            modifier = Modifier.fillMaxWidth(),
        ) {
            Text(stringResource(R.string.send_clipboard_button))
        }
        message?.let { Text(it, style = MaterialTheme.typography.bodyMedium) }

        ScreenshotsCard(screenshots, photoRequestDenied, onScreenshotsChange, onOpenSettings)

        if (computers.isNotEmpty()) {
            Text(stringResource(R.string.paired_computers), style = MaterialTheme.typography.titleSmall)
        }
        computers.forEach { computer ->
            Card(modifier = Modifier.fillMaxWidth()) {
                Row(
                    modifier = Modifier.padding(start = 16.dp, end = 8.dp, top = 8.dp, bottom = 8.dp),
                    verticalAlignment = Alignment.CenterVertically,
                ) {
                    val isConnected = computer.deviceId in connected
                    Column(modifier = Modifier.weight(1f)) {
                        Text(computer.name, style = MaterialTheme.typography.bodyLarge)
                        Text(
                            stringResource(if (isConnected) R.string.connected else R.string.offline),
                            style = MaterialTheme.typography.bodySmall,
                        )
                    }
                    TextButton(onClick = { onForget(computer) }) {
                        Text(stringResource(R.string.forget))
                    }
                }
            }
        }

        Spacer(modifier = Modifier.padding(8.dp))
        Text(
            text = stringResource(R.string.this_phone, deviceId.take(12)),
            style = MaterialTheme.typography.bodySmall,
        )
    }
}

@Composable
private fun ScreenshotsCard(
    setting: ScreenshotSetting,
    photoRequestDenied: Boolean,
    onChange: (Boolean) -> Unit,
    onOpenSettings: () -> Unit,
) {
    val problem = when {
        setting.access == PhotoAccess.PARTIAL -> R.string.screenshots_partial
        setting.access == PhotoAccess.NONE && (setting.wanted || photoRequestDenied) -> R.string.screenshots_denied
        else -> null
    }
    Card(modifier = Modifier.fillMaxWidth()) {
        Column(modifier = Modifier.padding(16.dp), verticalArrangement = Arrangement.spacedBy(8.dp)) {
            Row(verticalAlignment = Alignment.CenterVertically) {
                Column(modifier = Modifier.weight(1f)) {
                    Text(stringResource(R.string.screenshots_title), style = MaterialTheme.typography.bodyLarge)
                    Text(
                        stringResource(if (setting.on) R.string.screenshots_on else R.string.screenshots_off),
                        style = MaterialTheme.typography.bodySmall,
                    )
                }
                Switch(checked = setting.on, onCheckedChange = onChange)
            }
            if (problem != null) {
                Text(stringResource(problem), style = MaterialTheme.typography.bodySmall)
                TextButton(onClick = onOpenSettings) { Text(stringResource(R.string.open_settings)) }
            }
        }
    }
}
