// SPDX-License-Identifier: GPL-3.0-or-later

//! Starting paste-get when the user logs in.
//!
//! - Windows: a value in `HKEY_CURRENT_USER\Software\Microsoft\Windows\CurrentVersion\Run`,
//!   the same list as Task Manager's "Startup apps".
//! - Linux: a desktop entry in `$XDG_CONFIG_HOME/autostart` (`~/.config/autostart`), which KDE
//!   Plasma, GNOME and most desktops start. Hyprland doesn't read it: use `exec-once` in
//!   `hyprland.conf` instead (see `docs/TESTING.md`, section 10).
//!
//! Each profile has its own entry, so `--profile phone` test setups don't replace the real one.

use std::path::PathBuf;

use anyhow::{Result, bail};

use crate::config::DEFAULT_PROFILE;

/// Names the desktop entry, as on Android and in Tauri's configuration.
pub const APP_ID: &str = "io.github.nexoryx.pasteget";

/// What to start at login.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Launch {
    pub program: PathBuf,
    pub args: Vec<String>,
}

impl Launch {
    /// This running program with `--profile <profile>`.
    pub fn this_program(profile: &str) -> Result<Self> {
        // An AppImage runs from a temporary mount: start the AppImage itself.
        #[cfg(all(unix, not(target_os = "macos")))]
        let program = std::env::var_os("APPIMAGE").map(PathBuf::from);
        #[cfg(not(all(unix, not(target_os = "macos"))))]
        let program: Option<PathBuf> = None;
        let program = match program {
            Some(program) => program,
            None => std::env::current_exe()?,
        };
        Ok(Self {
            program,
            args: vec!["--profile".into(), profile.into()],
        })
    }
}

/// The login entry of one profile.
#[derive(Clone, Debug)]
pub struct Autostart {
    #[cfg(all(unix, not(target_os = "macos")))]
    file: PathBuf,
    #[cfg(windows)]
    key: String,
    #[cfg(windows)]
    value: String,
}

/// Whether this OS is supported. macOS isn't yet.
pub const SUPPORTED: bool = cfg!(any(windows, all(unix, not(target_os = "macos"))));

impl Autostart {
    /// The entry for `profile`, in the user's real login list.
    pub fn for_profile(profile: &str) -> Result<Self> {
        #[cfg(all(unix, not(target_os = "macos")))]
        {
            let Some(config) = dirs::config_dir() else {
                bail!("could not find the user's configuration directory");
            };
            Ok(Self::in_dir(config.join("autostart"), profile))
        }
        #[cfg(windows)]
        {
            Ok(Self::in_key(
                r"Software\Microsoft\Windows\CurrentVersion\Run",
                profile,
            ))
        }
        #[cfg(not(any(windows, all(unix, not(target_os = "macos")))))]
        {
            let _ = profile;
            bail!("starting at login isn't supported on this system yet")
        }
    }

    /// The entry for `profile` in another autostart directory. For tests.
    #[cfg(all(unix, not(target_os = "macos")))]
    pub fn in_dir(dir: PathBuf, profile: &str) -> Self {
        let name = if profile == DEFAULT_PROFILE {
            format!("{APP_ID}.desktop")
        } else {
            format!("{APP_ID}-{profile}.desktop")
        };
        Self {
            file: dir.join(name),
        }
    }

    /// The entry for `profile` under another key of `HKEY_CURRENT_USER`. For tests.
    #[cfg(windows)]
    pub fn in_key(key: &str, profile: &str) -> Self {
        let value = if profile == DEFAULT_PROFILE {
            "paste-get".to_owned()
        } else {
            format!("paste-get ({profile})")
        };
        Self {
            key: key.to_owned(),
            value,
        }
    }

    pub fn is_enabled(&self) -> bool {
        #[cfg(all(unix, not(target_os = "macos")))]
        {
            self.file.exists()
        }
        #[cfg(windows)]
        {
            windows_registry::CURRENT_USER
                .open(&self.key)
                .and_then(|key| key.get_string(&self.value))
                .is_ok()
        }
        #[cfg(not(any(windows, all(unix, not(target_os = "macos")))))]
        {
            false
        }
    }

