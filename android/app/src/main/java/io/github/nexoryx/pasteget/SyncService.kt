// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.nexoryx.pasteget

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.app.Service
import android.content.Context
import android.content.Intent
import android.content.pm.ServiceInfo
import android.net.ConnectivityManager
import android.net.Network
import android.os.Handler
import android.os.IBinder
import android.os.Looper
import android.os.SystemClock
import android.util.Log
import androidx.core.app.NotificationCompat
import androidx.core.app.ServiceCompat
import androidx.core.content.ContextCompat
import uniffi.pasteget_ffi.ClipDelivery
import uniffi.pasteget_ffi.PhoneListener

/**
 * Keeps the connections to the paired computers alive while the app is in the background.
 *
 * A `connectedDevice` foreground service: unlike `dataSync`, Android 15 doesn't cap its running
 * time. Its notification shows how many computers are connected. While one of them isn't, it
 * looks for it on the local network. When the user turned them on, it also sends new screenshots
 * and what they copy ([AutoCopy]).
 */
class SyncService : Service() {
    private val main = Handler(Looper.getMainLooper())
    private lateinit var finder: ComputerFinder
    private lateinit var screenshots: ScreenshotWatcher

    /** Callbacks from the client's threads may still arrive after onDestroy. */
    private var running = false

    private val listener = object : PhoneListener {
        override fun onConnectionChanged(deviceId: String, connected: Boolean) {
            pasteGet.setConnected(deviceId, connected)
            main.post {
                if (running) {
                    updateNotification()
                    updateFinder()
                }
            }
        }

        override fun onClipReceived(mimeType: String, data: ByteArray, sensitive: Boolean) {
            ReceivedClips.apply(applicationContext, mimeType, data, sensitive)
        }

        override fun onClipDelivered(delivery: ClipDelivery) {
            val size = delivery.size.toLong()
            val screenshot = lastScreenshot?.takeIf { delivery.mimeType.startsWith("image/") && it.size == size }
            val millis = screenshot?.let { SystemClock.elapsedRealtime() - it.announcedAtMs } ?: delivery.millis.toLong()
            Log.i(
                TAG,
                "${delivery.mimeType}, $size bytes: delivered ${delivery.millis} ms after sending" +
                    (screenshot?.let { ", $millis ms after Android announced the screenshot" } ?: "") +
                    if (delivery.accepted) "" else ", refused",
            )
            pasteGet.setLastDelivery(LastDelivery(screenshot != null, size, millis, delivery.accepted))
        }
    }

    /** The last screenshot sent, to time it from its announcement to the computer's ack. */
    private class SentScreenshot(val size: Long, val announcedAtMs: Long)

    @Volatile
    private var lastScreenshot: SentScreenshot? = null

    /** On a new network, computers are dialed right away instead of after their backoff. */
    private val network = object : ConnectivityManager.NetworkCallback() {
        override fun onAvailable(network: Network) {
            main.post {
                if (running) {
                    pasteGet.client.networkChanged()
                    finder.restart()
                }
            }
        }
    }

    override fun onCreate() {
        super.onCreate()
        createChannel()
        ServiceCompat.startForeground(
            this,
            NOTIFICATION_ID,
            notification(),
            ServiceInfo.FOREGROUND_SERVICE_TYPE_CONNECTED_DEVICE,
        )
        finder = ComputerFinder(this, pasteGet.client)
        screenshots = ScreenshotWatcher(this, ::sendScreenshot)
        running = true
        pasteGet.client.start(listener)
        getSystemService(ConnectivityManager::class.java).registerDefaultNetworkCallback(network)
    }

    /** Also called again by the app after pairing or a settings change. */
    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        updateFinder()
        updateScreenshots()
        updateAutoCopy()
        return START_STICKY
    }

    override fun onDestroy() {
        running = false
        getSystemService(ConnectivityManager::class.java).unregisterNetworkCallback(network)
        finder.setActive(false)
        screenshots.stop()
        pasteGet.autoCopy.stop()
        main.removeCallbacksAndMessages(null)
        pasteGet.client.stop()
        pasteGet.clearConnected()
        super.onDestroy()
    }

    override fun onBind(intent: Intent?): IBinder? = null

    private fun notification(): Notification {
        val connected = pasteGet.connected.value.size
        val text = when {
            pasteGet.client.pairedComputers().isEmpty() -> getString(R.string.notification_no_computer)
            connected == 0 -> getString(R.string.notification_connecting)
            else -> resources.getQuantityString(R.plurals.notification_connected, connected, connected)
        }
        val openApp = PendingIntent.getActivity(
            this,
            0,
            Intent(this, MainActivity::class.java),
            PendingIntent.FLAG_IMMUTABLE,
        )
        return NotificationCompat.Builder(this, CHANNEL_ID)
            .setSmallIcon(R.drawable.ic_tile)
            .setContentTitle(getString(R.string.app_name))
            .setContentText(text)
            .setContentIntent(openApp)
            .setOngoing(true)
            .setSilent(true)
            .setPriority(NotificationCompat.PRIORITY_LOW)
            .build()
    }

    /** Searches the network only while a paired computer is missing. */
    private fun updateFinder() {
        val connected = pasteGet.connected.value
        finder.setActive(pasteGet.client.pairedComputers().any { it.deviceId !in connected })
    }

    private fun updateScreenshots() {
        if (Preferences.sendScreenshots(this) && PhotoAccess.of(this) == PhotoAccess.FULL) {
            screenshots.start()
        } else {
            screenshots.stop()
        }
    }

    private fun updateAutoCopy() {
        if (Preferences.sendCopies(this)) pasteGet.autoCopy.start() else pasteGet.autoCopy.stop()
    }

    /** Runs on a background thread. Never logs contents: size only. */
    private fun sendScreenshot(image: ByteArray, announcedAtMs: Long) {
        val sendable = Sender.sendable(image)
        lastScreenshot = SentScreenshot(sendable.size.toLong(), announcedAtMs)
        val sentTo = pasteGet.client.sendImage(sendable).toInt()
        val readMs = SystemClock.elapsedRealtime() - announcedAtMs
        Log.i(TAG, "screenshot of ${sendable.size} bytes read in $readMs ms, sent to $sentTo computers")
    }

    private fun updateNotification() {
        getSystemService(NotificationManager::class.java).notify(NOTIFICATION_ID, notification())
    }

    private fun createChannel() {
        val channel = NotificationChannel(
            CHANNEL_ID,
            getString(R.string.notification_channel),
            NotificationManager.IMPORTANCE_LOW,
        )
        getSystemService(NotificationManager::class.java).createNotificationChannel(channel)
    }

    companion object {
        private const val TAG = "paste-get"
        private const val CHANNEL_ID = "connection"
        private const val NOTIFICATION_ID = 1

        fun start(context: Context) {
            ContextCompat.startForegroundService(context, Intent(context, SyncService::class.java))
        }
    }
}
