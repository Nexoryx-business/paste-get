// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.nexoryx.pasteget

import android.Manifest
import android.content.Context
import android.content.pm.PackageManager
import android.os.Build
import androidx.core.content.ContextCompat

/** How much of the phone's photos the app may read. New screenshots need [FULL]. */
enum class PhotoAccess {
    FULL,

    /** Android 14+: only photos the user picked. New screenshots stay invisible. */
    PARTIAL,
    NONE,
    ;

    companion object {
        fun of(context: Context): PhotoAccess {
            fun granted(permission: String) =
                ContextCompat.checkSelfPermission(context, permission) == PackageManager.PERMISSION_GRANTED
            return when {
                Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU ->
                    if (granted(Manifest.permission.READ_MEDIA_IMAGES)) FULL else partialOrNone(::granted)
                granted(Manifest.permission.READ_EXTERNAL_STORAGE) -> FULL
                else -> NONE
            }
        }

        private fun partialOrNone(granted: (String) -> Boolean): PhotoAccess =
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.UPSIDE_DOWN_CAKE &&
                granted(Manifest.permission.READ_MEDIA_VISUAL_USER_SELECTED)
            ) {
                PARTIAL
            } else {
                NONE
            }

        /** What to ask for. On Android 14+, asking for both lets the user pick "Allow all". */
        fun permissions(): Array<String> = when {
            Build.VERSION.SDK_INT >= Build.VERSION_CODES.UPSIDE_DOWN_CAKE -> arrayOf(
                Manifest.permission.READ_MEDIA_IMAGES,
                Manifest.permission.READ_MEDIA_VISUAL_USER_SELECTED,
            )
            Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU -> arrayOf(Manifest.permission.READ_MEDIA_IMAGES)
            else -> arrayOf(Manifest.permission.READ_EXTERNAL_STORAGE)
        }
    }
}
