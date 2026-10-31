// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.nexoryx.pasteget

import android.content.ClipboardManager
import android.widget.Toast
import androidx.activity.ComponentActivity
import androidx.lifecycle.lifecycleScope
import kotlinx.coroutines.launch

/**
 * Sends the current clipboard, then closes. Opened by the Quick Settings tile.
 *
 * Since Android 10 only the app with focus may read the clipboard, so this invisible activity
 * waits until its window has focus before reading it.
 */
class SendClipboardActivity : ComponentActivity() {
    private var started = false

    override fun onWindowFocusChanged(hasFocus: Boolean) {
        super.onWindowFocusChanged(hasFocus)
        if (!hasFocus || started) return
        started = true
        val clip = getSystemService(ClipboardManager::class.java).primaryClip
        lifecycleScope.launch {
            val message = Sender.send(this@SendClipboardActivity, clip)
            Toast.makeText(applicationContext, message, Toast.LENGTH_SHORT).show()
            finish()
        }
    }
}
