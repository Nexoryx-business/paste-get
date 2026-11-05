// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.nexoryx.pasteget

import android.app.Application
import androidx.core.app.NotificationCompat
import androidx.core.app.Person
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.RuntimeEnvironment
import org.robolectric.annotation.Config

/** v0.2: what the 2FA codes feature reads in a notification, and what it sends once. */
@RunWith(RobolectricTestRunner::class)
@Config(sdk = [29, 35])
class OneTimeCodesTest {
    private val app: Application = RuntimeEnvironment.getApplication()

    @Test
    fun theTitleTextAndLastMessagesAreRead() {
        val plain = NotificationCompat.Builder(app, "sms")
            .setSmallIcon(android.R.drawable.ic_dialog_info)
            .setContentTitle("Google")
            .setContentText("G-482913 is your Google verification code.")
            .build()
        assertEquals("Google\nG-482913 is your Google verification code.", OneTimeCodes.textOf(plain))

        val bank = Person.Builder().setName("BANK").build()
        val conversation = NotificationCompat.Builder(app, "sms")
            .setSmallIcon(android.R.drawable.ic_dialog_info)
            .setStyle(
                NotificationCompat.MessagingStyle(Person.Builder().setName("me").build())
                    .addMessage("old message", 1, bank)
                    .addMessage("Votre code : 584921", 2, bank),
            )
            .build()
        assertTrue(OneTimeCodes.textOf(conversation).contains("Votre code : 584921"))
    }

    @Test
    fun aCodeIsSentOnceEvenIfTheNotificationComesBack() {
        val recent = RecentCodes(forMillis = 1_000)

        assertTrue(recent.isNew("482913", now = 0))
        assertFalse(recent.isNew("482913", now = 500))
        assertTrue(recent.isNew("111111", now = 600))
        // Long after, the same digits are a new code.
        assertTrue(recent.isNew("482913", now = 2_000))
    }
}
