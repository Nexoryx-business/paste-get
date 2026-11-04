// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.nexoryx.pasteget;

/** Implemented by the app: the clipboard changed. */
oneway interface IClipboardChanged {
    void changed();
}
