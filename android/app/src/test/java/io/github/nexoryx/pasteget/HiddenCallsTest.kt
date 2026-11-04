// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.nexoryx.pasteget

import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Assert.assertThrows
import org.junit.Test

/** Fake `IClipboard`s with the shapes found on real phones. Each records its last call. */
class HiddenCallsTest {
    interface Listener

    abstract class Recording {
        var last: List<Any?> = emptyList()

        protected fun record(vararg arguments: Any?): String {
            last = arguments.toList()
            return "clip"
        }
    }

    @Suppress("unused") // Called by reflection.
    class Android10 : Recording() {
        fun getPrimaryClip(pkg: String, userId: Int) = record(pkg, userId)
        fun addPrimaryClipChangedListener(listener: Listener, pkg: String, userId: Int) = record(listener, pkg, userId)
    }

    @Suppress("unused")
    class Android12 : Recording() {
        fun getPrimaryClip(pkg: String, attributionTag: String?, userId: Int) = record(pkg, attributionTag, userId)
        fun addPrimaryClipChangedListener(listener: Listener, pkg: String, attributionTag: String?, userId: Int) =
            record(listener, pkg, attributionTag, userId)
    }

    @Suppress("unused")
    class Android14 : Recording() {
        fun getPrimaryClip(pkg: String, attributionTag: String?, userId: Int, deviceId: Int) =
            record(pkg, attributionTag, userId, deviceId)
        fun addPrimaryClipChangedListener(
            listener: Listener,
            pkg: String,
            attributionTag: String?,
            userId: Int,
            deviceId: Int,
        ) = record(listener, pkg, attributionTag, userId, deviceId)
    }

    @Suppress("unused")
    class Vendor : Recording() {
        fun getPrimaryClip(pkg: String, userId: Int, attributionTag: String?) = record(pkg, userId, attributionTag)
        fun getPrimaryClipDescription(pkg: String, userId: Int, userOperate: Boolean) = record(pkg, userId, userOperate)
    }

    @Suppress("unused")
    class Unknown {
        fun getPrimaryClip(pkg: String, userId: Int, options: Map<String, String>) = "clip"
        fun getPrimaryClipDescription(userId: Int) = "description"
        fun addPrimaryClipChangedListener(pkg: String, userId: Int) = Unit
        fun failing(pkg: String, userId: Int): String = throw SecurityException("not allowed")
    }

    private val listener = object : Listener {}
    private val shell = "com.android.shell"

    private fun read(clipboard: Any) = HiddenCalls(clipboard, shell).call("getPrimaryClip", 10)

    private fun watch(clipboard: Any) =
        HiddenCalls(clipboard, shell).call("addPrimaryClipChangedListener", 10, listener, Listener::class.java)

    @Test
    fun readsTheClipboardOfEveryKnownVersion() {
        val android10 = Android10().also { assertEquals("clip", read(it)) }
        val android12 = Android12().also { read(it) }
        val android14 = Android14().also { read(it) }
        val vendor = Vendor().also { read(it) }

        assertEquals(listOf(shell, 10), android10.last)
        assertEquals(listOf(shell, null, 10), android12.last)
        assertEquals(listOf(shell, null, 10, 0), android14.last)
        assertEquals(listOf(shell, 10, null), vendor.last)
    }

    @Test
    fun registersTheListenerOfEveryKnownVersion() {
        val android10 = Android10().also { watch(it) }
        val android12 = Android12().also { watch(it) }
        val android14 = Android14().also { watch(it) }

        assertEquals(listOf(listener, shell, 10), android10.last)
        assertEquals(listOf(listener, shell, null, 10), android12.last)
        assertEquals(listOf(listener, shell, null, 10, 0), android14.last)
    }

    @Test
    fun booleansAreFalse() {
        val vendor = Vendor()

        HiddenCalls(vendor, shell).call("getPrimaryClipDescription", 10)

        assertEquals(listOf(shell, 10, false), vendor.last)
    }

    @Test
    fun unknownShapesAreRefusedWithTheirSignature() {
        val unknown = HiddenCalls(Unknown(), shell)

        val refused = assertThrows(UnsupportedOperationException::class.java) { unknown.call("getPrimaryClip", 10) }
        assertEquals("unknown signature getPrimaryClip(String, int, Map)", refused.message)
        // No package, or no listener where one is needed.
        assertThrows(UnsupportedOperationException::class.java) { unknown.call("getPrimaryClipDescription", 10) }
        assertThrows(UnsupportedOperationException::class.java) { watch(Unknown()) }
        // A listener where none is expected.
        assertNull(HiddenCalls.arguments(arrayOf(Listener::class.java, String::class.java, Integer.TYPE), shell, 0, null, null))
    }

    @Test
    fun missingMethodsAreRefused() {
        val refused = assertThrows(UnsupportedOperationException::class.java) {
            HiddenCalls(Android10(), shell).call("getPrimaryClipDescription", 10)
        }
        assertEquals("no method getPrimaryClipDescription", refused.message)
    }

    @Test
    fun exceptionsOfTheMethodComeThroughAsTheyAre() {
        val thrown = assertThrows(SecurityException::class.java) { HiddenCalls(Unknown(), shell).call("failing", 10) }
        assertEquals("not allowed", thrown.message)
    }

    @Test
    fun signaturesNameTheParameterTypes() {
        assertEquals(
            "getPrimaryClip(String, String, int, int)",
            HiddenCalls(Android14(), shell).signature("getPrimaryClip"),
        )
    }
}
