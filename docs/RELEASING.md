# Releasing paste-get

How the packages are built, and the steps of a release. CI does the building
(`.github/workflows/release.yml`); a person decides when to release.

## What gets built

| Package | Built by | Where it ends up |
|---|---|---|
| Windows installer, `paste-get_<version>_x64-setup.exe` | `packaging/build-desktop.sh nsis` on `windows-latest` | Installs for the current user (no admin prompt) in `%LOCALAPPDATA%\paste-get`, with a Start menu entry and an uninstaller. `pasteget.exe` is installed next to `pasteget-tray.exe` |
| Linux AppImage, `paste-get_<version>_amd64.AppImage` | `packaging/build-desktop.sh appimage` on Ubuntu 22.04 | One file; runs on distributions at least as new as Ubuntu 22.04. Contains `pasteget` too, but only the tray is started from outside |
| Arch package `paste-get-git` | `packaging/aur/paste-get-git/PKGBUILD`, tested with `makepkg` in an Arch container | `/usr/bin/pasteget` and `pasteget-tray`, a desktop entry and an icon. Built from source on the user's machine |

Every push that changes the packaging builds all three, without releasing: they're in the run's
artifacts. A `v*` tag builds them and puts them, with a `SHA256SUMS` file, in a **draft**
GitHub release.

The Windows installer doesn't install WebView2: the tray has no window, so it never needs it.

### Building them yourself

```sh
cargo install tauri-cli --version =2.12.4 --locked
packaging/build-desktop.sh appimage    # Linux; nsis on Windows, from Git Bash
```

The script builds `pasteget`, copies it to `desktop/tray/binaries/` under the target triple (as
Tauri expects for a "sidecar" program), then bundles the tray with
`desktop/tray/tauri.bundle.conf.json` on top of `tauri.conf.json`. That file is kept apart
because declaring the sidecar makes `cargo build` look for it, and everyday builds don't have it.

## The AUR package

`paste-get-git` builds the latest commit of the default branch, so it needs no update per
release. The CI job builds it from the commit being tested and checks that `.SRCINFO` matches the
PKGBUILD. After changing the PKGBUILD, regenerate it on Arch:
`makepkg --printsrcinfo > .SRCINFO`.

Publishing it on the AUR is done once, by hand, by whoever maintains it there (it needs an AUR
account with an SSH key):
```sh
git clone ssh://aur@aur.archlinux.org/paste-get-git.git
cp packaging/aur/paste-get-git/{PKGBUILD,.SRCINFO} paste-get-git/
cd paste-get-git && git add PKGBUILD .SRCINFO && git commit -m "Initial upload" && git push
```
Until then, Arch users can build it from the repository:
`cd packaging/aur/paste-get-git && makepkg -si`.

A `paste-get` package built from release tarballs can follow once releases are regular.

## Not signed yet

- **Windows:** the installer and programs are unsigned. SmartScreen says "Windows protected your
  PC" (*More info* → *Run anyway*), and Smart App Control refuses to run them at all. The plan is
  [SignPath](https://signpath.org/)'s free signing for open-source projects, which needs an
  application from the maintainer once the repository is public.
- **Linux:** the AppImage isn't signed; check it against `SHA256SUMS`.

## Making a release

1. Everything for the version is merged on `main`, and CI is green.
2. Bump the version in `Cargo.toml` (`[workspace.package] version`) and in
   `android/app/build.gradle.kts` (`versionName`, and `versionCode` + 1). Commit.
3. Tag and push: `git tag v0.1.0 && git push origin v0.1.0`.
4. Wait for the *Release* workflow, then open the draft release on GitHub. Install each package
   on a real machine (Windows 11, Arch) and run `docs/TESTING.md`, sections 9–11.
5. Write the release notes (what works, what's known not to), then publish.
