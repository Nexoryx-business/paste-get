// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.nexoryx.pasteget

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertThrows
import org.junit.Test
import java.io.ByteArrayInputStream
import java.io.ByteArrayOutputStream
import java.io.DataInputStream
import java.io.DataOutputStream
import java.io.IOException

class ShellLinkTest {
    private fun bytes(write: (DataOutputStream) -> Unit): ByteArray =
        ByteArrayOutputStream().also { write(DataOutputStream(it)) }.toByteArray()

    private fun input(bytes: ByteArray) = DataInputStream(ByteArrayInputStream(bytes))

    @Test
    fun messagesFromTheHelperRoundTrip() {
        val messages = listOf(
            ShellLink.FromHelper.Hello(ShellLink.VERSION, 1_700_000_000_000),
            ShellLink.FromHelper.Watching("addPrimaryClipChangedListener(String, int)"),
            ShellLink.FromHelper.Copy(CopiedText("Bonjour 👋\nsecond line", sensitive = false)),
            ShellLink.FromHelper.Copy(CopiedText("hunter2", sensitive = true)),
            ShellLink.FromHelper.Failed("unknown signature"),
        )
        val stream = input(bytes { out -> messages.forEach { ShellLink.write(out, it) } })

        for (message in messages) assertEquals(message, ShellLink.readFromHelper(stream))
    }

    @Test
    fun messagesFromTheAppRoundTrip() {
        val stream = input(
            bytes { out ->
                ShellLink.write(out, ShellLink.ToHelper.Watch(10))
                ShellLink.write(out, ShellLink.ToHelper.Exit)
            },
        )

        assertEquals(ShellLink.ToHelper.Watch(10), ShellLink.readToHelper(stream))
        assertEquals(ShellLink.ToHelper.Exit, ShellLink.readToHelper(stream))
    }

    @Test
    fun copiesNeverPrintTheirText() {
        val copy = ShellLink.FromHelper.Copy(CopiedText("my password", sensitive = true))

        assertFalse(copy.toString().contains("password"))
    }

    @Test
    fun garbageIsRefused() {
        assertThrows(IOException::class.java) { ShellLink.readFromHelper(input(bytes { it.writeInt(99) })) }
        assertThrows(IOException::class.java) { ShellLink.readToHelper(input(bytes { it.writeInt(1) })) }
        // A text size that would need a huge buffer.
        val huge = bytes {
            it.writeInt(4) // FAILED
            it.writeInt(Int.MAX_VALUE)
        }
        assertThrows(IOException::class.java) { ShellLink.readFromHelper(input(huge)) }
        // Cut short.
        val cut = bytes { ShellLink.write(it, ShellLink.FromHelper.Failed("abc")) }.dropLast(1).toByteArray()
        assertThrows(IOException::class.java) { ShellLink.readFromHelper(input(cut)) }
    }
}
