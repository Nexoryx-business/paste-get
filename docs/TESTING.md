# Testing paste-get on real machines

Automated tests use in-memory clipboards. Only real machines can show whether the **system
clipboard** works. Sections 1–4 test the desktop daemon on its own; section 5 adds the phone,
section 6 automatic screenshots, section 7 the way back, from the PC to the phone.

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

## 5. The phone (M3)

What the Android app does so far: pair by QR code, then send text or images **by hand** (share
sheet, Quick Settings tile, or the button in the app). Text sent from the PC lands on the phone
clipboard. Automatic sync of every copy comes with Shizuku (M5).

### Install the app

The CI builds a debug APK on every push. On GitHub: **Actions** → latest **CI** run →
**Artifacts** → `paste-get-android-debug`. Unzip it, copy the `.apk` to the phone and open it
(allow installing from that source when asked).

With `adb` and USB debugging instead: `adb install -r app-foss-debug.apk`.

Then follow [`devices/xiaomi-hyperos.md`](devices/xiaomi-hyperos.md), section 3 (Autostart, no
battery restrictions), or HyperOS will stop the connection in the background.

### Checklist

The phone and the PC must be on the **same Wi-Fi**. On the PC, run `pasteget daemon`.

| # | Do this | Expected |
|---|---|---|
| P1 | Open paste-get, allow notifications | A persistent "paste-get" notification appears |
| P2 | PC: `pasteget pair`. Phone: **Pair a computer**, scan the code | Phone: "Paired with …", then "Connected to 1 computer". PC: `✓ Paired with …` |
| P3 | In any app, select text → **Share** → **Send to PC**, then `Ctrl+V` on the PC | The text is pasted |
| P4 | Gallery → a screenshot → **Share** → **Send to PC**, then paste on the PC (Paint, GIMP, Discord) | The image is pasted |
| P5 | Add the **Send clipboard** tile to Quick Settings. Copy text, pull down the shade, tap the tile | "Sent to 1 computer"; `Ctrl+V` on the PC pastes it |
| P6 | PC: `pasteget send "depuis le PC"`, then long-press a text field on the phone → Paste | `depuis le PC` is pasted |
| P7 | Lock the phone for 5 minutes, unlock, repeat P3 | Still works, without opening the app |
| P8 | Restart the phone, wait a minute, repeat P3 | Still works (the app restarts on boot) |
| P9 | Scan the PC's pairing QR code with the **normal camera app** (only if it offers to open the link; not every camera does) | paste-get opens and pairs |

Phone logs never contain clipboard contents either. To get them: `adb logcat -s paste-get`.

## 6. Screenshots (M4)

Goal: take a screenshot on the phone, press `Ctrl+V` on the PC, and it's there, in under a
second. Update both sides first: `git pull` and `cargo build --release` on the PC, and the APK of
the latest CI run on the phone (it installs over the previous one; pairings are kept).

On the phone, turn on **Send new screenshots** and choose **Allow all** for photos. "Select
photos" hides new screenshots: the card then says so and offers **Open settings**.

On Hyprland, notifications need a notification daemon (mako, dunst, swaync…). KDE Plasma and
Windows have one built in.

| # | Do this | Expected |
|---|---|---|
| S1 | Take a screenshot on the phone, then `Ctrl+V` in Discord (or Paint, GIMP) on the PC | The screenshot is pasted, upright. The PC shows "Image from POCO M8 5G ✓" |
| S2 | Look at the app's home screen | "Last screenshot (… kB) reached your computer in … ms" |
| S3 | Share a photo from the gallery with **Send to PC**, paste on the PC | The photo is pasted the right way up, even if taken in portrait |
| S4 | Turn Wi-Fi off on the phone for a minute, then on again | "Connected to 1 computer" again within a few seconds |
| S5 | Restart the PC into the other OS (Windows ↔ Arch), start `pasteget daemon` there (pair it once if it isn't yet) | The phone connects to it within a few seconds, without a new QR code |
| S6 | Give the PC a new IP address (switch it from Wi-Fi to Ethernet, or the other way), keep the daemon running | The phone reconnects by itself, within about 90 s at worst |
| S7 | Lock the phone, wait 5 minutes, unlock it, take a screenshot | S1 still works |

### Measuring latency (target: under 1 s)

Two numbers, both on the phone:
- The home screen (S2) counts from the moment Android announced the new screenshot file to the
  PC's acknowledgement. Repeat S1 five times and note the values.
- `adb logcat -s paste-get` shows each step: "screenshot of … bytes read in … ms", then
  "delivered … ms after sending, … ms after Android announced the screenshot".

On the PC, the daemon logs "clip received … ms" (the transfer) and "clip applied to the clipboard
… ms" (decoding the image and setting the clipboard). Add that last number to the phone's to get
the whole path. The time HyperOS takes to save the screenshot comes before all of these.

Report the five values per OS, and whether S1 felt instant.

### Settings

Notifications are set in `settings.toml`, in `%APPDATA%\paste-get\default\` on Windows and
`~/.config/paste-get/default/` on Linux: `notify = "images"` (the default), `"all"` (text too) or
`"off"`. Restart the daemon after a change.

## 7. From the PC to the phone (M6)

Whatever you copy on the PC lands on the phone's clipboard, ready to paste, with nothing to tap
on the phone. Update both sides first, as in section 6.

| # | Do this | Expected |
|---|---|---|
| C1 | PC: copy some text. Phone: long-press a text field → **Paste** | The text is pasted |
| C2 | PC: open a text file over 1 MB (a log, for example), select all, copy. Phone: paste it in a notes app | The whole text is pasted, and the paste-get notification is still there |
| C3 | Lock the phone for 5 minutes, unlock it, repeat C1 | Still works, without opening the app |
| C4 | PC: take a screenshot to the clipboard (as in section 3). Phone: in Telegram, WhatsApp or Google Messages, long-press the message field → **Paste** | The screenshot is attached, ready to send |
| C5 | PC: in a browser, right-click an image → **Copy image**. Phone: paste it as in C4 | The image is attached |
| C6 | If you use Gboard: after C4, open its clipboard (the clipboard icon above the keys) | The screenshot is there; tapping it inserts it in apps that take images (Discord, for example) |

`adb logcat -s paste-get` shows "clip received: …, … bytes" for each clip. Images, and texts over
256 KB (C2), go on the clipboard as a link to a file, since Android can't pass that much in one
go: the log then also says "clip saved as clip-….png for the clipboard".

Report which rows passed with the PC on Windows, KDE Plasma and Hyprland, and in which phone apps
C4 worked.

## Known limitations

- If the PC changes address while the phone is connected, the phone notices only when the old
  connection times out (up to 90 s), then finds it again by mDNS. Phones that can't receive
  mDNS (some guest or office networks block it) still need a new pairing.
- Screenshots taken while the phone has no connection to a computer are not sent later.
- The phone shows nothing of its own when a clip from the PC arrives (Android 13 and later may
  show their clipboard preview).
- Changes on the PC are detected by polling every 0.5 s. On Linux this re-reads images while one sits on
  the clipboard (row 6 measures the cost). Native change events are planned.
- IPv4 only.
- The identity key is a file readable only by your user, not yet in the OS keystore.
- Text and images only (PNG and JPEG on the wire). Copying a file in a file manager isn't synced.
