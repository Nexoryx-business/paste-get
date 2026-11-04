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
import android.text.format.DateFormat
import android.text.format.Formatter
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
import androidx.core.net.toUri
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import androidx.lifecycle.lifecycleScope
import com.journeyapps.barcodescanner.ScanContract
import com.journeyapps.barcodescanner.ScanOptions
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext
import uniffi.pasteget_ffi.PairedComputer
import uniffi.pasteget_ffi.PastegetException
import java.util.Date

class MainActivity : ComponentActivity() {
    private var computers by mutableStateOf(emptyList<PairedComputer>())
    private var message by mutableStateOf<String?>(null)
    private var screenshots by mutableStateOf(ScreenshotSetting(wanted = false, access = PhotoAccess.NONE))
    private var photoRequestDenied by mutableStateOf(false)
    private var copiesOn by mutableStateOf(false)
    private var fallback by mutableStateOf(FallbackAccess(readLogs = false, overlay = false))

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
                val lastDelivery by pasteGet.lastDelivery.collectAsStateWithLifecycle()
                val shizuku by pasteGet.autoCopy.shizuku.state.collectAsStateWithLifecycle()
                val logs by pasteGet.autoCopy.logs.state.collectAsStateWithLifecycle()
                val lastCopy by pasteGet.autoCopy.lastCopy.collectAsStateWithLifecycle()
                val lastReceived by ReceivedClips.last.collectAsStateWithLifecycle()
                HomeScreen(
                    deviceId = pasteGet.client.deviceId(),
                    computers = computers,
                    connected = connected,
                    message = message,
                    onPair = ::scan,
                    onSendClipboard = ::sendClipboard,
                    onForget = ::forget,
                    lastDelivery = lastDelivery,
                    lastReceived = lastReceived,
                    screenshots = screenshots,
                    photoRequestDenied = photoRequestDenied,
                    onScreenshotsChange = ::toggleScreenshots,
                    onOpenSettings = ::openAppSettings,
                    autoCopy = AutoCopyCardState(copiesOn, shizuku, logs, fallback, lastCopy),
                    autoCopyActions = AutoCopyActions(
                        onChange = ::setSendCopies,
                        onAllowShizuku = pasteGet.autoCopy.shizuku::requestPermission,
                        onOpenShizuku = ::openShizuku,
                        onGetShizuku = ::getShizuku,
                        onGrantFallback = ::grantFallback,
                        onAllowOverlay = ::openOverlaySettings,
                        onRetryLogs = pasteGet.autoCopy.logs::retry,
                    ),
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
        copiesOn = Preferences.sendCopies(this)
        fallback = FallbackAccess.of(this)
        // Android 13+ asks for log access only while the app is in front (mode B).
        pasteGet.autoCopy.setInFront(true)
    }

