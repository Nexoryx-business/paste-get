# Security review guide

Before release 0.1, someone other than its authors reviews the crypto and protocol code
(ROADMAP M8). This guide says where to look and what must hold, so a reviewer can do it in a
day or two. It doesn't replace reading the code.

**Status: not reviewed yet.** When a review is done, add it to the log at the end.

## Scope

About 3,000 lines of Rust in `core/src/`, plus the parts of the desktop and phone code that
make trust decisions. In order of risk:

| File | What it does | Lines |
|---|---|---|
| `core/src/transport.rs` | TLS 1.3 setup, custom certificate verifiers (pinning), ALPN, `UnverifiedConnection` | ~620 |
| `core/src/pairing.rs` | Invite (QR code) parsing, pairing window, secret check, `pair_with` / `accept_pairing` | ~540 |
| `core/src/identity.rs` | Ed25519 key, device ID = SHA-256 of the SPKI, certificate generation | ~200 |
| `core/src/session.rs` | Session per device, reconnects, who may stay connected | ~670 |
| `core/src/clip.rs` | Clip offer/chunks/ack: sizes, offsets, hash check, reassembly | ~510 |
| `core/src/framing.rs` | Length-prefixed frames, 1 MiB limit | ~190 |
| `core/src/echo.rs` | Echo-loop guard (not a security boundary, but it hashes untrusted data) | ~120 |
| `desktop/src/daemon.rs` | `handle_incoming`: known device → session, unknown → pairing only while a window is open | part |
| `desktop/src/control.rs` | Local control channel: Unix socket in a 0700 directory, Windows named pipe | ~550 |
| `desktop/src/config.rs` | Key file permissions (0600), profile names (path traversal) | part |
| `ffi/src/lib.rs` | The same decisions on the phone side, through the core | part |
| `ffi/src/wireless_adb/` | ADB client for the phone's own wireless debugging: pairing (TLS 1.3 + BoringSSL SPAKE2 + AES-GCM), `exec:` over TLS | ~700 |
| `android/…/ShellHelper.kt`, `ShellLinkServer.kt`, `BuiltInShell.kt` | The shell helper the app starts through it, and the local socket between them | ~600 |

The protocol itself is specified in `docs/PROTOCOL.md`; the review should also say whether the
code matches it.

## What must hold

Each line is a claim to check. "→" points at where it's enforced.

### Trust and authentication
1. A client connecting to a device ID completes the handshake **only** if the server's key
   hashes to that ID. → `PinnedServerVerifier::verify_server_cert`
2. Both verifiers delegate the proof of key possession to rustls
   (`verify_tls13_signature`); a certificate alone, without the private key, is useless.
3. TLS 1.2 can't be negotiated. → `with_protocol_versions(&[&TLS13])`, `tls12_disabled`
4. An accepted connection can't send or receive application data before the daemon decided
   the peer is trusted. → `UnverifiedConnection::authorize`
5. The device ID is computed the same way everywhere (SPKI DER, not the whole certificate,
   which is regenerated on every start). → `DeviceId::of_certificate`
6. A peer that is no longer paired (forgotten on the phone) can't keep or reopen a session.
   → `session.rs`, `ffi/src/lib.rs` (`forget`)

### Pairing
7. An unknown device can do nothing but send one `PairRequest`, and only while a window is
   open. → `desktop/src/daemon.rs` `handle_incoming`, `PAIRING_REQUEST_TIMEOUT`
8. The secret: 16 bytes from the system RNG (`ring::rand::SystemRandom`), compared in
   constant time, valid once, for 5 minutes, at most 3 wrong tries. → `PairingWindow::check`
9. The secret is zeroized after use and never logged or `Debug`-printed. → `PairingSecret`
10. The phone pins the device ID from the QR code before sending the secret, so a machine that
    doesn't own that key never sees the secret. → `pair_with`
