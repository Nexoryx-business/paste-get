# Working on paste-get

This project is built with heavy AI assistance and will be reviewed by human developers
before any public release. These rules keep the code reviewable.

## Read first

- `docs/AUDIT.md`: architecture and the reasoning behind every technical choice
- `docs/ROADMAP.md`: what we're building now. Work on the current milestone only.

## Stack

- `core/`: Rust library. Protocol, crypto, pairing, history. No platform code here.
- `desktop/`: Rust daemon + CLI first, Tauri 2 tray UI later.
- `android/`: Kotlin + Jetpack Compose. Uses `core` through UniFFI (built with `cargo-ndk`).
- `proto/`: Protobuf definitions. **`docs/PROTOCOL.md` must be updated in the same commit
  as any change here.**

## Target devices (tier 1, must work)

- Windows 11 (HP All-in-One)
- Arch Linux on the same PC (dual boot), on **KDE Plasma 6 (Wayland)** and **Hyprland**
- POCO M8 5G, HyperOS 2 / Android 15 (see `docs/devices/xiaomi-hyperos.md`)

Tier 2 (best effort): macOS, GNOME, other Android vendors.

## Rules

1. **Small steps.** One roadmap item per change. Each change builds, passes tests, and is committed.
2. **Tests come with the code.** Protocol and crypto code needs unit tests; parsers need a fuzz target.
3. **No crypto of our own.** Use rustls / audited crates only. Never disable certificate checks,
   even in tests (use test keys instead).
4. **Never log clipboard contents.** Log sizes, hashes and MIME types only.
5. **No telemetry, no network calls** except to paired devices (and a user-configured relay later).
6. **No proprietary Android dependencies** (Play Services, ML Kit, Firebase) in the `foss` flavor.
7. **Hidden Android APIs** (via Shizuku) go behind an interface with a fallback, because their signatures
   change between Android versions and vendors.
8. Code, comments, commits and docs are in **English**. Chat can be in any language.
9. Before finishing a task: `cargo fmt --check`, `cargo clippy -- -D warnings`, `cargo test`
   (and the Gradle equivalents once `android/` exists).
