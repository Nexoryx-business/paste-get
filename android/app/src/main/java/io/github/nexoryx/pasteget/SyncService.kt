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
import android.os.IBinder
import androidx.core.app.NotificationCompat
import androidx.core.app.ServiceCompat
import androidx.core.content.ContextCompat
import uniffi.pasteget_ffi.PhoneListener

/**
 * Keeps the connections to the paired computers alive while the app is in the background.
 *
 * A `connectedDevice` foreground service: unlike `dataSync`, Android 15 doesn't cap its running
 * time. Its notification shows how many computers are connected.
 */
class SyncService : Service() {
    private val listener = object : PhoneListener {
        override fun onConnectionChanged(deviceId: String, connected: Boolean) {
            pasteGet.setConnected(deviceId, connected)
            updateNotification()
        }

        override fun onClipReceived(mimeType: String, data: ByteArray, sensitive: Boolean) {
            ReceivedClips.apply(applicationContext, mimeType, data, sensitive)
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
        pasteGet.client.start(listener)
    }

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int = START_STICKY

    override fun onDestroy() {
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
        private const val CHANNEL_ID = "connection"
        private const val NOTIFICATION_ID = 1

        fun start(context: Context) {
            ContextCompat.startForegroundService(context, Intent(context, SyncService::class.java))
        }
    }
}
