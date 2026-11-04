// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.nexoryx.pasteget

import android.util.Log
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow

/**
 * The automatic copy: what the user copies on the phone goes to the computers, without a tap.
 *
 * Since Android 10 only the focused app and the keyboard may read the clipboard, so a helper
 * with more rights reads it: mode A, [ShizukuClipboard], a Shizuku service running as the shell
 * user. Without it, the user sends by hand: the Quick Settings tile or the share sheet (mode D).
 */
class AutoCopy(private val app: PasteGetApp) {
    val shizuku = ShizukuClipboard(app, ::copied)

    private val lastCopyState = MutableStateFlow<LastCopy?>(null)

    /** The last copy sent automatically. */
    val lastCopy: StateFlow<LastCopy?> = lastCopyState.asStateFlow()

    /** On the main thread. */
    fun start() = shizuku.start()

    /** On the main thread. */
    fun stop() = shizuku.stop()

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
    }
}

/** A copy the automatic copy sent, at [atMillis] (wall clock), to [sentTo] computers. */
data class LastCopy(val atMillis: Long, val bytes: Int, val sentTo: Int)
