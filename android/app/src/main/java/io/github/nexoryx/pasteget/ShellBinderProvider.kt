// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.nexoryx.pasteget

import android.content.ContentProvider
import android.content.ContentValues
import android.database.Cursor
import android.net.Uri
import android.os.Binder
import android.os.Bundle

/**
 * Where the built-in mode's [ShellHelper] hands its binder to the app, as Shizuku's server does
 * with its own provider. Only the shell user or root is listened to: the manifest asks for
 * `INTERACT_ACROSS_USERS_FULL`, which apps can't get, and [call] checks the caller's UID too.
 */
class ShellBinderProvider : ContentProvider() {
    override fun onCreate(): Boolean = true

    override fun call(method: String, arg: String?, extras: Bundle?): Bundle {
        val uid = Binder.getCallingUid()
        if (uid != SHELL_UID && uid != ROOT_UID) throw SecurityException("uid $uid may not call this")
        val accepted = method == METHOD_HELPER &&
            arg?.toLongOrNull()?.let { build ->
                extras?.getBinder(EXTRA_BINDER)?.let { binder ->
                    requireNotNull(context).pasteGet.autoCopy.builtIn.helperArrived(binder, build)
                }
            } == true
        return Bundle().apply { putBoolean(EXTRA_ACCEPTED, accepted) }
    }

    override fun query(
        uri: Uri,
        projection: Array<out String>?,
        selection: String?,
        selectionArgs: Array<out String>?,
        sortOrder: String?,
    ): Cursor? = null

    override fun getType(uri: Uri): String? = null

    override fun insert(uri: Uri, values: ContentValues?): Uri? = null

    override fun delete(uri: Uri, selection: String?, selectionArgs: Array<out String>?): Int = 0

    override fun update(uri: Uri, values: ContentValues?, selection: String?, selectionArgs: Array<out String>?): Int = 0

    companion object {
        const val METHOD_HELPER = "helper"
        const val EXTRA_BINDER = "binder"
        const val EXTRA_ACCEPTED = "accepted"
        private const val SHELL_UID = 2000
        private const val ROOT_UID = 0

        fun authority(packageName: String) = "$packageName.shell"
    }
}
