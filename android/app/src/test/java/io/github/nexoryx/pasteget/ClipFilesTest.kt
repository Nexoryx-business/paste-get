// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.nexoryx.pasteget

import org.junit.Assert.assertArrayEquals
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Rule
import org.junit.Test
import org.junit.rules.TemporaryFolder
import java.io.File

class ClipFilesTest {
    @get:Rule
    val temp = TemporaryFolder()

    private val text = ClipFiles.TEXT

    private fun names(dir: File) = dir.list().orEmpty().sorted()

    @Test
    fun onlyLongTextsGoThroughAFile() {
        assertFalse(ClipFiles.needsFile(text, 0))
        assertFalse(ClipFiles.needsFile(text, ClipFiles.MAX_INLINE_TEXT))
        assertTrue(ClipFiles.needsFile(text, ClipFiles.MAX_INLINE_TEXT + 1))
    }

    @Test
    fun aSavedTextIsATextFile() {
        val dir = File(temp.root, "clips")
        val file = ClipFiles(dir).save(text, "Bonjour 👋".toByteArray())!!

        assertEquals("clip-1.txt", file.name)
        assertEquals(dir, file.parentFile)
        assertEquals("Bonjour 👋", file.readText())
        assertEquals("text/plain", ClipFiles.clipboardType(text))
    }

    @Test
    fun onlyTheNewestFilesAreKept() {
        val files = ClipFiles(temp.root, keep = 2)

        files.save(text, byteArrayOf(1))
        files.save(text, byteArrayOf(2))
        val newest = files.save(text, byteArrayOf(3))!!

        assertEquals(listOf("clip-2.txt", "clip-3.txt"), names(temp.root))
        assertArrayEquals(byteArrayOf(3), newest.readBytes())
    }

    @Test
    fun numbersAreNotReusedAfterARestart() {
        ClipFiles(temp.root, keep = 1).save(text, byteArrayOf(1))
        ClipFiles(temp.root, keep = 1).save(text, byteArrayOf(2))

        assertEquals(listOf("clip-2.txt"), names(temp.root))
    }

    @Test
    fun otherFilesAreLeftAlone() {
        File(temp.root, "notes.txt").writeText("not a clip")
        File(temp.root, "clip-x.txt").writeText("not numbered")

        ClipFiles(temp.root, keep = 1).save(text, byteArrayOf(1))

        assertEquals(listOf("clip-1.txt", "clip-x.txt", "notes.txt"), names(temp.root))
    }

    @Test
    fun typesWithoutAFileFormAreNotSaved() {
        assertNull(ClipFiles(temp.root).save("image/gif", byteArrayOf(1)))
        assertNull(ClipFiles.clipboardType("image/gif"))
        assertTrue(names(temp.root).isEmpty())
    }
}
