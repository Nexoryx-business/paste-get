# Testing paste-get on real machines

Automated tests use in-memory clipboards. Only a real desktop can show whether the **system
clipboard** works. This guide checks it with what exists today (M2): the desktop daemon,
before the Android app.

The trick: on one computer, run the real daemon (system clipboard) **and** a second "fake phone"
daemon with a private in-memory clipboard. Whatever reaches the fake phone came out of your real
clipboard, and whatever the fake phone sends must show up when you press `Ctrl+V`.

Logs never contain clipboard contents (only types and sizes), so they're safe to share.

## 1. Build

### Windows 11
1. Install [rustup](https://rustup.rs/). When asked, let it install the Visual Studio C++
   Build Tools.
2. In PowerShell:
   ```powershell
   git clone https://github.com/Nexoryx-business/paste-get
   cd paste-get
   git checkout ccr-8ee1704b-80slpz   # until this branch is merged
   cargo build --release
   ```
3. The program is `target\release\pasteget.exe`. Below, `pasteget` means that file.

### Arch Linux (KDE Plasma or Hyprland)
```sh
sudo pacman -S --needed rustup git base-devel
rustup default stable
git clone https://github.com/Nexoryx-business/paste-get
cd paste-get
git checkout ccr-8ee1704b-80slpz   # until this branch is merged
cargo build --release
alias pasteget="$PWD/target/release/pasteget"
```
No other library is needed: Wayland and X11 are spoken directly.

## 2. Start both daemons

Use three terminals.

```sh
# Terminal 1: the real daemon, on your real clipboard
pasteget daemon

# Terminal 2: the fake phone, with its own clipboard and port
pasteget --profile phone daemon --memory-clipboard --port 47291
```

Windows asks whether to allow `pasteget.exe` on the network: allow **private networks**.

```sh
# Terminal 3: pair them
pasteget pair
```
`pair` prints a QR code and a `pasteget join "…"` line. Copy that line, add `--profile phone`,
and run it in a fourth terminal (or stop `pair` with Ctrl+C after it succeeds):

```sh
pasteget --profile phone join "pasteget://pair?…"
```
Both sides print `✓ Paired with …`. `pasteget status` should show the phone as `● connected`.

## 3. Checklist

Watch terminal 2 (the fake phone): each clip it receives logs `clip received … mime_type=… len=…`.

| # | Do this | Expected |
|---|---|---|
| 1 | Copy some text in any app | Terminal 2: `clip received … text/plain … len=<length>` |
| 2 | `pasteget --profile phone send "Bonjour 👋"`, then `Ctrl+V` in a text editor | `Bonjour 👋` is pasted |
| 3 | Take a screenshot to the clipboard (see below) | Terminal 2: `clip received … image/png` |
| 4 | `pasteget --profile phone send --file some-image.png`, then `Ctrl+V` in an app that takes images (Discord, GIMP, Paint, a chat) | The image is pasted |
| 5 | After step 2, wait 5 s | Terminal 1 does **not** log `local copy` for it (no echo) |
| 6 | Leave an image on the clipboard for a minute | `pasteget` stays under a few % CPU (Task Manager / `htop`) |
| 7 | Stop terminal 2 (Ctrl+C), start it again | Terminal 1 logs `connected` again within ~30 s |

Screenshot to clipboard:
- **Windows:** `Win+Shift+S`
- **KDE Plasma:** Spectacle → *Copy to clipboard*, or `Meta+Shift+Print`
- **Hyprland:** `grim -g "$(slurp)" - | wl-copy` (packages `grim`, `slurp`, `wl-clipboard`)

## 4. Report

For each environment (Windows 11, KDE Plasma Wayland, Hyprland), report which rows passed. For
failures, attach the output of terminals 1 and 2 and of `pasteget status`.

| Environment | 1 | 2 | 3 | 4 | 5 | 6 | 7 |
|---|---|---|---|---|---|---|---|
| Windows 11 | | | | | | | |
| KDE Plasma 6 (Wayland) | | | | | | | |
| Hyprland | | | | | | | |

## Known limitations (M2)

- Changes are detected by polling every 0.5 s. On Linux this re-reads images while one sits on
  the clipboard (row 6 measures the cost). Native change events are planned.
- IPv4 only.
- The identity key is a file readable only by your user, not yet in the OS keystore.
- Text and PNG only. Copying a file in a file manager isn't synced.
