// SPDX-License-Identifier: GPL-3.0-or-later

//! What makes the tray visible when something goes wrong, or when it seems to do nothing: a log
//! file (a Windows app has no console to print to), an error box when it can't start, one tray
//! per profile, and a notice saying where its icon is.

use std::fs::{self, File, OpenOptions, TryLockError};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use anyhow::Result;
use pasteget_desktop::config::{DEFAULT_PROFILE, Profile};
use pasteget_desktop::notify::{DesktopNotifier, Notifier};
use tracing_subscriber::fmt::writer::MakeWriterExt;

/// Past this size, the log starts over (the previous one is kept as `.old.log`).
const MAX_LOG_BYTES: u64 = 1024 * 1024;

/// Where the tray's icon is, for the notices.
#[cfg(windows)]
const ICON_PLACE: &str = "in the notification area, under ^ in the taskbar";
#[cfg(not(windows))]
const ICON_PLACE: &str = "in the system tray";

/// Logs to stderr and to a file, which it returns. Never clipboard contents: nothing logs them.
pub fn init_logs(profile: &str) -> Option<PathBuf> {
    let filter =
        || tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into());
    match open_log(profile) {
        Some((path, file)) => {
            tracing_subscriber::fmt()
                .with_env_filter(filter())
                .with_ansi(false)
                .with_writer(std::io::stderr.and(Mutex::new(file)))
                .init();
            Some(path)
        }
        None => {
            tracing_subscriber::fmt()
                .with_env_filter(filter())
                .with_writer(std::io::stderr)
                .init();
            None
        }
    }
}

/// `%LOCALAPPDATA%\paste-get\logs\pasteget-tray.log` on Windows,
/// `~/.local/share/paste-get/logs/pasteget-tray.log` on Linux.
fn open_log(profile: &str) -> Option<(PathBuf, File)> {
    let dir = dirs::data_local_dir()?.join("paste-get").join("logs");
    fs::create_dir_all(&dir).ok()?;
    let name = if profile == DEFAULT_PROFILE {
        "pasteget-tray".to_owned()
    } else {
        format!("pasteget-tray-{profile}")
    };
    let path = dir.join(format!("{name}.log"));
    if fs::metadata(&path).is_ok_and(|m| m.len() > MAX_LOG_BYTES) {
        let _ = fs::rename(&path, dir.join(format!("{name}.old.log")));
    }
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .ok()?;
    Some((path, file))
}

/// Held while this tray runs. `None` if another tray already runs for the profile.
pub fn lock_profile(profile: &Profile) -> Result<Option<File>> {
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(profile.dir().join("tray.lock"))?;
    match file.try_lock() {
        Ok(()) => Ok(Some(file)),
        Err(TryLockError::WouldBlock) => Ok(None),
        Err(TryLockError::Error(e)) => Err(e.into()),
    }
}

pub fn already_running_notice() -> String {
    format!("paste-get is already running: its icon is {ICON_PLACE}")
}

pub fn running_notice() -> String {
    format!("paste-get is running: its icon is {ICON_PLACE}")
}

/// For the second tray, which has no daemon of its own: shows `summary`, then waits for the
/// notification to be handed over before the process ends.
pub fn notify_once(summary: String) {
    tauri::async_runtime::block_on(async move {
        let notifier = DesktopNotifier::start();
        notifier.notice(summary);
        tokio::time::sleep(Duration::from_secs(2)).await;
    });
}

/// The tray couldn't start: say so where the user sees it, not only in the log.
pub fn show_fatal(error: &str, log: Option<&Path>) {
    let mut text = format!("paste-get could not start.\n\n{error}");
    if let Some(log) = log {
        text.push_str(&format!("\n\nDetails: {}", log.display()));
    }
    #[cfg(windows)]
    message_box(&text);
    #[cfg(not(windows))]
    eprintln!("{text}");
}

#[cfg(windows)]
#[allow(unsafe_code)]
fn message_box(text: &str) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{MB_ICONERROR, MB_OK, MessageBoxW};

    let wide = |s: &str| s.encode_utf16().chain(Some(0)).collect::<Vec<u16>>();
    let (text, title) = (wide(text), wide("paste-get"));
    // SAFETY: both strings are NUL-terminated and outlive the call; no owner window.
    unsafe {
        MessageBoxW(
            std::ptr::null_mut(),
            text.as_ptr(),
            title.as_ptr(),
            MB_OK | MB_ICONERROR,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_second_tray_for_a_profile_is_turned_away() {
        let home = tempfile::tempdir().unwrap();
        let profile = Profile::open_in(home.path(), "pc").unwrap();
        let other = Profile::open_in(home.path(), "laptop").unwrap();

        let first = lock_profile(&profile).unwrap();
        assert!(first.is_some());
        assert!(lock_profile(&profile).unwrap().is_none());
        // Other profiles have trays of their own.
        assert!(lock_profile(&other).unwrap().is_some());

        drop(first);
        assert!(lock_profile(&profile).unwrap().is_some());
    }
}
