# Security policy

> **Draft.** paste-get is in beta (0.1.0) and its crypto and protocol code has not been reviewed
> by anyone but its authors yet (see [`docs/REVIEW.md`](docs/REVIEW.md)). Don't rely on it to
> protect anything you couldn't afford to leak.

paste-get sees everything you copy. This page says what it protects against, what it doesn't,
and how to report a problem.

## Reporting a vulnerability

Please **don't open a public issue**. Use GitHub's private reporting instead: the repository's
*Security* tab → *Report a vulnerability*. Include what an attacker needs (same Wi-Fi? a paired
device? local access?), what they get, and how to reproduce it.

You'll get an answer within a week. Fixes for confirmed problems go out as soon as they're ready,
and the advisory credits you unless you'd rather not.

Supported versions: only the latest release, while paste-get is below 1.0.

## How it works, in short

- Each device has an **Ed25519 key**. Its **device ID** is the SHA-256 of the public key.
- Devices talk over **TLS 1.3 with mutual authentication** (rustls). Each side checks that the
  other's key is one it paired with; there are no certificate authorities. TLS 1.2 is off.
- **Pairing**: the PC shows a QR code with its device ID, its addresses and a one-time 128-bit
  secret. The phone connects pinning that device ID and sends the secret inside TLS. The secret
  works once, for 5 minutes. Wrong tries don't close the window, so someone on the network can't
  keep closing it; guessing a 128-bit secret in 5 minutes is hopeless. While the tray's pairing
  window stays open, a new code (a new secret) replaces each one that expires.
- **No server, no account, no telemetry, no update checks.** Clips go straight from one device
  to another on the local network; paste-get makes no other network call. The tray's *About*
  window and pairing window show web addresses as text to copy, never as links.

The full protocol is in [`docs/PROTOCOL.md`](docs/PROTOCOL.md).

## What it protects against

| Threat | Protection |
|---|---|
| Someone on the same Wi-Fi reading or changing clips | TLS 1.3 to a pinned key; nothing is sent to a device that isn't paired |
| Someone on the network pretending to be your PC or phone | Their key isn't the pinned one: the handshake fails |
| Device reconnaissance over mDNS | `announce_name = false` in `settings.toml` suppresses the computer's name in local mDNS TXT announcements, advertising only the protocol version and device ID |
| Pairing with the wrong PC | The QR code carries the PC's device ID; TLS fails for any other key |
| A device pairing without seeing your screen | It needs the one-time secret from the QR code. On Windows, the pairing window sets display affinity (`SetWindowDisplayAffinity`) to exclude the QR code and secret from screen recordings and screenshots |
| A web page or message opening a pairing link (`pasteget://pair?…`) on the phone | The phone never pairs on its own: it shows the name and addresses the link gives (unchecked, without characters that could disguise them) and pairs only when you tap *Pair* under "Only continue if this code is on your computer's screen right now". Another link arriving while that question is shown is ignored, so it can't take the place of the code you are reading. Even then it connects only to addresses on the local network: never to a server on the Internet, nor to another app on the phone itself (at none of the phone's own addresses: loopback, Wi-Fi, hotspot) |
| Another user of the same computer | The local control channel is a socket in a directory only you can open (Linux), or a named pipe that rejects remote clients and includes your user name (Windows). `identity.key` and configuration files are restricted to the current user (mode 0600/0700 on Unix; explicit DACL via `icacls` on Windows even under custom `PASTEGET_HOME`). `pasteget`, `pasteget status --json` and the tray's `--action` all go through it |
| Devices on a café or hotel Wi-Fi reaching paste-get (Windows) | Windows' firewall blocks paste-get on networks marked *Public*. paste-get says when one is, and the lines its Settings window suggests replace paste-get's rules by one that allows *private* networks only |
| Passwords ending up in logs | Logs contain sizes, MIME types and hashes only, never clipboard contents |
| Passwords lingering | A copy that a password manager marks as secret is sent marked sensitive. On the PC that is `x-kde-passwordManagerHint` on Wayland (KeePassXC, KDE apps) and the formats KeePass, KeePassXC, Bitwarden and 1Password set on Windows; on the phone, Android's `IS_SENSITIVE` flag. When the PC can't check a copy for the mark (another app holds the clipboard, or the copy changed meanwhile), it checks again and, failing that, sends it as sensitive. Sensitive clips, whether the phone sends them by itself or by hand (tile, notification, shortcut), stay out of the tray's *Recent* list and never wait for a device that is away. Where they arrive, they skip Windows' clipboard history and Klipper on the PC, and are flagged sensitive on the phone's clipboard, which hides Android's preview |
| 2FA codes leaking | Opt-in. The phone reads notifications on the phone only; just the code's digits go to paired computers, as a secret (out of history and *Recent*). The PC's notification names the app, never the code. Neither the code nor the message is logged |
| A link from a paired device opening by itself | Nothing opens without a click on its notification. Only `http`/`https` links that are the whole clip; the site is shown first; links with a `user@` part (which hide the real site) and non-ASCII hosts are refused |
| Clips leaving the local network through the OS | On Windows, received clips are marked so the cloud clipboard doesn't upload them to the Microsoft account (they stay in the local Win+V history unless sensitive) |
| A secret spreading past the devices you paired | A password or 2FA code goes only to the devices paired with the one where it was copied: a computer never passes it on to its other devices (another phone, a computer that joined it). Other copies are passed on once, and a device takes each copy once; to tell copies apart it keeps random IDs for 10 minutes, in memory, never a hash of what was copied |
| A copy kept for a device that is away | Only the last non-sensitive clip waits, until the device confirms it got it, in memory, never on disk; passwords and 2FA codes are never kept, and a copy made before one stops waiting. Forgetting the device drops it |
| Someone on the network opening connections to the PC by the thousand | Connections not known to come from a paired device get 3 s for the TLS handshake and 3 s for their first message, 8 at once per address and 64 in all. Past 64, the oldest of the address holding the most is closed, and never one from an address a paired device connected from since the PC started: a paired device still connects during a flood of silent connections, and during one that never pauses if it connects from such an address or the flood comes from fewer than 64 addresses. Not protected: a device with more addresses than that, flooding without pause, can keep out one connecting from a new address |
| Oversized or malformed input from a paired device | Frames are limited to 1 MiB, clips to 64 MiB, images are size-checked before decoding; the parsers are fuzzed |
| A lost or stolen phone, once you unpair it | On the PC: tray menu → the phone → *Forget this device*, or `pasteget forget <name>`. Its connection ends at once and it can't connect again without a new pairing code; the phone is told before the clips still queued for it, which it no longer gets, and forgets the PC too. The phone can forget a PC too (*Devices* → the PC → *Forget this computer…*, which asks first): if connected, the PC is told and forgets the phone too |

