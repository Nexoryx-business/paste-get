// SPDX-License-Identifier: GPL-3.0-or-later

//! Global hotkeys: pause or resume syncing, and send the clipboard now.
//!
//! - Windows: `RegisterHotKey`, on a thread of their own.
//! - KDE Plasma (and other desktops with the `GlobalShortcuts` portal): the portal. Plasma asks
//!   the user to confirm them once, and lists them in System Settings → Shortcuts.
//! - Hyprland: nothing here. Hyprland's own `bind` runs the command instead, for instance
//!   `bind = CTRL ALT SHIFT, V, exec, pasteget send --clipboard` (see `docs/TESTING.md`).
//!
//! The keys are set in `settings.toml`, `[hotkeys]` table; an empty string turns one off.

use std::fmt;
use std::str::FromStr;

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

/// What a hotkey does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    TogglePause,
    SendClipboard,
}

impl Action {
    /// Stable identifier, used as the portal's shortcut id.
    pub fn id(self) -> &'static str {
        match self {
            Self::TogglePause => "toggle-pause",
            Self::SendClipboard => "send-clipboard",
        }
    }

    pub fn description(self) -> &'static str {
        match self {
            Self::TogglePause => "Pause or resume syncing",
            Self::SendClipboard => "Send the clipboard to your devices",
        }
    }

    /// The command that does the same, for desktops where hotkeys are bound to commands.
    pub fn command(self) -> &'static str {
        match self {
            Self::TogglePause => "pasteget pause --toggle",
            Self::SendClipboard => "pasteget send --clipboard",
        }
    }

    pub fn from_id(id: &str) -> Option<Self> {
        [Self::TogglePause, Self::SendClipboard]
            .into_iter()
            .find(|action| action.id() == id)
    }
}

/// `[hotkeys]` in `settings.toml`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct HotkeySettings {
    pub toggle_pause: String,
    pub send_clipboard: String,
}

impl Default for HotkeySettings {
    fn default() -> Self {
        // With Shift: on Windows, Ctrl+Alt is also AltGr, which types characters such as `@`
        // on many keyboard layouts.
        Self {
            toggle_pause: "Ctrl+Alt+Shift+P".into(),
            send_clipboard: "Ctrl+Alt+Shift+V".into(),
        }
    }
}

impl HotkeySettings {
    /// The hotkeys that are turned on. Invalid ones are skipped with a warning.
    pub fn bindings(&self) -> Vec<Binding> {
        [
            (Action::TogglePause, &self.toggle_pause),
            (Action::SendClipboard, &self.send_clipboard),
        ]
        .into_iter()
        .filter(|(_, text)| !text.trim().is_empty())
        .filter_map(|(action, text)| match text.parse() {
            Ok(hotkey) => Some(Binding { action, hotkey }),
            Err(e) => {
                tracing::warn!(hotkey = %text, error = %e, "hotkey skipped");
                None
            }
        })
        .collect()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Binding {
    pub action: Action,
    pub hotkey: Hotkey,
}

/// A key with modifiers, written like `Ctrl+Alt+Shift+V`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Hotkey {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    /// The Windows or Super key.
    pub logo: bool,
    pub key: Key,
}

/// Keys a hotkey can use: the ones that are the same on every keyboard layout.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Key {
    /// `A` to `Z`.
    Letter(char),
    /// `0` to `9`.
    Digit(char),
    /// `F1` to `F24`.
    Function(u8),
}

impl FromStr for Hotkey {
    type Err = anyhow::Error;

    fn from_str(text: &str) -> Result<Self> {
        let (mut ctrl, mut alt, mut shift, mut logo) = (false, false, false, false);
        let mut key = None;
        for part in text.split('+').map(str::trim) {
            if key.is_some() {
                bail!("the key must come last, after the modifiers");
            }
            let flag = match part.to_ascii_lowercase().as_str() {
                "ctrl" | "control" => &mut ctrl,
                "alt" => &mut alt,
                "shift" => &mut shift,
                "super" | "win" | "meta" | "logo" => &mut logo,
                _ => {
                    key = Some(parse_key(part)?);
                    continue;
                }
            };
            if *flag {
                bail!("{part} appears twice");
            }
            *flag = true;
        }
        let Some(key) = key else {
            bail!("no key after the modifiers");
        };
        if !(ctrl || alt || logo || matches!(key, Key::Function(_))) {
            bail!("needs Ctrl, Alt or Super, except for F1 to F24: other keys are for typing");
        }
        Ok(Self {
            ctrl,
            alt,
            shift,
            logo,
            key,
        })
    }
}

