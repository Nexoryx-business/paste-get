# Technical audit

How every planned feature can be built, what's hard, and what isn't possible.
Written before any code, October 2026. Claims marked *(verify)* still need testing on a real device.

Legend: ✅ straightforward · 🟡 doable, with traps · 🔴 limited by Android or the OS

## TL;DR

- **Almost everything is feasible.** About 70% is ordinary development, 20% needs Shizuku,
  10% is limited by Android or the desktop OS.
- **The three real obstacles:**
  1. reading the Android clipboard in the background;
  2. GNOME on Wayland (no clipboard-manager protocol);
  3. Android vendors (Xiaomi included) killing background apps.
- **Not possible as originally imagined:**
  - "Copy a file in an Android file manager, paste it in Explorer": file managers almost never
    use the system clipboard. Files go through the share sheet instead.
  - "Continue on PC the page open on the phone" can't be automatic: Android doesn't expose
    another app's current URL. Share sheet again (2 taps).

## Target platforms

| Tier | Platform | Notes |
|---|---|---|
| 1 | Windows 11 | Clean clipboard APIs |
| 1 | Arch Linux + KDE Plasma 6 (Wayland) | KWin supports data-control: background clipboard access works |
| 1 | Arch Linux + Hyprland | wlroots data-control: works. Hotkeys can be plain `bind = …, exec, pasteget …` |
| 1 | Android 15 / HyperOS 2 (POCO M8 5G) | Aggressive background killing, extra Shizuku toggle. See `devices/xiaomi-hyperos.md` |
| 2 | macOS | No clipboard change event: poll every 250–500 ms |
| 2 | GNOME (Wayland) | Needs a GNOME Shell extension (like GSConnect) |
| 2 | Other Android vendors | Test matrix later |

**Dual boot:** Windows and Arch are two separate devices for paste-get, each with its own key and
pairing. Only one is online at a time; the phone just sends to whichever answers.

## 1. Architecture

```
┌─────────── Android ────────────┐          ┌──────────── PC ─────────────┐
│ Kotlin + Jetpack Compose UI    │          │ Tauri tray UI (later)       │
│ Foreground service             │          │ Rust daemon + CLI           │
│  ├ Watchers: clipboard,        │   LAN    │  ├ Clipboard per OS         │
│  │  screenshots, notifications │◄────────►│  ├ Hotkeys, native OCR      │
│  ├ Shizuku service (as shell)  │ mTLS     │  ├ Local API                │
│  └ Rust core (UniFFI)          │          │  └ Rust core (same crate)   │
└────────────────────────────────┘          └─────────────────────────────┘
      core = protocol + crypto + pairing + history + tests
```

| Piece | Choice | Why | Rejected |
|---|---|---|---|
| Shared core | **Rust**, used from Android through UniFFI | One implementation of protocol and crypto, testable and fuzzable. Delta Chat ships a Rust core on F-Droid | Two implementations (Kotlin + Rust): twice the protocol bugs |
| Android | Kotlin + Compose + a Shizuku user service | System and hidden APIs are Java/Kotlin only | Flutter: everything important would still be native |
| Desktop | **Headless Rust daemon + CLI first**, Tauri 2 tray UI on top | The daemon is testable without a UI; Tauri gives tray, global shortcuts, autostart, updater in ~10 MB | Electron: 150 MB for a tray app |
| Wire format | Protobuf (`prost`) | Clear, versioned spec; binary-friendly for images | JSON: bad for binary data |

## 2. Reading the Android clipboard in the background

Since Android 10, only the focused app and the default keyboard can read the clipboard.

