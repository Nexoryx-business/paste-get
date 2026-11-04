// SPDX-License-Identifier: GPL-3.0-or-later

//! "Image from POCO M8 5G ✓" notifications when a clip from another device lands on the
//! clipboard. They never show contents: only the kind of clip and the device it came from.
//!
//! Windows shows them as toasts, Linux through the desktop's notification service (KDE Plasma,
//! or mako, dunst, swaync… on Hyprland). Without one, nothing is shown and the daemon carries on.

use std::fmt;

use tokio::sync::mpsc;

use crate::config::NotifyFor;

/// A clip that was just put on the clipboard.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Received {
    pub image: bool,
    /// Name of the device it came from.
    pub from: String,
}

impl Received {
    pub fn summary(&self) -> String {
        let kind = if self.image { "Image" } else { "Text" };
        format!("{kind} from {} ✓", self.from)
    }

    /// Whether `setting` asks for a notification about this clip.
    pub fn wanted_by(&self, setting: NotifyFor) -> bool {
        match setting {
            NotifyFor::Off => false,
            NotifyFor::Images => self.image,
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
}

/// The desktop's own notifications, shown one at a time by a task of their own.
#[derive(Debug)]
pub struct DesktopNotifier {
    queue: mpsc::Sender<(String, &'static str)>,
}

/// Notifications waiting to be shown. When clips arrive faster, the extra ones are skipped.
const QUEUE: usize = 4;

/// Long enough to read, short enough not to pile up.
const SHOWN_FOR_MS: i32 = 4000;

const BODY: &str = "Ready to paste.";
const NOTICE_BODY: &str = "paste-get";

impl DesktopNotifier {
    /// Starts the notification task. Call from within the Tokio runtime.
    pub fn start() -> Self {
        let (queue, mut shown) = mpsc::channel::<(String, &'static str)>(QUEUE);
        tokio::spawn(async move {
            let mut desktop = platform::Desktop::new();
            while let Some((summary, body)) = shown.recv().await {
                if let Err(e) = desktop.show(summary, body).await {
                    tracing::debug!(error = %e, "notification not shown");
                }
            }
        });
        Self { queue }
    }
}

impl Notifier for DesktopNotifier {
    fn received(&self, received: Received) {
        let _ = self.queue.try_send((received.summary(), BODY));
    }

    fn notice(&self, summary: String) {
        let _ = self.queue.try_send((summary, NOTICE_BODY));
    }
}

/// The freedesktop notification service, over D-Bus.
#[cfg(all(unix, not(target_os = "macos")))]
mod platform {
    use std::collections::HashMap;

    use zbus::{Connection, Variant};

    use super::SHOWN_FOR_MS;

    pub struct Desktop {
        /// Opened on first use, and again after an error.
        connection: Option<Connection>,
    }

    impl Desktop {
        pub fn new() -> Self {
            Self { connection: None }
        }

        pub async fn show(&mut self, summary: String, body: &str) -> zbus::Result<()> {
            let connection = match &self.connection {
                Some(connection) => connection.clone(),
                None => Connection::session().await?,
            };
            // Transient: don't keep it in the notification history.
            let hints = HashMap::from([("transient", Variant::from(true))]);
            // The summary is plain text everywhere; bodies may be parsed as markup, so the
            // untrusted device name stays out of the body.
            let result = connection
                .call_method(
                    Some("org.freedesktop.Notifications"),
                    "/org/freedesktop/Notifications",
                    Some("org.freedesktop.Notifications"),
                    "Notify",
                    &(
                        "paste-get",
                        0u32,
                        "edit-paste",
                        summary,
                        body,
                        Vec::<&str>::new(),
                        hints,
                        SHOWN_FOR_MS,
                    ),
                )
                .await;
            self.connection = result.is_ok().then_some(connection);
            result.map(|_| ())
        }
    }
}

/// Windows toasts and macOS notifications.
#[cfg(any(windows, target_os = "macos"))]
mod platform {
    use super::SHOWN_FOR_MS;

    pub struct Desktop;

    impl Desktop {
        pub fn new() -> Self {
            Self
        }

        pub async fn show(&mut self, summary: String, body: &'static str) -> anyhow::Result<()> {
            tokio::task::spawn_blocking(move || {
                notify_rust::Notification::new()
                    .appname("paste-get")
                    .summary(&summary)
                    .body(body)
                    .timeout(notify_rust::Timeout::Milliseconds(
                        SHOWN_FOR_MS.unsigned_abs(),
                    ))
                    .show()
                    .map(|_| ())
            })
            .await??;
            Ok(())
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
        };
        assert_eq!(image.summary(), "Image from POCO M8 5G ✓");

        let text = Received {
            image: false,
            ..image.clone()
        };
        assert_eq!(text.summary(), "Text from POCO M8 5G ✓");
    }

    #[test]
    fn the_setting_picks_which_clips_notify() {
        let image = Received {
            image: true,
            from: "phone".into(),
        };
        let text = Received {
            image: false,
            from: "phone".into(),
        };

        assert!(image.wanted_by(NotifyFor::Images));
        assert!(!text.wanted_by(NotifyFor::Images));
        assert!(text.wanted_by(NotifyFor::All));
        assert!(!image.wanted_by(NotifyFor::Off));
    }
}
