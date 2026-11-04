// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.nexoryx.pasteget

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

class LogWatchTest {
    private val app = "io.github.nexoryx.pasteget"

    @Test
    fun recognizesTheDenialOfEveryVersion() {
        // Android 12 and later.
        assertTrue(
            LogWatch.isDenialFor(
                "E/ClipboardService( 1610): Denying clipboard access to $app, application is not in focus nor " +
                    "is it a system service for user 0",
                app,
            ),
        )
        // Android 10 and 11.
        assertTrue(
            LogWatch.isDenialFor(
                "E/ClipboardService( 1610): Denying clipboard access to $app, application is not in focus " +
                    "neither is a system service for user 0",
                app,
            ),
        )
        // Another output format.
        assertTrue(
            LogWatch.isDenialFor("11-04 12:00:00.000  1610  1702 E ClipboardService: Denying clipboard access to $app, …", app),
        )
    }

    @Test
    fun otherAppsAndOtherLinesAreIgnored() {
        assertFalse(LogWatch.isDenialFor("E/ClipboardService( 1610): Denying clipboard access to com.other.app, …", app))
        assertFalse(LogWatch.isDenialFor("E/ClipboardService( 1610): Denying clipboard access to $app.debug, …", app))
        assertFalse(LogWatch.isDenialFor("E/ClipboardService( 1610): Denying clipboard access to io.github, …", app))
        assertFalse(LogWatch.isDenialFor("I/ActivityTaskManager( 1610): START u0 {cmp=$app/.MainActivity}", app))
        assertFalse(LogWatch.isDenialFor("", app))
    }

    @Test
    fun systemLinesProveTheLogIsReadable() {
        assertTrue(LogWatch.isFromSystem("I/ActivityTaskManager( 1610): START u0 {cmp=android/.LogAccessDialogActivity}"))
        assertTrue(LogWatch.isFromSystem("E/ClipboardService( 1610): Denying clipboard access to x, …"))
        assertFalse(LogWatch.isFromSystem("--------- beginning of system"))
        assertFalse(LogWatch.isFromSystem("I/paste-get(12345): clip received"))
    }

    @Test
    fun readsOnlyWhatItNeedsFromNowOn() {
        assertEquals(
            listOf(
                "logcat", "-b", "system", "-v", "brief", "-T", "1730721600.042",
                "ClipboardService:W", "ActivityTaskManager:I", "*:S",
            ),
            LogWatch.command(1_730_721_600_042),
        )
    }
}
