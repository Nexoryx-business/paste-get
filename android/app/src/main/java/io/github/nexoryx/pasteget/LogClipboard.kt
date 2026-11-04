// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.nexoryx.pasteget

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.content.ClipboardManager
import android.content.Context
import android.content.Intent
import android.graphics.PixelFormat
import android.os.Build
import android.os.Handler
import android.os.Looper
import android.os.SystemClock
import android.util.Log
import android.view.Gravity
import android.view.View
import android.view.WindowManager
import androidx.core.app.NotificationCompat
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import java.io.File
import java.util.concurrent.Executors
import kotlin.concurrent.thread

/** Where mode B stands. */
enum class LogStatus {
    /** Not needed: Shizuku is watching, or the automatic copy is off. */
    OFF,

    /** The log or overlay permission is missing ([FallbackAccess]). */
    NEEDS_PERMISSIONS,

    /** Android 13+ lets the app read the log only after it asks from the front: open the app. */
    WAITING_FOR_APP,

    /** Reading the log, but no line from the system yet: Android may be asking the user. */
    STARTING,

    /** Copies are seen. */
    RUNNING,
}

/**
 * Mode B of the automatic copy, for when Shizuku isn't running. It uses two permissions that
 * last across reboots ([FallbackAccess]), as ClipCascade does:
 * 1. the app listens to the clipboard, so Android logs a line when it changes and the app is
 *    denied it ([LogWatch]);
 * 2. on that line, the app shows a transparent 1×1 window over other apps for a moment. With
 *    focus, it may read the clipboard; the window then goes away.
 *
 * Taking focus for a moment may close the keyboard. Since Android 13, reading the system log also
 * needs the user's consent each time the app starts, asked by Android while the app is in front:
 * after a restart of the phone, [LogStatus.WAITING_FOR_APP] posts a notification to open it.
 */
