// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.nexoryx.pasteget

import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

class ImagesTest {
    private fun bytes(vararg values: Int) = ByteArray(values.size) { values[it].toByte() }

    private val pngStart = bytes(0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a)
    private val pngEnd = bytes(0, 0, 0, 0, 0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82)

    @Test
    fun aPngIsCompleteOnceItsLastChunkIsWritten() {
        val chunks = bytes(1, 2, 3, 4)

        assertTrue(Images.looksComplete(pngStart + chunks + pngEnd))
        assertFalse(Images.looksComplete(pngStart + chunks))
        assertFalse(Images.looksComplete(pngStart + chunks + pngEnd.copyOf(6)))
    }

    @Test
    fun aJpegIsCompleteOnceItsEndMarkerIsWritten() {
        val jpeg = bytes(0xff, 0xd8, 0xff, 0xe0, 0x10, 0x20)

        assertTrue(Images.looksComplete(jpeg + bytes(0xff, 0xd9)))
        assertFalse(Images.looksComplete(jpeg))
        // Extra data after the end marker: left to the size check.
        assertFalse(Images.looksComplete(jpeg + bytes(0xff, 0xd9, 0x53, 0x45)))
    }

    @Test
    fun otherFilesAreNeverJudgedComplete() {
        assertFalse(Images.looksComplete(ByteArray(0)))
        assertFalse(Images.looksComplete("GIF89a".toByteArray()))
        assertFalse(Images.looksComplete(bytes(0xff, 0xd8, 0xff)))
    }

    @Test
    fun recognizesScreenshotFolders() {
        assertTrue(Images.isScreenshotFolder("DCIM/Screenshots/"))
        assertTrue(Images.isScreenshotFolder("Pictures/Screenshots/"))
        assertTrue(Images.isScreenshotFolder("Pictures/screenshots"))

        assertFalse(Images.isScreenshotFolder("DCIM/Camera/"))
        assertFalse(Images.isScreenshotFolder("Pictures/Screenshots of friends/"))
        assertFalse(Images.isScreenshotFolder("Download/"))
        assertFalse(Images.isScreenshotFolder(null))
    }
}
