// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.nexoryx.pasteget

import android.Manifest
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.content.pm.PackageManager
import android.net.ConnectivityManager
import android.net.Network
import android.net.NetworkCapabilities
import android.net.NetworkRequest
import android.os.Handler
import android.os.Looper
import android.os.Process
import android.os.SystemClock
import android.provider.Settings
import android.util.Log
import androidx.core.app.NotificationCompat
import androidx.core.app.RemoteInput
import androidx.core.content.ContextCompat
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import uniffi.pasteget_ffi.AdbException
import uniffi.pasteget_ffi.adbExec
import uniffi.pasteget_ffi.adbPair
import java.util.concurrent.Executors

/** Where the built-in mode stands. */
enum class BuiltInStatus {
    /** The automatic copy is off. */
    OFF,

    /** Wireless debugging hasn't been paired with the app yet. */
    NOT_SET_UP,

    /** Waiting for the pairing code. */
    PAIRING,
    STARTING,

    /** Copies are seen. */
    WATCHING,

    /** Wireless debugging needs Wi-Fi. */
    NEEDS_WIFI,

    /** See [BuiltInState.detail]. */
    FAILED,
}

data class BuiltInState(val status: BuiltInStatus, val detail: String? = null)

/**
 * The built-in mode of the automatic copy: what Shizuku does for mode A, done by the app itself.
 *
 * 1. Once: the user pairs the app with Android's wireless debugging, typing the 6-digit code
 *    from Settings into a notification ([beginPairing], [submitCode]). The app's key ([AdbKey])
 *    is then trusted like a paired computer, and the app grants itself the right to switch
 *    wireless debugging on and off (`WRITE_SECURE_SETTINGS`).
 * 2. Whenever the automatic copy starts (also after a reboot, on Wi-Fi): the app switches
 *    wireless debugging on, starts [ShellHelper] as the shell user through it, and switches it
 *    back off. The helper then reports copies over [ShellLinkServer].
 *
 * Only the phone itself is ever reached (the Rust client refuses other addresses).
 */