    /// Starts `launch` at every login from now on.
    pub fn enable(&self, launch: &Launch) -> Result<()> {
        #[cfg(all(unix, not(target_os = "macos")))]
        {
            use anyhow::Context;

            let entry = desktop_entry(launch)?;
            if let Some(dir) = self.file.parent() {
                std::fs::create_dir_all(dir)
                    .with_context(|| format!("could not create {}", dir.display()))?;
            }
            std::fs::write(&self.file, entry)
                .with_context(|| format!("could not write {}", self.file.display()))?;
            tracing::info!(file = %self.file.display(), "starting at login");
            Ok(())
        }
        #[cfg(windows)]
        {
            let command = windows_command(launch)?;
            windows_registry::CURRENT_USER
                .create(&self.key)?
                .set_string(&self.value, &command)?;
            tracing::info!(value = %self.value, "starting at login");
            Ok(())
        }
        #[cfg(not(any(windows, all(unix, not(target_os = "macos")))))]
        {
            let _ = launch;
            bail!("starting at login isn't supported on this system yet")
        }
    }

    /// Stops starting at login. Does nothing if it wasn't enabled.
    pub fn disable(&self) -> Result<()> {
        #[cfg(all(unix, not(target_os = "macos")))]
        {
            match std::fs::remove_file(&self.file) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.into()),
                _ => {
                    tracing::info!("not starting at login");
                    Ok(())
                }
            }
        }
        #[cfg(windows)]
        {
            let Ok(key) = windows_registry::CURRENT_USER
                .options()
                .read()
                .write()
                .open(&self.key)
            else {
                return Ok(());
            };
            if key.get_string(&self.value).is_ok() {
                key.remove_value(&self.value)?;
            }
            tracing::info!("not starting at login");
            Ok(())
        }
        #[cfg(not(any(windows, all(unix, not(target_os = "macos")))))]
        {
            Ok(())
        }
    }
}

/// A desktop entry that runs `launch`, per the Desktop Entry Specification.
#[cfg_attr(not(all(unix, not(target_os = "macos"))), allow(dead_code))]
fn desktop_entry(launch: &Launch) -> Result<String> {
    let Some(program) = launch.program.to_str() else {
        bail!("the program's path is not valid UTF-8");
    };
    let exec = std::iter::once(program)
        .chain(launch.args.iter().map(String::as_str))
        .map(exec_argument)
        .collect::<Result<Vec<_>>>()?
        .join(" ");
    Ok(format!(
        "[Desktop Entry]\n\
         Type=Application\n\
         Name=paste-get\n\
         Comment=Copy on your phone, paste on your PC\n\
         Exec={exec}\n\
         Terminal=false\n\
         X-GNOME-Autostart-enabled=true\n\
         X-KDE-autostart-phase=2\n"
    ))
}

/// One argument of an `Exec` key: always quoted. Inside quotes, `"`, `` ` ``, `$` and `\` take a
/// backslash; then the string escapes double every backslash. `%` starts field codes, so it is
/// written `%%`.
fn exec_argument(arg: &str) -> Result<String> {
    if arg.chars().any(char::is_control) {
        bail!("a path or argument contains a control character");
    }
    let mut out = String::with_capacity(arg.len() + 2);
    out.push('"');
    for c in arg.chars() {
        match c {
            '"' | '`' | '$' => {
                out.push_str("\\\\");
                out.push(c);
            }
            '\\' => out.push_str("\\\\\\\\"),
            '%' => out.push_str("%%"),
            _ => out.push(c),
        }
    }
    out.push('"');
    Ok(out)
}

