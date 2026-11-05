// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.nexoryx.pasteget

import android.app.Application
import android.content.ClipData
import android.content.ClipboardManager
import androidx.core.content.FileProvider
import org.junit.Assert.assertArrayEquals
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Before
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.annotation.Config
import org.robolectric.RuntimeEnvironment
import java.io.File
import java.util.concurrent.TimeUnit

/**
 * Roadmap M6 on the phone's side: what a clip from a computer becomes on the clipboard, read back
 * the way an app that pastes reads it. On Android 10 (the oldest the app runs on), 15 (the POCO
 * M8 5G) and 16.
 */
@RunWith(RobolectricTestRunner::class)
@Config(sdk = [29, 35, 36])
class ReceivedClipsTest {
    private val app: Application = RuntimeEnvironment.getApplication()
    private val clipboard = app.getSystemService(ClipboardManager::class.java)

    /**
     * FileProvider remembers each authority's folders in a static map, and every Robolectric test
     * has a data directory of its own: forget the previous test's. (On a phone, it never changes.)
     */
    @Before
    fun forgetThePreviousTestsFolders() {
        FileProvider::class.java.getDeclaredField("sCache").apply { isAccessible = true }
            .let { (it.get(null) as MutableMap<*, *>).clear() }
    }

    private val png = byteArrayOf(0x89.toByte(), 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 1, 2, 3)
    private val jpeg = byteArrayOf(0xff.toByte(), 0xd8.toByte(), 0xff.toByte(), 0xe0.toByte(), 4, 5, 6)

    /**
     * Applies a clip and waits until it's handled: [ReceivedClips] works on a thread of its own.
     * (Not by watching [ReceivedClips.last]: two clips of the same type and size in the same
     * millisecond look alike, and a `StateFlow` ignores a value equal to the one it has.)
     */
    private fun receive(mimeType: String, data: ByteArray, sensitive: Boolean = false): LastReceived {
        ReceivedClips.apply(app, mimeType, data, sensitive).get(5, TimeUnit.SECONDS)
        return ReceivedClips.last.value!!
    }

    private val clip: ClipData
        get() = clipboard.primaryClip!!

    private fun extra(name: String) = clip.description.extras?.getBoolean(name) == true

    private fun linkedBytes(): ByteArray =
        app.contentResolver.openInputStream(clip.getItemAt(0).uri)!!.use { it.readBytes() }

    @Test
    fun textGoesOnTheClipboardAsItIs() {
        val received = receive(ClipFiles.TEXT, "Bonjour 👋".toByteArray())

        assertTrue(received.onClipboard)
        assertEquals("Bonjour 👋", clip.getItemAt(0).text)
        assertNull(clip.getItemAt(0).uri)
        assertEquals("text/plain", clip.description.getMimeType(0))
        assertEquals("paste-get", clip.description.label)
        // Marked as coming from another device: the automatic copy won't send it back.
        assertTrue(extra(CopiedText.EXTRA_IS_REMOTE_DEVICE))
        assertFalse(extra(CopiedText.EXTRA_IS_SENSITIVE))
        assertNull(CopiedText.of(clip))
    }

    @Test
    fun secretsAreMarkedSensitive() {
        receive(ClipFiles.TEXT, "hunter2".toByteArray(), sensitive = true)

        assertTrue(extra(CopiedText.EXTRA_IS_SENSITIVE))
    }

    @Test
    fun aLongTextGoesAsALinkThatPastesAsText() {
        val text = "ligne de journal, encore une 👋\n".repeat(20_000)
        assertTrue(text.toByteArray().size > ClipFiles.MAX_INLINE_TEXT)

        assertTrue(receive(ClipFiles.TEXT, text.toByteArray()).onClipboard)

        val item = clip.getItemAt(0)
        assertNull(item.text)
        assertEquals("content", item.uri.scheme)
        assertEquals("text/plain", clip.description.getMimeType(0))
        // What an EditText does when the user pastes.
        assertEquals(text, item.coerceToText(app).toString())
    }

    @Test
    fun aPngGoesAsALinkToTheSameBytes() {
        assertTrue(receive(ClipFiles.PNG, png).onClipboard)

        val uri = clip.getItemAt(0).uri
        assertEquals("${app.packageName}.clips", uri.authority)
        assertEquals("image/png", clip.description.getMimeType(0))
        assertEquals("image/png", app.contentResolver.getType(uri))
        assertArrayEquals(png, linkedBytes())
        assertTrue(extra(CopiedText.EXTRA_IS_REMOTE_DEVICE))
        // Not text: the automatic copy doesn't even read it.
        assertFalse(CopiedText.worthReading(clip.description))
    }

    @Test
    fun aJpegStaysAJpeg() {
        assertTrue(receive(ClipFiles.JPEG, jpeg).onClipboard)

        assertEquals("image/jpeg", clip.description.getMimeType(0))
        assertEquals("image/jpeg", app.contentResolver.getType(clip.getItemAt(0).uri))
        assertArrayEquals(jpeg, linkedBytes())
    }

    @Test
    fun theNewestClipWinsAndOldFilesGo() {
        repeat(5) { receive(ClipFiles.PNG, png + byteArrayOf(it.toByte())) }

        assertArrayEquals(png + byteArrayOf(4), linkedBytes())
        val files = File(app.cacheDir, "clips").list().orEmpty()
        assertEquals(ClipFiles.KEEP, files.size)
    }

    @Test
    fun theSameClipTwiceInARowIsHandledTwice() {
        // Both may land in the same millisecond: then [ReceivedClips.last] doesn't change.
        receive(ClipFiles.TEXT, "again".toByteArray())
        clipboard.setPrimaryClip(ClipData.newPlainText("other app", "meanwhile"))

        assertTrue(receive(ClipFiles.TEXT, "again".toByteArray()).onClipboard)
        assertEquals("again", clip.getItemAt(0).text)
    }

    @Test
    fun unknownTypesLeaveTheClipboardAlone() {
        receive(ClipFiles.TEXT, "before".toByteArray())

        val received = receive("image/gif", "GIF89a".toByteArray())

        assertFalse(received.onClipboard)
        assertEquals("before", clip.getItemAt(0).text)
    }

    @Test
    fun theChangeTheAppCausesIsKnownToBeItsOwn() {
        receive(ClipFiles.TEXT, "from the PC".toByteArray())

        assertTrue(ReceivedClips.justApplied())
        assertNotNull(ReceivedClips.last.value)
        assertEquals(ClipFiles.TEXT, ReceivedClips.last.value?.mimeType)
    }
}
