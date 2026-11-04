// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.nexoryx.pasteget

/** Pairing codes shown as QR codes by the computer. The Rust core does the real parsing. */
object PairingCode {
    private const val PREFIX = "pasteget://pair?"

    /** The pairing code in scanned or received [text], or null if it isn't one. */
    fun extract(text: String?): String? = text?.trim()?.takeIf { it.startsWith(PREFIX) }
}