class BuiltInShell(
    private val context: Context,
    private val onCopied: (CopiedText) -> Unit,
    /** Called on the main thread once pairing worked, so the automatic copy can switch to this mode. */
    private val onSetUp: () -> Unit,
) {
    private val mutableState = MutableStateFlow(BuiltInState(BuiltInStatus.OFF))
    val state: StateFlow<BuiltInState> = mutableState.asStateFlow()

    private val main = Handler(Looper.getMainLooper())
    private val worker = Executors.newSingleThreadExecutor()
    private val notifications = context.getSystemService(NotificationManager::class.java)
    private val connectivity = context.getSystemService(ConnectivityManager::class.java)
    private val userId = Process.myUid() / PER_USER_RANGE

    @Volatile
    private var active = false

    /** Only touched on [worker]. */
    private var link: ShellLinkServer? = null
    private var restarts = 0
    private var restartsSince = 0L

    val isSetUp: Boolean
        get() = Preferences.builtInPaired(context) && AdbKey.exists(context.filesDir)

    private val events = object : ShellLinkServer.Events {
        override fun watching(how: String) {
            Log.i(TAG, "built-in mode: watching the clipboard as shell with $how")
            set(BuiltInStatus.WATCHING)
        }

        override fun copied(copy: CopiedText) = onCopied(copy)

        override fun failed(reason: String) {
            Log.w(TAG, "built-in mode: the helper can't watch the clipboard: $reason")
            set(BuiltInStatus.FAILED, reason)
        }

        override fun disconnected() {
            if (!active) return
            Log.i(TAG, "built-in mode: the helper stopped")
            // Android may have ended it: start another, but not in a loop.
            val now = SystemClock.elapsedRealtime()
            if (now - restartsSince > RESTART_WINDOW_MS) {
                restartsSince = now
                restarts = 0
            }
            if (++restarts <= MAX_RESTARTS) {
                set(BuiltInStatus.STARTING)
                worker.execute { launch() }
            } else {
                set(BuiltInStatus.FAILED, "the shell helper keeps stopping")
            }
        }
    }

    /** Wi-Fi came back: wireless debugging can run again. */
    private val wifi = object : ConnectivityManager.NetworkCallback() {
        override fun onAvailable(network: Network) {
            val status = mutableState.value.status
            if (active && (status == BuiltInStatus.NEEDS_WIFI || status == BuiltInStatus.FAILED)) {
                worker.execute { launch() }
            }
        }
    }

    /** On the main thread. */
    fun start() {
        if (active) return
        active = true
        if (!isSetUp) {
            if (mutableState.value.status != BuiltInStatus.PAIRING) set(BuiltInStatus.NOT_SET_UP)
            return
        }
        connectivity.registerNetworkCallback(
            NetworkRequest.Builder().addTransportType(NetworkCapabilities.TRANSPORT_WIFI).build(),
            wifi,
        )
        set(BuiltInStatus.STARTING)
        worker.execute {
            try {
                ensureLink()
            } catch (e: java.io.IOException) {
                set(BuiltInStatus.FAILED, "local socket: ${e.message}")
                return@execute
            }
            launch()
        }
    }

    /** On the main thread. Also ends the helper. */
    fun stop() {
        if (!active) return
        active = false
        runCatching { connectivity.unregisterNetworkCallback(wifi) }
        worker.execute {
            link?.stop()
            link = null
        }
        if (mutableState.value.status != BuiltInStatus.PAIRING) set(BuiltInStatus.OFF)
    }

    /** On the main thread: the "Retry" button. */
    fun retry() {
        if (active && isSetUp) worker.execute { launch() }
    }

    /**
     * Step 1 of pairing: shows the notification where the user types the code while Settings
     * shows it. The caller opens Settings.
     */
    fun beginPairing() {
        set(BuiltInStatus.PAIRING)
        notifyPairing(context.getString(R.string.builtin_pair_text))
    }

    /** Step 2: the code typed in the notification. On a background thread. */
    fun submitCode(code: String) {
        worker.execute { pair(code) }
    }

    /** Forgets the pairing (the user can also remove "paste-get" from Settings' paired devices). */
    fun reset() {
        stop()
        Preferences.setBuiltInPaired(context, false)
        AdbKey.delete(context.filesDir)
        set(BuiltInStatus.NOT_SET_UP)
    }

    private fun pair(code: String) {
        notifyPairing(context.getString(R.string.builtin_pair_working))
        try {
            val key = AdbKey.loadOrCreate(context.filesDir)
            val pairing = LocalAdbServices.find(context, LocalAdbServices.PAIRING, FIND_TIMEOUT_MS)
                ?: return pairingFailed(context.getString(R.string.builtin_pair_no_dialog))
            onHosts(pairing) { host -> adbPair(host, pairing.port.toUShort(), code, key.pkcs8, key.adbPublicKey()) }
            Log.i(TAG, "built-in mode: paired with wireless debugging")
            // So it can switch wireless debugging on by itself after a reboot.
            val connect = LocalAdbServices.find(context, LocalAdbServices.CONNECT, FIND_TIMEOUT_MS)
                ?: return pairingFailed(context.getString(R.string.builtin_no_debugging))
            onHosts(connect) { host ->
                adbExec(host, connect.port.toUShort(), key.pkcs8, ShellCommands.grantSecureSettings(context.packageName))
            }
            Preferences.setBuiltInPaired(context, true)
            notifications.cancel(NOTIFICATION_ID)
            main.post {
                // The UI follows the automatic copy, which now starts this mode.
                set(BuiltInStatus.STARTING)
                onSetUp()
            }
        } catch (e: AdbException.WrongCode) {
            pairingFailed(context.getString(R.string.builtin_pair_wrong_code))
        } catch (e: AdbException) {
            pairingFailed(e.message ?: e.javaClass.simpleName)
        } catch (e: RuntimeException) {
            pairingFailed(e.message ?: e.javaClass.simpleName)
        }
    }

    private fun pairingFailed(reason: String) {
        Log.w(TAG, "built-in mode: pairing failed: $reason")
        // Offer the code field again.
        notifyPairing(context.getString(R.string.builtin_pair_failed, reason))
        set(BuiltInStatus.PAIRING, reason)
    }

    private fun ensureLink(): ShellLinkServer {
        link?.let { return it }
        val build = context.packageManager.getPackageInfo(context.packageName, 0).lastUpdateTime
        val server = ShellLinkServer("pasteget.shell.${Process.myUid()}", build, userId, events)
        server.start()
        link = server
        return server
    }

    /** Starts the helper, unless one is already there. On the worker thread. */
    private fun launch() {
        if (!active || !isSetUp) return
        val server = link ?: return
        // A helper from before (the app was restarted) reconnects within a few seconds.
        if (waitFor(HELPER_RETURN_MS) { server.connected }) return
        if (!onWifi()) return set(BuiltInStatus.NEEDS_WIFI)
        set(BuiltInStatus.STARTING)
        val switchedOn = !debuggingOn()
        if (switchedOn) {
            if (!canSwitchDebugging()) return set(BuiltInStatus.FAILED, context.getString(R.string.builtin_no_debugging))
            setDebugging(true)
        }
        try {
            val key = AdbKey.loadOrCreate(context.filesDir)
            val connect = LocalAdbServices.find(context, LocalAdbServices.CONNECT, FIND_TIMEOUT_MS)
                ?: return set(BuiltInStatus.FAILED, context.getString(R.string.builtin_debugging_not_found))
            val command = ShellCommands.helperLaunch(
                context.applicationInfo.sourceDir,
                server.socketName,
                Process.myUid(),
                context.packageManager.getPackageInfo(context.packageName, 0).lastUpdateTime,
            )
            onHosts(connect) { host -> adbExec(host, connect.port.toUShort(), key.pkcs8, command) }
            if (!waitFor(HELPER_START_MS) { server.connected }) {
                set(BuiltInStatus.FAILED, context.getString(R.string.builtin_helper_silent))
            }
        } catch (e: AdbException.NotAuthorized) {
            set(BuiltInStatus.FAILED, context.getString(R.string.builtin_not_authorized))
        } catch (e: AdbException) {
            set(BuiltInStatus.FAILED, e.message)
        } catch (e: RuntimeException) {
            set(BuiltInStatus.FAILED, e.message ?: e.javaClass.simpleName)
        } finally {
            // Off again once the helper runs: it doesn't need it, and nothing else should use it.
            if (switchedOn) setDebugging(false)
        }
    }

    /** Tries loopback first, then the phone's own addresses, until one answers. */
    private fun <T> onHosts(found: LocalAdbServices.Found, call: (String) -> T): T {
        var last: AdbException? = null
        for (host in found.hosts) {
            try {
                return call(host)
            } catch (e: AdbException.Unreachable) {
                last = e
            }
        }
        throw last ?: AdbException.Unreachable("no address")
    }

    private fun debuggingOn(): Boolean =
        Settings.Global.getInt(context.contentResolver, ADB_WIFI_ENABLED, 0) == 1

    private fun canSwitchDebugging(): Boolean =
        ContextCompat.checkSelfPermission(context, Manifest.permission.WRITE_SECURE_SETTINGS) ==
            PackageManager.PERMISSION_GRANTED

    private fun setDebugging(on: Boolean) {
        try {
            Settings.Global.putInt(context.contentResolver, ADB_WIFI_ENABLED, if (on) 1 else 0)
            Log.i(TAG, "built-in mode: wireless debugging switched ${if (on) "on" else "off"}")
        } catch (e: SecurityException) {
            Log.w(TAG, "built-in mode: can't switch wireless debugging: ${e.message}")
        }
    }

    private fun onWifi(): Boolean =
        connectivity.getNetworkCapabilities(connectivity.activeNetwork)
            ?.hasTransport(NetworkCapabilities.TRANSPORT_WIFI) == true

    private fun waitFor(timeoutMs: Long, condition: () -> Boolean): Boolean {
        val end = SystemClock.elapsedRealtime() + timeoutMs
        while (SystemClock.elapsedRealtime() < end) {
            if (condition()) return true
            SystemClock.sleep(POLL_MS)
        }
        return condition()
    }

    private fun notifyPairing(text: String) {
        notifications.createNotificationChannel(
            NotificationChannel(
                CHANNEL_ID,
                context.getString(R.string.builtin_channel),
                NotificationManager.IMPORTANCE_HIGH,
            ),
        )
        val reply = PendingIntent.getBroadcast(
            context,
            0,
            Intent(context, PairingCodeReceiver::class.java),
            // Mutable: Android adds the typed code to it.
            PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_MUTABLE,
        )
        val action = NotificationCompat.Action.Builder(R.drawable.ic_tile, context.getString(R.string.builtin_pair_action), reply)
            .addRemoteInput(RemoteInput.Builder(CODE_KEY).setLabel(context.getString(R.string.builtin_pair_hint)).build())
            .build()
        val notification = NotificationCompat.Builder(context, CHANNEL_ID)
            .setSmallIcon(R.drawable.ic_tile)
            .setContentTitle(context.getString(R.string.builtin_pair_title))
            .setContentText(text)
            .setStyle(NotificationCompat.BigTextStyle().bigText(text))
            .addAction(action)
            .setOngoing(true)
            .setOnlyAlertOnce(true)
            .setPriority(NotificationCompat.PRIORITY_HIGH)
            .build()
        notifications.notify(NOTIFICATION_ID, notification)
    }

    private fun set(status: BuiltInStatus, detail: String? = null) {
        mutableState.value = BuiltInState(status, detail)
    }

    /** Receives the code typed in the pairing notification. */
    class PairingCodeReceiver : BroadcastReceiver() {
        override fun onReceive(context: Context, intent: Intent) {
            val code = RemoteInput.getResultsFromIntent(intent)?.getCharSequence(CODE_KEY)?.toString() ?: return
            context.pasteGet.autoCopy.builtIn.submitCode(code.filter(Char::isDigit))
        }
    }

    private companion object {
        const val TAG = "paste-get"
        const val CHANNEL_ID = "builtin_setup"
        const val NOTIFICATION_ID = 3
        const val CODE_KEY = "code"

        /** `Settings.Global.ADB_WIFI_ENABLED`, hidden from the SDK. */
        const val ADB_WIFI_ENABLED = "adb_wifi_enabled"
        const val FIND_TIMEOUT_MS = 15_000L
        const val HELPER_RETURN_MS = 4_000L
        const val HELPER_START_MS = 10_000L
        const val POLL_MS = 200L
        const val MAX_RESTARTS = 5
        const val RESTART_WINDOW_MS = 60 * 60 * 1000L

        /** UIDs per Android user: an app's user is its UID divided by this. */
        const val PER_USER_RANGE = 100_000
    }
}
