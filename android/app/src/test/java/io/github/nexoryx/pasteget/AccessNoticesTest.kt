// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.nexoryx.pasteget

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

class AccessNoticesTest {
    @Test
    fun beforeAndroid12ThereIsNothingToHide() {
        val notices = AccessNotices.decide(sdk = 30, value = null, canWrite = true)
        assertFalse(notices.supported)
        assertFalse(notices.switchable)
    }

    @Test
    fun zeroMeansHiddenAndAnythingElseShown() {
        assertEquals(true, AccessNotices.decide(sdk = 35, value = 0, canWrite = true).hidden)
        assertEquals(false, AccessNotices.decide(sdk = 35, value = 1, canWrite = true).hidden)
        assertTrue(AccessNotices.decide(sdk = 35, value = 1, canWrite = true).switchable)
    }

    @Test
    fun withoutThePermissionOrTheValueTheUserGoesToSettings() {
        assertFalse(AccessNotices.decide(sdk = 35, value = 1, canWrite = false).switchable)
        val unreadable = AccessNotices.decide(sdk = 35, value = null, canWrite = true)
        assertNull(unreadable.hidden)
        assertFalse(unreadable.switchable)
    }
}
