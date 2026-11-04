// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.nexoryx.pasteget

import android.content.Context
import androidx.core.content.edit

/** The user's choices, in the app's private storage. */
object Preferences {
    private const val FILE = "settings"
    private const val SEND_SCREENSHOTS = "send_screenshots"

    /** Off until the user turns it on and grants photo access. */
    fun sendScreenshots(context: Context): Boolean =
        context.getSharedPreferences(FILE, Context.MODE_PRIVATE).getBoolean(SEND_SCREENSHOTS, false)

    fun setSendScreenshots(context: Context, send: Boolean) {
        context.getSharedPreferences(FILE, Context.MODE_PRIVATE).edit { putBoolean(SEND_SCREENSHOTS, send) }
    }
}
