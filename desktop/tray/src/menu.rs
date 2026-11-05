// SPDX-License-Identifier: GPL-3.0-or-later

//! What the tray menu shows, as plain data so it can be tested without a desktop. `main.rs`
//! turns [`Entry`]s into Tauri menu items and menu clicks back into [`Command`]s.

use pasteget_desktop::control::{DeviceStatusReply, RecentReply, StatusReply};
use pasteget_desktop::recent;

pub const PAIR: &str = "pair";
pub const SETTINGS: &str = "settings";
pub const PAUSE: &str = "pause";
pub const START_AT_LOGIN: &str = "start-at-login";
pub const QUIT: &str = "quit";
const RECENT_PREFIX: &str = "recent:";
const FORGET_PREFIX: &str = "forget:";

/// What the tray knows about the daemon.
#[derive(Debug)]
pub enum State {
    Starting,
    Running {
        status: StatusReply,
        recent: Vec<RecentReply>,
    },
    /// The daemon isn't running and couldn't be started.
    Down(String),
}

impl State {
    pub fn paused(&self) -> bool {
        matches!(self, Self::Running { status, .. } if status.paused)
    }

    /// One line, for the tray icon's tooltip.
    pub fn summary(&self) -> String {
        match self {
            Self::Starting => "Starting…".into(),
            Self::Down(_) => "Not running".into(),
            Self::Running { status, .. } => {
                let connected = status.devices.iter().filter(|d| d.connected).count();
                if status.paused {
                    "Paused".into()
                } else if connected == 1 {
                    "Syncing with 1 device".into()
                } else if connected > 1 {
                    format!("Syncing with {connected} devices")
                } else {
                    "No device connected".into()
                }
            }
        }
    }
}

/// One menu line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Entry {
    /// Greyed-out text.
    Info(String),
    Separator,
    Action {
        id: String,
        label: String,
    },
    Check {
        id: String,
        label: String,
        checked: bool,
    },
    Submenu {
        label: String,
        entries: Vec<Entry>,
    },
}

/// What a click asks for.
#[derive(Debug, PartialEq, Eq)]
pub enum Command {
    Pair,
    Settings,
    TogglePause,
    ToggleStartAtLogin,
    CopyRecent(u64),
    /// Unpair the device with this ID.
    Forget(String),
    Quit,
}

impl Command {
    pub fn from_id(id: &str) -> Option<Self> {
        match id {
            PAIR => Some(Self::Pair),
            SETTINGS => Some(Self::Settings),
            PAUSE => Some(Self::TogglePause),
            START_AT_LOGIN => Some(Self::ToggleStartAtLogin),
            QUIT => Some(Self::Quit),
            _ => {
                if let Some(device) = id.strip_prefix(FORGET_PREFIX) {
                    return Some(Self::Forget(device.into()));
                }
                id.strip_prefix(RECENT_PREFIX)
                    .and_then(|n| n.parse().ok())
                    .map(Self::CopyRecent)
            }
        }
    }
}

/// The menu. `start_at_login` is `None` where that isn't supported.
pub fn entries(state: &State, start_at_login: Option<bool>) -> Vec<Entry> {
    let mut entries = daemon_entries(state);
    entries.push(Entry::Separator);
    entries.push(Entry::Action {
        id: SETTINGS.into(),
        label: "Settings…".into(),
    });
    if let Some(checked) = start_at_login {
        entries.push(Entry::Check {
            id: START_AT_LOGIN.into(),
            label: "Start at login".into(),
            checked,
        });
    }
    entries.push(Entry::Action {
        id: QUIT.into(),
        label: "Quit".into(),
    });
    entries
}

fn daemon_entries(state: &State) -> Vec<Entry> {
    match state {
        State::Starting => vec![Entry::Info("paste-get is starting…".into())],
        State::Down(reason) => vec![
            Entry::Info("paste-get is not running".into()),
            Entry::Info(text(reason)),
        ],
        State::Running { status, recent } => {
            let mut entries = vec![
                Entry::Info(format!("paste-get · {}", text(&status.device_name))),
                Entry::Info(state.summary()),
                Entry::Separator,
            ];
            if status.devices.is_empty() {
                entries.push(Entry::Info("No paired device yet".into()));
            } else {
                entries.push(Entry::Info("Devices".into()));
                entries.extend(status.devices.iter().map(device_entry));
            }
            entries.push(Entry::Action {
                id: PAIR.into(),
                label: "Pair a phone…".into(),
            });
            entries.push(Entry::Separator);
            entries.push(Entry::Submenu {
                label: "Recent".into(),
                entries: if recent.is_empty() {
                    vec![Entry::Info("Nothing yet".into())]
                } else {
                    recent.iter().map(recent_entry).collect()
                },
            });
            entries.push(Entry::Check {
                id: PAUSE.into(),
                label: "Pause syncing".into(),
                checked: status.paused,
            });
            entries
        }
    }
}

/// "● POCO M8", with a way to unpair it one level down, so it isn't clicked by accident.
fn device_entry(device: &DeviceStatusReply) -> Entry {
    let dot = if device.connected { "●" } else { "○" };
    Entry::Submenu {
        label: format!("{dot}  {}", text(&device.name)),
        entries: vec![
            Entry::Info(
                if device.connected {
                    "Connected"
                } else {
                    "Not connected"
                }
                .into(),
            ),
            Entry::Action {
                id: format!("{FORGET_PREFIX}{}", device.id),
                label: "Forget this device".into(),
            },
        ],
    }
}

