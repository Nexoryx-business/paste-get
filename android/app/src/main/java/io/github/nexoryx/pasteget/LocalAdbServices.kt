// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.nexoryx.pasteget

import android.content.Context
import android.net.nsd.NsdManager
import android.net.nsd.NsdServiceInfo
import android.os.Build
import android.util.Log
import androidx.annotation.RequiresApi
import java.net.InetAddress
import java.net.NetworkInterface
import java.util.concurrent.CountDownLatch
import java.util.concurrent.Executors
import java.util.concurrent.TimeUnit
import java.util.concurrent.atomic.AtomicReference

/**
 * Finds this phone's own wireless debugging ports. Android announces them on the network with
 * mDNS, like `adb mdns services` sees them: the pairing port while Settings shows a pairing
 * code, the connect port while wireless debugging is on. Other phones on the Wi-Fi announce
 * theirs too, so only an announcement with one of this phone's addresses counts.
 */
object LocalAdbServices {
    const val PAIRING = "_adb-tls-pairing._tcp"
    const val CONNECT = "_adb-tls-connect._tcp"
    private const val TAG = "paste-get"

    /** The hosts to try (loopback first), and the port. */
    data class Found(val hosts: List<String>, val port: Int)

    /** Blocks up to [timeoutMs]. Call off the main thread. */
    fun find(context: Context, type: String, timeoutMs: Long): Found? {
        val nsd = context.getSystemService(NsdManager::class.java)
        val result = AtomicReference<Found?>()
        val done = CountDownLatch(1)
        val resolver = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.UPSIDE_DOWN_CAKE) {
            FollowingResolver(nsd)
        } else {
            QueuedResolver(nsd)
        }
        val onResolved = { info: NsdServiceInfo, addresses: List<InetAddress> ->
            val own = addresses.filter(::isOwnAddress)
            if (own.isNotEmpty() && info.port in 1..65535 && result.compareAndSet(null, Found(hosts(own), info.port))) {
                done.countDown()
            }
        }
        val listener = object : NsdManager.DiscoveryListener {
            override fun onDiscoveryStarted(serviceType: String) = Unit

            override fun onDiscoveryStopped(serviceType: String) = Unit

            override fun onStartDiscoveryFailed(serviceType: String, errorCode: Int) {
                Log.w(TAG, "built-in mode: discovery of $type failed to start: $errorCode")
                done.countDown()
            }

            override fun onStopDiscoveryFailed(serviceType: String, errorCode: Int) = Unit

            override fun onServiceFound(info: NsdServiceInfo) = resolver.resolve(info, onResolved)

            override fun onServiceLost(info: NsdServiceInfo) = Unit
        }
        nsd.discoverServices(type, NsdManager.PROTOCOL_DNS_SD, listener)
        try {
            done.await(timeoutMs, TimeUnit.MILLISECONDS)
        } finally {
            // Throws if discovery never started: nothing to stop then.
            runCatching { nsd.stopServiceDiscovery(listener) }
            resolver.stop()
        }
        return result.get()
    }

    private fun hosts(own: List<InetAddress>): List<String> =
        (listOf("127.0.0.1") + own.mapNotNull { it.hostAddress?.substringBefore('%') }).distinct()

    private fun isOwnAddress(address: InetAddress): Boolean = runCatching {
        NetworkInterface.getNetworkInterfaces()?.asSequence().orEmpty()
            .flatMap { it.inetAddresses.asSequence() }
            .any { it == address }
    }.getOrDefault(false)

    private interface Resolver {
        fun resolve(info: NsdServiceInfo, onResolved: (NsdServiceInfo, List<InetAddress>) -> Unit)

        fun stop()
    }

    @RequiresApi(Build.VERSION_CODES.UPSIDE_DOWN_CAKE)
    private class FollowingResolver(private val nsd: NsdManager) : Resolver {
        private val executor = Executors.newSingleThreadExecutor()
        private val callbacks = mutableListOf<NsdManager.ServiceInfoCallback>()

        override fun resolve(info: NsdServiceInfo, onResolved: (NsdServiceInfo, List<InetAddress>) -> Unit) {
            val callback = object : NsdManager.ServiceInfoCallback {
                override fun onServiceInfoCallbackRegistrationFailed(errorCode: Int) = Unit

                override fun onServiceUpdated(updated: NsdServiceInfo) = onResolved(updated, updated.hostAddresses)

                override fun onServiceLost() = Unit

                override fun onServiceInfoCallbackUnregistered() = Unit
            }
            synchronized(callbacks) { callbacks += callback }
            nsd.registerServiceInfoCallback(info, executor, callback)
        }

        override fun stop() {
            synchronized(callbacks) {
                callbacks.forEach { runCatching { nsd.unregisterServiceInfoCallback(it) } }
                callbacks.clear()
            }
            executor.shutdown()
        }
    }

    /** Android 10 to 13: one resolution at a time. */
    private class QueuedResolver(private val nsd: NsdManager) : Resolver {
        private val queue = ArrayDeque<Pair<NsdServiceInfo, (NsdServiceInfo, List<InetAddress>) -> Unit>>()
        private var busy = false

        @Synchronized
        override fun resolve(info: NsdServiceInfo, onResolved: (NsdServiceInfo, List<InetAddress>) -> Unit) {
            queue.addLast(info to onResolved)
            next()
        }

        @Synchronized
        override fun stop() = queue.clear()

        @Synchronized
        private fun finished() {
            busy = false
            next()
        }

        @Suppress("DEPRECATION") // The replacement, registerServiceInfoCallback, is API 34+.
        private fun next() {
            if (busy) return
            val (info, onResolved) = queue.removeFirstOrNull() ?: return
            busy = true
            nsd.resolveService(
                info,
                object : NsdManager.ResolveListener {
                    override fun onServiceResolved(resolved: NsdServiceInfo) {
                        onResolved(resolved, listOfNotNull(resolved.host))
                        finished()
                    }

                    override fun onResolveFailed(failed: NsdServiceInfo, errorCode: Int) = finished()
                },
            )
        }
    }
}
