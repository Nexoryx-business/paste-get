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

    /**
     * The command line, for `sh -c` (adb's `exec:`), that starts [ShellHelper] from the app's APK
     * and leaves it running after the adb connection closes: in a session of its own (`setsid`,
     * or `nohup` where there is no `setsid`), with no terminal.
     */
    fun helperLaunch(apk: String, socket: String, appUid: Int, build: Long): String {
        val helper = "app_process /system/bin --nice-name=pasteget_shell ${ShellHelper::class.java.name} " +
            "${quote(socket)} $appUid $build"
        val classpath = "CLASSPATH=${quote(apk)}"
        return "if command -v setsid >/dev/null 2>&1; then $classpath setsid $helper; " +
            "else $classpath nohup $helper; fi </dev/null >/dev/null 2>&1 &"
    }

    /** Lets the app turn wireless debugging on by itself after a restart of the phone. */
    fun grantSecureSettings(app: String): String = "pm grant ${quote(app)} android.permission.WRITE_SECURE_SETTINGS"

    /** Single quotes for `sh`: nothing inside is special, and `'` is closed, escaped, reopened. */
    fun quote(text: String): String = "'" + text.replace("'", "'\\''") + "'"

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
