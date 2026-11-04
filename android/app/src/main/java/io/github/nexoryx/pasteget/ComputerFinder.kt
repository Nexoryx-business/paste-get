// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.nexoryx.pasteget

import android.content.Context
import android.net.nsd.NsdManager
import android.net.nsd.NsdServiceInfo
import android.os.Build
import android.os.Handler
import android.os.Looper
import android.util.Log
import androidx.annotation.MainThread
import androidx.annotation.RequiresApi
import androidx.core.content.ContextCompat
import uniffi.pasteget_ffi.PhoneClient
import java.net.Inet4Address
import java.net.InetAddress

/**
 * Finds paired computers on the local network (mDNS / DNS-SD, see `docs/PROTOCOL.md`, section
 * 2.4) and tells [PhoneClient] where they are. A computer whose address changed (new DHCP lease,
 * other network, the other OS of a dual boot) is then reconnected right away.
 *
 * Uses the system's NsdManager: no multicast lock and no extra permission. The service turns it
 * on only while a paired computer is disconnected. Call everything on the main thread.
 */
class ComputerFinder(context: Context, private val client: PhoneClient) {
    private val nsd = context.getSystemService(NsdManager::class.java)
    private val main = Handler(Looper.getMainLooper())
    private val resolver: Resolver =
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.UPSIDE_DOWN_CAKE) {
            FollowingResolver(context)
        } else {
            QueuedResolver()
        }

    private var discovery: NsdManager.DiscoveryListener? = null

    @MainThread
    fun setActive(active: Boolean) {
        if (active) start() else stop()
    }

    /** The phone joined another network: search that one. */
    @MainThread
    fun restart() {
        if (discovery != null) {
            stop()
            start()
        }
    }

    private fun start() {
        if (discovery != null) return
        val listener = object : NsdManager.DiscoveryListener {
            override fun onDiscoveryStarted(serviceType: String) = Unit

            override fun onDiscoveryStopped(serviceType: String) = Unit

            override fun onStartDiscoveryFailed(serviceType: String, errorCode: Int) {
                Log.w(TAG, "network discovery failed to start: $errorCode")
                main.post { if (discovery === this) discovery = null }
            }

            override fun onStopDiscoveryFailed(serviceType: String, errorCode: Int) = Unit

            override fun onServiceFound(info: NsdServiceInfo) {
                main.post {
                    if (discovery === this && isPaired(info.serviceName)) resolver.resolve(info)
                }
            }

            override fun onServiceLost(info: NsdServiceInfo) {
                main.post { resolver.lost(info.serviceName) }
            }
        }
        discovery = listener
        nsd.discoverServices(SERVICE_TYPE, NsdManager.PROTOCOL_DNS_SD, listener)
    }

    private fun stop() {
        val listener = discovery ?: return
        discovery = null
        resolver.stop()
        // Throws if discovery never started or already stopped: nothing left to stop then.
        runCatching { nsd.stopServiceDiscovery(listener) }
    }

    /** Announcements are named `paste-get-<first 12 characters of the device ID>`. */
    private fun isPaired(serviceName: String): Boolean {
        val name = serviceName.lowercase()
        return client.pairedComputers().any { name.startsWith(LABEL_PREFIX + it.deviceId.take(12).lowercase()) }
    }

    /** A resolved announcement. The TXT `id` must be a paired computer's full device ID. */
    private fun report(info: NsdServiceInfo, addresses: List<InetAddress>) {
        if (discovery == null) return
        val id = info.attributes["id"]?.decodeToString() ?: return
        val computer = client.pairedComputers().firstOrNull { it.deviceId.equals(id, ignoreCase = true) }
            ?: return
        // Protocol version 0 is IPv4 only.
        val ips = addresses.filterIsInstance<Inet4Address>().mapNotNull { it.hostAddress }
        if (ips.isEmpty() || info.port !in 1..65535) return
        Log.i(TAG, "found ${computer.name} on the network")
        client.computerFound(computer.deviceId, ips, info.port.toUShort())
    }

    private interface Resolver {
        fun resolve(info: NsdServiceInfo)

        fun lost(serviceName: String)

        fun stop()
    }

    /** Android 14+: follows each announcement and hears about every address change. */
    @RequiresApi(Build.VERSION_CODES.UPSIDE_DOWN_CAKE)
    private inner class FollowingResolver(context: Context) : Resolver {
        private val executor = ContextCompat.getMainExecutor(context)
        private val followed = mutableMapOf<String, NsdManager.ServiceInfoCallback>()

        override fun resolve(info: NsdServiceInfo) {
            if (info.serviceName in followed) return
            val callback = object : NsdManager.ServiceInfoCallback {
                override fun onServiceInfoCallbackRegistrationFailed(errorCode: Int) {
                    followed.remove(info.serviceName)
                }

                override fun onServiceUpdated(updated: NsdServiceInfo) = report(updated, updated.hostAddresses)

                override fun onServiceLost() = Unit

                override fun onServiceInfoCallbackUnregistered() = Unit
            }
            followed[info.serviceName] = callback
            nsd.registerServiceInfoCallback(info, executor, callback)
        }

        override fun lost(serviceName: String) {
            followed.remove(serviceName)?.let(::unfollow)
        }

        override fun stop() {
            followed.values.forEach(::unfollow)
            followed.clear()
        }

        private fun unfollow(callback: NsdManager.ServiceInfoCallback) {
            runCatching { nsd.unregisterServiceInfoCallback(callback) }
        }
    }

    /** Android 10 to 13: one resolution at a time, the only way these versions allow. */
    private inner class QueuedResolver : Resolver {
        private val queue = ArrayDeque<NsdServiceInfo>()
        private var busy = false

        override fun resolve(info: NsdServiceInfo) {
            if (queue.none { it.serviceName == info.serviceName }) queue.addLast(info)
            next()
        }

        override fun lost(serviceName: String) {
            queue.removeAll { it.serviceName == serviceName }
        }

        // A resolution in flight finishes on its own; report() drops it once stopped.
        override fun stop() = queue.clear()

        @Suppress("DEPRECATION") // The replacement, registerServiceInfoCallback, is API 34+.
        private fun next() {
            if (busy) return
            val info = queue.removeFirstOrNull() ?: return
            busy = true
            nsd.resolveService(
                info,
                object : NsdManager.ResolveListener {
                    override fun onServiceResolved(resolved: NsdServiceInfo) {
                        main.post {
                            busy = false
                            report(resolved, listOfNotNull(resolved.host))
                            next()
                        }
                    }

                    override fun onResolveFailed(failed: NsdServiceInfo, errorCode: Int) {
                        main.post {
                            busy = false
                            if (errorCode == NsdManager.FAILURE_ALREADY_ACTIVE) {
                                queue.addFirst(failed)
                                main.postDelayed(::next, RETRY_DELAY_MS)
                            } else {
                                next()
                            }
                        }
                    }
                },
            )
        }
    }

    private companion object {
        const val TAG = "paste-get"
        const val SERVICE_TYPE = "_pasteget._tcp"
        const val LABEL_PREFIX = "paste-get-"
        const val RETRY_DELAY_MS = 500L
    }
}
