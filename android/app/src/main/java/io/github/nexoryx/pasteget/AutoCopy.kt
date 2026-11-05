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
import kotlinx.coroutines.flow.combine
import kotlinx.coroutines.flow.distinctUntilChanged
import kotlinx.coroutines.launch

/** How the automatic copy works right now. */
enum class CopyMode {
    /** A, without Shizuku: the app's own helper, started through wireless debugging ([BuiltInShell]). */
    BUILT_IN,

    /** A: Shizuku's service, running as the shell user, watches the clipboard. */
    SHIZUKU,

    /** B: the system log says when the clipboard changes, a window shown for a moment reads it. */
    LOGS,

    /** D: nothing watches; the user sends by hand (Quick Settings tile, share sheet). */
    MANUAL,
    ;

    companion object {
        fun of(builtIn: BuiltInState, shizuku: ShizukuState, logs: LogStatus): CopyMode = when {
            builtIn.status == BuiltInStatus.WATCHING -> BUILT_IN
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
 * reads it, best first: once set up, the built-in mode, [BuiltInShell]; otherwise mode A,
 * [ShizukuClipboard]; when neither runs, mode B, [LogClipboard]; otherwise the user sends by hand.
 */
class AutoCopy(private val app: PasteGetApp) {
    val builtIn = BuiltInShell(app, ::copied, ::builtInReady)
    val shizuku = ShizukuClipboard(app, ::copied)
    val logs = LogClipboard(app, ::copied)

    private val lastCopyState = MutableStateFlow<LastCopy?>(null)

    /** The last copy sent automatically. */
    val lastCopy: StateFlow<LastCopy?> = lastCopyState.asStateFlow()

    private var scope: CoroutineScope? = null

    /** On the main thread. */
    fun start() {
        if (scope != null) return
        builtIn.start()
        // The built-in mode replaces Shizuku once set up: two watchers would send each copy twice.
        if (!builtIn.isSetUp) shizuku.start()
        val running = CoroutineScope(SupervisorJob() + Dispatchers.Main)
        scope = running
        running.launch {
            combine(shizuku.state, builtIn.state) { shizukuState, builtInState ->
                val shizukuIdle = shizukuState.status != ShizukuStatus.WATCHING &&
                    shizukuState.status != ShizukuStatus.CONNECTING
                val builtInIdle = builtInState.status != BuiltInStatus.WATCHING &&
                    builtInState.status != BuiltInStatus.STARTING
                shizukuIdle && builtInIdle
            }
                .distinctUntilChanged()
                .collectLatest { noShell ->
                    // Shizuku's binder, or the helper, arrives a moment after the app starts:
                    // don't make Android ask for log access in the meantime.
                    if (noShell) delay(SHIZUKU_GRACE_MS)
                    logs.setActive(noShell)
                }
        }
    }

    /** On the main thread: the built-in mode was just set up, and takes over from Shizuku. */
    private fun builtInReady() {
        if (scope == null) return
        shizuku.stop()
        builtIn.stop()
        builtIn.start()
    }

    /** On the main thread. */
    fun stop() {
        scope?.cancel()
        scope = null
        builtIn.stop()
        shizuku.stop()
        logs.setActive(false)
    }

    /** A service running as the shell user, if the built-in mode or Shizuku has one now. */
    fun shell(): IShellClipboard? = builtIn.shell() ?: shizuku.shell()

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
