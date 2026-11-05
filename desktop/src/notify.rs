// SPDX-License-Identifier: GPL-3.0-or-later

//! "Image from POCO M8 5G ✓" notifications when a clip from another device lands on the
//! clipboard. They never show contents: only the kind of clip and the device it came from.
//! The one exception is a link, whose site is shown so the user can decide to open it.
//!
//! Windows shows them as toasts, Linux through the desktop's notification service (KDE Plasma,
//! or mako, dunst, swaync… on Hyprland). Without one, nothing is shown and the daemon carries on.

use std::fmt;

use pasteget_core::detect::Link;
use tokio::sync::mpsc;

use crate::config::NotifyFor;

/// A clip that was just put on the clipboard.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Received {
    pub image: bool,
    /// Name of the device it came from.
    pub from: String,
    /// For a one-time code: the app whose notification held it. The code itself is never shown.
    pub code_from: Option<String>,
}

impl Received {
    pub fn summary(&self) -> String {
        if let Some(app) = &self.code_from {
            return format!("Code from {app} ({}) ✓", self.from);
        }
        let kind = if self.image { "Image" } else { "Text" };
        format!("{kind} from {} ✓", self.from)
    }

    /// Whether `setting` asks for a notification about this clip. A one-time code always gets
    /// one, unless notifications are off: it replaced what was on the clipboard by itself.
    pub fn wanted_by(&self, setting: NotifyFor) -> bool {
        match setting {
            NotifyFor::Off => false,
            NotifyFor::Images => self.image || self.code_from.is_some(),
            NotifyFor::All => true,
        }
    }
}

/// Shows notifications. Called from the clipboard thread: must not block.
pub trait Notifier: Send + Sync + fmt::Debug + 'static {
    fn received(&self, received: Received);

    /// What a hotkey just did, such as "Syncing paused". Never clipboard contents.
    fn notice(&self, summary: String) {
        let _ = summary;
    }

    /// A link from device `from` is on the clipboard: offer to open it. Nothing opens unless the
    /// user clicks.
    fn link(&self, from: String, link: Link) {
        let _ = (from, link);
    }
}

/// "Link from POCO M8 5G: example.com". The host is plain text in the summary, never markup.
pub fn link_summary(from: &str, link: &Link) -> String {
    format!("Link from {from}: {}", link.host)
}

/// One notification to show.
#[derive(Debug)]
struct Shown {
    summary: String,
    body: &'static str,
    /// Opened in the browser when the notification is clicked.
    open: Option<String>,
}

/// The desktop's own notifications, shown one at a time by a task of their own.
#[derive(Debug)]
pub struct DesktopNotifier {
    queue: mpsc::Sender<Shown>,
}

/// Notifications waiting to be shown. When clips arrive faster, the extra ones are skipped.
const QUEUE: usize = 4;

/// Long enough to read, short enough not to pile up.
const SHOWN_FOR_MS: i32 = 4000;

/// A link stays up longer: it asks for a click. (Windows has its own "short" toast duration.)
#[cfg(all(unix, not(target_os = "macos")))]
const LINK_SHOWN_FOR_MS: i32 = 8000;

const BODY: &str = "Ready to paste.";
const NOTICE_BODY: &str = "paste-get";
const LINK_BODY: &str = "Copied. Click to open it in your browser.";

impl DesktopNotifier {
    /// Starts the notification task. Call from within the Tokio runtime.
    pub fn start() -> Self {
        let (queue, mut queued) = mpsc::channel::<Shown>(QUEUE);
        tokio::spawn(async move {
            let mut desktop = platform::Desktop::new();
            while let Some(shown) = queued.recv().await {
                if let Err(e) = desktop.show(shown).await {
                    tracing::debug!(error = %e, "notification not shown");
                }
            }
        });
        Self { queue }
    }

    fn queue(&self, summary: String, body: &'static str, open: Option<String>) {
        let _ = self.queue.try_send(Shown {
            summary,
            body,
            open,
        });
    }
}

impl Notifier for DesktopNotifier {
    fn received(&self, received: Received) {
        self.queue(received.summary(), BODY, None);
    }

    fn notice(&self, summary: String) {
        self.queue(summary, NOTICE_BODY, None);
    }

    fn link(&self, from: String, link: Link) {
        self.queue(link_summary(&from, &link), LINK_BODY, Some(link.url));
    }
}

/// Opens a link from [`pasteget_core::detect::link`] in the default browser. Only called when
/// the user clicks its notification.
fn open_in_browser(url: &str) {
    tracing::info!(len = url.len(), "opening a link");
    if let Err(e) = platform::open(url) {
        tracing::warn!(error = %e, "could not open the link");
    }
}

/// The freedesktop notification service, over D-Bus.
#[cfg(all(unix, not(target_os = "macos")))]
mod platform {
    use std::collections::{HashMap, VecDeque};
    use std::sync::{Arc, Mutex, PoisonError};

    use futures_util::StreamExt;
    use zbus::{Connection, Proxy, Variant};

