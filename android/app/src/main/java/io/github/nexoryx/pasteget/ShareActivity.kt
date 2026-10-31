// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.nexoryx.pasteget

import android.content.Intent
import android.net.Uri
import android.os.Bundle
import android.widget.Toast
import androidx.activity.ComponentActivity
import androidx.core.content.IntentCompat
import androidx.lifecycle.lifecycleScope
import kotlinx.coroutines.launch

/** "Send to PC" in the share sheet. Sends, shows the result, and closes without any UI. */
class ShareActivity : ComponentActivity() {
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        val shared = intent
        lifecycleScope.launch {
            val message = when {
                shared.action != Intent.ACTION_SEND -> getString(R.string.unsupported_content)
                shared.type?.startsWith("image/") == true -> {
                    val uri = IntentCompat.getParcelableExtra(shared, Intent.EXTRA_STREAM, Uri::class.java)
                    if (uri == null) getString(R.string.unsupported_content) else Sender.sendImage(this@ShareActivity, uri)
                }
                else -> {
                    val text = shared.getCharSequenceExtra(Intent.EXTRA_TEXT)?.toString()
                    if (text.isNullOrEmpty()) getString(R.string.unsupported_content) else Sender.sendText(this@ShareActivity, text)
                }
            }
            Toast.makeText(applicationContext, message, Toast.LENGTH_SHORT).show()
            finish()
        }
    }
}
