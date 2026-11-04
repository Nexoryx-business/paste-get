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
- [ ] Measure latency (target < 1 s on LAN)
- **Done when:** screenshot on the POCO → `Ctrl+V` in Discord works on Windows and Arch.

### M5 — Automatic clipboard on Android
- [ ] Mode A: Shizuku user service listening as shell, with reflection fallbacks
- [ ] One-time grants through Shizuku (`READ_LOGS`, overlay, …)
- [ ] Mode B: `READ_LOGS` + overlay fallback
- [ ] Health screen (current mode, Shizuku state, last sync)
- **Done when:** copy on the phone → paste on the PC, no tap, even after the POCO reboots (mode B).

### M6 — PC → phone
- [ ] Text into the Android clipboard
- [ ] Images through `FileProvider`
- **Done when:** a PC screenshot pastes into a phone app.

### M7 — Tray app
- [ ] Tauri 2 tray: status, paired devices, pause, last items
- [ ] Autostart on Windows and Linux
- [ ] Hotkeys: `RegisterHotKey` (Windows), GlobalShortcuts portal (KDE), `bind … exec pasteget …` (Hyprland)

### M8 — Release 0.1
- [ ] CI builds: Windows installer, AppImage, AUR `PKGBUILD`, signed APK
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
