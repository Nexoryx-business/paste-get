// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.nexoryx.pasteget

import android.net.LocalSocket
import android.net.LocalSocketAddress
import android.os.Process
import android.os.SystemClock
import android.util.Log
import androidx.annotation.Keep
import java.io.DataInputStream
import java.io.DataOutputStream
import java.io.IOException
import java.util.concurrent.Executors
import kotlin.system.exitProcess

/**
 * The built-in mode's shell process: what Shizuku's service does for mode A, started by the app
 * itself through wireless debugging ([BuiltInShell]), with
 * `app_process /system/bin io.github.nexoryx.pasteget.ShellHelper <socket> <app uid> <build>`.
 *
 * Like [ShellClipboardService], which it uses, it runs as the shell user, with none of the app
 * around it. It connects to the app's local socket ([ShellLinkServer]) and only talks to it if
 * the socket belongs to the app's UID, so another app can't pose as paste-get to get the copies.
 * If the app goes away (Android may stop it), it keeps trying to reach it for a while, then ends.
 */
@Keep
object ShellHelper {
    private const val TAG = "paste-get"
    private const val RETRY_MS = 3_000L

    /** How long it waits for the app to come back before ending. */
    private const val GIVE_UP_MS = 30 * 60 * 1000L

    @JvmStatic
    fun main(args: Array<String>) {
        val socketName = args.getOrNull(0)
        val appUid = args.getOrNull(1)?.toIntOrNull()
        val build = args.getOrNull(2)?.toLongOrNull()
        if (socketName == null || appUid == null || build == null) {
            System.err.println("usage: ShellHelper <socket> <app uid> <build>")
            exitProcess(2)
        }
        Log.i(TAG, "shell helper started as uid ${Process.myUid()}")
        Helper(socketName, appUid, build).run()
    }

    private class Helper(private val socketName: String, private val appUid: Int, private val build: Long) {
        private val clipboard = ShellClipboardService(null)
        private val reader = Executors.newSingleThreadExecutor()

        @Volatile
        private var out: DataOutputStream? = null
        private var userId = 0

        private val changed = object : IClipboardChanged.Stub() {
            override fun changed() {
                reader.execute(::readNewCopy)
            }
        }

        /** When the app was last there. */
        private var lastConnected = SystemClock.elapsedRealtime()

        fun run() {
            while (SystemClock.elapsedRealtime() - lastConnected < GIVE_UP_MS) {
                try {
                    serve()
                } catch (e: IOException) {
                    Log.i(TAG, "shell helper: app not reachable (${e.message})")
                }
                out = null
                SystemClock.sleep(RETRY_MS)
            }
            Log.i(TAG, "shell helper: the app didn't come back, ending")
            exitProcess(0)
        }

        /** One connection to the app, until it closes. */
        private fun serve() {
            LocalSocket().use { socket ->
                socket.connect(LocalSocketAddress(socketName, LocalSocketAddress.Namespace.ABSTRACT))
                val owner = socket.peerCredentials.uid
                if (owner != appUid) throw IOException("the socket belongs to uid $owner, not the app")
                val output = DataOutputStream(socket.outputStream.buffered())
                val input = DataInputStream(socket.inputStream.buffered())
                synchronized(this) { ShellLink.write(output, ShellLink.FromHelper.Hello(ShellLink.VERSION, build)) }
                out = output
                try {
                    while (true) {
                        when (val message = ShellLink.readToHelper(input)) {
                            is ShellLink.ToHelper.Watch -> watch(message.userId)
                            ShellLink.ToHelper.Exit -> {
                                Log.i(TAG, "shell helper: the app asked it to end")
                                exitProcess(0)
                            }
                        }
                    }
                } finally {
                    // The wait for the app to come back starts now.
                    lastConnected = SystemClock.elapsedRealtime()
                }
            }
        }

        private fun watch(user: Int) {
            userId = user
            val reply = try {
                ShellLink.FromHelper.Watching(clipboard.watch(changed, user))
            } catch (e: RuntimeException) {
                // Unknown hidden API on this phone: the app falls back to another mode.
                ShellLink.FromHelper.Failed(e.message ?: e.javaClass.simpleName)
            }
            send(reply)
        }

        /** Never logs the clip: the app does, with its size only. */
        private fun readNewCopy() {
            try {
                // A description first: reading it doesn't make Android say the clipboard was read.
                val description = clipboard.describe(userId) ?: return
                if (!CopiedText.worthReading(description)) return
                val copy = clipboard.read(userId)?.let(CopiedText::of) ?: return
                if (copy.text.length > ShellLink.MAX_TEXT_BYTES / 4) return
                send(ShellLink.FromHelper.Copy(copy))
            } catch (e: RuntimeException) {
                Log.w(TAG, "shell helper: copy not read: ${e.javaClass.simpleName}")
            }
        }

        private fun send(message: ShellLink.FromHelper) {
            val output = out ?: return
            try {
                synchronized(this) { ShellLink.write(output, message) }
            } catch (e: IOException) {
                // The connection broke: serve() notices and reconnects.
                Log.i(TAG, "shell helper: could not reach the app: ${e.message}")
            }
        }
    }
}