11. Invite parsing rejects what `PROTOCOL.md` §5.1 says to reject (length, version, duplicates,
    missing keys) and can't panic. → `PairingInvite::from_str`, fuzz target `pairing_invite`
12. Device names from peers are sanitized before display or storage.
    → `sanitize_device_name`

### Untrusted input from a paired device
13. No allocation is sized by a peer beyond the limits: frame ≤ 1 MiB, chunk ≤ 256 KiB, clip ≤
    64 MiB. Offsets and lengths can't overflow or overlap. → `framing.rs`, `clip.rs`, fuzz
    targets `frame_decoder`, `clip_receiver`
14. A reassembled clip is checked against the offered SHA-256 before use.
15. Images are checked (dimensions ≤ 16,384 px, decoded size ≤ 512 MiB) before decoding.
    → `desktop/src/clipboard.rs`
16. A clip whose origin is this device is dropped (echo through another device). → `echo.rs`

### Local attack surface
17. Another user on the same machine can't talk to the daemon: socket in a 0700 profile
    directory (Unix); named pipe with the user name, `first_pipe_instance`, remote clients
    rejected (Windows). → `control.rs`
18. The identity key file is readable by the user only. Unix: created with mode 0600 (via a
    temporary file, then renamed). Windows: no ACL of its own; it relies on `%APPDATA%` being
    private to the user. → `config.rs` `write_private_file`
19. Profile names can't escape the configuration directory. → `Profile::open_in`
20. Clipboard contents never reach the logs. Grep for `tracing::` near `data`, `text`,
    `preview`.

### Phone
21. Clips written to the Android clipboard carry `EXTRA_IS_SENSITIVE` when sensitive and
    `EXTRA_IS_REMOTE_DEVICE`. Images and long texts go through a `FileProvider` URI that other
    apps can read only with a granted permission.
22. Only this app can call the Shizuku service, which runs as `shell`. Shizuku hands the
    service's binder to the app that bound it; the service doesn't check the caller's UID itself.
    Is that enough? → `ShellClipboardService`, `IShellClipboard.aidl`

### Built-in mode (wireless debugging, no Shizuku)
23. The ADB client only connects to loopback or this phone's own addresses. → `check_local`
24. Pairing: the password is the code plus 64 bytes exported from the TLS session (label
    `adb-label\0`), SPAKE2 is BoringSSL's with AOSP's names, the AES-128-GCM key comes from HKDF
    as in `aes_128_gcm.cpp`, nonces never repeat. Compare with AOSP `pairing_connection.cpp` and
    `pairing_auth.cpp`.
25. adbd's server certificate is not verified, by necessity (adbd makes a random key at every
    start; Google's client accepts any). Is the reasoning in the module's SECURITY note sound?
26. The local socket: the app only listens to UID 2000 or 0 running this very build; the helper
    only talks to a socket owned by the app's UID. Can another app squat the abstract name in
    a way that matters? → `ShellLinkServer`, `ShellHelper`
27. The app's RSA key (`files/adb.key`) gives full adb access to the phone while wireless
    debugging is on. It is private to the app, never backed up (`allowBackup=false`), and the app
    switches wireless debugging off again after starting the helper.

## Known gaps (not bugs to report, but worth an opinion)

- The PC can't revoke a paired phone (only edit `devices.toml`). Planned before 0.1 is
  announced widely.
- The identity key is a file, not in the OS keystore (v0.2).
- No forward-secrecy concerns beyond TLS 1.3's own; no replay protection beyond TLS.
- mDNS announces the device name to the local network.

## How to run things

```sh
cargo test --workspace                       # includes two-daemon and phone-API end-to-end tests
cd fuzz && cargo +nightly fuzz run <target>  # frame_decoder, pairing_invite, clip_receiver, hotkey
cargo deny check                             # dependency advisories and licenses
```

## Review log

| Date | Reviewer | Scope | Findings / outcome |
|---|---|---|---|
| | | | |
