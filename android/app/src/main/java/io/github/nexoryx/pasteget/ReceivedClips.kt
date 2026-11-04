// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.nexoryx.pasteget

import android.content.ClipData
import android.content.ClipDescription
import android.content.ClipboardManager
import android.content.Context
import android.os.PersistableBundle
import android.os.SystemClock
import android.util.Log
import androidx.core.content.FileProvider
import java.io.File
import java.util.concurrent.Executors

/**
 * Puts clips received from a computer on the phone's clipboard. Android lets any app write the
 * clipboard, also from the background: only reading it is restricted.
 *
 * Text goes on the clipboard as it is. Bigger clips go as a `content://` link to a file of
 * [ClipFiles], served by the app's FileProvider: a clip passes through Binder, whose buffer is
 * 1 MB per process, so a big one would fail to copy. The clipboard gives the app that pastes
 * access to the link, and takes it away once it holds something else.
 */
object ReceivedClips {
    private const val TAG = "paste-get"

    /** How long the clipboard change caused by the app's own write takes to be reported. */
    private const val OWN_CHANGE_MS = 1_000L

    /** Clips are applied one at a time, in order, and off the client's threads, which must not wait. */
    private val worker = Executors.newSingleThreadExecutor()

    /** Only used on [worker]. */
    private var files: ClipFiles? = null

    /** When the app last set the clipboard ([SystemClock.elapsedRealtime]). */
    @Volatile
    private var appliedAt = Long.MIN_VALUE / 2

    /** Whether the app set the clipboard itself, just now: the change it causes isn't a copy. */
    fun justApplied(): Boolean = SystemClock.elapsedRealtime() - appliedAt < OWN_CHANGE_MS

    fun apply(context: Context, mimeType: String, data: ByteArray, sensitive: Boolean) {
        // Never log contents: type and size only.
        Log.i(TAG, "clip received: $mimeType, ${data.size} bytes")
        val app = context.applicationContext
        worker.execute { put(app, mimeType, data, sensitive) }
    }

    private fun put(context: Context, mimeType: String, data: ByteArray, sensitive: Boolean) {
        val clip = try {
            clipOf(context, mimeType, data)
        } catch (e: Exception) {
            // A full disk, or a file the FileProvider won't serve. An uncaught exception would end
            // the app. The message names a file, never what's in it.
            Log.w(TAG, "clip not saved: $e")
            return
        }
        if (clip == null) {
            Log.i(TAG, "$mimeType can't go on the clipboard")
            return
        }
        clip.description.extras = PersistableBundle().apply {
            putBoolean(CopiedText.EXTRA_IS_REMOTE_DEVICE, true)
            if (sensitive) putBoolean(CopiedText.EXTRA_IS_SENSITIVE, true)
        }
        try {
            appliedAt = SystemClock.elapsedRealtime()
            context.getSystemService(ClipboardManager::class.java).setPrimaryClip(clip)
        } catch (e: RuntimeException) {
            // A refused link or an oversized clip. The message has sizes and links, not the clip.
            Log.w(TAG, "clipboard not set: $e")
        }
    }

    /** The clip as it goes on the clipboard: the text itself, or a link to a file holding it. */
    private fun clipOf(context: Context, mimeType: String, data: ByteArray): ClipData? {
        val label = context.getString(R.string.app_name)
        if (!ClipFiles.needsFile(mimeType, data.size)) {
            return ClipData.newPlainText(label, data.decodeToString())
        }
        val type = ClipFiles.clipboardType(mimeType) ?: return null
        val file = files(context).save(mimeType, data) ?: return null
        val uri = FileProvider.getUriForFile(context, "${context.packageName}.clips", file)
        Log.i(TAG, "clip saved as ${file.name} for the clipboard")
        return ClipData(ClipDescription(label, arrayOf(type)), ClipData.Item(uri))
    }

    /** In the cache directory, which `res/xml/clip_files.xml` lets the FileProvider serve. */
    private fun files(context: Context): ClipFiles =
        files ?: ClipFiles(File(context.cacheDir, "clips")).also { files = it }
}
