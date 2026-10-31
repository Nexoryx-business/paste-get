// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.nexoryx.pasteget

import android.app.Application
import android.content.Context
import android.os.Build
import android.provider.Settings
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.flow.update
import uniffi.pasteget_ffi.PhoneClient

/** Holds the app's single [PhoneClient] and the connection state the UI shows. */
class PasteGetApp : Application() {
    /** Identity and pairings live in the app's private files directory. */
    val client: PhoneClient by lazy { PhoneClient(filesDir.absolutePath, deviceName()) }

    private val connectedIds = MutableStateFlow<Set<String>>(emptySet())

    /** Device IDs of the computers connected right now. */
    val connected: StateFlow<Set<String>> = connectedIds.asStateFlow()

    fun setConnected(deviceId: String, isConnected: Boolean) {
        connectedIds.update { if (isConnected) it + deviceId else it - deviceId }
    }

    fun clearConnected() {
        connectedIds.value = emptySet()
    }

    /** The name the user gave the phone, e.g. "POCO M8 5G". */
    private fun deviceName(): String =
        Settings.Global.getString(contentResolver, Settings.Global.DEVICE_NAME)
            ?: "${Build.MANUFACTURER} ${Build.MODEL}"
}

val Context.pasteGet: PasteGetApp
    get() = applicationContext as PasteGetApp
