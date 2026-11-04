# Contributing to paste-get

Thanks for your interest! paste-get is in early development, so the best way to help right now is
to try it, report bugs, and test on devices we don't have.

## Before you start

- Read [`docs/AUDIT.md`](docs/AUDIT.md) for the architecture and [`docs/ROADMAP.md`](docs/ROADMAP.md)
  for what's being built. Pull requests outside the current milestone may wait.
- For anything bigger than a small fix, open an issue first so we can agree on the approach.

## Development setup

You need [rustup](https://rustup.rs/). The toolchain version is pinned in `rust-toolchain.toml`
and installs automatically on the first `cargo` command.

```sh
cargo build --workspace
cargo run -p pasteget-desktop     # runs the `pasteget` CLI
```

Linux only: clipboard support will need the Wayland/X11 development headers once it lands;
this section will list them.

## Before opening a pull request

All of these must pass (CI runs them on Linux and Windows):

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo deny check          # cargo install cargo-deny
```

### Android app

Needs a JDK 17+, the Android SDK (platform `android-37.2`, build-tools, NDK
`30.0.16248370`), the Rust targets and `cargo-ndk`:

```sh
rustup target add aarch64-linux-android x86_64-linux-android
cargo install cargo-ndk
cd android
./gradlew assembleFossDebug testFossDebugUnitTest lintFossDebug
```

Gradle builds the Rust library and generates the Kotlin bindings itself. Android Studio works
too: open the `android/` folder.

Unit tests that need Android classes run on the JVM with [Robolectric](https://robolectric.org/),
some on several Android versions. Their first run downloads those versions of Android
(about 200 MB each) from Maven Central.

### Fuzzing

Parsers of untrusted input have fuzz targets in `fuzz/`. They need a nightly toolchain:

```sh
cargo +nightly install cargo-fuzz
cd fuzz
cargo +nightly fuzz run frame_decoder -- -max_total_time=60
```

CI runs every target for 60 seconds. A new parser needs a new target.

## Rules

- **One change per pull request**, with tests.
- **Never log clipboard contents**: log sizes, hashes and MIME types only.
- **No hand-written crypto.** Use the audited crates already in the project.
- **No telemetry** and no network traffic except to paired devices.
- **Protocol changes** (`proto/`) must update `docs/PROTOCOL.md` in the same pull request.
- New source files start with `// SPDX-License-Identifier: GPL-3.0-or-later`.
- Code, comments and commit messages are in English. Commit subjects use the imperative mood
  ("Add pairing QR code", not "Added…").

## Reporting security issues

Please don't open a public issue for vulnerabilities. Use GitHub's
[private vulnerability reporting](https://github.com/Nexoryx-business/paste-get/security/advisories/new)
instead.

## License

By contributing, you agree that your contributions are licensed under
[GPL-3.0-or-later](LICENSE).