    override fun onPause() {
        pasteGet.autoCopy.setInFront(false)
        super.onPause()
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

    private fun setSendCopies(on: Boolean) {
        Preferences.setSendCopies(this, on)
        copiesOn = on
        // The service picks up the change.
        SyncService.start(this)
    }

    private fun openShizuku() {
        packageManager.getLaunchIntentForPackage(SHIZUKU_PACKAGE)?.let(::startActivity)
    }

    /** Android may restart the app to apply the log permission: the screen then comes back fresh. */
    private fun grantFallback() {
        message = getString(R.string.fallback_granting)
        pasteGet.autoCopy.shizuku.grantFallback { failed ->
            runOnUiThread {
                fallback = FallbackAccess.of(this)
                message = failed?.let { getString(R.string.fallback_failed, it) } ?: getString(R.string.fallback_granted)
            }
        }
    }

    private fun openOverlaySettings() {
        startActivity(Intent(Settings.ACTION_MANAGE_OVERLAY_PERMISSION, Uri.fromParts("package", packageName, null)))
    }

    private fun getShizuku() {
        startActivity(Intent(Intent.ACTION_VIEW, SHIZUKU_DOWNLOAD.toUri()))
    }

    private fun openAppSettings() {
        startActivity(Intent(Settings.ACTION_APPLICATION_DETAILS_SETTINGS, Uri.fromParts("package", packageName, null)))
    }

    /** This activity has focus, so it may read the clipboard directly. */
    private fun sendClipboard() {
        val clip = getSystemService(ClipboardManager::class.java).primaryClip
        lifecycleScope.launch { message = Sender.send(this@MainActivity, clip) }
    }

    private companion object {
        const val SHIZUKU_PACKAGE = "moe.shizuku.privileged.api"
        const val SHIZUKU_DOWNLOAD = "https://shizuku.rikka.app/download/"
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
    lastDelivery: LastDelivery?,
    lastReceived: LastReceived?,
    screenshots: ScreenshotSetting,
    photoRequestDenied: Boolean,
    onScreenshotsChange: (Boolean) -> Unit,
    onOpenSettings: () -> Unit,
    autoCopy: AutoCopyCardState,
    autoCopyActions: AutoCopyActions,
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
        lastDelivery?.let { LastDeliveryText(it) }
        lastReceived?.let { LastReceivedText(it) }
        AutoCopyCard(autoCopy, autoCopyActions)

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

/** How long the last clip took to reach a computer: the latency the roadmap aims to keep under a second. */
@Composable
private fun LastDeliveryText(delivery: LastDelivery) {
    val text = if (delivery.accepted) {
        val size = Formatter.formatShortFileSize(LocalContext.current, delivery.bytes)
        val format = if (delivery.screenshot) R.string.last_screenshot else R.string.last_send
        stringResource(format, size, delivery.millis)
    } else {
        stringResource(R.string.last_refused)
    }
    Text(text, style = MaterialTheme.typography.bodySmall)
}

/** What the automatic copy card shows. */
private data class AutoCopyCardState(
    val on: Boolean,
    val shizuku: ShizukuState,
    val logs: LogStatus,
    val fallback: FallbackAccess,
    val lastCopy: LastCopy?,
)

private class AutoCopyActions(
    val onChange: (Boolean) -> Unit,
    val onAllowShizuku: () -> Unit,
    val onOpenShizuku: () -> Unit,
    val onGetShizuku: () -> Unit,
    val onGrantFallback: () -> Unit,
    val onAllowOverlay: () -> Unit,
    val onRetryLogs: () -> Unit,
)

/** The automatic copy: on or off, how it works right now, and what to do if it doesn't. */
@Composable
private fun AutoCopyCard(state: AutoCopyCardState, actions: AutoCopyActions) {
    Card(modifier = Modifier.fillMaxWidth()) {
        Column(modifier = Modifier.padding(16.dp), verticalArrangement = Arrangement.spacedBy(8.dp)) {
            Row(verticalAlignment = Alignment.CenterVertically) {
                Column(modifier = Modifier.weight(1f)) {
                    Text(stringResource(R.string.copies_title), style = MaterialTheme.typography.bodyLarge)
                    Text(
                        if (state.on) modeText(state) else stringResource(R.string.copies_off),
                        style = MaterialTheme.typography.bodySmall,
                    )
                }
                Switch(checked = state.on, onCheckedChange = actions.onChange)
            }
            if (state.on) {
                // What isn't working yet, and the step that fixes it.
                if (state.shizuku.status != ShizukuStatus.WATCHING) {
                    ShizukuLine(state.shizuku, actions)
                    LogLine(state.logs, actions)
                }
                FallbackLine(state.shizuku, state.fallback, actions)
                state.lastCopy?.let { LastCopyText(it) }
            }
        }
    }
}

/** The health of the automatic copy in one line: how copies are read right now. */
@Composable
private fun modeText(state: AutoCopyCardState): String = when (CopyMode.of(state.shizuku, state.logs)) {
    CopyMode.SHIZUKU -> stringResource(R.string.mode_shizuku, state.shizuku.detail.orEmpty())
    CopyMode.LOGS -> stringResource(R.string.mode_logs)
    CopyMode.MANUAL -> stringResource(R.string.mode_manual)
}

@Composable
private fun ShizukuLine(shizuku: ShizukuState, actions: AutoCopyActions) {
    val text = when (shizuku.status) {
        ShizukuStatus.OFF, ShizukuStatus.CONNECTING -> stringResource(R.string.shizuku_connecting)
        ShizukuStatus.NOT_INSTALLED -> stringResource(R.string.shizuku_not_installed)
        ShizukuStatus.NOT_RUNNING -> stringResource(R.string.shizuku_not_running)
        ShizukuStatus.NO_PERMISSION -> stringResource(R.string.shizuku_no_permission)
        ShizukuStatus.WATCHING -> stringResource(R.string.shizuku_watching, shizuku.detail.orEmpty())
        ShizukuStatus.FAILED -> stringResource(R.string.shizuku_failed, shizuku.detail.orEmpty())
    }
    Text(text, style = MaterialTheme.typography.bodySmall)
    when (shizuku.status) {
        ShizukuStatus.NOT_INSTALLED ->
            TextButton(onClick = actions.onGetShizuku) { Text(stringResource(R.string.shizuku_get)) }
        ShizukuStatus.NOT_RUNNING ->
            TextButton(onClick = actions.onOpenShizuku) { Text(stringResource(R.string.shizuku_open)) }
        ShizukuStatus.NO_PERMISSION ->
            TextButton(onClick = actions.onAllowShizuku) { Text(stringResource(R.string.shizuku_allow)) }
        else -> {}
    }
}

/** The permissions that keep copies going when Shizuku stops, and how to get them. */
@Composable
private fun FallbackLine(shizuku: ShizukuState, fallback: FallbackAccess, actions: AutoCopyActions) {
    val watching = shizuku.status == ShizukuStatus.WATCHING
    when {
        fallback.ready -> if (watching) Text(stringResource(R.string.fallback_ready), style = MaterialTheme.typography.bodySmall)
        watching -> {
            Text(stringResource(R.string.fallback_offer), style = MaterialTheme.typography.bodySmall)
            TextButton(onClick = actions.onGrantFallback) { Text(stringResource(R.string.fallback_grant)) }
        }
        fallback.readLogs -> {
            Text(stringResource(R.string.fallback_needs_overlay), style = MaterialTheme.typography.bodySmall)
            TextButton(onClick = actions.onAllowOverlay) { Text(stringResource(R.string.fallback_allow_overlay)) }
        }
        else -> {}
    }
}

/** Mode B, while Shizuku isn't watching. */
@Composable
private fun LogLine(logs: LogStatus, actions: AutoCopyActions) {
    val text = when (logs) {
        LogStatus.OFF, LogStatus.NEEDS_PERMISSIONS -> return
        LogStatus.WAITING_FOR_APP -> R.string.logs_waiting
        LogStatus.STARTING -> R.string.logs_starting
        LogStatus.RUNNING -> R.string.logs_running
    }
    Text(stringResource(text), style = MaterialTheme.typography.bodySmall)
    if (logs == LogStatus.STARTING) {
        TextButton(onClick = actions.onRetryLogs) { Text(stringResource(R.string.logs_retry)) }
    }
}

/** What the last clip from a computer was, and whether it made it to the phone's clipboard. */
@Composable
private fun LastReceivedText(received: LastReceived) {
    val context = LocalContext.current
    val kind = stringResource(if (received.mimeType.startsWith("image/")) R.string.kind_image else R.string.kind_text)
    val size = Formatter.formatShortFileSize(context, received.bytes.toLong())
    val time = DateFormat.getTimeFormat(context).format(Date(received.atMillis))
    val format = if (received.onClipboard) R.string.last_received else R.string.last_received_failed
    Text(stringResource(format, kind, size, time), style = MaterialTheme.typography.bodySmall)
}

@Composable
private fun LastCopyText(copy: LastCopy) {
    val context = LocalContext.current
    val size = Formatter.formatShortFileSize(context, copy.bytes.toLong())
    val time = DateFormat.getTimeFormat(context).format(Date(copy.atMillis))
    Text(
        pluralStringResource(R.plurals.last_copy, copy.sentTo, size, time, copy.sentTo),
        style = MaterialTheme.typography.bodySmall,
    )
}