## The built-in mode (wireless debugging)

To copy automatically without Shizuku, the app pairs itself with Android's wireless debugging,
once, with the code Settings shows. Its key then gives it the same rights as a paired computer
running `adb`: that is how it starts its shell helper. Consequences:
- The key is a file only the app can read, and it is never backed up. Taking it would need root
  or a debugging session already open on the phone, which give full control anyway.
- The app only connects to the phone itself, and switches wireless debugging off again as soon
  as the helper runs; it switches it on for a moment only on Wi-Fi.
- To revoke it: Settings → Developer options → Wireless debugging → Paired devices → paste-get →
  Forget, or "Pair again" in the app.
- With **Send 2FA codes**, the same shell grants the app notification access and, on Android 15,
  the right to see codes Android hides from notification listeners
  (`RECEIVE_SENSITIVE_NOTIFICATIONS`). Revoke them in Settings → Notification access, or with
  `adb shell appops set io.github.nexoryx.pasteget RECEIVE_SENSITIVE_NOTIFICATIONS default`.
- The same grant lets the app change secure settings. It uses it for wireless debugging and,
  only if you turn on *Hide "pasted from your clipboard"*, for Android's *Show clipboard access*
  switch, which then applies to every app.

## The clipboard history (being built, off by default)

The history store is in the core (`core/src/history`). The phone API can record into it
(`ffi/src/history.rs`), but the app doesn't turn it on yet. On a PC it is on only once
`settings.toml` says so (`[history]`, `enabled = true`). When it is on:
- It stays on the device, in a folder only paste-get can open on the phone (one that is never
  backed up), and only your user account on a PC (mode 0700 on Linux and macOS, set when the
  history opens): `%LOCALAPPDATA%\paste-get\<profile>\history` on Windows, which doesn't roam
  with the profile, and `~/.local/share/paste-get/<profile>/history` on Linux, outside
  `~/.config`, which dotfile repositories often hold. It is never sent anywhere.
- Images from other devices are decoded on the PC to make thumbnails (`image` crate, 512 MiB
  of memory at most); they come from paired devices only.
- **Never kept:** clips marked sensitive (password managers), 2FA codes (and copies of their
  digits made just before or in the 15 minutes after), and anything that looks like a private
  key, a 2FA seed (`otpauth://`), an API token, a card number or a generated password
  (`core/src/detect.rs`). Only how many were kept out is counted. A number of 4 to 8
  digits alone is kept for 15 minutes: then it is hidden, and deleted like below the next time
  the history applies its limits (after the next clip, or when the app's timer for it fires).
- **Not encrypted by paste-get in this version.** It relies on the device's disk encryption:
  Android encrypts app data; on a PC, that is BitLocker or LUKS. **On a PC without disk
  encryption, anyone who can read the disk can read the history.** Encryption with a key in the
  OS keystore is planned, together with the identity key.
- Deleting a clip removes it from the database, its journal (WAL) and its search index. Flash
  storage may still hold old copies of the blocks until it reuses them. Clips the history drops
  for their age or its size limits leave the database at once, and the journal at the next
  checkpoint.
- Logs and error messages never contain what was copied or searched.

## What it doesn't protect against (yet)

- **A photographed QR code.** Someone who sees the code can pair within the 5 minutes, before
  you do. Pair where nobody can see the screen. The pairing window closes by itself once a
  device paired; otherwise, close it: while it is open, it shows a new code every 5 minutes.
- **A stolen or lost phone, until you unpair it.** It stays paired and keeps receiving what you
  copy whenever it is on the same network. Unpair it from the PC as soon as you can (above).
- **A compromised paired device.** Paired devices are trusted with your clipboard: anything you
  copy goes to them.
- **Malware running as you on the PC or phone.** It can read the clipboard anyway, and the
  identity key is a file readable by your user (not yet in the OS keystore).
- **Passwords that aren't marked.** paste-get only knows a copy is a password when the app
  marks it (above). On X11 and GNOME the PC can't see the mark, and some apps never set it: such
  a password goes out as an ordinary clip, shows in the tray's *Recent* list and waits for a
  device that is away.
- **Network metadata.** mDNS announces that paste-get runs on this computer, with its device
  name. Traffic sizes and timing are visible on the network.
- **Unsigned packages.** Windows builds aren't code-signed yet; check downloads against the
  release's `SHA256SUMS`.

## Supply chain

- Dependencies come from crates.io only and are checked by `cargo-deny` (licenses, known
  vulnerabilities, sources) on every push.
- No home-made cryptography: rustls, ring and rcgen do it.
- Release packages are built by GitHub Actions from the tagged commit (`docs/RELEASING.md`).
