# paste-get

> Screenshot on your phone, `Ctrl+V` on your PC.

**paste-get** is an open-source bridge between your Android phone and your computer.
Copy something or take a screenshot on the phone, and it's already on your PC's clipboard,
with no button to press. Local-first, end-to-end encrypted, no account, no cloud.

> **Status: pre-alpha.** The protocol core, the desktop daemon and a first Android app work:
> take a screenshot on the phone and it's on the PC clipboard, with a notification; share text or
> an image and it lands there too. Text and images copied on the PC land on the phone clipboard,
> and text copied on the phone goes to the PC by itself, through [Shizuku](https://shizuku.rikka.app/)
> (with a fallback for when Shizuku isn't running). All of it still needs testing on real phones:
> see [`docs/TESTING.md`](docs/TESTING.md). The name is a working title.

## Planned highlights

- **Screenshot → PC clipboard** in under a second
- **Text sync in both directions**, automatic on Android 10+ through [Shizuku](https://shizuku.rikka.app/)
- **Images in both directions**, any size
- **Skips sensitive copies** (passwords, card numbers)
- **2FA codes** from phone notifications, pasteable on the PC
- **OCR**: paste a screenshot as an image *or* as its text
- **Windows, Linux (X11, KDE Plasma, Hyprland/wlroots) and macOS**

## Try the desktop side

```sh
cargo build --release
./target/release/pasteget daemon        # keep it running
./target/release/pasteget pair          # shows a pairing QR code
./target/release/pasteget status
./target/release/pasteget-tray          # or: a tray icon that runs the daemon for you
```

On Linux, the tray needs WebKitGTK and libayatana-appindicator (see
[`CONTRIBUTING.md`](CONTRIBUTING.md)). `pasteget --help` lists every command. [`docs/TESTING.md`](docs/TESTING.md) shows how to test
with a second, in-memory daemon standing in for the phone.

## Docs

| Document | What's in it |
|---|---|
| [`docs/AUDIT.md`](docs/AUDIT.md) | Technical audit: how every feature can be built, what's hard, what's impossible |
| [`docs/ROADMAP.md`](docs/ROADMAP.md) | Milestones and step-by-step plan |
| [`docs/PROTOCOL.md`](docs/PROTOCOL.md) | Wire protocol: transport, pairing, messages |
| [`docs/TESTING.md`](docs/TESTING.md) | Testing on real machines |
| [`docs/RELEASING.md`](docs/RELEASING.md) | How packages are built and released |
| [`docs/REVIEW.md`](docs/REVIEW.md) | Guide for the security review of the crypto and protocol code |
| [`SECURITY.md`](SECURITY.md) | Threat model and how to report a vulnerability |
| [`docs/devices/xiaomi-hyperos.md`](docs/devices/xiaomi-hyperos.md) | Phone setup for Xiaomi / POCO / Redmi (HyperOS) |
| [`CONTRIBUTING.md`](CONTRIBUTING.md) | How to build, test and contribute |
| [`CLAUDE.md`](CLAUDE.md) | Rules for AI-assisted development in this repo |

## Layout

```
paste-get/
├─ core/            Rust: protocol, crypto, pairing, sessions (shared by every app)
├─ proto/           Protobuf message definitions
├─ desktop/         Rust daemon + `pasteget` CLI (Windows, Linux, macOS)
│  └─ tray/         Tauri 2 tray app
├─ ffi/             The phone API, exported to Kotlin with UniFFI
├─ uniffi-bindgen/  Generates the Kotlin bindings
├─ android/         Kotlin + Jetpack Compose app
├─ packaging/       Package builds (Windows installer, AppImage)
├─ fuzz/            Fuzz targets for every parser of untrusted input
└─ docs/
```

## License

paste-get is free software, licensed under the
[GNU General Public License v3.0 or later](LICENSE) (`GPL-3.0-or-later`).
