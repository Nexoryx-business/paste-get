// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.nexoryx.pasteget

import android.annotation.SuppressLint
import android.content.AttributionSource
import android.os.Binder
import android.os.Build
import android.os.Bundle
import android.os.Handler
import android.os.Looper
import android.os.Process
import android.os.SystemClock
import android.util.Log
import androidx.annotation.Keep
import kotlin.system.exitProcess

/**
 * The built-in mode's shell process: Shizuku's part in mode A, started by the app itself through
 * wireless debugging ([BuiltInShell]), with
 * `app_process /system/bin io.github.nexoryx.pasteget.ShellHelper <authority> <user> <build> <package>`.
 *
 * It runs [ShellClipboardService], the same service Shizuku runs, and hands its binder to the
 * app through the app's [ShellBinderProvider], as Shizuku's server does ([HiddenProviders]):
 * Android's SELinux rules don't let the shell connect to an app's local socket, but allow binder
 * calls. It ends when the app goes away (Android may stop it), when the app asks
 * ([IShellClipboard.destroy]), or if the app doesn't take it.
 */
@Keep
object ShellHelper {
    private const val TAG = "paste-get"
    private const val ATTEMPTS = 5
    private const val RETRY_MS = 2_000L

    /** Time the app has to start watching once it took the binder. */
    private const val WATCH_TIMEOUT_MS = 60_000L

    @JvmStatic
    fun main(args: Array<String>) {
        val authority = args.getOrNull(0)
        val userId = args.getOrNull(1)?.toIntOrNull()
        val build = args.getOrNull(2)?.toLongOrNull()
        val appPackage = args.getOrNull(3)
        if (authority == null || userId == null || build == null || appPackage == null) {
            System.err.println("usage: ShellHelper <authority> <user> <build> <package>")
            exitProcess(2)
        }
        // Uncaught errors go to the log file the app reads back ([ShellCommands.HELPER_LOG]).
        val existing = Thread.getDefaultUncaughtExceptionHandler()
        Thread.setDefaultUncaughtExceptionHandler { thread, e ->
            say("shell helper crashed: $e at ${e.stackTrace.take(3).joinToString(" < ")}")
            existing?.uncaughtException(thread, e) ?: exitProcess(1)
        }
        say("shell helper started as uid ${Process.myUid()}")
        Looper.prepareMainLooper()
        val service = ShellClipboardService(null, appPackage)
        // Nothing to do once the app is gone: Android may have stopped it; it starts a new helper.
        service.onAppGone = {
            say("shell helper: the app went away, ending")
            exitProcess(0)
        }
        repeat(ATTEMPTS) {
            if (handOver(service, authority, userId, build, appPackage)) {
                Handler(Looper.getMainLooper()).postDelayed({
                    if (!service.watched) {
                        say("shell helper: the app didn't start watching, ending")
                        exitProcess(0)
                    }
                }, WATCH_TIMEOUT_MS)
                // Binder threads serve the app from now on.
                Looper.loop()
            }
            SystemClock.sleep(RETRY_MS)
        }
        say("shell helper: the app didn't take the helper, ending")
        exitProcess(1)
    }

    private fun handOver(
        service: ShellClipboardService,
        authority: String,
        userId: Int,
        build: Long,
        appPackage: String,
    ): Boolean {
        val callingPackage = if (Process.myUid() == 0) "root" else "com.android.shell"
        return try {
            val providers = HiddenProviders(activityManager()) { attributionSource(callingPackage) }
            val extras = Bundle().apply { putBinder(ShellBinderProvider.EXTRA_BINDER, service) }
            val reply = providers.call(
                authority,
                userId,
                Binder(),
                callingPackage,
                ShellBinderProvider.METHOD_HELPER,
                build.toString(),
                extras,
            )
            // Only the app it was started for: the provider's owner must be that package.
            if (reply.packageName != null && reply.packageName != appPackage) {
                say("shell helper: $authority belongs to ${reply.packageName}, not $appPackage")
                exitProcess(1)
            }
            val accepted = (reply.result as? Bundle)?.getBoolean(ShellBinderProvider.EXTRA_ACCEPTED) == true
            if (!accepted) say("shell helper: the app didn't take it (another build, or not ready)")
            accepted
        } catch (e: Exception) {
            say("shell helper: could not reach the app: $e")
            false
        } catch (e: LinkageError) {
            say("shell helper: could not reach the app: $e")
            false
        }
    }

    /** To the system log and to the helper's own log file. Never clipboard contents. */
    private fun say(message: String) {
        Log.i(TAG, message)
        System.err.println(message)
    }

    /** The hidden `IActivityManager`. Allowed here: this isn't an app process. */
    @SuppressLint("PrivateApi", "DiscouragedPrivateApi")
    private fun activityManager(): Any =
        Class.forName("android.app.ActivityManager").getMethod("getService").invoke(null)
            ?: throw IllegalStateException("no activity manager")

    /** Only called on Android 12+, where `IContentProvider.call` takes one. */
    private fun attributionSource(callingPackage: String): Any =
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.S) {
            AttributionSource.Builder(Process.myUid()).setPackageName(callingPackage).build()
        } else {
            throw UnsupportedOperationException("AttributionSource needs Android 12")
        }
}
