# Roadmap

Each step is sized for one working session: it builds, has tests, and ends in a commit.
Work top to bottom. The audit (`AUDIT.md`) explains the why.

## v0.1 — "it just works"

Goal: take a screenshot on the POCO, press `Ctrl+V` on Windows or Arch, and it's there.

### M0 — Foundations
- [x] Choose a license: GPL-3.0-or-later
- [x] Cargo workspace (`core`, `desktop`), `rustfmt`, `clippy`, `cargo-deny` config
- [x] GitHub Actions: fmt + clippy + tests on Linux and Windows, `cargo-deny`, Dependabot
- [x] `CONTRIBUTING.md`, issue templates

### M1 — Protocol and core (`core/`, `proto/`)
- [x] `proto/` messages: `Hello`, `ClipOffer`, `ClipChunk`, `Ack`, `Ping`, `Pong`
- [x] Length-prefixed framing + decoder fuzz target
- [x] Device identity (keypair) + mutual TLS with pinned keys
- [x] Pairing payload (what the QR encodes) + one-time secret
- [x] Echo-loop guard (origin + hash)
- [x] `docs/PROTOCOL.md` v0
- [x] Clip transfer (offer, chunks, ack) and `Hello` exchange
- **Done when:** two test peers in `cargo test` pair and exchange text and a PNG.
  ✅ `core/tests/end_to_end.rs`

### M2 — Desktop daemon, headless (`desktop/`)
- [x] `pasteget daemon`: watch and set the clipboard (text + image) through `arboard`
- [ ] Verified on Windows 11, KDE Plasma 6 Wayland, Hyprland
- [x] mDNS advertising
- [x] `pasteget pair` prints a QR code in the terminal (and `pasteget join` for PC-to-PC)
- [x] `pasteget send <text|file>`, `pasteget status`
- **Done when:** two daemons (different profiles on one machine) sync clipboards.

### M3 — Android skeleton (`android/`)
- [x] Session engine moved into `core` so phone and desktop share it
- [x] `ffi/`: phone API through UniFFI, tested against a real desktop daemon
- [x] Compose app, `foss` flavor, Rust core through UniFFI + `cargo-ndk`
- [x] `connectedDevice` foreground service with status notification
- [x] Pairing by scanning the QR code (in-app scanner or system camera)
- [x] Manual sending: share sheet + Quick Settings tile
- [ ] Verified on the POCO M8 5G
- **Done when:** sharing text from the POCO puts it on the PC clipboard.
  ✅ in `ffi/tests/phone_and_pc.rs` against a real daemon; pending on the real phone.

### M4 — Screenshots
- [x] Phone finds its computers again by mDNS when their address changes
- [x] MediaStore observer, screenshot folder detection, `IS_PENDING` handling
- [x] Send the image (JPEG as it is, other formats as PNG); the PC sets it as an image and shows a "Received ✓" notification
- [x] Measure latency (target < 1 s on LAN): shown in the app and logged on both sides; a 2 MB
  image takes 13 ms on loopback (`ffi/tests/phone_and_pc.rs`)
- [ ] Verified on the POCO M8 5G with Windows and Arch, latency measured on Wi-Fi
  (`TESTING.md`, section 6)
- **Done when:** screenshot on the POCO → `Ctrl+V` in Discord works on Windows and Arch.
  ✅ up to the clipboard in automated tests; pending on the real devices.

### M5 — Automatic clipboard on Android
- [x] Mode A: Shizuku user service listening as shell, with reflection fallbacks
- [x] One-time grants through Shizuku (`READ_LOGS`, overlay)
- [x] Mode B: `READ_LOGS` + overlay fallback
- [x] Health screen (current mode, Shizuku state, last sync): a card on the home screen
- [ ] Verified on the POCO M8 5G (`TESTING.md`, section 8), including which `IClipboard`
  signatures HyperOS has and whether Android shows "pasted from your clipboard" notices
- **Done when:** copy on the phone → paste on the PC, no tap, even after the POCO reboots (mode B).
  ✅ up to the phone API in `ffi/tests/phone_and_pc.rs`, and the app's logic in Robolectric tests;
  pending on the real phone. On Android 13+, mode B needs one tap after a reboot: Android lets an
  app read the system log only after asking the user, while the app is in front. Android 10–12
  start it at boot.

### M6 — PC → phone
- [x] Text into the Android clipboard. Long texts go as a link to a file, like images: clips pass
  through Binder, whose buffer is 1 MB
- [x] Images through `FileProvider`
- [ ] Verified on the POCO M8 5G with Windows and Arch (`TESTING.md`, section 7)
- **Done when:** a PC screenshot pastes into a phone app.
  ✅ up to the phone API in `ffi/tests/phone_and_pc.rs`; pending on the real devices.

### M7 — Tray app
- [x] Tauri 2 tray: status, paired devices, pause, last items (`pasteget-tray`, runs the daemon
  in-process unless one already runs; `pasteget pause`, `resume` and `recent` on the command line)
- [x] Autostart on Windows and Linux: the tray's *Start at login* (`Run` key, XDG autostart;
  `exec-once` on Hyprland, which ignores XDG autostart)
- [x] Hotkeys: `RegisterHotKey` (Windows), GlobalShortcuts portal (KDE), `bind … exec pasteget …` (Hyprland).
  Pause/resume and "send the clipboard now" (`pasteget send --clipboard`), set in `settings.toml`
- [ ] Verified on Windows 11, KDE Plasma 6 Wayland and Hyprland (`TESTING.md`, sections 9–11)

### M8 — Release 0.1
- [ ] CI builds: Windows installer, AppImage, AUR `PKGBUILD`, signed APK
  - [x] Windows installer (NSIS, per user) and AppImage, with `pasteget` inside: `release.yml`,
    a draft release for each `v*` tag (`RELEASING.md`)
  - [x] AUR `paste-get-git` PKGBUILD, built with `makepkg` in an Arch container by CI (publishing
    it on the AUR needs a maintainer account)
- [ ] README demo GIF, `SECURITY.md` draft
- [ ] Human review of crypto and protocol code

## v0.2 — "smart"
History + search, 2FA codes, PC-side OCR, sensitive filters, auto-crop, smart actions.
Desktop identity key in the OS keystore when one is available (M2 stores it in a user-only file,
like SSH keys, because Hyprland setups often run no Secret Service).

## v0.3 — "deep"
Insert from phone (camera), links both ways, files, multiple PCs, GNOME extension.

## v1.0 — "platform"
Local API + hooks, browser extension, relay / USB fallback, F-Droid, signed releases, website and final name.
