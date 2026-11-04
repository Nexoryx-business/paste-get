// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.nexoryx.pasteget

import android.content.ClipData
import android.content.ClipDescription
import android.net.Uri
import android.os.PersistableBundle
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.annotation.Config

@RunWith(RobolectricTestRunner::class)
@Config(sdk = [29, 35, 36])
class CopiedTextTest {
    private fun text(text: String, vararg extras: String): ClipData = ClipData.newPlainText("label", text).apply {
        description.extras = PersistableBundle().apply { extras.forEach { putBoolean(it, true) } }
    }

    @Test
    fun copiedTextIsSent() {
        assertEquals(CopiedText("Bonjour 👋", sensitive = false), CopiedText.of(text("Bonjour 👋")))
    }

    @Test
    fun passwordsAreMarkedSensitive() {
        assertEquals(
            CopiedText("hunter2", sensitive = true),
            CopiedText.of(text("hunter2", CopiedText.EXTRA_IS_SENSITIVE)),
        )
    }

    @Test
    fun clipsFromAComputerAreNotSentBack() {
        val fromPc = text("from the PC", CopiedText.EXTRA_IS_REMOTE_DEVICE)

        assertFalse(CopiedText.worthReading(fromPc.description))
        assertNull(CopiedText.of(fromPc))
    }

    @Test
    fun htmlCopiesSendTheirText() {
        val html = ClipData.newHtmlText("label", "bold text", "<b>bold</b> text")

        assertTrue(CopiedText.worthReading(html.description))
        assertEquals("bold text", CopiedText.of(html)?.text)
    }

    @Test
    fun imagesAndEmptyClipsAreSkipped() {
        val image = ClipData(ClipDescription("label", arrayOf("image/png")), ClipData.Item(Uri.parse("content://x/1")))

        assertFalse(CopiedText.worthReading(image.description))
        assertNull(CopiedText.of(image))
        assertNull(CopiedText.of(text("")))
    }
}
