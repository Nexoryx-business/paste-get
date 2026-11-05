// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.nexoryx.pasteget

import java.io.DataInputStream
import java.io.DataOutputStream
import java.io.IOException

/**
 * The messages between the app and [ShellHelper], the shell process the built-in mode starts
 * through wireless debugging. They go over a local socket that each side authenticates by the
 * other's Linux UID ([ShellLinkServer], [ShellHelper]).
 *
 * Each message is an `int` type, then its fields; strings are an `int` byte count and UTF-8.
 * No Android APIs here, so the unit tests run on the JVM.
 */
object ShellLink {
    /** Raise when a message changes: the app then replaces a helper left running. */
    const val VERSION = 1

    /** Longest text carried. Longer copies are skipped, as the computers wouldn't take them. */
    const val MAX_TEXT_BYTES = 16 * 1024 * 1024

    private const val HELLO = 1
    private const val WATCHING = 2
    private const val COPY = 3
    private const val FAILED = 4
    private const val WATCH = 10
    private const val EXIT = 11

    /** What the helper sends. */
    sealed interface FromHelper {
        /** First message. [build] is the app build that started it, so an old helper can be told to go. */
        data class Hello(val version: Int, val build: Long) : FromHelper

        /** The clipboard is watched, [how] tells which hidden methods, for logs. */
        data class Watching(val how: String) : FromHelper

        data class Copy(val copy: CopiedText) : FromHelper {
            // Never print the text.
            override fun toString() = "Copy(${copy.text.length} chars, sensitive=${copy.sensitive})"
        }

        data class Failed(val reason: String) : FromHelper
    }

    /** What the app sends. */
    sealed interface ToHelper {
        data class Watch(val userId: Int) : ToHelper

        data object Exit : ToHelper
    }

    fun write(out: DataOutputStream, message: FromHelper) {
        when (message) {
            is FromHelper.Hello -> {
                out.writeInt(HELLO)
                out.writeInt(message.version)
                out.writeLong(message.build)
            }
            is FromHelper.Watching -> {
                out.writeInt(WATCHING)
                writeString(out, message.how)
            }
            is FromHelper.Copy -> {
                out.writeInt(COPY)
                out.writeBoolean(message.copy.sensitive)
                writeString(out, message.copy.text)
            }
            is FromHelper.Failed -> {
                out.writeInt(FAILED)
                writeString(out, message.reason)
            }
        }
        out.flush()
    }

    fun write(out: DataOutputStream, message: ToHelper) {
        when (message) {
            is ToHelper.Watch -> {
                out.writeInt(WATCH)
                out.writeInt(message.userId)
            }
            ToHelper.Exit -> out.writeInt(EXIT)
        }
        out.flush()
    }

    fun readFromHelper(input: DataInputStream): FromHelper = when (val type = input.readInt()) {
        HELLO -> FromHelper.Hello(input.readInt(), input.readLong())
        WATCHING -> FromHelper.Watching(readString(input))
        COPY -> {
            val sensitive = input.readBoolean()
            FromHelper.Copy(CopiedText(readString(input), sensitive))
        }
        FAILED -> FromHelper.Failed(readString(input))
        else -> throw IOException("unknown message $type from the shell helper")
    }

    fun readToHelper(input: DataInputStream): ToHelper = when (val type = input.readInt()) {
        WATCH -> ToHelper.Watch(input.readInt())
        EXIT -> ToHelper.Exit
        else -> throw IOException("unknown message $type from the app")
    }

    private fun writeString(out: DataOutputStream, text: String) {
        val bytes = text.encodeToByteArray()
        if (bytes.size > MAX_TEXT_BYTES) throw IOException("text too long: ${bytes.size} bytes")
        out.writeInt(bytes.size)
        out.write(bytes)
    }

    private fun readString(input: DataInputStream): String {
        val size = input.readInt()
        if (size !in 0..MAX_TEXT_BYTES) throw IOException("bad text size $size")
        val bytes = ByteArray(size)
        input.readFully(bytes)
        return bytes.decodeToString()
    }
}
