// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.nexoryx.pasteget

import android.util.Log
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.cancel
import kotlinx.coroutines.delay
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.flow.collectLatest
import kotlinx.coroutines.flow.distinctUntilChanged
import kotlinx.coroutines.flow.map
import kotlinx.coroutines.launch

/** How the automatic copy works right now. */
enum class CopyMode {
    /** A: Shizuku's service, running as the shell user, watches the clipboard. */
    SHIZUKU,

    /** B: the system log says when the clipboard changes, a window shown for a moment reads it. */
    LOGS,

    /** D: nothing watches; the user sends by hand (Quick Settings tile, share sheet). */
    MANUAL,
    ;

    companion object {
        fun of(shizuku: ShizukuState, logs: LogStatus): CopyMode = when {
            shizuku.status == ShizukuStatus.WATCHING -> SHIZUKU
            logs == LogStatus.RUNNING -> LOGS
            else -> MANUAL
        }
    }
}

/**
 * The automatic copy: what the user copies on the phone goes to the computers, without a tap.
 *
 * Since Android 10 only the focused app and the keyboard may read the clipboard, so a helper
 * reads it, best first: mode A, [ShizukuClipboard]; when Shizuku isn't running (it stops at every
 * restart of the phone), mode B, [LogClipboard]; otherwise the user sends by hand.
 */
class AutoCopy(private val app: PasteGetApp) {
    val shizuku = ShizukuClipboard(app, ::copied)
    val logs = LogClipboard(app, ::copied)

    private val lastCopyState = MutableStateFlow<LastCopy?>(null)

    /** The last copy sent automatically. */
    val lastCopy: StateFlow<LastCopy?> = lastCopyState.asStateFlow()

    private var scope: CoroutineScope? = null

    /** On the main thread. */
    fun start() {
        if (scope != null) return
        shizuku.start()
        val running = CoroutineScope(SupervisorJob() + Dispatchers.Main)
        scope = running
        running.launch {
            shizuku.state
                .map { it.status != ShizukuStatus.WATCHING && it.status != ShizukuStatus.CONNECTING }
                .distinctUntilChanged()
                .collectLatest { withoutShizuku ->
                    // Shizuku's binder arrives a moment after the app starts: don't make Android
                    // ask for log access in the meantime.
                    if (withoutShizuku) delay(SHIZUKU_GRACE_MS)
                    logs.setActive(withoutShizuku)
                }
        }
    }

    /** On the main thread. */
    fun stop() {
        scope?.cancel()
        scope = null
        shizuku.stop()
        logs.setActive(false)
    }

    /** On the main thread, when an activity of the app comes to the front or leaves it. */
    fun setInFront(front: Boolean) = logs.setInFront(front)

    /** Called on a background thread. Never logs the text: its size only. */
    private fun copied(copy: CopiedText) {
        val bytes = copy.text.encodeToByteArray().size
        val sentTo = app.client.copiedText(copy.text, copy.sensitive)?.toInt()
        if (sentTo == null) {
            Log.i(TAG, "copy of $bytes bytes seen before, not sent")
            return
        }
        Log.i(TAG, "copy of $bytes bytes sent to $sentTo computers")
        lastCopyState.value = LastCopy(System.currentTimeMillis(), bytes, sentTo)
    }

    private companion object {
        const val TAG = "paste-get"
        const val SHIZUKU_GRACE_MS = 3_000L
    }
}

/** A copy the automatic copy sent, at [atMillis] (wall clock), to [sentTo] computers. */
data class LastCopy(val atMillis: Long, val bytes: Int, val sentTo: Int)
