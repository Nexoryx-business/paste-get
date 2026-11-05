// SPDX-License-Identifier: GPL-3.0-or-later

//! Where the daemon keeps its state: one directory per profile.
//!
//! ```text
//! <config dir>/paste-get/<profile>/
//!   identity.key    Ed25519 private key (PKCS#8 DER). Readable by the user only.
//!   settings.toml   Device name, port, notifications, links, hotkeys. Created with defaults, safe
//!                   to edit.
//!   devices.toml    Paired devices.
//! ```
//!
//! `<config dir>` is `%APPDATA%` on Windows and `$XDG_CONFIG_HOME` (`~/.config`) on Linux.
//! `PASTEGET_HOME` replaces `<config dir>/paste-get`. Profiles let several daemons run on one
//! machine, which is how the daemon is tested without a phone.

use std::fs;
use std::io::Write;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use pasteget_core::identity::{DeviceId, Identity};
use pasteget_core::pairing::{PairedDevice, sanitize_device_name};

/// Longest device name, in characters: names from other devices are cut there too.
pub use pasteget_core::pairing::MAX_DEVICE_NAME_CHARS;
use serde::{Deserialize, Serialize};

use crate::hotkeys::HotkeySettings;

pub const DEFAULT_PROFILE: &str = "default";
pub const DEFAULT_PORT: u16 = 47290;

const IDENTITY_FILE: &str = "identity.key";
const SETTINGS_FILE: &str = "settings.toml";
const DEVICES_FILE: &str = "devices.toml";

/// A profile's directory.
#[derive(Clone, Debug)]
pub struct Profile {
    name: String,
    dir: PathBuf,
}

impl Profile {
    /// Opens (creating if needed) the profile `name` in the default location.
    pub fn open(name: &str) -> Result<Self> {
        let base = match std::env::var_os("PASTEGET_HOME") {
            Some(home) => PathBuf::from(home),
            None => dirs::config_dir()
                .context("could not find the user's configuration directory")?
                .join("paste-get"),
        };
        Self::open_in(&base, name)
    }

    /// Opens (creating if needed) the profile `name` under `base`.
    pub fn open_in(base: &Path, name: &str) -> Result<Self> {
        let valid = !name.is_empty()
            && name.len() <= 32
            && name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
        if !valid {
            bail!("profile names use 1 to 32 letters, digits, '-' or '_'");
        }
        let dir = base.join(name);
        create_private_dir(&dir)?;
        Ok(Self {
            name: name.to_owned(),
            dir,
        })
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Loads this device's identity, creating it on first use.
    pub fn identity(&self) -> Result<Identity> {
        let path = self.dir.join(IDENTITY_FILE);
        match fs::read(&path) {
            Ok(der) => Identity::from_pkcs8_der(&der)
                .with_context(|| format!("{} is not a valid key", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let identity = Identity::generate()?;
                write_private_file(&path, identity.pkcs8_der())?;
                Ok(identity)
            }
            Err(e) => Err(e).with_context(|| format!("could not read {}", path.display())),
        }
    }

    /// Loads the settings, writing the defaults on first use.
    pub fn settings(&self) -> Result<Settings> {
        let path = self.dir.join(SETTINGS_FILE);
        match fs::read_to_string(&path) {
            Ok(text) => {
                let mut settings: Settings = toml::from_str(&text)
                    .with_context(|| format!("{} is not valid", path.display()))?;
                settings.device_name = sanitize_device_name(&settings.device_name);
                Ok(settings)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let settings = Settings::default();
                write_private_file(&path, toml::to_string(&settings)?.as_bytes())?;
                Ok(settings)
            }
            Err(e) => Err(e).with_context(|| format!("could not read {}", path.display())),
        }
    }

    /// Replaces `settings.toml` (a hand-written file loses its comments). The device name is
    /// cleaned up as when it is read.
    pub fn save_settings(&self, settings: &Settings) -> Result<()> {
        let settings = Settings {
            device_name: sanitize_device_name(&settings.device_name),
            ..settings.clone()
        };
        write_private_file(
            &self.dir.join(SETTINGS_FILE),
            toml::to_string(&settings)?.as_bytes(),
        )
    }

    pub fn known_devices(&self) -> Result<KnownDevices> {
        let path = self.dir.join(DEVICES_FILE);
        match fs::read_to_string(&path) {
            Ok(text) => {
                let file: DevicesFile = toml::from_str(&text)
                    .with_context(|| format!("{} is not valid", path.display()))?;
                file.device
                    .into_iter()
                    .map(KnownDevice::try_from)
                    .collect::<Result<Vec<_>>>()
                    .map(KnownDevices)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(KnownDevices::default()),
            Err(e) => Err(e).with_context(|| format!("could not read {}", path.display())),
        }
    }

    pub fn save_known_devices(&self, devices: &KnownDevices) -> Result<()> {
        let file = DevicesFile {
            device: devices.0.iter().map(StoredDevice::from).collect(),
        };
        write_private_file(
            &self.dir.join(DEVICES_FILE),
            toml::to_string(&file)?.as_bytes(),
        )
    }
}

/// User-editable settings.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Shown on other devices.
    pub device_name: String,
    /// TCP port the daemon listens on.
    pub port: u16,
    /// Which received clips show a notification.
    pub notify: NotifyFor,
    /// A link copied on another device shows a notification that opens it when clicked.
    pub offer_to_open_links: bool,
    /// Global hotkeys. Must stay last: TOML puts tables after plain keys.
    pub hotkeys: HotkeySettings,
}

/// `notify = "images"` in `settings.toml`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NotifyFor {
    Off,
    /// Images only: text is copied too often for a notification each time.
    #[default]
    Images,
    All,
}

impl Default for Settings {
    fn default() -> Self {
        let host = gethostname::gethostname().to_string_lossy().into_owned();
        let os = match std::env::consts::OS {
            "windows" => "Windows",
            "linux" => "Linux",
            "macos" => "macOS",
            other => other,
        };
        Self {
            // The OS tells apart the two sides of a dual-boot machine.
            device_name: sanitize_device_name(&format!("{host} ({os})")),
            port: DEFAULT_PORT,
            notify: NotifyFor::default(),
            offer_to_open_links: true,
            hotkeys: HotkeySettings::default(),
        }
    }
}

/// A paired device.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KnownDevice {
    pub id: DeviceId,
    pub name: String,
    /// Where to reach it, for devices this one paired with by scanning their code.
    /// Empty for devices that connect to us (phones).
    pub addresses: Vec<SocketAddr>,
}

