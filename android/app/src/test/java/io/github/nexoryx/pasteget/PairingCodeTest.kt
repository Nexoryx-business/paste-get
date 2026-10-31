// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.nexoryx.pasteget

import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Test

class PairingCodeTest {
    @Test
    fun acceptsCodesAndTrimsWhitespace() {
        val code = "pasteget://pair?v=0&id=ABC&s=DEF&a=192.168.1.2%3A47290&n=PC"

        assertEquals(code, PairingCode.extract("  $code\n"))
    }

    @Test
    fun rejectsEverythingElse() {
        assertNull(PairingCode.extract(null))
        assertNull(PairingCode.extract(""))
        assertNull(PairingCode.extract("https://example.com/?pasteget://pair?"))
        assertNull(PairingCode.extract("pasteget://other?v=0"))
    }
}
