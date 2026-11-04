// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.nexoryx.pasteget

/** Checks on image files. No Android APIs here, so the unit tests run on the JVM. */
object Images {
    private val PNG_SIGNATURE = bytes(0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a)
    private val JPEG_SIGNATURE = bytes(0xff, 0xd8, 0xff)

    /** The last chunk of every PNG: its length (0), "IEND", and its CRC. */
    private val PNG_END = bytes(0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82)

    /** JPEG's "end of image" marker. */
    private val JPEG_END = bytes(0xff, 0xd9)

    fun isPng(image: ByteArray): Boolean = image.startsWith(PNG_SIGNATURE)

    fun isJpeg(image: ByteArray): Boolean = image.startsWith(JPEG_SIGNATURE)

    /**
     * Whether a PNG or JPEG ends the way a fully written file does. `false` for other formats,
     * and for JPEGs with extra data after the image (some phones add their own): those are judged
     * by whether their size stops changing.
     */
    fun looksComplete(image: ByteArray): Boolean = when {
        isPng(image) -> image.endsWith(PNG_END)
        isJpeg(image) -> image.size > JPEG_SIGNATURE.size && image.endsWith(JPEG_END)
        else -> false
    }

    /**
     * Whether a MediaStore `RELATIVE_PATH` is a screenshot folder: `DCIM/Screenshots/` on HyperOS,
     * One UI and others, `Pictures/Screenshots/` on Pixels.
     */
    fun isScreenshotFolder(relativePath: String?): Boolean =
        relativePath.orEmpty().split('/').any { it.equals("Screenshots", ignoreCase = true) }

    private fun ByteArray.startsWith(prefix: ByteArray): Boolean =
        size >= prefix.size && copyOfRange(0, prefix.size).contentEquals(prefix)

    private fun ByteArray.endsWith(suffix: ByteArray): Boolean =
        size >= suffix.size && copyOfRange(size - suffix.size, size).contentEquals(suffix)

    private fun bytes(vararg values: Int) = ByteArray(values.size) { values[it].toByte() }
}