    use super::{LINK_SHOWN_FOR_MS, SHOWN_FOR_MS, Shown};

    const SERVICE: &str = "org.freedesktop.Notifications";
    const PATH: &str = "/org/freedesktop/Notifications";
    /// The action of a click on the notification itself.
    const DEFAULT_ACTION: &str = "default";
    const OPEN_ACTION: &str = "open";
    /// Links of the last notifications shown, by notification ID.
    const LINKS_KEPT: usize = 8;

    type Links = Arc<Mutex<VecDeque<(u32, String)>>>;

    pub struct Desktop {
        /// Opened on first use, and again after an error.
        connection: Option<Connection>,
        links: Links,
    }

    impl Desktop {
        pub fn new() -> Self {
            Self {
                connection: None,
                links: Links::default(),
            }
        }

        pub async fn show(&mut self, shown: Shown) -> zbus::Result<()> {
            let connection = match &self.connection {
                Some(connection) => connection.clone(),
                None => {
                    let connection = Connection::session().await?;
                    listen_for_clicks(&connection, Arc::clone(&self.links)).await;
                    connection
                }
            };
            // Transient: don't keep it in the notification history.
            let hints = HashMap::from([("transient", Variant::from(true))]);
            let (actions, timeout) = match shown.open {
                Some(_) => (
                    vec![DEFAULT_ACTION, "Open", OPEN_ACTION, "Open"],
                    LINK_SHOWN_FOR_MS,
                ),
                None => (Vec::new(), SHOWN_FOR_MS),
            };
            // The summary is plain text everywhere; bodies may be parsed as markup, so the
            // untrusted device name and link host stay out of the body.
            let result = connection
                .call_method(
                    Some(SERVICE),
                    PATH,
                    Some(SERVICE),
                    "Notify",
                    &(
                        "paste-get",
                        0u32,
                        "edit-paste",
                        shown.summary,
                        shown.body,
                        actions,
                        hints,
                        timeout,
                    ),
                )
                .await;
            self.connection = result.is_ok().then_some(connection);
            let id: u32 = result?.body().deserialize()?;
            if let Some(url) = shown.open {
                let mut links = self.links.lock().unwrap_or_else(PoisonError::into_inner);
                if links.len() == LINKS_KEPT {
                    links.pop_front();
                }
                links.push_back((id, url));
            }
            Ok(())
        }
    }

    /// Opens the link of a notification when it, or its Open button, is clicked.
    async fn listen_for_clicks(connection: &Connection, links: Links) {
        let signals = async {
            let proxy = Proxy::new(connection, SERVICE, PATH, SERVICE).await?;
            proxy.receive_signal("ActionInvoked").await
        };
        let mut clicks = match signals.await {
            Ok(clicks) => clicks,
            Err(e) => {
                tracing::debug!(error = %e, "notification clicks not available");
                return;
            }
        };
        tokio::spawn(async move {
            while let Some(signal) = clicks.next().await {
                let Ok((id, action)) = signal.body().deserialize::<(u32, String)>() else {
                    continue;
                };
                if action != DEFAULT_ACTION && action != OPEN_ACTION {
                    continue;
                }
                let url = {
                    let mut links = links.lock().unwrap_or_else(PoisonError::into_inner);
                    let index = links.iter().position(|(shown, _)| *shown == id);
                    index
                        .and_then(|index| links.remove(index))
                        .map(|(_, url)| url)
                };
                if let Some(url) = url {
                    super::open_in_browser(&url);
                }
            }
        });
    }

    /// `xdg-open`, which every desktop provides.
    pub fn open(url: &str) -> std::io::Result<()> {
        let mut child = std::process::Command::new("xdg-open")
            .arg(url)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()?;
        // Reaped off the async threads: xdg-open may wait for the browser to start.
        std::thread::spawn(move || child.wait());
        Ok(())
    }
}

/// Windows toasts and macOS notifications.
#[cfg(any(windows, target_os = "macos"))]
mod platform {
    use super::{SHOWN_FOR_MS, Shown};

    pub struct Desktop {
        /// The app the toasts come from: paste-get once registered, else PowerShell.
        #[cfg(windows)]
        app_id: &'static str,
    }

    impl Desktop {
        pub fn new() -> Self {
            Self {
                #[cfg(windows)]
                app_id: windows::register_app_id(),
            }
        }

        pub async fn show(&mut self, shown: Shown) -> anyhow::Result<()> {
            #[cfg(windows)]
            let app_id = self.app_id;
            tokio::task::spawn_blocking(move || {
                #[cfg(windows)]
                if let Some(url) = shown.open {
                    return windows::show_link(app_id, &shown.summary, shown.body, url);
                }
                let mut notification = notify_rust::Notification::new();
                #[cfg(windows)]
                notification.app_id(app_id);
                notification
                    .appname("paste-get")
                    .summary(&shown.summary)
                    .body(shown.body)
                    .timeout(notify_rust::Timeout::Milliseconds(
                        SHOWN_FOR_MS.unsigned_abs(),
                    ))
                    .show()
                    .map(|_| ())
                    .map_err(anyhow::Error::from)
            })
            .await??;
            Ok(())
        }
    }

