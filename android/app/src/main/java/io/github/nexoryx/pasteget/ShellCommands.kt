// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.nexoryx.pasteget

import java.util.concurrent.TimeUnit

/**
 * Commands [ShellClipboardService] runs as the shell user, as a user would type them in
 * `adb shell`. No Android APIs here, so the unit tests run on the JVM.
 */
object ShellCommands {
    private const val TIMEOUT_S = 10L

    /**
     * What the fallback mode needs, granted to [app]. `appops` comes first: granting a permission
     * that carries a Linux group (READ_LOGS: `log`) makes Android restart the app.
     */
    fun fallbackGrants(app: String): List<List<String>> = listOf(
        listOf("appops", "set", app, "SYSTEM_ALERT_WINDOW", "allow"),
        listOf("pm", "grant", app, "android.permission.READ_LOGS"),
    )

    /** Runs [commands] in order. Returns what failed, or an empty string. */
    fun runAll(commands: List<List<String>>): String = commands.mapNotNull(::run).joinToString("; ")

    /** Runs [command] (no shell involved). Returns `null` if it worked, else the command and what it said. */
    fun run(command: List<String>): String? {
        val process = try {
            ProcessBuilder(command).redirectErrorStream(true).start()
        } catch (e: java.io.IOException) {
            return "${command.joinToString(" ")}: ${e.message}"
        }
        val finished = process.waitFor(TIMEOUT_S, TimeUnit.SECONDS)
        if (!finished) {
            process.destroy()
            return "${command.joinToString(" ")}: timed out"
        }
        val output = process.inputStream.bufferedReader().readText().trim()
        return if (process.exitValue() == 0) null else "${command.joinToString(" ")}: ${output.ifEmpty { "failed" }}"
    }
}
