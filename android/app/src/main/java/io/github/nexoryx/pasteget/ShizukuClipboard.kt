// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.nexoryx.pasteget

import android.content.ComponentName
import android.content.Context
import android.content.ServiceConnection
import android.content.pm.PackageManager
import android.os.IBinder
import android.os.Process
import android.util.Log
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import rikka.shizuku.Shizuku
import java.util.concurrent.Executors

/** Where mode A stands. */
enum class ShizukuStatus {
    /** The automatic copy is off. */
    OFF,
    NOT_INSTALLED,

    /** Installed, but not started since the phone booted. */
    NOT_RUNNING,

    /** paste-get isn't allowed to use it yet. */
    NO_PERMISSION,
    CONNECTING,

    /** Copies are seen. */
    WATCHING,

    /** It runs, but couldn't watch the clipboard: see [ShizukuState.detail]. */
    FAILED,
}

data class ShizukuState(val status: ShizukuStatus, val detail: String? = null)

/**
 * Mode A of the automatic copy: [ShellClipboardService], started by Shizuku as the shell user,
 * reports each clipboard change, and this reads the new text from it and hands it to [onCopied].
 *
 * Shizuku has to be started again after every reboot (without root). It tells the app when it
 * comes and goes, and [state] follows.
 */
class ShizukuClipboard(
    private val context: Context,
    private val onCopied: (CopiedText) -> Unit,
) {
    private val mutableState = MutableStateFlow(ShizukuState(ShizukuStatus.OFF))
    val state: StateFlow<ShizukuState> = mutableState.asStateFlow()

    /** Binder calls to the service, one at a time and off the main thread. */
    private val worker = Executors.newSingleThreadExecutor()
    private val userId = Process.myUid() / PER_USER_RANGE
    private var started = false

    /** Times the service stopped since Shizuku started: a service that keeps crashing is given up. */
    private var restarts = 0

    @Volatile
    private var service: IShellClipboard? = null

    private val serviceArgs = Shizuku.UserServiceArgs(
        ComponentName(context.packageName, ShellClipboardService::class.java.name),
    )
        .daemon(false) // Ends with the app.
        .processNameSuffix("clipboard")
        .tag("clipboard")
        .version(SERVICE_VERSION)

    private val binderReceived = Shizuku.OnBinderReceivedListener {
        restarts = 0
        connect()
    }
    private val binderDead = Shizuku.OnBinderDeadListener {
        service = null
        set(ShizukuStatus.NOT_RUNNING)
    }
    private val permissionResult = Shizuku.OnRequestPermissionResultListener { _, _ -> connect() }

    private val connection = object : ServiceConnection {
        override fun onServiceConnected(name: ComponentName?, binder: IBinder?) {
            // Turned off while Shizuku was starting the service: end it.
            if (!started) return unbind()
            val connected = IShellClipboard.Stub.asInterface(binder ?: return)
            worker.execute { watch(connected) }
        }

        override fun onServiceDisconnected(name: ComponentName?) {
            service = null
            when {
                !started -> {}
                ++restarts <= MAX_RESTARTS -> connect()
                else -> set(ShizukuStatus.FAILED, "the clipboard service keeps stopping")
            }
        }
    }

    private val changed = object : IClipboardChanged.Stub() {
        override fun changed() {
            worker.execute(::readNewCopy)
        }
    }

    /** On the main thread. */
    fun start() {
        if (started) return
        started = true
        Shizuku.addBinderDeadListener(binderDead)
        Shizuku.addRequestPermissionResultListener(permissionResult)
        // Called right away if Shizuku is running already.
        Shizuku.addBinderReceivedListenerSticky(binderReceived)
        if (!Shizuku.pingBinder()) set(if (isInstalled()) ShizukuStatus.NOT_RUNNING else ShizukuStatus.NOT_INSTALLED)
    }

    /** On the main thread. */
    fun stop() {
        if (!started) return
        started = false
        Shizuku.removeBinderReceivedListener(binderReceived)
        Shizuku.removeBinderDeadListener(binderDead)
        Shizuku.removeRequestPermissionResultListener(permissionResult)
        unbind()
        service = null
        set(ShizukuStatus.OFF)
    }

    /** Ends the service, if Shizuku runs it. */
    private fun unbind() {
        if (Shizuku.pingBinder()) runCatching { Shizuku.unbindUserService(serviceArgs, connection, true) }
    }

    /**
     * Grants the fallback mode's permissions ([FallbackAccess]) through Shizuku. [onDone] gets
     * `null` once granted, or what failed, on a background thread. Android restarts the app to
     * apply the log permission, so [onDone] may never be called.
     */
    fun grantFallback(onDone: (String?) -> Unit) {
        val connected = service ?: return onDone("Shizuku is not connected")
        worker.execute {
            val failed = try {
                connected.grantFallback().ifEmpty { null }
            } catch (e: Exception) {
                e.message ?: e.javaClass.simpleName
            }
            if (failed != null) Log.w(TAG, "Shizuku: grants failed: $failed")
            onDone(failed)
        }
    }

    /** Shows Shizuku's own dialog. Call it from an activity in front. */
    fun requestPermission() {
        if (Shizuku.pingBinder()) Shizuku.requestPermission(PERMISSION_REQUEST)
    }

    /** On the main thread. */
    private fun connect() {
        if (!started || !Shizuku.pingBinder()) return
        try {
            when {
                Shizuku.isPreV11() -> set(ShizukuStatus.FAILED, "this version of Shizuku is too old")
                Shizuku.checkSelfPermission() != PackageManager.PERMISSION_GRANTED -> set(ShizukuStatus.NO_PERMISSION)
                else -> {
                    set(ShizukuStatus.CONNECTING)
                    Shizuku.bindUserService(serviceArgs, connection)
                }
            }
        } catch (e: RuntimeException) {
            // Shizuku stopped meanwhile: its binder-dead listener follows.
            Log.w(TAG, "Shizuku: can't connect: $e")
        }
    }

    private fun watch(connected: IShellClipboard) {
        try {
            val how = connected.watch(changed, userId)
            service = connected
            Log.i(TAG, "Shizuku: watching the clipboard as uid ${Shizuku.getUid()} with $how")
            set(ShizukuStatus.WATCHING, if (Shizuku.getUid() == ROOT_UID) "root" else "shell")
        } catch (e: Exception) {
            // Unknown hidden API, or the service died: other modes take over.
            Log.w(TAG, "Shizuku: can't watch the clipboard: $e")
            set(ShizukuStatus.FAILED, e.message)
        }
    }

    /** Never logs the clip: type and size only. */
    private fun readNewCopy() {
        val connected = service ?: return
        try {
            // A description first: reading it doesn't make Android say the clipboard was read.
            val description = connected.describe(userId) ?: return
            if (!CopiedText.worthReading(description)) return
            val copied = connected.read(userId)?.let(CopiedText::of) ?: return
            onCopied(copied)
        } catch (e: Exception) {
            Log.w(TAG, "Shizuku: copy not read: ${e.javaClass.simpleName}")
        }
    }

    private fun isInstalled(): Boolean = try {
        context.packageManager.getPackageInfo(SHIZUKU_PACKAGE, 0)
        true
    } catch (e: PackageManager.NameNotFoundException) {
        false
    }

    private fun set(status: ShizukuStatus, detail: String? = null) {
        mutableState.value = ShizukuState(status, detail)
    }

    private companion object {
        const val TAG = "paste-get"
        const val SHIZUKU_PACKAGE = "moe.shizuku.privileged.api"
        const val PERMISSION_REQUEST = 1
        const val ROOT_UID = 0
        const val MAX_RESTARTS = 5

        /** UIDs per Android user: an app's user is its UID divided by this. */
        const val PER_USER_RANGE = 100_000

        /** Raise when IShellClipboard changes: Shizuku then replaces a service left running. */
        const val SERVICE_VERSION = 1
    }
}
