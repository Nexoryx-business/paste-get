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

    @Test
    fun theHelperIsStartedFromTheApkInItsOwnSession() {
        val command = ShellCommands.helperLaunch(
            "/data/app/~~a1==/io.github.nexoryx.pasteget-b2==/base.apk",
            "pasteget.shell.10234",
            10234,
            1_700_000_000_000,
        )

        assertEquals(
            "if command -v setsid >/dev/null 2>&1; then " +
                "CLASSPATH='/data/app/~~a1==/io.github.nexoryx.pasteget-b2==/base.apk' setsid " +
                "app_process /system/bin --nice-name=pasteget_shell io.github.nexoryx.pasteget.ShellHelper " +
                "'pasteget.shell.10234' 10234 1700000000000; " +
                "else CLASSPATH='/data/app/~~a1==/io.github.nexoryx.pasteget-b2==/base.apk' nohup " +
                "app_process /system/bin --nice-name=pasteget_shell io.github.nexoryx.pasteget.ShellHelper " +
                "'pasteget.shell.10234' 10234 1700000000000; fi </dev/null >/dev/null 2>&1 &",
            command,
        )
    }

    @Test
    fun quotingKeepsEverythingLiteral() {
        assertEquals("'plain'", ShellCommands.quote("plain"))
        assertEquals("'it'\\''s; rm -rf \$HOME'", ShellCommands.quote("it's; rm -rf \$HOME"))
        // Checked with a real shell.
        val printed = ProcessBuilder("sh", "-c", "printf %s " + ShellCommands.quote("it's \$HOME `id`"))
            .start().inputStream.bufferedReader().readText()
        assertEquals("it's \$HOME `id`", printed)
    }
}
