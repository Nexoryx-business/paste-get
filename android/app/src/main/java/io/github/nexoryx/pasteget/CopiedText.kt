// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.nexoryx.pasteget

import android.content.ClipData
import android.content.ClipDescription

/** Text the automatic copy may send, taken from what the phone's clipboard holds. */
data class CopiedText(val text: String, val sensitive: Boolean) {
    companion object {
        // Constants added in API 33, with the same string values on older versions.
        const val EXTRA_IS_SENSITIVE = "android.content.extra.IS_SENSITIVE"
        const val EXTRA_IS_REMOTE_DEVICE = "android.content.extra.IS_REMOTE_DEVICE"

        /**
         * Whether a clip is worth reading: it may hold text, and didn't come from another device
         * (paste-get marks what it puts on the clipboard, so a computer's clip isn't sent back).
         * Reading only the description keeps Android from saying the clipboard was read.
         */
        fun worthReading(description: ClipDescription): Boolean =
            description.hasMimeType("text/*") && description.extras?.getBoolean(EXTRA_IS_REMOTE_DEVICE) != true

        /** The text of the clip's first item. `null` if it holds no text (an image, a file…). */
        fun of(clip: ClipData): CopiedText? {
            if (clip.itemCount == 0 || !worthReading(clip.description)) return null
            val text = clip.getItemAt(0).text?.toString()?.takeIf { it.isNotEmpty() } ?: return null
            return CopiedText(text, clip.description.extras?.getBoolean(EXTRA_IS_SENSITIVE) == true)
        }
    }
}
