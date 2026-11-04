// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.nexoryx.pasteget;

import android.content.ClipData;
import android.content.ClipDescription;
import io.github.nexoryx.pasteget.IClipboardChanged;

/**
 * The clipboard as the shell user sees it: served by ShellClipboardService, which Shizuku runs
 * as shell (or root). Unlike the app, shell may read the clipboard while in the background.
 */
interface IShellClipboard {
    /** Shizuku calls this to stop the service. Shizuku fixes its transaction code. */
    void destroy() = 16777114;

    /**
     * Reports every change of the clipboard of `userId` to `listener`, replacing any earlier
     * listener. Returns how the clipboard is reached, for logs. Throws
     * UnsupportedOperationException if this Android version hides it in an unknown way.
     */
    String watch(IClipboardChanged listener, int userId) = 1;

    /** What the clipboard of `userId` holds, without reading it. Null if it's empty. */
    ClipDescription describe(int userId) = 2;

    /** The clip on the clipboard of `userId`. Null if it's empty or the phone is locked. */
    ClipData read(int userId) = 3;
}
