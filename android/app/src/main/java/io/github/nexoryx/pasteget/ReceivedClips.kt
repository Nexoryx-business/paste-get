// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.nexoryx.pasteget

import android.content.ClipData
import android.content.ClipboardManager
import android.content.Context
import android.os.Handler
import android.os.Looper
import android.os.PersistableBundle
import android.util.Log

/** Puts clips received from a computer on the phone's clipboard. */
object ReceivedClips {
    private const val TAG = "paste-get"
    private const val TEXT = "text/plain;charset=utf-8"

    // Constants added in API 33, with the same string values on older versions.
    private const val EXTRA_IS_SENSITIVE = "android.content.extra.IS_SENSITIVE"
    private const val EXTRA_IS_REMOTE_DEVICE = "android.content.extra.IS_REMOTE_DEVICE"

    fun apply(context: Context, mimeType: String, data: ByteArray, sensitive: Boolean) {
        // Never log contents: type and size only.
        Log.i(TAG, "clip received: $mimeType, ${data.size} bytes")
        if (mimeType != TEXT) {
            // Images from the PC arrive in roadmap step M6.
            return
        }
        val clip = ClipData.newPlainText(context.getString(R.string.app_name), data.decodeToString())
        clip.description.extras = PersistableBundle().apply {
            putBoolean(EXTRA_IS_REMOTE_DEVICE, true)
            if (sensitive) putBoolean(EXTRA_IS_SENSITIVE, true)
        }
        Handler(Looper.getMainLooper()).post {
            context.getSystemService(ClipboardManager::class.java).setPrimaryClip(clip)
        }
    }
}
