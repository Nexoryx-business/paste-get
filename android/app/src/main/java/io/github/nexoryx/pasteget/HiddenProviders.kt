// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.nexoryx.pasteget

import java.lang.reflect.InvocationTargetException
import java.lang.reflect.Method

/**
 * Calls an app's content provider from a process that has no `Context`: the built-in mode's
 * shell helper uses it to hand its binder to the app ([ShellBinderProvider]), as Shizuku's server
 * does. Android's SELinux rules don't let the shell connect to an app's local socket, but binder
 * calls are allowed.
 *
 * It goes through the hidden `IActivityManager.getContentProviderExternal` and
 * `IContentProvider.call`, whose parameters changed between Android versions; like
 * [HiddenCalls], each method is found by name and filled in by parameter type
 * ([getProviderArguments], [callArguments]). No Android APIs in the argument rules, so they are
 * unit-tested on the JVM against fake interfaces.
 */
class HiddenProviders(
    private val activityManager: Any,
    /** Builds an `android.content.AttributionSource` for Android 12+, given its class. */
    private val attributionSource: () -> Any,
) {
    /** What a provider call returned, and the package that owns the provider. */
    data class Reply(val packageName: String?, val result: Any?)

    /**
     * Calls [method] of the provider [authority] of [userId] with [extras] (a `Bundle`).
     * [token] is any binder, for Android to tell this caller apart.
     */
    fun call(
        authority: String,
        userId: Int,
        token: Any,
        callingPackage: String,
        method: String,
        arg: String?,
        extras: Any,
    ): Reply {
        val get = find(activityManager, "getContentProviderExternal")
        val getArguments = getProviderArguments(get.parameterTypes, authority, userId, token)
            ?: throw UnsupportedOperationException("unknown signature ${signature(get)}")
        val holder = invoke(get, activityManager, getArguments)
            ?: throw IllegalStateException("no provider $authority")
        val provider = field(holder, "provider") ?: throw IllegalStateException("provider $authority not running")
        val packageName = field(holder, "info")?.let { field(it, "packageName") } as String?
        try {
            val calls = provider.javaClass.methods.filter { it.name == "call" }
            // The newest shape first: AttributionSource, then the most parameters.
            val ranked = calls.sortedWith(
                compareByDescending<Method> { m -> m.parameterTypes.any { it.name == ATTRIBUTION_SOURCE } }
                    .thenByDescending { it.parameterTypes.size },
            )
            for (candidate in ranked) {
                val arguments = callArguments(
                    candidate.parameterTypes,
                    callingPackage,
                    authority,
                    method,
                    arg,
                    extras,
                    attributionSource,
                ) ?: continue
                return Reply(packageName, invoke(candidate, provider, arguments))
            }
            throw UnsupportedOperationException("unknown signatures ${calls.joinToString(transform = ::signature)}")
        } finally {
            release(authority, userId, token)
        }
    }

    /** Tells Android this caller is done with the provider. Best effort. */
    private fun release(authority: String, userId: Int, token: Any) {
        val method = activityManager.javaClass.methods.firstOrNull { it.name == "removeContentProviderExternalAsUser" }
            ?: activityManager.javaClass.methods.firstOrNull { it.name == "removeContentProviderExternal" }
            ?: return
        val arguments = getProviderArguments(method.parameterTypes, authority, userId, token, tagged = false) ?: return
        runCatching { invoke(method, activityManager, arguments) }
    }

    companion object {
        const val ATTRIBUTION_SOURCE = "android.content.AttributionSource"

        /**
         * `getContentProviderExternal(String name, int userId, IBinder token, String tag)` on
         * Android 10+, `(String name, int userId, IBinder token)` before; and
         * `removeContentProviderExternal[AsUser](String name, IBinder token[, int userId])`.
         * The first string is the authority, a second one the tag (the authority again).
         */
        fun getProviderArguments(
            types: Array<Class<*>>,
            authority: String,
            userId: Int,
            token: Any,
            tagged: Boolean = true,
        ): Array<Any?>? {
            var strings = 0
            var ints = 0
            var tokens = 0
            val arguments = types.map<Class<*>, Any?> { type ->
                when {
                    type == String::class.java -> when (strings++) {
                        0 -> authority
                        1 -> if (tagged) authority else return null
                        else -> return null
                    }
                    type == Integer.TYPE -> if (ints++ == 0) userId else return null
                    type.isInstance(token) -> token.also { tokens++ }
                    else -> return null
                }
            }
            return arguments.toTypedArray().takeIf { strings >= 1 && tokens == 1 }
        }

        /**
         * `IContentProvider.call`:
         * - Android 12+: `(AttributionSource, String authority, String method, String arg, Bundle)`;
         * - Android 11: `(String pkg, String featureId, String authority, String method, String arg, Bundle)`;
         * - Android 10: `(String pkg, String authority, String method, String arg, Bundle)`.
         */
        fun callArguments(
            types: Array<Class<*>>,
            callingPackage: String,
            authority: String,
            method: String,
            arg: String?,
            extras: Any,
            attributionSource: () -> Any,
        ): Array<Any?>? {
            val strings = when (types.count { it == String::class.java }) {
                3 -> listOf(authority, method, arg)
                4 -> listOf(callingPackage, authority, method, arg)
                5 -> listOf(callingPackage, null, authority, method, arg)
                else -> return null
            }.iterator()
            var bundles = 0
            val arguments = types.map { type ->
                when {
                    type == String::class.java -> strings.next()
                    type.name == ATTRIBUTION_SOURCE -> attributionSource()
                    type.isInstance(extras) -> extras.also { bundles++ }
                    else -> return null
                }
            }
            val hasSource = types.any { it.name == ATTRIBUTION_SOURCE }
            // Three strings only come with an AttributionSource; the older shapes have none.
            val shapeFits = if (types.count { it == String::class.java } == 3) hasSource else !hasSource
            return arguments.toTypedArray().takeIf { bundles == 1 && shapeFits }
        }

        private fun find(target: Any, name: String): Method =
            target.javaClass.methods.firstOrNull { it.name == name }
                ?: throw UnsupportedOperationException("no method $name")

        private fun invoke(method: Method, target: Any, arguments: Array<Any?>): Any? = try {
            method.invoke(target, *arguments)
        } catch (e: InvocationTargetException) {
            throw e.targetException
        }

        private fun field(target: Any, name: String): Any? =
            runCatching { target.javaClass.getField(name).get(target) }.getOrNull()

        private fun signature(method: Method): String =
            "${method.name}(${method.parameterTypes.joinToString { it.simpleName }})"
    }
}
