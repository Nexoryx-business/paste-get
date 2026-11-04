# Security policy

> **Draft.** paste-get is pre-alpha and its crypto and protocol code has not been reviewed by
> anyone but its authors yet (see [`docs/REVIEW.md`](docs/REVIEW.md)). Don't rely on it to
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
  works once, for 5 minutes, and 3 wrong tries close the window.
- **No server, no account, no telemetry.** Clips go straight from one device to another on the
  local network.

The full protocol is in [`docs/PROTOCOL.md`](docs/PROTOCOL.md).

## What it protects against

| Threat | Protection |
|---|---|
| Someone on the same Wi-Fi reading or changing clips | TLS 1.3 to a pinned key; nothing is sent to a device that isn't paired |
| Someone on the network pretending to be your PC or phone | Their key isn't the pinned one: the handshake fails |
| Pairing with the wrong PC | The QR code carries the PC's device ID; TLS fails for any other key |
| A device pairing without seeing your screen | It needs the one-time secret from the QR code |
| Another user of the same computer | The local control channel is a socket in a directory only you can open (Linux), or a named pipe that rejects remote clients and includes your user name (Windows) |
| Passwords ending up in logs | Logs contain sizes, MIME types and hashes only, never clipboard contents |
| Passwords lingering | Clips marked sensitive (password managers) skip clipboard history on Windows and the tray's *Recent* list |
| Oversized or malformed input from a paired device | Frames are limited to 1 MiB, clips to 64 MiB, images are size-checked before decoding; the parsers are fuzzed |

## What it doesn't protect against (yet)

- **A photographed QR code.** Someone who sees the code can pair within the 5 minutes, before
  you do. Pair where nobody can see the screen.
- **A stolen or lost phone.** It stays paired. The phone can forget a PC, but **the PC can't
  revoke a phone yet**: the only way today is to stop paste-get and delete the device from
  `devices.toml` in the profile directory.
- **A compromised paired device.** Paired devices are trusted with your clipboard: anything you
  copy goes to them.
- **Malware running as you on the PC or phone.** It can read the clipboard anyway, and the
  identity key is a file readable by your user (not yet in the OS keystore).
- **Network metadata.** mDNS announces that paste-get runs on this computer, with its device
  name. Traffic sizes and timing are visible on the network.
- **Unsigned packages.** Windows builds aren't code-signed yet; check downloads against the
  release's `SHA256SUMS`.

## Supply chain

- Dependencies come from crates.io only and are checked by `cargo-deny` (licenses, known
  vulnerabilities, sources) on every push.
- No home-made cryptography: rustls, ring and rcgen do it.
- Release packages are built by GitHub Actions from the tagged commit (`docs/RELEASING.md`).
