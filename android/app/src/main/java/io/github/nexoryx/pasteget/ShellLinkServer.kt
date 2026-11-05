// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.nexoryx.pasteget

import android.net.LocalServerSocket
import android.net.LocalSocket
import android.net.LocalSocketAddress
import android.util.Log
import java.io.DataInputStream
import java.io.DataOutputStream
import java.io.IOException
import kotlin.concurrent.thread

/**
 * The app's end of the link with [ShellHelper]: a local socket in Linux's abstract namespace.
 *
 * Any app may connect to such a socket, so each connection is checked: only a process of the
 * shell user (or root) running the helper of this very build is listened to. One helper at a
 * time; a newer one replaces the older.
 */
class ShellLinkServer(
    val socketName: String,
    private val build: Long,
    private val userId: Int,
    private val events: Events,
) {
    interface Events {
        /** Called on a background thread, like the others. */
        fun watching(how: String)

        fun copied(copy: CopiedText)

        fun failed(reason: String)

        /** The helper went away, or was the wrong build and was told to end. */
        fun disconnected()
    }

    private var server: LocalServerSocket? = null

    @Volatile
    private var running = false

    @Volatile
    private var current: Connection? = null

    val connected: Boolean
        get() = current != null

    /** Starts listening. Throws if the name is taken (another copy of the app holds it). */
    @Synchronized
    fun start() {
        if (running) return
        server = LocalServerSocket(socketName)
        running = true
        thread(name = "shell-link", isDaemon = true) { acceptLoop() }
    }

    /** Asks the helper to end, then stops listening. */
    @Synchronized
    fun stop() {
        if (!running) return
        running = false
        current?.send(ShellLink.ToHelper.Exit)
        current?.close()
        current = null
        // close() doesn't wake a blocked accept(): a connection does.
        runCatching { LocalSocket().use { it.connect(LocalSocketAddress(socketName, LocalSocketAddress.Namespace.ABSTRACT)) } }
        runCatching { server?.close() }
        server = null
    }

    private fun acceptLoop() {
        while (running) {
            val socket = try {
                server?.accept() ?: return
            } catch (e: IOException) {
                if (running) Log.w(TAG, "shell link: accept failed: ${e.message}")
                return
            }
            if (!running) {
                socket.close()
                return
            }
            val uid = runCatching { socket.peerCredentials.uid }.getOrDefault(-1)
            if (uid != SHELL_UID && uid != ROOT_UID) {
                Log.w(TAG, "shell link: refused a connection from uid $uid")
                socket.close()
                continue
            }
            thread(name = "shell-link-reader", isDaemon = true) { serve(Connection(socket)) }
        }
    }

    private fun serve(connection: Connection) {
        try {
            val hello = ShellLink.readFromHelper(connection.input)
            if (hello !is ShellLink.FromHelper.Hello || hello.version != ShellLink.VERSION || hello.build != build) {
                // A helper left over from another version of the app.
                Log.i(TAG, "shell link: an old helper connected, telling it to end")
                connection.send(ShellLink.ToHelper.Exit)
                return
            }
            val previous = synchronized(this) {
                if (!running) return
                current.also { current = connection }
            }
            previous?.send(ShellLink.ToHelper.Exit)
            previous?.close()
            connection.send(ShellLink.ToHelper.Watch(userId))
            while (true) {
                when (val message = ShellLink.readFromHelper(connection.input)) {
                    is ShellLink.FromHelper.Watching -> events.watching(message.how)
                    is ShellLink.FromHelper.Copy -> events.copied(message.copy)
                    is ShellLink.FromHelper.Failed -> events.failed(message.reason)
                    is ShellLink.FromHelper.Hello -> throw IOException("unexpected hello")
                }
            }
        } catch (e: IOException) {
            Log.i(TAG, "shell link: helper disconnected (${e.message})")
        } finally {
            connection.close()
            val wasCurrent = synchronized(this) {
                (current === connection).also { if (it) current = null }
            }
            if (wasCurrent) events.disconnected()
        }
    }

    private class Connection(private val socket: LocalSocket) {
        val input = DataInputStream(socket.inputStream.buffered())
        private val output = DataOutputStream(socket.outputStream.buffered())

        fun send(message: ShellLink.ToHelper) {
            try {
                synchronized(output) { ShellLink.write(output, message) }
            } catch (e: IOException) {
                Log.i(TAG, "shell link: could not reach the helper: ${e.message}")
            }
        }

        fun close() {
            runCatching { socket.close() }
        }
    }

    private companion object {
        const val TAG = "paste-get"
        const val SHELL_UID = 2000
        const val ROOT_UID = 0
    }
}
