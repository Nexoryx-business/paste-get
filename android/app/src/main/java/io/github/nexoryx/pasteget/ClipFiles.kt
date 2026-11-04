// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.nexoryx.pasteget

import java.io.File
import java.io.IOException

/**
 * Files for clips from a computer that go on the clipboard as a `content://` link to their bytes
 * rather than as the bytes themselves (see [ReceivedClips]). No Android APIs here, so the unit tests
 * run on the JVM.
 *
 * Files are numbered in the order they are saved, and only the newest [keep] are kept. Once the
 * clipboard holds something else, Android takes other apps' access to the old link away, so older
 * files are no use. The few before the newest stay for an app that is still opening one.
 */
class ClipFiles(private val dir: File, private val keep: Int = KEEP) {
    init {
        require(keep >= 1) { "keep at least the newest file" }
    }

    /**
     * Writes [data] to a new file, deletes the oldest files beyond [keep], and returns the new
     * file. `null` if [mimeType] has no file form.
     */
    @Throws(IOException::class)
    fun save(mimeType: String, data: ByteArray): File? {
        val kind = KINDS[mimeType] ?: return null
        if (!dir.isDirectory && !dir.mkdirs()) throw IOException("cannot create $dir")
        val existing = numbered()
        val number = (existing.lastOrNull()?.first ?: 0L) + 1
        val file = File(dir, "$PREFIX$number.${kind.extension}")
        try {
            file.writeBytes(data)
        } catch (e: IOException) {
            file.delete()
            throw e
        }
        existing.dropLast(keep - 1).forEach { (_, old) -> old.delete() }
        return file
    }

    /** The clip files in [dir], oldest first, with their numbers. */
    private fun numbered(): List<Pair<Long, File>> =
        dir.listFiles().orEmpty()
            .mapNotNull { file -> numberOf(file.name)?.let { it to file } }
            .sortedBy { it.first }

    private fun numberOf(name: String): Long? =
        name.takeIf { it.startsWith(PREFIX) }?.substring(PREFIX.length)?.substringBefore('.')?.toLongOrNull()

    /** How a clip type is stored: the extension tells Android's FileProvider what type to report. */
    private class Kind(val extension: String, val clipboardType: String)

    companion object {
        const val TEXT = "text/plain;charset=utf-8"
        const val PNG = "image/png"
        const val JPEG = "image/jpeg"

        /** The newest file and the two before it. */
        const val KEEP = 3

        /**
         * Longest text, in UTF-8 bytes, put on the clipboard as it is. Clips pass through Binder,
         * whose buffer is 1 MB per process, and this text takes at most twice its size in UTF-16.
         */
        const val MAX_INLINE_TEXT = 256 * 1024

        private const val PREFIX = "clip-"

        private val KINDS = mapOf(
            TEXT to Kind("txt", "text/plain"),
            PNG to Kind("png", PNG),
            JPEG to Kind("jpg", JPEG),
        )

        /** Whether a clip of [size] bytes goes on the clipboard as a file link. */
        fun needsFile(mimeType: String, size: Int): Boolean = mimeType != TEXT || size > MAX_INLINE_TEXT

        /** The type the clipboard announces for a file of [mimeType], or `null` if it has no file form. */
        fun clipboardType(mimeType: String): String? = KINDS[mimeType]?.clipboardType
    }
}
