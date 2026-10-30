# paste-get

> Screenshot on your phone, `Ctrl+V` on your PC.

**paste-get** is an open-source bridge between your Android phone and your computer.
Copy something or take a screenshot on the phone, and it's already on your PC's clipboard,
with no button to press. Local-first, end-to-end encrypted, no account, no cloud.

> **Status: design phase.** No code yet. The technical audit and the roadmap are below.
> The name is a working title.

## Planned highlights

- **Screenshot → PC clipboard** in under a second
- **Text sync in both directions**, automatic on Android 10+ through [Shizuku](https://shizuku.rikka.app/)
- **Images in both directions**, any size
- **Skips sensitive copies** (passwords, card numbers)
- **2FA codes** from phone notifications, pasteable on the PC
- **OCR**: paste a screenshot as an image *or* as its text
- **Windows, Linux (X11, KDE Plasma, Hyprland/wlroots) and macOS**

## Docs

| Document | What's in it |
|---|---|
| [`docs/AUDIT.md`](docs/AUDIT.md) | Technical audit: how every feature can be built, what's hard, what's impossible |
| [`docs/ROADMAP.md`](docs/ROADMAP.md) | Milestones and step-by-step plan |
| [`docs/devices/xiaomi-hyperos.md`](docs/devices/xiaomi-hyperos.md) | Phone setup for Xiaomi / POCO / Redmi (HyperOS) |
| [`CLAUDE.md`](CLAUDE.md) | Rules for AI-assisted development in this repo |

## Planned layout

```
paste-get/
├─ core/       Rust: protocol, crypto, pairing, history (shared by every app)
├─ proto/      Protobuf message definitions
├─ desktop/    Rust daemon + CLI, then a Tauri 2 tray app (Windows, Linux, macOS)
├─ android/    Kotlin + Jetpack Compose app, Shizuku service, core via UniFFI
└─ docs/
```

## License

paste-get is free software, licensed under the
[GNU General Public License v3.0 or later](LICENSE) (`GPL-3.0-or-later`).
