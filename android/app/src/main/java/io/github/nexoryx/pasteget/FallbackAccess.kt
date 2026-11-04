// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.nexoryx.pasteget

import android.Manifest
import android.content.Context
import android.content.pm.PackageManager
import android.provider.Settings
import androidx.core.content.ContextCompat

/**
 * What the fallback mode of the automatic copy needs, for when Shizuku isn't running (it stops at
 * every restart of the phone): reading the system log, and showing a window over other apps.
 * Shizuku can grant both once ([ShizukuClipboard.grantFallback]); the grants then last.
 */
data class FallbackAccess(val readLogs: Boolean, val overlay: Boolean) {
    val ready: Boolean
        get() = readLogs && overlay

    companion object {
        fun of(context: Context) = FallbackAccess(
            readLogs = ContextCompat.checkSelfPermission(context, Manifest.permission.READ_LOGS) ==
                PackageManager.PERMISSION_GRANTED,
            overlay = Settings.canDrawOverlays(context),
        )
    }
}