| Mode | How | Pros | Cons |
|---|---|---|---|
| **A. Shizuku service as `shell`** | A Shizuku user service runs with the shell identity and listens to the clipboard through the hidden `IClipboard` interface. Same technique as scrcpy, which syncs the device clipboard automatically | Event-driven, clean | Hidden API: signatures change across Android versions and vendors ([scrcpy #4703](https://github.com/Genymobile/scrcpy/issues/4703)), so several reflection fallbacks are needed. Shizuku dies on reboot (no root). A "Shell pasted from your clipboard" toast may appear on Android 12+ *(verify)* |
| **B. `READ_LOGS` + invisible overlay** | Watch logcat for the "denied clipboard access" message, then briefly focus a transparent overlay to read the clip ([ClipCascade](https://github.com/Sathvik-Rao/ClipCascade)) | Grants **survive reboots** | Hacky: steals focus, may close the keyboard. Android 13+ asks for log access every time the service starts |
| **C. Our own keyboard (IME)** | The default keyboard may read the clipboard | Official, never breaks | Huge project; user must switch keyboards |
| **D. Manual** | Quick Settings tile, share sheet, text selection menu | Works everywhere | Not automatic |

**Decision:** automatic fallback **A → B → D**, shown on a health screen ("Mode: Shizuku ✓").

**Key idea:** also use Shizuku as a **one-time permission granter**: `READ_LOGS`,
`SYSTEM_ALERT_WINDOW`, `RECEIVE_SENSITIVE_NOTIFICATIONS`, usage stats. These grants persist after
a reboot, so when Shizuku is down the app drops to mode B instead of stopping, and offers a
"restart Shizuku" shortcut.

## 3. Keeping the Android service alive

- **Foreground service type `connectedDevice`, not `dataSync`.** On Android 15, `dataSync`
  is capped at **6 hours per 24 hours**
  ([docs](https://developer.android.com/develop/background-work/services/fgs/timeout)).
- **Start on boot:** Android 15 forbids some types (including `dataSync`) from `BOOT_COMPLETED`;
  `connectedDevice` should be allowed *(verify)*.
- **Xiaomi/HyperOS:** Autostart permission, "No restrictions" battery mode, lock in recents.
  The app should detect and guide the user (see `devices/xiaomi-hyperos.md`).
- **Network:**
  - hold a `MulticastLock` for mDNS;
  - reconnect on network changes;
  - newer Android versions are adding a local network permission *(verify)*.

## 4. Screenshots → PC

- **Standard approach:** `ContentObserver` on MediaStore images, filter the `Screenshots` folder
  (path varies by vendor; Xiaomi uses `DCIM/Screenshots` *(verify)*), wait for `IS_PENDING = 0`.
- **Android 14+ trap:** with "selected photos only" access, detection sees nothing. Request full
  access and explain why.
- **Shizuku alternative:** watch the folder directly as shell, no photo permission needed.
- Android 14's screenshot callback only works for one's own activity, so it's useless here.

## 5. Desktop clipboard per OS

| | Watch changes | Images | Mark as sensitive | Global hotkeys | Type text | Verdict |
|---|---|---|---|---|---|---|
| **Windows** | `AddClipboardFormatListener` | PNG + DIB | Exclusion formats so Windows history and Cloud Clipboard skip it | ✅ `RegisterHotKey` | ✅ `SendInput` | ✅ |
| **KDE Plasma 6 (Wayland)** | ✅ data-control | ✅ | `x-kde-passwordManagerHint` | GlobalShortcuts portal | 🟡 RemoteDesktop portal | ✅ |
| **Hyprland** | ✅ wlr-data-control | ✅ | idem | Config `bind` → CLI | 🟡 portal / `wtype` | ✅ |
| **X11** | ✅ (daemon owns the selection) | ✅ | idem | ✅ | ✅ | ✅ |
| **macOS** | Poll `changeCount` | ✅ | `org.nspasteboard.ConcealedType` | ✅ | 🟡 Accessibility permission | ✅ |
| **GNOME (Wayland)** | ❌ Mutter has no data-control ([source](https://docs.rs/lamco-data-control)) | ❌ | | 🟡 | 🟡 | 🔴 → extension |

Library: `arboard` (with the `wayland-data-control` feature), wrapped behind our own trait so
OS-specific extras (sensitive markers, file lists, HTML) can be added per platform.

## 6. Network, pairing, security

- **Discovery:** mDNS `_pasteget._tcp`.
  - Windows 10+ has a native DNS-SD API, so no Bonjour needed.
  - Manual IP as fallback, which also makes Tailscale work for free.
- **Transport:** TCP + TLS 1.3, mutual authentication, keys pinned at pairing (rustls).
  - Windows shows a firewall prompt on first run; Arch may need a port opened if a firewall is enabled.
- **Pairing:** the PC shows a QR code (device id, key fingerprint, IPs, port, one-time secret); the
  phone scans it. Fallback: 6-digit code with SPAKE2. Devices can be revoked from either side.
- **Transfers:** the sender offers metadata first (hash, MIME types, size), then streams resumable chunks.
- **No echo loops:** every clip carries origin device + content hash. Clips we just applied are
  ignored by the local watcher. On Android 13+, clips we write carry `EXTRA_IS_REMOTE_DEVICE`.
- **History:** SQLite + FTS5, encrypted with a key stored in the OS keystore (Android Keystore,
  Windows DPAPI, Secret Service on Linux, Keychain on macOS).
- **Away from home:**
  - user-hosted encrypted relay;
  - evaluate *iroh* for NAT traversal;
  - **USB via `adb reverse`**, since Shizuku users already have USB debugging enabled.

### Threat model (to expand in `SECURITY.md`)

| Threat | Mitigation |
|---|---|
| Attacker on the same Wi-Fi | Pinned keys, no unauthenticated endpoints |
| Stolen phone | Revoke from the PC; history encrypted at rest |
| Other local app on the PC | Local API on a user-only socket/named pipe + token |
| Passwords leaking through sync | Sensitive flag, app denylist, pattern detection, auto-expiry |
| Supply chain | `cargo-deny`, pinned dependencies, signed releases, reproducible builds |

## 7. Feature by feature

| Feature | How | Difficulty | Trap |
|---|---|---|---|
| Screenshot → PC clipboard | Section 4 | ✅ | Full photo access required |
| Text both ways | Section 2. Writing the Android clipboard from the background is allowed | 🟡 | Background reading |
| Images PC → phone | Share via `FileProvider` URI, not raw bytes | 🟡 | 1 MB Binder limit |
| Skip sensitive copies | `EXTRA_IS_SENSITIVE`, foreground app (usage stats via Shizuku), patterns (Luhn, IBAN, private keys) | 🟡 | Many apps don't set the flag |
| 2FA codes | Notification listener + code extraction | 🟡 | **Android 15 hides OTPs** from untrusted listeners. Fix via Shizuku: `appops set <pkg> RECEIVE_SENSITIVE_NOTIFICATIONS allow` ([source](https://androidauthority.com/android-15-two-factor-authentication-codes-3492585)) |
| OCR | **On the PC**, with the OS engine (Windows.Media.Ocr, Tesseract on Linux, Vision on macOS) | ✅ | Not ML Kit on the phone: proprietary, breaks F-Droid |
| Blur sensitive data | v1 on the PC (OCR returns word positions). Later on the phone (ONNX models) | 🔴 | Hardest image feature |
| Crop status/nav bars | Inset sizes + orientation | 🟡 | Full-screen apps have no bars: make it optional |
| Strip GPS, convert HEIC | `ExifInterface`, `ImageDecoder` | ✅ | |
| Clean paste (links, plain text, JSON) | Desktop only | ✅ | |
| Smart actions | Links, colors, addresses ✅. "Call from phone" 🟡 | 🟡 | Background activity starts blocked → `am start` via Shizuku |
| History, search, pins | SQLite FTS; last-writer-wins sync | ✅ | |
| Expiring clips, private mode | Timer + Quick Settings tile | ✅ | |
| Rich text (HTML) | Per-OS format | 🟡 | Windows "HTML Format" header |
| Files phone → PC | Share sheet; pasteable as real files on the PC | ✅ | ❌ from Android file managers |
| Send from PC | Explorer "Send to", Dolphin service menu, macOS Services | ✅ | ❌ drag onto a Windows tray icon is unsupported |
| Links PC → phone | Browser extension (MV3) + native messaging | 🟡 | Registration per browser and OS |
| Links phone → PC | Share sheet | ✅ | ❌ not automatic |
| "Insert from phone" (camera) | PC hotkey → phone opens camera via Shizuku `am start` | 🟡 | Locked phone → notification. FOSS document scanner (OpenCV) later |
| QR → PC | zxing-cpp | ✅ | |
| Type on phone with PC keyboard | Key injection via Shizuku; Unicode via clipboard + `KEYCODE_PASTE` | 🟡 | `input text` has no Unicode. Xiaomi needs "USB debugging (Security settings)" |
| Dictation phone → PC cursor | Speech recognition on the phone, typing on the PC | 🟡 / 🔴 Wayland | Mic needs an action on the phone; can't be triggered from the PC |
| Send to the active PC | Each PC reports idle time | 🟡 | Different API per Wayland desktop |
| Wi-Fi rules | Network name | 🟡 | **Reading the SSID needs location** on Android 10+ |
| Ring phone, battery level | | ✅ | |
| CLI, local API, launchers | Token-protected local socket / named pipe | ✅ | |
| Plugins / hooks | v1: "run a command" only | 🟡 | No remote code execution |

## 8. Distribution

- **Android:** GitHub Releases + IzzyOnDroid + Obtainium first, then F-Droid (`foss` / `full` flavors).
  Play Store is risky (`READ_LOGS`, Shizuku, foreground service justification): later.
- **Windows:** unsigned builds trigger SmartScreen. SignPath offers free signing for open source.
- **Linux:** AppImage, AUR package, .deb/.rpm. Avoid Flatpak at first (sandbox vs clipboard access).
- **macOS:** notarization needs a paid Apple developer account. Homebrew cask in the meantime.

## 9. Main risks

| Risk | Impact | Mitigation |
|---|---|---|
| Hidden clipboard API changes (Android version or vendor) | High | Reflection fallbacks, A → B → D modes, device matrix |
| HyperOS kills the app | High (it's the main test phone) | `connectedDevice` service, setup guide, health screen |
| Scope creep | High | Strict roadmap |
| Trust (the app sees everything you copy) | High | Open source, local-only, sensitive filters, visible activity log |
| GNOME Wayland | Medium (not tier 1) | Shell extension later |
| Signing costs | Low | SignPath; Apple later |

## Sources

- [scrcpy clipboard docs](https://github.com/Genymobile/scrcpy/blob/master/doc/control.md), [scrcpy #4703](https://github.com/Genymobile/scrcpy/issues/4703)
- [ClipCascade](https://github.com/Sathvik-Rao/ClipCascade), [Sefirah](https://github.com/shrimqy/Sefirah-Android)
- [Android FGS timeouts](https://developer.android.com/develop/background-work/services/fgs/timeout), [Secure clipboard handling](https://developer.android.com/privacy-and-security/risks/secure-clipboard-handling)
- [Android 15 hides 2FA codes](https://androidauthority.com/android-15-two-factor-authentication-codes-3492585)
- [lamco-data-control (compositor support)](https://docs.rs/lamco-data-control)
- [Shizuku setup guide](https://shizuku.rikka.app/guide/setup/)
- [KDE Connect clipboard discussion](https://discuss.kde.org/t/is-it-possible-to-send-text-or-links-from-to-android-windows-without-enabling-permanent-clipboard-sync/33799)