fn parse_key(part: &str) -> Result<Key> {
    let mut chars = part.chars();
    match (chars.next(), chars.next()) {
        (Some(c), None) if c.is_ascii_alphabetic() => Ok(Key::Letter(c.to_ascii_uppercase())),
        (Some(c), None) if c.is_ascii_digit() => Ok(Key::Digit(c)),
        (Some('F' | 'f'), Some(_)) => match part[1..].parse::<u8>() {
            Ok(n @ 1..=24) if !part[1..].starts_with('0') => Ok(Key::Function(n)),
            _ => bail!("unknown key {part:?}: function keys go from F1 to F24"),
        },
        _ => bail!("unknown key {part:?}: use a letter, a digit or F1 to F24"),
    }
}

impl fmt::Display for Hotkey {
    /// The canonical form, which parses back to the same hotkey.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (on, name) in [
            (self.ctrl, "Ctrl+"),
            (self.alt, "Alt+"),
            (self.shift, "Shift+"),
            (self.logo, "Super+"),
        ] {
            if on {
                f.write_str(name)?;
            }
        }
        match self.key {
            Key::Letter(c) | Key::Digit(c) => write!(f, "{c}"),
            Key::Function(n) => write!(f, "F{n}"),
        }
    }
}

impl Hotkey {
    /// The portal's format, from the XDG shortcuts specification: `CTRL+ALT+SHIFT+v`.
    pub fn portal_trigger(&self) -> String {
        let mut trigger = String::new();
        for (on, name) in [
            (self.ctrl, "CTRL+"),
            (self.alt, "ALT+"),
            (self.shift, "SHIFT+"),
            (self.logo, "LOGO+"),
        ] {
            if on {
                trigger.push_str(name);
            }
        }
        match self.key {
            // XKB key names: lowercase letters.
            Key::Letter(c) => trigger.push(c.to_ascii_lowercase()),
            Key::Digit(c) => trigger.push(c),
            Key::Function(n) => trigger.push_str(&format!("F{n}")),
        }
        trigger
    }

    /// Hyprland's `bind` modifiers: `CTRL ALT SHIFT`.
    pub fn hyprland_mods(&self) -> String {
        [
            (self.ctrl, "CTRL"),
            (self.alt, "ALT"),
            (self.shift, "SHIFT"),
            (self.logo, "SUPER"),
        ]
        .into_iter()
        .filter_map(|(on, name)| on.then_some(name))
        .collect::<Vec<_>>()
        .join(" ")
    }

    /// The key alone, as Hyprland and Windows name it: `V`, `5`, `F9`.
    pub fn key_name(&self) -> String {
        match self.key {
            Key::Letter(c) | Key::Digit(c) => c.to_string(),
            Key::Function(n) => format!("F{n}"),
        }
    }
}

/// Keeps the hotkeys registered. Dropping it unregisters them.
pub struct Hotkeys {
    _listener: platform::Listener,
}

/// Registers `bindings` and sends each press on `pressed`. Returns `None` where this desktop
/// binds hotkeys itself (Hyprland), or has no way for an app to register them; the log says
/// what to do instead.
pub fn start(bindings: &[Binding], pressed: mpsc::Sender<Action>) -> Option<Hotkeys> {
    if bindings.is_empty() {
        return None;
    }
    match platform::Listener::start(bindings, pressed) {
        Ok(Some(listener)) => Some(Hotkeys {
            _listener: listener,
        }),
        Ok(None) => None,
        Err(e) => {
            tracing::warn!(error = %format!("{e:#}"), "hotkeys not available");
            log_commands(bindings);
            None
        }
    }
}

