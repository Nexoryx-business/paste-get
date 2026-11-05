// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.nexoryx.pasteget

import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Test

/** The argument rules of [HiddenProviders], against the shapes of each Android version. */
class HiddenProvidersTest {
    interface Token

    private val token = object : Token {}

    /** Stand-ins: the real classes are hidden, and only their names and order matter here. */
    class Extras

    private val extras = Extras()
    private val source = "attribution source"

    private fun types(vararg types: Class<*>) = arrayOf(*types)

    @Test
    fun getsTheProviderOnEachVersion() {
        // Android 10+: (String name, int userId, IBinder token, String tag)
        assertEquals(
            listOf("auth", 10, token, "auth"),
            HiddenProviders.getProviderArguments(
                types(String::class.java, Integer.TYPE, Token::class.java, String::class.java),
                "auth",
                10,
                token,
            )?.toList(),
        )
        // Older: (String name, int userId, IBinder token)
        assertEquals(
            listOf("auth", 0, token),
            HiddenProviders.getProviderArguments(
                types(String::class.java, Integer.TYPE, Token::class.java),
                "auth",
                0,
                token,
            )?.toList(),
        )
        // removeContentProviderExternalAsUser(String name, IBinder token, int userId)
        assertEquals(
            listOf("auth", token, 0),
            HiddenProviders.getProviderArguments(
                types(String::class.java, Token::class.java, Integer.TYPE),
                "auth",
                0,
                token,
                tagged = false,
            )?.toList(),
        )
        // Unknown shapes are refused.
        assertNull(HiddenProviders.getProviderArguments(types(String::class.java, Integer.TYPE), "auth", 0, token))
        assertNull(
            HiddenProviders.getProviderArguments(
                types(String::class.java, Token::class.java, Boolean::class.java),
                "auth",
                0,
                token,
            ),
        )
    }

    private fun call(vararg types: Class<*>): List<Any?>? =
        HiddenProviders.callArguments(types(*types), "com.android.shell", "auth", "helper", "42", extras) { source }
            ?.toList()

    @Test
    fun callsTheProviderOnEachVersion() {
        // Only its name matters: no code of it runs.
        val attribution = android.content.AttributionSource::class.java
        // Android 12+: (AttributionSource, String authority, String method, String arg, Bundle)
        assertEquals(
            listOf(source, "auth", "helper", "42", extras),
            call(attribution, String::class.java, String::class.java, String::class.java, Extras::class.java),
        )
        // Android 11: (String pkg, String featureId, String authority, String method, String arg, Bundle)
        assertEquals(
            listOf("com.android.shell", null, "auth", "helper", "42", extras),
            call(String::class.java, String::class.java, String::class.java, String::class.java, String::class.java, Extras::class.java),
        )
        // Android 10: (String pkg, String authority, String method, String arg, Bundle)
        assertEquals(
            listOf("com.android.shell", "auth", "helper", "42", extras),
            call(String::class.java, String::class.java, String::class.java, String::class.java, Extras::class.java),
        )
        // Three strings without an AttributionSource, or no Bundle: refused.
        assertNull(call(String::class.java, String::class.java, String::class.java, Extras::class.java))
        assertNull(call(String::class.java, String::class.java, String::class.java, String::class.java))
    }
}
