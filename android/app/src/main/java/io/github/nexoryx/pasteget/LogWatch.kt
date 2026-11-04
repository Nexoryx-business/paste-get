// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.nexoryx.pasteget

import java.util.Locale

/**
 * What the fallback mode of the automatic copy reads in the system log. No Android APIs here, so
 * the unit tests run on the JVM.
 *
 * When the clipboard changes, Android checks every app listening to it, and logs a line like
 * this one for each app in the background (ClipboardService):
 *
 * `E/ClipboardService( 1234): Denying clipboard access to io.github.nexoryx.pasteget, application is not in focus nor is it a system service for user 0`
 *
 * For paste-get, that line means "the clipboard just changed".
 */
object LogWatch {
    private val DENIAL = Regex("""Denying clipboard access to ([\w.]+)""")

    /** Tags whose lines prove that the system log is readable: the clipboard's, and activity starts. */
    private val SYSTEM_TAGS = listOf("/ClipboardService", "/ActivityTaskManager")

    /**
     * The `logcat` command: ClipboardService's warnings and errors, and ActivityTaskManager's
     * activity starts, from [sinceMillis] (wall clock) on. Activity starts come often, including
     * Android's own "allow access to the logs?" dialog: seeing one proves the log is readable.
     */
    fun command(sinceMillis: Long): List<String> = listOf(
        "logcat", "-b", "system", "-v", "brief",
        "-T", String.format(Locale.ROOT, "%d.%03d", sinceMillis / 1000, sinceMillis % 1000),
        "ClipboardService:W", "ActivityTaskManager:I", "*:S",
    )

    /** Whether [line] says that [packageName] was just denied the clipboard. */
    fun isDenialFor(line: String, packageName: String): Boolean =
        DENIAL.find(line)?.groupValues?.get(1) == packageName

    /** Whether [line] comes from the system (and not from logcat itself, or this app's own log). */
    fun isFromSystem(line: String): Boolean = SYSTEM_TAGS.any { line.contains(it) }
}