/// "Text: hello world · POCO M8, 2 min". Clicking it copies the clip again.
fn recent_entry(clip: &RecentReply) -> Entry {
    let from = clip.from.as_deref().unwrap_or("this computer");
    Entry::Action {
        id: format!("{RECENT_PREFIX}{}", clip.id),
        label: format!(
            "{} · {}, {}",
            text(&clip.describe()),
            text(from),
            recent::ago(clip.age_secs)
        ),
    }
}

/// Menu-safe text from a device or a clip: one short line, and `&` shown as itself rather
/// than as a keyboard mnemonic (Windows and GTK menus both use it).
fn text(s: &str) -> String {
    recent::preview(s).replace('&', "&&")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status(paused: bool, devices: &[(&str, bool)]) -> StatusReply {
        StatusReply {
            device_id: "ID".into(),
            device_name: "Desk & Co (Linux)".into(),
            listening_on: "0.0.0.0:47290".into(),
            pairing_open: false,
            paused,
            devices: devices
                .iter()
                .map(|(name, connected)| DeviceStatusReply {
                    id: format!("id-{name}"),
                    name: (*name).into(),
                    connected: *connected,
                })
                .collect(),
        }
    }

    fn clip(id: u64, preview: Option<&str>, from: Option<&str>) -> RecentReply {
        RecentReply {
            id,
            mime_type: if preview.is_some() {
                "text/plain".into()
            } else {
                "image/jpeg".into()
            },
            len: 1_258_291,
            from: from.map(Into::into),
            age_secs: 120,
            preview: preview.map(Into::into),
        }
    }

    fn labels(entries: &[Entry]) -> Vec<String> {
        entries
            .iter()
            .map(|entry| match entry {
                Entry::Info(label) | Entry::Action { label, .. } => label.clone(),
                Entry::Check { label, checked, .. } => {
                    format!("[{}] {label}", if *checked { "x" } else { " " })
                }
                Entry::Submenu { label, entries } => {
                    format!("{label} > {}", labels(entries).join(" | "))
                }
                Entry::Separator => "---".into(),
            })
            .collect()
    }

    #[test]
    fn shows_status_devices_recent_clips_and_pause() {
        let state = State::Running {
            status: status(false, &[("POCO M8 5G", true), ("Laptop", false)]),
            recent: vec![
                clip(7, Some("hello world"), Some("POCO M8 5G")),
                clip(6, None, None),
            ],
        };

        assert_eq!(
            labels(&entries(&state, Some(true))),
            [
                "paste-get · Desk && Co (Linux)",
                "Syncing with 1 device",
                "---",
                "Devices",
                "●  POCO M8 5G > Connected | Forget this device",
                "○  Laptop > Not connected | Forget this device",
                "Pair a phone…",
                "---",
                "Recent > Text: hello world · POCO M8 5G, 2 min | Image (JPEG, 1.2 MB) · this computer, 2 min",
                "[ ] Pause syncing",
                "---",
                "Settings…",
                "[x] Start at login",
                "Quit",
            ]
        );
    }

    #[test]
    fn says_when_paused_or_alone() {
        let paused = State::Running {
            status: status(true, &[("POCO", true)]),
            recent: Vec::new(),
        };
        assert!(paused.paused());
        assert_eq!(paused.summary(), "Paused");
        let shown = labels(&entries(&paused, None));
        assert!(shown.contains(&"[x] Pause syncing".to_owned()));
        assert!(shown.contains(&"Recent > Nothing yet".to_owned()));

        let alone = State::Running {
            status: status(false, &[]),
            recent: Vec::new(),
        };
        assert!(!alone.paused());
        assert_eq!(alone.summary(), "No device connected");
        let shown = labels(&entries(&alone, None));
        assert!(shown.contains(&"No paired device yet".to_owned()));
        assert!(shown.contains(&"Pair a phone…".to_owned()));
    }

    #[test]
    fn a_stopped_daemon_still_offers_quit() {
        let state = State::Down("could not listen on 0.0.0.0:47290: address in use".into());

        let entries = entries(&state, Some(false));
        assert!(!state.paused());
        assert_eq!(
            entries.last(),
            Some(&Entry::Action {
                id: QUIT.into(),
                label: "Quit".into()
            })
        );
        assert!(labels(&entries)[1].starts_with("could not listen"));
        assert!(labels(&entries).contains(&"[ ] Start at login".to_owned()));
    }

    #[test]
    fn clicks_map_back_to_commands() {
        assert_eq!(Command::from_id(PAUSE), Some(Command::TogglePause));
        assert_eq!(Command::from_id(QUIT), Some(Command::Quit));
        assert_eq!(Command::from_id(PAIR), Some(Command::Pair));
        assert_eq!(Command::from_id(SETTINGS), Some(Command::Settings));
        assert_eq!(
            Command::from_id(START_AT_LOGIN),
            Some(Command::ToggleStartAtLogin)
        );
        assert_eq!(Command::from_id("recent:42"), Some(Command::CopyRecent(42)));
        assert_eq!(Command::from_id("recent:x"), None);
        assert_eq!(
            Command::from_id("forget:ABC"),
            Some(Command::Forget("ABC".into()))
        );
        assert_eq!(Command::from_id("other"), None);

        // Every clickable entry maps to a command.
        let state = State::Running {
            status: status(false, &[("POCO", true)]),
            recent: vec![clip(3, Some("a"), None)],
        };
        fn ids(entries: &[Entry]) -> Vec<String> {
            entries
                .iter()
                .flat_map(|entry| match entry {
                    Entry::Action { id, .. } | Entry::Check { id, .. } => vec![id.clone()],
                    Entry::Submenu { entries, .. } => ids(entries),
                    _ => Vec::new(),
                })
                .collect()
        }
        for id in ids(&entries(&state, Some(false))) {
            assert!(Command::from_id(&id).is_some(), "{id}");
        }
    }
}
