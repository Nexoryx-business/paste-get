# paste-get

> Screenshot on your phone, `Ctrl+V` on your PC.

**paste-get** is an open-source bridge between your Android phone and your computer.
Copy something or take a screenshot on the phone, and it's already on your PC's clipboard,
with no button to press. Local-first, end-to-end encrypted, no account, no cloud.

> **Status: pre-alpha.** The protocol core, the desktop daemon and a first Android app work:
> share text or an image from the phone and it lands on the PC clipboard. Automatic sync of
> every copy (through Shizuku) is next. See [`docs/TESTING.md`](docs/TESTING.md) to try it.
> The name is a working title.

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
```

`pasteget --help` lists every command. [`docs/TESTING.md`](docs/TESTING.md) shows how to test
with a second, in-memory daemon standing in for the phone.

## Docs

| Document | What's in it |
|---|---|
| [`docs/AUDIT.md`](docs/AUDIT.md) | Technical audit: how every feature can be built, what's hard, what's impossible |
| [`docs/ROADMAP.md`](docs/ROADMAP.md) | Milestones and step-by-step plan |
| [`docs/PROTOCOL.md`](docs/PROTOCOL.md) | Wire protocol: transport, pairing, messages |
| [`docs/TESTING.md`](docs/TESTING.md) | Testing on real machines |
| [`docs/devices/xiaomi-hyperos.md`](docs/devices/xiaomi-hyperos.md) | Phone setup for Xiaomi / POCO / Redmi (HyperOS) |
| [`CONTRIBUTING.md`](CONTRIBUTING.md) | How to build, test and contribute |
| [`CLAUDE.md`](CLAUDE.md) | Rules for AI-assisted development in this repo |

## Layout

```
paste-get/
├─ core/            Rust: protocol, crypto, pairing, sessions (shared by every app)
├─ proto/           Protobuf message definitions
├─ desktop/         Rust daemon + `pasteget` CLI (Windows, Linux, macOS); tray app later
├─ ffi/             The phone API, exported to Kotlin with UniFFI
├─ uniffi-bindgen/  Generates the Kotlin bindings
├─ android/         Kotlin + Jetpack Compose app
├─ fuzz/            Fuzz targets for every parser of untrusted input
└─ docs/
```

## License

paste-get is free software, licensed under the
[GNU General Public License v3.0 or later](LICENSE) (`GPL-3.0-or-later`).