/// The command line stored in the `Run` key. Arguments are profile names and flags, which
/// never need quoting; the program's path may have spaces, so it is quoted.
#[cfg_attr(not(windows), allow(dead_code))]
fn windows_command(launch: &Launch) -> Result<String> {
    let program = launch.program.to_string_lossy();
    if program.contains('"') {
        bail!("the program's path contains a quote");
    }
    let mut command = format!("\"{program}\"");
    for arg in &launch.args {
        if arg.is_empty() || arg.contains(|c: char| c.is_whitespace() || c == '"') {
            bail!("unexpected argument for the login command: {arg:?}");
        }
        command.push(' ');
        command.push_str(arg);
    }
    Ok(command)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn launch(program: &str) -> Launch {
        Launch {
            program: PathBuf::from(program),
            args: vec!["--profile".into(), "default".into()],
        }
    }

    #[test]
    fn desktop_entries_quote_the_command() {
        let entry = desktop_entry(&launch("/opt/paste get/pasteget-tray")).unwrap();

        assert!(entry.starts_with("[Desktop Entry]\nType=Application\n"));
        assert!(
            entry.contains("\nExec=\"/opt/paste get/pasteget-tray\" \"--profile\" \"default\"\n")
        );
    }

    #[test]
    fn exec_arguments_escape_reserved_characters() {
        assert_eq!(exec_argument("plain").unwrap(), r#""plain""#);
        assert_eq!(exec_argument(r#"a"b"#).unwrap(), r#""a\\"b""#);
        assert_eq!(exec_argument("$HOME").unwrap(), r#""\\$HOME""#);
        assert_eq!(exec_argument(r"back\slash").unwrap(), r#""back\\\\slash""#);
        assert_eq!(exec_argument("100%").unwrap(), r#""100%%""#);
        assert!(exec_argument("new\nline").is_err());
    }

    #[test]
    fn windows_commands_quote_the_program() {
        assert_eq!(
            windows_command(&launch(r"C:\Program Files\paste-get\pasteget-tray.exe")).unwrap(),
            r#""C:\Program Files\paste-get\pasteget-tray.exe" --profile default"#
        );
        assert!(windows_command(&launch(r#"C:\a"b.exe"#)).is_err());
    }

    #[test]
    fn launches_this_program_with_its_profile() {
        let launch = Launch::this_program("phone").unwrap();

        assert_eq!(launch.args, ["--profile", "phone"]);
        assert!(launch.program.is_absolute());
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    #[test]
    fn enable_and_disable_write_and_remove_the_entry() {
        let dir = tempfile::tempdir().unwrap();
        let autostart = Autostart::in_dir(dir.path().join("autostart"), DEFAULT_PROFILE);
        let other = Autostart::in_dir(dir.path().join("autostart"), "phone");

        assert!(!autostart.is_enabled());
        autostart.enable(&launch("/usr/bin/pasteget-tray")).unwrap();
        assert!(autostart.is_enabled());
        assert!(!other.is_enabled());
        let file = dir
            .path()
            .join("autostart")
            .join(format!("{APP_ID}.desktop"));
        assert!(
            std::fs::read_to_string(file)
                .unwrap()
                .contains("Exec=\"/usr/bin/pasteget-tray\"")
        );

        autostart.disable().unwrap();
        assert!(!autostart.is_enabled());
        // Disabling twice is fine.
        autostart.disable().unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn enable_and_disable_write_and_remove_the_value() {
        let key = format!(r"Software\paste-get-tests\{}\Run", std::process::id());
        let autostart = Autostart::in_key(&key, DEFAULT_PROFILE);
        let other = Autostart::in_key(&key, "phone");

        assert!(!autostart.is_enabled());
        autostart
            .enable(&launch(r"C:\pg\pasteget-tray.exe"))
            .unwrap();
        assert!(autostart.is_enabled());
        assert!(!other.is_enabled());
        let stored = windows_registry::CURRENT_USER
            .open(&key)
            .unwrap()
            .get_string("paste-get")
            .unwrap();
        assert_eq!(stored, r#""C:\pg\pasteget-tray.exe" --profile default"#);

        autostart.disable().unwrap();
        assert!(!autostart.is_enabled());
        autostart.disable().unwrap();
        let _ = windows_registry::CURRENT_USER.remove_tree(r"Software\paste-get-tests");
    }
}
