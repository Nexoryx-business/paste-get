// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.nexoryx.pasteget

import android.Manifest
import android.content.Context
import android.content.pm.PackageManager
import android.os.Build
import android.provider.Settings
import android.util.Log
import androidx.core.content.ContextCompat

/**
 * Android 12+ shows "<app> pasted from your clipboard" whenever an app reads the clipboard, so
 * each copy sent shows "Shell pasted from your clipboard" (or paste-get's name in the fallback
 * mode). The system's own switch for it, Settings → Privacy → *Show clipboard access*, is the
 * hidden secure setting [SETTING].
 *
 * With `WRITE_SECURE_SETTINGS`, which the built-in mode grants the app, paste-get can flip that
 * switch for the user. It applies to every app, so it is only ever changed when the user asks,
 * and can be turned back on here or in Settings. Without the permission, or where the setting
 * can't be read, the fallback is opening the privacy settings.
 */
data class AccessNotices(
    /** Android 12+: there are notices to hide. */
    val supported: Boolean,
    /** Whether they are hidden now; `null` if the setting couldn't be read. */
    val hidden: Boolean?,
    /** Whether paste-get may change the setting itself. */
    val canChange: Boolean,
) {
    /** Show the in-app switch; otherwise a button that opens the privacy settings. */
    val switchable: Boolean get() = supported && canChange && hidden != null

    companion object {
        const val TAG = "paste-get"

        /** `Settings.Secure.CLIPBOARD_SHOW_ACCESS_NOTIFICATIONS`, hidden from the SDK. */
        const val SETTING = "clipboard_show_access_notifications"

        /** No Android calls, for the JVM tests: [value] is the setting, `null` if unreadable. */
        fun decide(sdk: Int, value: Int?, canWrite: Boolean): AccessNotices = AccessNotices(
            supported = sdk >= Build.VERSION_CODES.S,
            // Unset means shown: Android's default.
            hidden = value?.let { it == 0 },
            canChange = canWrite,
        )

        fun of(context: Context): AccessNotices {
            val sdk = Build.VERSION.SDK_INT
            if (sdk < Build.VERSION_CODES.S) return decide(sdk, null, canWrite = false)
            val value = try {
                Settings.Secure.getInt(context.contentResolver, SETTING, 1)
            } catch (e: SecurityException) {
                Log.w(TAG, "can't read $SETTING: $e")
                null
            }
            return decide(sdk, value, canWrite(context))
        }

        /** Hides or shows the notices for every app. Returns whether it worked. */
        fun set(context: Context, hide: Boolean): Boolean {
            if (!canWrite(context)) return false
            return try {
                Settings.Secure.putInt(context.contentResolver, SETTING, if (hide) 0 else 1)
            } catch (e: SecurityException) {
                Log.w(TAG, "can't change $SETTING: $e")
                false
            }
        }

        private fun canWrite(context: Context): Boolean =
            ContextCompat.checkSelfPermission(context, Manifest.permission.WRITE_SECURE_SETTINGS) ==
                PackageManager.PERMISSION_GRANTED
    }
}