/// Tells how to bind the hotkeys in the desktop's own settings instead.
fn log_commands(bindings: &[Binding]) {
    for binding in bindings {
        tracing::info!(
            hotkey = %binding.hotkey,
            command = binding.action.command(),
            "bind this hotkey to this command in your desktop's keyboard settings"
        );
    }
}

#[cfg(windows)]
mod platform {
    //! `RegisterHotKey` on a thread with its own message loop. Hotkeys registered without a
    //! window belong to the thread, and arrive as `WM_HOTKEY` messages in its queue.

    #![allow(unsafe_code)]

    use std::sync::mpsc as std_mpsc;
    use std::thread;

    use anyhow::{Context, Result, anyhow};
    use tokio::sync::mpsc;
    use windows_sys::Win32::System::Threading::GetCurrentThreadId;
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
        MOD_ALT, MOD_CONTROL, MOD_NOREPEAT, MOD_SHIFT, MOD_WIN, RegisterHotKey, UnregisterHotKey,
        VK_F1,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GetMessageW, MSG, PostThreadMessageW, WM_HOTKEY, WM_QUIT,
    };

    use super::{Action, Binding, Hotkey, Key};

    pub struct Listener {
        thread_id: u32,
    }

    impl Listener {
        pub fn start(bindings: &[Binding], pressed: mpsc::Sender<Action>) -> Result<Option<Self>> {
            let bindings = bindings.to_vec();
            let (started, thread_id) = std_mpsc::channel();
            thread::Builder::new()
                .name("hotkeys".into())
                .spawn(move || run(&bindings, &pressed, &started))
                .context("could not start the hotkey thread")?;
            let thread_id = thread_id
                .recv()
                .map_err(|_| anyhow!("the hotkey thread stopped"))??;
            Ok(Some(Self { thread_id }))
        }
    }

    impl Drop for Listener {
        fn drop(&mut self) {
            // SAFETY: posting a message to a thread id has no memory-safety requirements; a
            // thread that is gone just makes the call fail.
            unsafe {
                PostThreadMessageW(self.thread_id, WM_QUIT, 0, 0);
            }
        }
    }

    fn run(
        bindings: &[Binding],
        pressed: &mpsc::Sender<Action>,
        started: &std_mpsc::Sender<Result<u32>>,
    ) {
        // SAFETY: no arguments.
        let thread_id = unsafe { GetCurrentThreadId() };
        let mut registered = Vec::new();
        for (index, binding) in bindings.iter().enumerate() {
            let id = i32::try_from(index).unwrap_or(i32::MAX);
            let (modifiers, key) = win32(&binding.hotkey);
            // SAFETY: a null window registers the hotkey for this thread.
            let ok =
                unsafe { RegisterHotKey(std::ptr::null_mut(), id, modifiers | MOD_NOREPEAT, key) };
            if ok == 0 {
                tracing::warn!(
                    hotkey = %binding.hotkey,
                    "hotkey not registered: another app uses it. Change it in settings.toml"
                );
            } else {
                tracing::info!(hotkey = %binding.hotkey, action = binding.action.id(), "hotkey registered");
                registered.push((id, binding.action));
            }
        }
        if started.send(Ok(thread_id)).is_err() {
            return;
        }

        // SAFETY: MSG is plain data; all zeroes is a valid value.
        let mut message: MSG = unsafe { std::mem::zeroed() };
        // SAFETY: `message` is a valid, writable MSG. Returns 0 on WM_QUIT and -1 on error.
        while unsafe { GetMessageW(&mut message, std::ptr::null_mut(), 0, 0) } > 0 {
            if message.message != WM_HOTKEY {
                continue;
            }
            let id = i32::try_from(message.wParam).unwrap_or(-1);
            if let Some((_, action)) = registered.iter().find(|(r, _)| *r == id) {
                let _ = pressed.try_send(*action);
            }
        }
        for (id, _) in registered {
            // SAFETY: unregisters what this thread registered.
            unsafe {
                UnregisterHotKey(std::ptr::null_mut(), id);
            }
        }
    }

    /// `RegisterHotKey`'s modifiers and virtual-key code.
    fn win32(hotkey: &Hotkey) -> (u32, u32) {
        let mut modifiers = 0;
        for (on, flag) in [
            (hotkey.ctrl, MOD_CONTROL),
            (hotkey.alt, MOD_ALT),
            (hotkey.shift, MOD_SHIFT),
            (hotkey.logo, MOD_WIN),
        ] {
            if on {
                modifiers |= flag;
            }
        }
        // Virtual-key codes of letters and digits are their ASCII codes.
        let key = match hotkey.key {
            Key::Letter(c) | Key::Digit(c) => u32::from(c),
            Key::Function(n) => u32::from(VK_F1) + u32::from(n) - 1,
        };
        (modifiers, key)
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn maps_to_win32_codes() {
            let hotkey: Hotkey = "Ctrl+Alt+Shift+V".parse().unwrap();
            assert_eq!(win32(&hotkey), (MOD_CONTROL | MOD_ALT | MOD_SHIFT, 0x56));
            let hotkey: Hotkey = "Super+F12".parse().unwrap();
            assert_eq!(win32(&hotkey), (MOD_WIN, 0x7B));
        }
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
mod platform {
    //! The `org.freedesktop.portal.GlobalShortcuts` portal, over D-Bus.
    //!
    //! Portal calls answer through a `Response` signal on a request object whose path is known
    //! in advance, so the signal is subscribed to before each call.

    use std::collections::HashMap;

    use anyhow::{Context, Result, anyhow, bail};
    use futures_util::StreamExt;
    use tokio::sync::mpsc;
    use tokio::task::JoinHandle;
    use zbus::{Connection, OwnedObjectPath, OwnedVariant, Proxy, Variant};

    use super::{Action, Binding};
    use crate::autostart::APP_ID;

    const PORTAL: &str = "org.freedesktop.portal.Desktop";
    const PORTAL_PATH: &str = "/org/freedesktop/portal/desktop";
    const SHORTCUTS: &str = "org.freedesktop.portal.GlobalShortcuts";

    pub struct Listener {
        task: JoinHandle<()>,
    }

    impl Drop for Listener {
        fn drop(&mut self) {
            // Closing the connection ends the portal session too.
            self.task.abort();
        }
    }

    impl Listener {
        pub fn start(bindings: &[Binding], pressed: mpsc::Sender<Action>) -> Result<Option<Self>> {
            if std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE").is_some() {
                for binding in bindings {
                    tracing::info!(
                        "Hyprland: add to hyprland.conf: bind = {}, {}, exec, {}",
                        binding.hotkey.hyprland_mods(),
                        binding.hotkey.key_name(),
                        binding.action.command()
                    );
                }
                return Ok(None);
            }
            let bindings = bindings.to_vec();
            // Binding waits for the user to confirm on some desktops: don't hold up the daemon.
            let task = tokio::spawn(async move {
                if let Err(e) = listen(&bindings, &pressed).await {
                    tracing::warn!(error = %format!("{e:#}"), "hotkeys not available");
                    super::log_commands(&bindings);
                }
            });
            Ok(Some(Self { task }))
        }
    }

    async fn listen(bindings: &[Binding], pressed: &mpsc::Sender<Action>) -> Result<()> {
        let connection = Connection::session().await.context("no D-Bus session")?;
        register_app(&connection).await;
        let portal = Proxy::new(&connection, PORTAL, PORTAL_PATH, SHORTCUTS)
            .await
            .context("no desktop portal")?;
        // Subscribed before binding, so no press is missed.
        let mut activated = portal.receive_signal("Activated").await?;

        let session_token = token("session");
        let token_a = token("create");
        let options = HashMap::from([
            ("handle_token", Variant::from(token_a.as_str())),
            (
                "session_handle_token",
                Variant::from(session_token.as_str()),
            ),
        ]);
        let results = request(&connection, &token_a, async {
            portal
                .call_method("CreateSession", &(options,))
                .await
                .context("the desktop portal has no global shortcuts")
        })
        .await?;
        let session = results
            .get("session_handle")
            .and_then(|v| match &**v {
                Variant::Str(s) => Some(s.as_str().to_owned()),
                Variant::ObjectPath(p) => Some(p.as_str().to_owned()),
                _ => None,
            })
            .ok_or_else(|| anyhow!("the portal returned no session"))?;
        let session = OwnedObjectPath::try_from(session)?;

        let shortcuts: Vec<(&str, HashMap<&str, Variant<'_>>)> = bindings
            .iter()
            .map(|binding| {
                (
                    binding.action.id(),
                    HashMap::from([
                        ("description", Variant::from(binding.action.description())),
                        (
                            "preferred_trigger",
                            Variant::from(binding.hotkey.portal_trigger()),
                        ),
                    ]),
                )
            })
            .collect();
        let token_b = token("bind");
        let options = HashMap::from([("handle_token", Variant::from(token_b.as_str()))]);
        let results = request(&connection, &token_b, async {
            portal
                .call_method("BindShortcuts", &(&session, shortcuts, "", options))
                .await
                .context("could not bind the shortcuts")
        })
        .await?;
        log_bound(&results);

        while let Some(signal) = activated.next().await {
            let Ok((from, id, _, _)) =
                signal
                    .body()
                    .deserialize::<(OwnedObjectPath, String, u64, HashMap<String, OwnedVariant>)>()
            else {
                continue;
            };
            if from != session {
                continue;
            }
            if let Some(action) = Action::from_id(&id) {
                let _ = pressed.try_send(action);
            }
        }
        bail!("the desktop portal closed the session")
    }

    /// Tells xdg-desktop-portal 1.19+ which app this is: a program started outside Flatpak has
    /// no app id otherwise, and KDE needs one to list its shortcuts. Older portals don't have
    /// this call, which is fine.
    async fn register_app(connection: &Connection) {
        let options: HashMap<&str, Variant<'_>> = HashMap::new();
        let result = connection
            .call_method(
                Some(PORTAL),
                PORTAL_PATH,
                Some("org.freedesktop.host.portal.Registry"),
                "Register",
                &(APP_ID, options),
            )
            .await;
        if let Err(e) = result {
            tracing::debug!(error = %e, "portal app registration skipped");
        }
    }

    /// Makes a portal call and waits for its answer, which arrives as a `Response` signal.
    async fn request(
        connection: &Connection,
        token: &str,
        call: impl Future<Output = Result<zbus::Message>>,
    ) -> Result<HashMap<String, OwnedVariant>> {
        let sender = connection
            .unique_name()
            .ok_or_else(|| anyhow!("no D-Bus name"))?;
        let path = request_path(sender.as_str(), token);
        let proxy = Proxy::new(connection, PORTAL, path, "org.freedesktop.portal.Request").await?;
        let mut responses = proxy.receive_signal("Response").await?;
        call.await?;
        let response = responses
            .next()
            .await
            .ok_or_else(|| anyhow!("the portal did not answer"))?;
        let (code, results) = response
            .body()
            .deserialize::<(u32, HashMap<String, OwnedVariant>)>()?;
        match code {
            0 => Ok(results),
            1 => bail!("cancelled"),
            _ => bail!("the portal refused (code {code})"),
        }
    }

    fn log_bound(results: &HashMap<String, OwnedVariant>) {
        let bound = results
            .get("shortcuts")
            .and_then(|v| Vec::<(String, HashMap<String, OwnedVariant>)>::try_from(v.clone()).ok())
            .unwrap_or_default();
        for (id, details) in bound {
            let trigger = details
                .get("trigger_description")
                .and_then(|v| String::try_from(v.clone()).ok())
                .unwrap_or_default();
            tracing::info!(action = %id, %trigger, "hotkey registered");
        }
    }

    /// `/org/freedesktop/portal/desktop/request/SENDER/TOKEN`, where SENDER is the unique bus
    /// name without the leading `:` and with `.` replaced by `_`.
    pub(super) fn request_path(sender: &str, token: &str) -> String {
        let sender = sender.trim_start_matches(':').replace('.', "_");
        format!("{PORTAL_PATH}/request/{sender}/{token}")
    }

    /// Request and session tokens: unique within this process, and valid object path elements.
    fn token(kind: &str) -> String {
        use std::sync::atomic::{AtomicU32, Ordering};
        static NEXT: AtomicU32 = AtomicU32::new(0);
        format!(
            "pasteget_{kind}_{}_{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        )
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn request_paths_follow_the_portal_rule() {
            assert_eq!(
                request_path(":1.42", "pasteget_bind_7_1"),
                "/org/freedesktop/portal/desktop/request/1_42/pasteget_bind_7_1"
            );
            let a = token("x");
            let b = token("x");
            assert_ne!(a, b);
            assert!(a.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'));
        }
    }
}

#[cfg(target_os = "macos")]
mod platform {
    use anyhow::{Result, bail};
    use tokio::sync::mpsc;

    use super::{Action, Binding};

    pub struct Listener;

    impl Listener {
        pub fn start(_: &[Binding], _: mpsc::Sender<Action>) -> Result<Option<Self>> {
            bail!("hotkeys aren't supported on macOS yet")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> Result<Hotkey> {
        text.parse()
    }

    #[test]
    fn parses_modifiers_and_keys() {
        let hotkey = parse("Ctrl+Alt+Shift+V").unwrap();
        assert!(hotkey.ctrl && hotkey.alt && hotkey.shift && !hotkey.logo);
        assert_eq!(hotkey.key, Key::Letter('V'));

        assert_eq!(
            parse(" control + win + 7 ").unwrap().to_string(),
            "Ctrl+Super+7"
        );
        assert_eq!(parse("meta+p").unwrap().to_string(), "Super+P");
        assert_eq!(parse("F9").unwrap().key, Key::Function(9));
        assert_eq!(parse("shift+f24").unwrap().to_string(), "Shift+F24");
    }

    #[test]
    fn rejects_what_would_get_in_the_way_of_typing() {
        for bad in [
            "",
            "Ctrl",
            "Ctrl+",
            "V",
            "Shift+V",
            "Ctrl+Ctrl+V",
            "Ctrl+V+Alt",
            "Ctrl+VV",
            "Ctrl+F0",
            "Ctrl+F25",
            "Ctrl+F01",
            "Ctrl+é",
            "Ctrl+Space",
            "Ctrl++",
        ] {
            assert!(parse(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn display_parses_back() {
        for text in ["Ctrl+Alt+Shift+P", "Super+F1", "Alt+0"] {
            let hotkey = parse(text).unwrap();
            assert_eq!(hotkey.to_string(), text);
            assert_eq!(parse(&hotkey.to_string()).unwrap(), hotkey);
        }
    }

    #[test]
    fn converts_for_the_portal_and_hyprland() {
        let hotkey = parse("Ctrl+Alt+Shift+V").unwrap();
        assert_eq!(hotkey.portal_trigger(), "CTRL+ALT+SHIFT+v");
        assert_eq!(hotkey.hyprland_mods(), "CTRL ALT SHIFT");
        assert_eq!(hotkey.key_name(), "V");
        assert_eq!(parse("Super+F5").unwrap().portal_trigger(), "LOGO+F5");
    }

    #[test]
    fn settings_turn_hotkeys_on_and_off() {
        let defaults = HotkeySettings::default().bindings();
        assert_eq!(
            defaults.iter().map(|b| b.action).collect::<Vec<_>>(),
            [Action::TogglePause, Action::SendClipboard]
        );

        let settings = HotkeySettings {
            toggle_pause: String::new(),
            send_clipboard: "not a hotkey".into(),
        };
        assert!(settings.bindings().is_empty());
    }

    #[test]
    fn actions_round_trip_through_their_ids() {
        for action in [Action::TogglePause, Action::SendClipboard] {
            assert_eq!(Action::from_id(action.id()), Some(action));
        }
        assert_eq!(Action::from_id("other"), None);
    }
}
