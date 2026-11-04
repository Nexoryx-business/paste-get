// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.nexoryx.pasteget

import android.content.ContentUris
import android.content.Context
import android.database.ContentObserver
import android.net.Uri
import android.os.SystemClock
import android.provider.MediaStore
import android.util.Log
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.cancel
import kotlinx.coroutines.channels.Channel
import kotlinx.coroutines.delay
import kotlinx.coroutines.launch

/**
 * Watches MediaStore for new screenshots and hands each one, once fully written, to [onScreenshot]
 * with the [SystemClock.elapsedRealtime] at which MediaStore announced it. Needs full access to
 * photos (see [PhotoAccess]).
 *
 * Screenshot apps either insert a pending entry (`IS_PENDING = 1`), write it and publish it, or
 * write a file that MediaStore picks up. Queries only return published entries; a file that is
 * still being written is read again until it [looks complete][Images.looksComplete] or its size
 * stops changing.
 */
class ScreenshotWatcher(
    context: Context,
    private val onScreenshot: suspend (image: ByteArray, announcedAtMs: Long) -> Unit,
) {
    private val resolver = context.contentResolver
    private val changes = Channel<Long>(Channel.CONFLATED)
    private val observer = object : ContentObserver(null) {
        override fun onChange(selfChange: Boolean) {
            changes.trySend(SystemClock.elapsedRealtime())
        }
    }
    private var scope: CoroutineScope? = null

    /** Entries that are already sent or aren't screenshots. */
    private val handled = object : LinkedHashSet<Long>() {
        override fun add(element: Long): Boolean {
            if (size >= MAX_HANDLED) remove(first())
            return super.add(element)
        }
    }

    /** Screenshots that existed before the watcher started are never sent. */
    private var baseline = 0L

    fun start() {
        if (scope != null) return
        val running = CoroutineScope(SupervisorJob() + Dispatchers.IO)
        scope = running
        resolver.registerContentObserver(COLLECTION, true, observer)
        running.launch {
            baseline = newestId()
            for (changedAtMs in changes) look(changedAtMs)
        }
        Log.i(TAG, "watching for screenshots")
    }

    fun stop() {
        val running = scope ?: return
        scope = null
        resolver.unregisterContentObserver(observer)
        running.cancel()
        Log.i(TAG, "stopped watching for screenshots")
    }

    private suspend fun look(changedAtMs: Long) {
        for (entry in recentEntries()) {
            if (!handled.add(entry.id) || !Images.isScreenshotFolder(entry.relativePath)) continue
            val image = readWhenComplete(ContentUris.withAppendedId(COLLECTION, entry.id)) ?: continue
            try {
                onScreenshot(image, changedAtMs)
            } catch (e: Exception) {
                Log.w(TAG, "screenshot not sent: ${e.message}")
            }
        }
    }

    private class Entry(val id: Long, val relativePath: String?)

    /** Published images added in the last minute, oldest first. */
    private fun recentEntries(): List<Entry> {
        val projection = arrayOf(MediaStore.MediaColumns._ID, MediaStore.MediaColumns.RELATIVE_PATH)
        val selection = "${MediaStore.MediaColumns._ID} > ? AND ${MediaStore.MediaColumns.DATE_ADDED} >= ? " +
            "AND ${MediaStore.MediaColumns.IS_PENDING} = 0"
        val since = System.currentTimeMillis() / 1000 - RECENT_SECONDS
        val arguments = arrayOf(baseline.toString(), since.toString())
        val entries = mutableListOf<Entry>()
        resolver.query(COLLECTION, projection, selection, arguments, "${MediaStore.MediaColumns._ID} ASC")
            ?.use { cursor ->
                while (cursor.moveToNext()) entries += Entry(cursor.getLong(0), cursor.getString(1))
            }
        return entries
    }

    private fun newestId(): Long {
        val projection = arrayOf(MediaStore.MediaColumns._ID)
        return resolver.query(COLLECTION, projection, null, null, "${MediaStore.MediaColumns._ID} DESC")
            ?.use { cursor -> if (cursor.moveToFirst()) cursor.getLong(0) else 0L }
            ?: 0L
    }

    private suspend fun readWhenComplete(uri: Uri): ByteArray? {
        var previousSize = -1
        repeat(MAX_READS) {
            val image = try {
                resolver.openInputStream(uri)?.use { it.readBytes() }
            } catch (e: Exception) {
                null // Deleted already, or not readable yet.
            }
            if (image != null && image.isNotEmpty()) {
                if (Images.looksComplete(image) || image.size == previousSize) return image
                previousSize = image.size
            }
            delay(REREAD_DELAY_MS)
        }
        Log.w(TAG, "a screenshot never finished writing")
        return null
    }

    private companion object {
        const val TAG = "paste-get"
        val COLLECTION: Uri = MediaStore.Images.Media.EXTERNAL_CONTENT_URI
        const val RECENT_SECONDS = 60
        const val MAX_HANDLED = 200
        const val MAX_READS = 10
        const val REREAD_DELAY_MS = 300L
    }
}
