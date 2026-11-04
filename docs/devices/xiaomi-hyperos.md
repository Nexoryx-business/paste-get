# Xiaomi / POCO / Redmi (HyperOS) setup

Reference device: **POCO M8 5G**, shipped with HyperOS 2 on Android 15.
HyperOS is one of the most aggressive Android skins at killing background apps, so each step matters.
Menu names may differ slightly between HyperOS versions and regions *(to verify on the device)*.

## 1. Developer options
1. Settings → About phone → tap **OS version** 7 times.
2. Settings → Additional settings → Developer options:
   - **USB debugging**: on
   - **USB debugging (Security settings)**: on. This is separate from USB debugging and needed for
     Shizuku to grant permissions and inject input. It may require being signed in to a Mi account
     (sometimes with a SIM inserted).
   - **Wireless debugging**: on (used to start Shizuku without a PC).
3. Don't run the scan in the **Security** app: it can turn Developer options off
   ([Shizuku guide](https://shizuku.rikka.app/guide/setup/)).

## 2. Shizuku
1. Install Shizuku and start it through **Wireless debugging**.
2. If the pairing code notification doesn't show up, switch the notification style to "Android"
   in system settings ([Shizuku guide](https://shizuku.rikka.app/guide/setup/)).
3. Allow Shizuku to run in the background (step 3 below, for Shizuku too).
4. **After every reboot, Shizuku must be started again** (no root). Meanwhile paste-get falls back
   to mode B, using permissions granted earlier.
5. In paste-get, turn on **Send what you copy**, tap **Allow in Shizuku**, then **Grant them**
   (paste-get restarts): that prepares mode B for the next reboot.

## 3. Keep paste-get alive
Settings → Apps → Manage apps → paste-get:
- **Autostart**: on
- **Battery saver**: No restrictions
- **Notifications**: allowed (the persistent notification keeps the service running)

Also: open recent apps, long-press paste-get → **lock** it.

## 4. Permissions paste-get will ask for
| Permission | Why |
|---|---|
| Photos and videos: **Allow all** | Detect new screenshots ("selected photos" breaks detection) |
| Notifications | Foreground service + "sent ✓" feedback |
| Camera | Scan the pairing QR code |
| Shizuku: allow paste-get | Read the clipboard as the shell user (mode A) |
| Display over other apps (granted through Shizuku) | Mode B: a window shown for a moment reads the clipboard |
| Device logs (granted through Shizuku; Android asks again after each start) | Mode B: the log says when the clipboard changed |
| Notification access (v0.2) | 2FA codes |

## 5. Things to check on HyperOS *(verify)*
- Whether mode B's window may take focus from the background, or HyperOS also wants **Display
  pop-up windows while running in the background** (Settings → Apps → paste-get → Other
  permissions).
- Whether HyperOS shows its own notice when Shizuku reads the clipboard.
