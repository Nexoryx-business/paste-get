// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.nexoryx.pasteget

import java.lang.reflect.InvocationTargetException
import java.lang.reflect.Method

/**
 * Calls methods of a hidden Android interface whose parameters change between Android versions
 * and vendors (see scrcpy #4703). No Android APIs here, so the unit tests run on the JVM, against
 * fake interfaces.
 *
 * AIDL interfaces can't overload a method, so each method is found by its name alone. Its
 * arguments are then filled in by type, in order:
 * - the first `String` is the calling package, any later one an attribution tag (`null`);
 * - the first `int` is the user ID, any later one a virtual device ID (0: the phone itself);
 * - a `boolean` is `false`;
 * - the one parameter of the extra type, when there is one (a listener), is the extra.
 *
 * For `IClipboard.getPrimaryClip`, that covers:
 * - `(String pkg, int userId)`: Android 10 and 11;
 * - `(String pkg, String attributionTag, int userId)`: Android 12 and 13;
 * - `(String pkg, String attributionTag, int userId, int deviceId)`: Android 14 and later;
 * - `(String pkg, int userId, String attributionTag)`: some vendor builds.
 *
 * Any other shape is refused with an [UnsupportedOperationException] naming the signature found,
 * so it can be reported and added.
 */
class HiddenCalls(private val target: Any, private val callingPackage: String) {
    /** Calls [name]. Exceptions thrown by the method itself are rethrown as they are. */
    fun call(name: String, userId: Int, extra: Any? = null, extraType: Class<*>? = null): Any? {
        val method = find(name)
        val arguments = arguments(method.parameterTypes, callingPackage, userId, extra, extraType)
            ?: throw UnsupportedOperationException("unknown signature ${signature(method)}")
        return try {
            method.invoke(target, *arguments)
        } catch (e: InvocationTargetException) {
            throw e.targetException
        }
    }

    /** [name] and its parameter types, e.g. `getPrimaryClip(String, String, int, int)`. */
    fun signature(name: String): String = signature(find(name))

    private fun find(name: String): Method =
        target.javaClass.methods.firstOrNull { it.name == name }
            ?: throw UnsupportedOperationException("no method $name")

    companion object {
        /** The arguments for a method taking [types], or `null` if it has an unknown shape. */
        fun arguments(
            types: Array<Class<*>>,
            callingPackage: String,
            userId: Int,
            extra: Any?,
            extraType: Class<*>?,
        ): Array<Any?>? {
            var strings = 0
            var ints = 0
            var extras = 0
            val arguments = types.map { type ->
                when (type) {
                    extraType -> extra.also { extras++ }
                    String::class.java -> if (strings++ == 0) callingPackage else null
                    Integer.TYPE -> if (ints++ == 0) userId else 0
                    java.lang.Boolean.TYPE -> false
                    else -> return null
                }
            }
            // Every known shape names a package and a user; a listener is passed exactly once.
            val extrasFit = if (extraType == null) extras == 0 else extras == 1
            return arguments.toTypedArray().takeIf { strings > 0 && ints > 0 && extrasFit }
        }

        private fun signature(method: Method): String =
            "${method.name}(${method.parameterTypes.joinToString { it.simpleName }})"
    }
}
