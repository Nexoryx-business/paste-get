// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.nexoryx.pasteget

import android.content.ClipData
import android.content.Context
import android.graphics.Bitmap
import android.graphics.ImageDecoder
import android.net.Uri
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.delay
import kotlinx.coroutines.withContext
import uniffi.pasteget_ffi.PastegetException
import uniffi.pasteget_ffi.PhoneClient
import java.io.ByteArrayOutputStream
import java.nio.ByteBuffer

/** Sends text and images to the connected computers. Every function returns a message for the user. */
object Sender {
    private val PNG_SIGNATURE = byteArrayOf(0x89.toByte(), 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a)
    private val JPEG_SIGNATURE = byteArrayOf(0xff.toByte(), 0xd8.toByte(), 0xff.toByte())

    /** How long to wait for a connection when the service was just started. */
    private const val CONNECT_WAIT_MS = 5_000L

    /** Sends the first item of [clip]: its text, or the image it points to. */
    suspend fun send(context: Context, clip: ClipData?): String {
        val item = clip?.takeIf { it.itemCount > 0 }?.getItemAt(0)
            ?: return context.getString(R.string.clipboard_empty)
        val uri = item.uri
        val mimeType = uri?.let { context.contentResolver.getType(it) }
        return when {
            uri != null && mimeType?.startsWith("image/") == true -> sendImage(context, uri)
            item.text != null -> sendText(context, item.text.toString())
            else -> context.getString(R.string.unsupported_content)
        }
    }

    suspend fun sendText(context: Context, text: String): String =
        deliver(context) { it.sendText(text) }

    suspend fun sendImage(context: Context, uri: Uri): String = try {
        val image = withContext(Dispatchers.IO) { readImage(context, uri) }
        deliver(context) { it.sendImage(image) }
    } catch (e: Exception) {
        context.getString(R.string.send_failed, e.message ?: e.javaClass.simpleName)
    }

    private suspend fun deliver(context: Context, send: (PhoneClient) -> UInt): String {
        val client = connectedClient(context)
        val sentTo = try {
            withContext(Dispatchers.IO) { send(client).toInt() }
        } catch (e: PastegetException) {
            return context.getString(R.string.send_failed, e.message)
        }
        return if (sentTo == 0) {
            context.getString(R.string.nothing_connected)
        } else {
            context.resources.getQuantityString(R.plurals.sent_to, sentTo, sentTo)
        }
    }

    /** Makes sure the connection service runs, and gives it a moment to connect if it just started. */
    private suspend fun connectedClient(context: Context): PhoneClient {
        val client = context.pasteGet.client
        if (client.connectedCount() > 0u) return client
        SyncService.start(context)
        var waited = 0L
        while (client.connectedCount() == 0u && waited < CONNECT_WAIT_MS) {
            delay(100)
            waited += 100
        }
        return client
    }

    private fun readImage(context: Context, uri: Uri): ByteArray {
        val bytes = context.contentResolver.openInputStream(uri)?.use { it.readBytes() }
            ?: error("could not read the image")
        return sendable(bytes)
    }

    /**
     * PNG and JPEG bytes as they are (re-encoding a screenshot would cost more time than sending
     * it), other formats (WebP, HEIC…) re-encoded as PNG.
     */
    fun sendable(bytes: ByteArray): ByteArray {
        if (bytes.startsWith(PNG_SIGNATURE) || bytes.startsWith(JPEG_SIGNATURE)) return bytes
        // createSource(ByteBuffer) exists from API 28; the ByteArray overload only from API 31.
        val bitmap = ImageDecoder.decodeBitmap(ImageDecoder.createSource(ByteBuffer.wrap(bytes))) { decoder, _, _ ->
            decoder.allocator = ImageDecoder.ALLOCATOR_SOFTWARE
        }
        return ByteArrayOutputStream().use { out ->
            bitmap.compress(Bitmap.CompressFormat.PNG, 100, out)
            out.toByteArray()
        }
    }
}

private fun ByteArray.startsWith(prefix: ByteArray): Boolean =
    size >= prefix.size && copyOf(prefix.size).contentEquals(prefix)