impl KnownDevice {
    pub fn accepted(paired: PairedDevice) -> Self {
        Self {
            id: paired.device_id,
            name: paired.device_name,
            addresses: Vec::new(),
        }
    }

    pub fn joined(paired: PairedDevice, addresses: Vec<SocketAddr>) -> Self {
        Self {
            id: paired.device_id,
            name: paired.device_name,
            addresses,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct KnownDevices(Vec<KnownDevice>);

impl KnownDevices {
    pub fn contains(&self, id: &DeviceId) -> bool {
        self.0.iter().any(|device| device.id == *id)
    }

    pub fn get(&self, id: &DeviceId) -> Option<&KnownDevice> {
        self.0.iter().find(|device| device.id == *id)
    }

    /// Adds a device, replacing any previous entry with the same ID.
    pub fn insert(&mut self, device: KnownDevice) {
        self.0.retain(|known| known.id != device.id);
        self.0.push(device);
    }

    pub fn iter(&self) -> impl Iterator<Item = &KnownDevice> {
        self.0.iter()
    }

    /// Removes a device. Returns it, if it was there.
    pub fn remove(&mut self, id: &DeviceId) -> Option<KnownDevice> {
        let index = self.0.iter().position(|device| device.id == *id)?;
        Some(self.0.remove(index))
    }
}

#[derive(Serialize, Deserialize, Default)]
struct DevicesFile {
    #[serde(default)]
    device: Vec<StoredDevice>,
}

#[derive(Serialize, Deserialize)]
struct StoredDevice {
    id: String,
    name: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    addresses: Vec<SocketAddr>,
}

impl From<&KnownDevice> for StoredDevice {
    fn from(device: &KnownDevice) -> Self {
        Self {
            id: device.id.to_string(),
            name: device.name.clone(),
            addresses: device.addresses.clone(),
        }
    }
}

impl TryFrom<StoredDevice> for KnownDevice {
    type Error = anyhow::Error;

    fn try_from(stored: StoredDevice) -> Result<Self> {
        Ok(Self {
            id: stored
                .id
                .parse()
                .with_context(|| format!("invalid device ID {:?}", stored.id))?,
            name: sanitize_device_name(&stored.name),
            addresses: stored.addresses,
        })
    }
}

fn create_private_dir(dir: &Path) -> Result<()> {
    fs::create_dir_all(dir).with_context(|| format!("could not create {}", dir.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))
            .with_context(|| format!("could not restrict access to {}", dir.display()))?;
    }
    Ok(())
}

/// Writes `contents` atomically, readable and writable by the current user only.
///
/// On Windows, files under `%APPDATA%` already inherit a per-user ACL.
fn write_private_file(path: &Path, contents: &[u8]) -> Result<()> {
    let temp = path.with_extension("tmp");
    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(&temp)
        .with_context(|| format!("could not write {}", temp.display()))?;
    file.write_all(contents)?;
    file.sync_all()?;
    drop(file);
    fs::rename(&temp, path).with_context(|| format!("could not write {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile() -> (tempfile::TempDir, Profile) {
        let base = tempfile::tempdir().unwrap();
        let profile = Profile::open_in(base.path(), "test").unwrap();
        (base, profile)
    }

    #[test]
    fn identity_is_created_once_then_reused() {
        let (_base, profile) = profile();

        let first = profile.identity().unwrap();
        let second = profile.identity().unwrap();

        assert_eq!(first.device_id(), second.device_id());
    }

    #[cfg(unix)]
    #[test]
    fn key_and_profile_are_private() {
        use std::os::unix::fs::PermissionsExt;
        let (_base, profile) = profile();
        profile.identity().unwrap();

        let mode = |path: &Path| fs::metadata(path).unwrap().permissions().mode() & 0o777;

        assert_eq!(mode(&profile.dir().join(IDENTITY_FILE)), 0o600);
        assert_eq!(mode(profile.dir()), 0o700);
    }

    #[test]
    fn settings_default_then_respect_edits() {
        let (_base, profile) = profile();

        let defaults = profile.settings().unwrap();
        assert_eq!(defaults.port, DEFAULT_PORT);
        assert!(defaults.device_name.contains('('));

        fs::write(
            profile.dir().join(SETTINGS_FILE),
            "device_name = \"Mon PC\"\nport = 5000\n",
        )
        .unwrap();
        let edited = profile.settings().unwrap();
        assert_eq!(edited.device_name, "Mon PC");
        assert_eq!(edited.port, 5000);
        // Settings files from before a setting existed get its default.
        assert_eq!(edited.notify, NotifyFor::Images);

        fs::write(profile.dir().join(SETTINGS_FILE), "notify = \"all\"\n").unwrap();
        assert_eq!(profile.settings().unwrap().notify, NotifyFor::All);
        assert_eq!(edited.hotkeys, HotkeySettings::default());

        fs::write(
            profile.dir().join(SETTINGS_FILE),
            "[hotkeys]\ntoggle_pause = \"\"\n",
        )
        .unwrap();
        let hotkeys = profile.settings().unwrap().hotkeys;
        assert_eq!(hotkeys.toggle_pause, "");
        assert_eq!(
            hotkeys.send_clipboard,
            HotkeySettings::default().send_clipboard
        );
    }

    #[test]
    fn default_settings_file_is_valid_toml_with_the_hotkeys_table() {
        let (_base, profile) = profile();
        profile.settings().unwrap();

        let text = fs::read_to_string(profile.dir().join(SETTINGS_FILE)).unwrap();
        assert!(text.contains("[hotkeys]"));
        assert_eq!(profile.settings().unwrap(), Settings::default());
    }

    #[test]
    fn known_devices_round_trip() {
        let (_base, profile) = profile();
        let phone = Identity::generate().unwrap().device_id();
        let pc = Identity::generate().unwrap().device_id();
        let mut devices = profile.known_devices().unwrap();
        devices.insert(KnownDevice {
            id: phone,
            name: "POCO M8 5G".into(),
            addresses: vec![],
        });
        devices.insert(KnownDevice {
            id: pc,
            name: "Other PC".into(),
            addresses: vec!["192.168.1.30:47290".parse().unwrap()],
        });

        profile.save_known_devices(&devices).unwrap();
        let loaded = profile.known_devices().unwrap();

        assert_eq!(loaded, devices);
        assert!(loaded.contains(&phone));
        assert_eq!(loaded.get(&pc).unwrap().addresses.len(), 1);
    }

    #[test]
    fn re_pairing_replaces_the_old_entry() {
        let id = Identity::generate().unwrap().device_id();
        let mut devices = KnownDevices::default();

        for name in ["Old name", "New name"] {
            devices.insert(KnownDevice {
                id,
                name: name.into(),
                addresses: vec![],
            });
        }

        assert_eq!(devices.iter().count(), 1);
        assert_eq!(devices.get(&id).unwrap().name, "New name");
    }

    #[test]
    fn rejects_unsafe_profile_names() {
        let base = tempfile::tempdir().unwrap();

        for name in ["", "../escape", "a/b", &"x".repeat(33)] {
            assert!(Profile::open_in(base.path(), name).is_err(), "{name}");
        }
    }
}
