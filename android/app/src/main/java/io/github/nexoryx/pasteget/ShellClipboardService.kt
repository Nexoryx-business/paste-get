// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.nexoryx.pasteget

import android.annotation.SuppressLint
import android.content.ClipData
import android.content.ClipDescription
import android.content.Context
import android.os.Binder
import android.os.IBinder
import android.os.Parcel
import android.os.RemoteException
import androidx.annotation.Keep
import java.lang.reflect.Proxy
import kotlin.system.exitProcess

/**
 * Mode A of the automatic copy. Shizuku runs this in a process of its own as the shell user (or
 * root), which may read the clipboard while in the background: the same way scrcpy syncs it.
 *
 * Nothing else of the app runs in that process: no Rust library, no [PasteGetApp] (Shizuku gives
 * it a plain Application). The clipboard is reached through hidden APIs, which [HiddenCalls]
 * adapts to each Android version. Every failure goes back to the app as an exception of a type
 * Binder can carry, so the app can fall back to another mode instead of this process crashing.
 */
@Keep
class ShellClipboardService(private val context: Context?) : IShellClipboard.Stub() {
    /** For Shizuku servers older than API 13, which don't pass a Context. */
    @Keep
    constructor() : this(null)

    private val clipboard: HiddenCalls by lazy { HiddenCalls(clipboardService(), SHELL_PACKAGE) }

    @Volatile
    private var appListener: IClipboardChanged? = null
    private var registered = false

    override fun destroy() {
        exitProcess(0)
    }

    @Synchronized
    override fun watch(listener: IClipboardChanged, userId: Int): String = carried {
        appListener = listener
        if (!registered) {
            val type = listenerType()
            clipboard.call(ADD_LISTENER, userId, changeListener(type), type)
            registered = true
        }
        "${clipboard.signature(ADD_LISTENER)}, ${clipboard.signature(GET_DESCRIPTION)}, ${clipboard.signature(GET_CLIP)}"
    }

    override fun describe(userId: Int): ClipDescription? = carried {
        clipboard.call(GET_DESCRIPTION, userId) as ClipDescription?
    }

    override fun read(userId: Int): ClipData? = carried {
        clipboard.call(GET_CLIP, userId) as ClipData?
    }

    /** Grants only the app this service belongs to, which Shizuku tells through the Context. */
    override fun grantFallback(): String = carried {
        val app = context?.packageName ?: return@carried "Shizuku is too old to tell which app to grant"
        ShellCommands.runAll(ShellCommands.fallbackGrants(app))
    }

    /** Tells the app about each change. The app reads the clip itself, if it wants it. */
    private fun changed() {
        try {
            appListener?.changed()
        } catch (e: RemoteException) {
            appListener = null // The app is gone; Shizuku ends this process soon.
        }
    }

    /**
     * An `IOnPrimaryClipChangedListener`: a hidden interface, so it's implemented with a proxy
     * whose binder answers its single call, `dispatchPrimaryClipChanged`.
     */
    private fun changeListener(type: Class<*>): Any {
        val binder = ChangeBinder(::changed)
        return Proxy.newProxyInstance(javaClass.classLoader, arrayOf(type)) { proxy, method, arguments ->
            when (method.name) {
                "asBinder" -> binder
                "equals" -> proxy === arguments?.firstOrNull()
                "hashCode" -> System.identityHashCode(proxy)
                "toString" -> "paste-get clipboard listener"
                else -> null
            }
        }
    }

    private class ChangeBinder(private val onChange: () -> Unit) : Binder() {
        init {
            attachInterface(null, LISTENER)
        }

        override fun onTransact(code: Int, data: Parcel, reply: Parcel?, flags: Int): Boolean {
            if (code != FIRST_CALL_TRANSACTION) return super.onTransact(code, data, reply, flags)
            data.enforceInterface(LISTENER)
            onChange()
            return true
        }
    }

    private companion object {
        /** A package of the shell user. With it, Android lets shell read the clipboard in the background. */
        const val SHELL_PACKAGE = "com.android.shell"
        const val LISTENER = "android.content.IOnPrimaryClipChangedListener"
        const val ADD_LISTENER = "addPrimaryClipChangedListener"
        const val GET_DESCRIPTION = "getPrimaryClipDescription"
        const val GET_CLIP = "getPrimaryClip"

        /** The hidden `IOnPrimaryClipChangedListener`. Allowed here: this isn't an app process. */
        @SuppressLint("PrivateApi")
        fun listenerType(): Class<*> = Class.forName(LISTENER)

        /** The clipboard service's hidden `IClipboard` interface. Allowed here: this isn't an app process. */
        @SuppressLint("PrivateApi", "DiscouragedPrivateApi")
        fun clipboardService(): Any {
            val binder = Class.forName("android.os.ServiceManager")
                .getMethod("getService", String::class.java)
                .invoke(null, Context.CLIPBOARD_SERVICE) as IBinder?
                ?: throw IllegalStateException("no clipboard service")
            return Class.forName("android.content.IClipboard\$Stub")
                .getMethod("asInterface", IBinder::class.java)
                .invoke(null, binder)
                ?: throw IllegalStateException("no clipboard interface")
        }

        /**
         * Runs [block], turning any exception Binder can't carry back to the app (which would end
         * this process) into an [IllegalStateException] naming it.
         */
        inline fun <T> carried(block: () -> T): T = try {
            block()
        } catch (e: SecurityException) {
            throw e
        } catch (e: UnsupportedOperationException) {
            throw e
        } catch (e: IllegalStateException) {
            throw e
        } catch (e: Exception) {
            throw IllegalStateException("${e.javaClass.simpleName}: ${e.message}")
        } catch (e: LinkageError) {
            throw IllegalStateException("${e.javaClass.simpleName}: ${e.message}")
        }
    }
}
