// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.nexoryx.pasteget

import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent

/** Reconnects after the phone restarts, if a computer is paired. */
class BootReceiver : BroadcastReceiver() {
    override fun onReceive(context: Context, intent: Intent) {
        if (intent.action == Intent.ACTION_BOOT_COMPLETED &&
            context.pasteGet.client.pairedComputers().isNotEmpty()
        ) {
            SyncService.start(context)
        }
    }
}