    #[cfg(windows)]
    pub use windows::open;

    /// No click handling through notify-rust on macOS (tier 2): the link is only shown.
    #[cfg(target_os = "macos")]
    pub fn open(_url: &str) -> std::io::Result<()> {
        Ok(())
    }

    #[cfg(windows)]
    mod windows {
        use anyhow::Context;
        use tauri_winrt_notification::{Duration, Toast};

        use crate::autostart::APP_ID;

        /// Shown with the toasts, and in Settings → System → Notifications.
        const ICON: &[u8] = include_bytes!("../tray/icons/icon.png");

        /// Registers paste-get's AppUserModelID, so toasts show its name and icon instead of
        /// "Windows PowerShell": the way Windows documents for apps installed without a
        /// package, under `HKEY_CURRENT_USER\Software\Classes\AppUserModelId`. The uninstaller
        /// removes it (`desktop/tray/installer-hooks.nsh`). Returns the app ID to use, or
        /// PowerShell's if registering failed, since a toast with an unknown ID isn't shown.
        pub fn register_app_id() -> &'static str {
            match register() {
                Ok(()) => APP_ID,
                Err(e) => {
                    tracing::debug!(error = %format!("{e:#}"), "notifications show as PowerShell's");
                    Toast::POWERSHELL_APP_ID
                }
            }
        }

        fn register() -> anyhow::Result<()> {
            let dir = dirs::data_local_dir()
                .context("no local app data directory")?
                .join("paste-get");
            std::fs::create_dir_all(&dir)?;
            let icon = dir.join("notification-icon.png");
            if std::fs::read(&icon).ok().as_deref() != Some(ICON) {
                std::fs::write(&icon, ICON)?;
            }
            let key = windows_registry::CURRENT_USER
                .create(format!(r"Software\Classes\AppUserModelId\{APP_ID}"))?;
            key.set_string("DisplayName", "paste-get")?;
            key.set_string(
                "IconUri",
                icon.to_str().context("the icon path is not Unicode")?,
            )?;
            Ok(())
        }

        /// A toast that opens `url` when it, or its Open button, is clicked. Windows calls back
        /// while the daemon runs.
        pub fn show_link(
            app_id: &str,
            summary: &str,
            body: &str,
            url: String,
        ) -> anyhow::Result<()> {
            Toast::new(app_id)
                .title(summary)
                .text1(body)
                .duration(Duration::Short)
                .add_button("Open", "open")
                .on_activated(move |_| {
                    super::super::open_in_browser(&url);
                    Ok(())
                })
                .show()?;
            Ok(())
        }

        /// `ShellExecuteW` with "open": the default browser, no command line in between.
        #[allow(unsafe_code)]
        pub fn open(url: &str) -> std::io::Result<()> {
            use windows_sys::Win32::UI::Shell::ShellExecuteW;
            use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

            let wide = |s: &str| s.encode_utf16().chain(Some(0)).collect::<Vec<u16>>();
            let (verb, file) = (wide("open"), wide(url));
            // SAFETY: both strings are NUL-terminated and outlive the call; the other pointers
            // may be null.
            let result = unsafe {
                ShellExecuteW(
                    std::ptr::null_mut(),
                    verb.as_ptr(),
                    file.as_ptr(),
                    std::ptr::null(),
                    std::ptr::null(),
                    SW_SHOWNORMAL,
                )
            };
            // Values above 32 mean success.
            if result as usize > 32 {
                Ok(())
            } else {
                Err(std::io::Error::other(format!(
                    "ShellExecuteW failed ({})",
                    result as usize
                )))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn says_what_arrived_and_from_where() {
        let image = Received {
            image: true,
            from: "POCO M8 5G".into(),
            code_from: None,
        };
        assert_eq!(image.summary(), "Image from POCO M8 5G ✓");

        let text = Received {
            image: false,
            ..image.clone()
        };
        assert_eq!(text.summary(), "Text from POCO M8 5G ✓");

        let code = Received {
            image: false,
            from: "POCO M8 5G".into(),
            code_from: Some("Messages".into()),
        };
        assert_eq!(code.summary(), "Code from Messages (POCO M8 5G) ✓");
        assert!(code.wanted_by(NotifyFor::Images));
        assert!(!code.wanted_by(NotifyFor::Off));
    }

    #[test]
    fn the_setting_picks_which_clips_notify() {
        let image = Received {
            image: true,
            from: "phone".into(),
            code_from: None,
        };
        let text = Received {
            image: false,
            from: "phone".into(),
            code_from: None,
        };

        assert!(image.wanted_by(NotifyFor::Images));
        assert!(!text.wanted_by(NotifyFor::Images));
        assert!(text.wanted_by(NotifyFor::All));
        assert!(!image.wanted_by(NotifyFor::Off));
    }
}
