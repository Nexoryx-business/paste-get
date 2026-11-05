// SPDX-License-Identifier: GPL-3.0-or-later

//! The "Settings" window: the parts of `settings.toml` people change, without editing the file.
//!
//! The page (`dist/settings.html`) gets the current values and sends new ones through two
//! Tauri commands, `load_settings` and `save_settings` (in `main.rs`). What it sends is checked
//! here, by [`Form::apply`], before anything is written.

use anyhow::{Context, Result};
use pasteget_desktop::config::{MAX_DEVICE_NAME_CHARS, NotifyFor, Settings};
use pasteget_desktop::hotkeys::{Hotkey, HotkeySettings};
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager, Runtime, WebviewUrl, WebviewWindowBuilder};

pub const WINDOW: &str = "settings";

/// What the page shows and edits.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Form {
    pub device_name: String,
    pub notify: NotifyFor,
    pub offer_to_open_links: bool,
    /// Hotkeys as text, empty when off: "Ctrl+Alt+Shift+P".
    pub toggle_pause: String,
    pub send_clipboard: String,
    /// `None` where starting at login isn't supported: the page hides it.
    pub start_at_login: Option<bool>,
}

impl Form {
    pub fn of(settings: &Settings, start_at_login: Option<bool>) -> Self {
        Self {
            device_name: settings.device_name.clone(),
            notify: settings.notify,
            offer_to_open_links: settings.offer_to_open_links,
            toggle_pause: settings.hotkeys.toggle_pause.clone(),
            send_clipboard: settings.hotkeys.send_clipboard.clone(),
            start_at_login,
        }
    }

    /// `current` changed as the form says, or what's wrong with the form, to show the user.
    /// Settings the form doesn't cover (the port) are kept.
    pub fn apply(&self, current: &Settings) -> Result<Settings, String> {
        let device_name = self.device_name.trim();
        if device_name.is_empty() {
            return Err("Give this computer a name: it is shown on your phone.".into());
        }
        if device_name.chars().count() > MAX_DEVICE_NAME_CHARS {
            return Err(format!(
                "The name is too long: {MAX_DEVICE_NAME_CHARS} characters at most."
            ));
        }
        let toggle_pause = hotkey("Pause or resume syncing", &self.toggle_pause)?;
        let send_clipboard = hotkey("Send the clipboard now", &self.send_clipboard)?;
        if toggle_pause.is_some() && toggle_pause == send_clipboard {
            return Err("Both actions have the same hotkey: give each its own.".into());
        }
        let text = |hotkey: Option<Hotkey>| hotkey.map(|h| h.to_string()).unwrap_or_default();
        Ok(Settings {
            device_name: device_name.to_owned(),
            notify: self.notify,
            offer_to_open_links: self.offer_to_open_links,
            hotkeys: HotkeySettings {
                toggle_pause: text(toggle_pause),
                send_clipboard: text(send_clipboard),
            },
            ..current.clone()
        })
    }
}

/// A hotkey from the form: `None` when left empty.
fn hotkey(action: &str, text: &str) -> Result<Option<Hotkey>, String> {
    let text = text.trim();
    if text.is_empty() {
        return Ok(None);
    }
    text.parse()
        .map(Some)
        .map_err(|e| format!("“{action}”: {text} can't be a hotkey ({e})."))
}

/// What to tell the user once the form is saved.
pub fn saved_message(restart_needed: bool, hosting: bool) -> &'static str {
    match (restart_needed, hosting) {
        (false, _) => "Saved.",
        (true, true) => "Saved. paste-get restarted to use the new name or hotkeys.",
        (true, false) => {
            "Saved. Restart `pasteget daemon` to use the new name or hotkeys: it runs on its own."
        }
    }
}

/// Opens the window, or brings it to the front.
pub fn open<R: Runtime>(app: &AppHandle<R>) -> Result<()> {
    if let Some(window) = app.get_webview_window(WINDOW) {
        let _ = window.unminimize();
        window.set_focus()?;
        return Ok(());
    }
    WebviewWindowBuilder::new(app, WINDOW, WebviewUrl::App("settings.html".into()))
        .title("Settings · paste-get")
        .inner_size(460.0, 780.0)
        .min_inner_size(380.0, 480.0)
        .center()
        .focused(true)
        .build()
        .context("could not open the settings window")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn current() -> Settings {
        Settings {
            device_name: "HP-AIO (Windows)".into(),
            port: 47_290,
            ..Settings::default()
        }
    }

    #[test]
    fn the_form_shows_the_settings_and_saves_them_back() {
        let form = Form::of(&current(), Some(true));
        assert_eq!(form.device_name, "HP-AIO (Windows)");
        assert_eq!(form.toggle_pause, "Ctrl+Alt+Shift+P");
        assert_eq!(form.apply(&current()), Ok(current()));
    }

    #[test]
    fn changes_are_cleaned_up_and_the_port_is_kept() {
        let form = Form {
            device_name: "  Desk  ".into(),
            notify: NotifyFor::All,
            offer_to_open_links: false,
            toggle_pause: " ctrl + alt + p ".into(),
            send_clipboard: String::new(),
            start_at_login: None,
        };

        let saved = form.apply(&current()).unwrap();

        assert_eq!(saved.device_name, "Desk");
        assert_eq!(saved.notify, NotifyFor::All);
        assert!(!saved.offer_to_open_links);
        assert_eq!(saved.hotkeys.toggle_pause, "Ctrl+Alt+P");
        assert_eq!(saved.hotkeys.send_clipboard, "");
        assert_eq!(saved.port, 47_290);
    }

    #[test]
    fn mistakes_are_explained() {
        let form = |name: &str, pause: &str, send: &str| Form {
            device_name: name.into(),
            toggle_pause: pause.into(),
            send_clipboard: send.into(),
            ..Form::of(&current(), None)
        };

        assert!(
            form(" ", "", "")
                .apply(&current())
                .unwrap_err()
                .contains("name")
        );
        let long = "x".repeat(MAX_DEVICE_NAME_CHARS + 1);
        assert!(
            form(&long, "", "")
                .apply(&current())
                .unwrap_err()
                .contains("too long")
        );
        let typing = form("pc", "Shift+A", "").apply(&current()).unwrap_err();
        assert!(typing.contains("Pause or resume syncing") && typing.contains("Shift+A"));
        let same = form("pc", "Ctrl+Alt+K", "alt+ctrl+k").apply(&current());
        assert!(same.unwrap_err().contains("same hotkey"));
    }

    #[test]
    fn the_message_says_whether_a_restart_happened() {
        assert_eq!(saved_message(false, true), "Saved.");
        assert!(saved_message(true, true).contains("restarted"));
        assert!(saved_message(true, false).contains("pasteget daemon"));
    }
}
