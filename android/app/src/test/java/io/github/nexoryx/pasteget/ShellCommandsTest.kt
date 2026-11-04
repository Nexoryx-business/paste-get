// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.nexoryx.pasteget

import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

class ShellCommandsTest {
    @Test
    fun theOverlayIsGrantedBeforeTheLogPermissionRestartsTheApp() {
        assertEquals(
            listOf(
                listOf("appops", "set", "io.github.nexoryx.pasteget", "SYSTEM_ALERT_WINDOW", "allow"),
                listOf("pm", "grant", "io.github.nexoryx.pasteget", "android.permission.READ_LOGS"),
            ),
            ShellCommands.fallbackGrants("io.github.nexoryx.pasteget"),
        )
    }

    @Test
    fun successIsQuiet() {
        assertNull(ShellCommands.run(listOf("true")))
        assertEquals("", ShellCommands.runAll(listOf(listOf("true"), listOf("true"))))
    }

    @Test
    fun failuresSayWhichCommandAndWhatItPrinted() {
        val failed = ShellCommands.runAll(
            listOf(
                listOf("sh", "-c", "echo Unknown permission >&2; exit 255"),
                listOf("true"),
                listOf("false"),
            ),
        )

        assertEquals("sh -c echo Unknown permission >&2; exit 255: Unknown permission; false: failed", failed)
    }

    @Test
    fun aMissingCommandIsAFailureToo() {
        assertTrue(ShellCommands.run(listOf("no-such-command-here"))!!.startsWith("no-such-command-here: "))
    }
}