class LogClipboard(
    private val context: Context,
    private val onCopied: (CopiedText) -> Unit,
) {
    private val mutableState = MutableStateFlow(LogStatus.OFF)
    val state: StateFlow<LogStatus> = mutableState.asStateFlow()

    private val main = Handler(Looper.getMainLooper())
    private val clipboard = context.getSystemService(ClipboardManager::class.java)
    private val windows = context.getSystemService(WindowManager::class.java)
    private val notifications = context.getSystemService(NotificationManager::class.java)
    private val worker = Executors.newSingleThreadExecutor()

    /** Whether this mode should run. Only touched on the main thread, like the rest below. */
    private var active = false

    /** Whether an activity of the app is in front: then Android 13+ may ask for log access. */
    private var inFront = false
    private var reader: Process? = null
    private var overlay: View? = null

    /** Wait before starting logcat again after it ended, doubled when it ends right away. */
    private var restartDelayMs = RESTART_MIN_MS

    /** Called by Android when the app may read the clipboard: it's in front. */
    private val listener = ClipboardManager.OnPrimaryClipChangedListener { readFocused() }

    /** On the main thread. Starts or stops this mode; [AutoCopy] decides. */
    fun setActive(on: Boolean) {
        if (on == active) return
        active = on
        if (on) {
            clipboard.addPrimaryClipChangedListener(listener)
            startReader()
        } else {
            clipboard.removePrimaryClipChangedListener(listener)
            stopReader()
            closeOverlay()
            set(LogStatus.OFF)
        }
    }

    /** On the main thread. In front is when Android 13+ may ask to allow log access. */
    fun setInFront(front: Boolean) {
        inFront = front
        if (front && active && reader == null) startReader()
    }

    /** On the main thread, from the screen: Android may ask again for log access. */
    fun retry() {
        stopReader()
        startReader()
    }

    private fun startReader() {
        val access = FallbackAccess.of(context)
        when {
            !access.ready -> return set(LogStatus.NEEDS_PERMISSIONS)
            Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU && !inFront -> return set(LogStatus.WAITING_FOR_APP)
        }
        set(LogStatus.STARTING)
        val process = try {
            ProcessBuilder(LogWatch.command(System.currentTimeMillis()))
                .redirectError(ProcessBuilder.Redirect.to(File("/dev/null")))
                .start()
        } catch (e: java.io.IOException) {
            Log.w(TAG, "log: can't start logcat: $e")
            return restartLater(quick = true)
        }
        reader = process
        Log.i(TAG, "log: reading clipboard denials")
        val startedAt = SystemClock.elapsedRealtime()
        thread(name = "paste-get-log", isDaemon = true) { read(process, startedAt) }
    }

    private fun stopReader() {
        reader?.destroy()
        reader = null
    }

    /** On the reader thread. Lines are matched and dropped: never kept, never logged. */
    private fun read(process: Process, startedAt: Long) {
        val app = context.packageName
        var proven = false
        try {
            process.inputStream.bufferedReader().forEachLine { line ->
                if (!proven && LogWatch.isFromSystem(line)) {
                    proven = true
                    main.post { if (reader === process) set(LogStatus.RUNNING) }
                }
                if (LogWatch.isDenialFor(line, app)) main.post { if (reader === process) readThroughOverlay() }
            }
        } catch (e: java.io.IOException) {
            // Stopped.
        }
        main.post {
            if (reader !== process) return@post
            reader = null
            Log.w(TAG, "log: logcat ended")
            restartLater(quick = SystemClock.elapsedRealtime() - startedAt < RESTART_MIN_MS)
        }
    }

    /** Starts logcat again in a while: longer and longer while it keeps ending right away. */
    private fun restartLater(quick: Boolean) {
        restartDelayMs = if (quick) (restartDelayMs * 2).coerceAtMost(RESTART_MAX_MS) else RESTART_MIN_MS
        main.postDelayed({ if (active && reader == null) startReader() }, restartDelayMs)
    }

    /** Takes focus with a window no one sees, to read the clipboard, then removes it. */
    private fun readThroughOverlay() {
        // A clip paste-get just put there itself (from a computer) isn't worth stealing focus for.
        if (overlay != null || ReceivedClips.justApplied()) return
        val view = object : View(context) {
            override fun onWindowFocusChanged(hasWindowFocus: Boolean) {
                super.onWindowFocusChanged(hasWindowFocus)
                if (hasWindowFocus) {
                    readFocused()
                    closeOverlay()
                }
            }
        }
        val params = WindowManager.LayoutParams(
            1,
            1,
            WindowManager.LayoutParams.TYPE_APPLICATION_OVERLAY,
            // Focusable, so it gets focus, but not for the keyboard, and touches pass through.
            WindowManager.LayoutParams.FLAG_NOT_TOUCH_MODAL or WindowManager.LayoutParams.FLAG_ALT_FOCUSABLE_IM,
            PixelFormat.TRANSLUCENT,
        ).apply { gravity = Gravity.TOP or Gravity.START }
        try {
            windows.addView(view, params)
        } catch (e: RuntimeException) {
            // The overlay permission was taken back.
            Log.w(TAG, "log: no overlay: $e")
            return set(LogStatus.NEEDS_PERMISSIONS)
        }
        overlay = view
        main.postDelayed({ if (overlay === view) closeOverlay() }, OVERLAY_TIMEOUT_MS)
    }

    private fun closeOverlay() {
        val view = overlay ?: return
        overlay = null
        runCatching { windows.removeView(view) }
    }

    /** On the main thread, while the app has focus. */
    private fun readFocused() {
        val copied = clipboard.primaryClip?.let(CopiedText::of) ?: return
        worker.execute { onCopied(copied) }
    }

    private fun set(status: LogStatus) {
        if (mutableState.value == status) return
        mutableState.value = status
        if (status == LogStatus.WAITING_FOR_APP) notifyWaiting() else notifications.cancel(NOTIFICATION_ID)
    }

    /** After a restart of the phone (Android 13+): one tap brings the app to the front. */
    private fun notifyWaiting() {
        notifications.createNotificationChannel(
            NotificationChannel(
                CHANNEL_ID,
                context.getString(R.string.copies_channel),
                NotificationManager.IMPORTANCE_LOW,
            ),
        )
        val openApp = PendingIntent.getActivity(
            context,
            0,
            Intent(context, MainActivity::class.java).addFlags(Intent.FLAG_ACTIVITY_NEW_TASK),
            PendingIntent.FLAG_IMMUTABLE,
        )
        val notification: Notification = NotificationCompat.Builder(context, CHANNEL_ID)
            .setSmallIcon(R.drawable.ic_tile)
            .setContentTitle(context.getString(R.string.copies_paused_title))
            .setContentText(context.getString(R.string.copies_paused_text))
            .setContentIntent(openApp)
            .setAutoCancel(true)
            .setPriority(NotificationCompat.PRIORITY_LOW)
            .build()
        notifications.notify(NOTIFICATION_ID, notification)
    }

    private companion object {
        const val TAG = "paste-get"
        const val CHANNEL_ID = "copies"
        const val NOTIFICATION_ID = 2
        const val OVERLAY_TIMEOUT_MS = 1_500L
        const val RESTART_MIN_MS = 5_000L
        const val RESTART_MAX_MS = 300_000L
    }
}
