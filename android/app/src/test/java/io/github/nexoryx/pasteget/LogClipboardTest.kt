// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.nexoryx.pasteget

import android.Manifest
import android.app.Application
import android.app.NotificationManager
import android.content.ClipData
import android.content.ClipboardManager
import android.os.Looper
import android.os.PersistableBundle
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Before
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.RuntimeEnvironment
import org.robolectric.Shadows.shadowOf
import org.robolectric.shadows.ShadowSettings
import java.util.concurrent.LinkedBlockingQueue
import java.util.concurrent.TimeUnit

/** Mode B's states on Android 16 (Robolectric runs the app's target SDK). There is no logcat here. */
@RunWith(RobolectricTestRunner::class)
class LogClipboardTest {
    private val app: Application = RuntimeEnvironment.getApplication()
    private val copies = LinkedBlockingQueue<CopiedText>()
    private lateinit var logs: LogClipboard

    private val waitingNotification
        get() = shadowOf(app.getSystemService(NotificationManager::class.java)).getNotification(2)

    @Before
    fun setUp() {
        logs = LogClipboard(app) { copies.add(it) }
    }

    private fun grantFallback() {
        shadowOf(app).grantPermissions(Manifest.permission.READ_LOGS)
        ShadowSettings.setCanDrawOverlays(true)
    }

    private fun idle() = shadowOf(Looper.getMainLooper()).idle()

    @Test
    fun withoutItsPermissionsItWaitsForThem() {
        logs.setActive(true)

        assertEquals(LogStatus.NEEDS_PERMISSIONS, logs.state.value)
    }

    @Test
    fun inTheBackgroundItAsksToOpenTheAppUntilItsOpened() {
        grantFallback()

        logs.setActive(true)
        assertEquals(LogStatus.WAITING_FOR_APP, logs.state.value)
        assertNotNull(waitingNotification)

        logs.setInFront(true)
        assertEquals(LogStatus.STARTING, logs.state.value)
        assertNull(waitingNotification)
    }

    @Test
    fun turningItOffClearsItsNotification() {
        grantFallback()
        logs.setActive(true)

        logs.setActive(false)

        assertEquals(LogStatus.OFF, logs.state.value)
        assertNull(waitingNotification)
    }

    @Test
    fun copiesMadeWhileTheAppIsInFrontAreReadDirectly() {
        grantFallback()
        logs.setInFront(true)
        logs.setActive(true)
        val clipboard = app.getSystemService(ClipboardManager::class.java)

        clipboard.setPrimaryClip(ClipData.newPlainText("label", "copied in paste-get"))
        idle()

        assertEquals(CopiedText("copied in paste-get", sensitive = false), copies.poll(5, TimeUnit.SECONDS))
    }

    @Test
    fun clipsFromAComputerAreNotReportedAsCopies() {
        grantFallback()
        logs.setInFront(true)
        logs.setActive(true)
        val clipboard = app.getSystemService(ClipboardManager::class.java)
        val fromPc = ClipData.newPlainText("paste-get", "from the PC").apply {
            description.extras = PersistableBundle().apply { putBoolean(CopiedText.EXTRA_IS_REMOTE_DEVICE, true) }
        }

        clipboard.setPrimaryClip(fromPc)
        idle()

        assertNull(copies.poll(200, TimeUnit.MILLISECONDS))
        assertTrue(copies.isEmpty())
    }
}
