// SPDX-License-Identifier: GPL-3.0-or-later

//! Where the daemon keeps its state: one directory per profile.
//!
//! ```text
//! <config dir>/paste-get/<profile>/
//!   identity.key    Ed25519 private key (PKCS#8 DER). Readable by the user only.
//!   settings.toml   Device name, port, notifications, links, hotkeys, history. Created with
//!                   defaults, safe to edit.
//!   devices.toml    Paired devices. Moved to devices.toml.bad if it can't be read, copied
//!                   there if some entries can't.
//! <data dir>/paste-get/<profile>/
//!   history/        The clipboard history, when it is on (`core::history`).
//! ```
//!
//! `<config dir>` is `%APPDATA%` on Windows and `$XDG_CONFIG_HOME` (`~/.config`) on Linux.
//! `<data dir>` is `%LOCALAPPDATA%` on Windows, which doesn't roam to other PCs, and
//! `$XDG_DATA_HOME` (`~/.local/share`) on Linux, which dotfile repositories leave out.
//! `PASTEGET_HOME` replaces both `<config dir>/paste-get` and `<data dir>/paste-get`. Profiles let
//! several daemons run on one machine, which is how the daemon is tested without a phone.

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
    /// Where its larger data goes: the history.
    data_dir: PathBuf,
}

impl Profile {
    /// Opens (creating if needed) the profile `name` in the default location.
    pub fn open(name: &str) -> Result<Self> {
        if let Some(home) = std::env::var_os("PASTEGET_HOME") {
            return Self::open_in(Path::new(&home), name);
        }
        let config = dirs::config_dir()
            .context("could not find the user's configuration directory")?
            .join("paste-get");
        let data = dirs::data_local_dir()
            .context("could not find the user's data directory")?
            .join("paste-get");
        let mut profile = Self::open_in(&config, name)?;
        profile.data_dir = data.join(name);
        Ok(profile)
    }

    /// Opens (creating if needed) the profile `name` under `base`, which also holds its data.
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
            data_dir: dir.clone(),
            dir,
        })
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// The folder of the clipboard history, created when the history opens.
    pub fn history_dir(&self) -> PathBuf {
        self.data_dir.join("history")
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

    /// Loads the paired devices, see [`Profile::load_known_devices`].
    pub fn known_devices(&self) -> Result<KnownDevices> {
        Ok(self.load_known_devices()?.devices)
    }

    /// Loads the paired devices, so that the daemon still starts when some can't be read.
    ///
    /// A file that doesn't read as paired devices (a write cut short, a bad hand edit, a newer
    /// format) is moved aside to `devices.toml.bad` and none are loaded. An entry that doesn't
    /// read (an invalid device ID, a missing name) is left out, after the whole file was copied
    /// to `devices.toml.bad`. Either way the result says so ([`LoadedDevices::set_aside`]).
    pub fn load_known_devices(&self) -> Result<LoadedDevices> {
        let path = self.dir.join(DEVICES_FILE);
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(LoadedDevices::default());
            }
            Err(e) => return Err(e).with_context(|| format!("could not read {}", path.display())),
        };
        let aside = self.dir.join(format!("{DEVICES_FILE}.bad"));
        match parse_known_devices(&bytes) {
            Ok((devices, 0)) => Ok(LoadedDevices {
                devices,
                set_aside: false,
            }),
            Ok((devices, left_out)) => {
                // How many only: the file's contents stay out of the log.
                tracing::warn!(
                    left_out,
                    copied_to = %aside.display(),
                    "some paired devices could not be read: starting without them"
                );
                if let Err(e) = fs::copy(&path, &aside) {
                    tracing::warn!(error = %e, "could not copy the file aside");
                }
                Ok(LoadedDevices {
                    devices,
                    set_aside: true,
                })
            }
            Err(reason) => {
                // The reason only, likewise.
                tracing::warn!(
                    reason,
                    len = bytes.len(),
                    moved_to = %aside.display(),
                    "the paired devices could not be read: starting with none"
                );
                if let Err(e) = fs::rename(&path, &aside) {
                    tracing::warn!(error = %e, "could not move the unreadable file aside");
                }
                Ok(LoadedDevices {
                    devices: KnownDevices::default(),
                    set_aside: true,
                })
            }
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
    /// Announce the computer's name in mDNS TXT records. When false, the name is
    /// omitted for privacy on shared/public networks.
    #[serde(default = "default_announce_name")]
    pub announce_name: bool,
    /// Global hotkeys. Tables stay last: TOML puts them after plain keys.
    pub hotkeys: HotkeySettings,
    /// The clipboard history, `[history]`.
    pub history: HistorySettings,
}

/// `[history]` in `settings.toml`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct HistorySettings {
    /// Keep a history of what is copied here, sent and received. Off unless turned on.
    /// Passwords and codes are never kept.
    pub enabled: bool,
    /// Items not pinned are deleted this many days after they were last copied; 0 keeps them.
    pub keep_for_days: u32,
    /// Keep numbers of 4 to 8 digits alone (often codes) for 15 minutes; `false` never keeps
    /// them.
    pub keep_bare_codes: bool,
}

impl Default for HistorySettings {
    fn default() -> Self {
        Self {
            enabled: false,
            keep_for_days: 90,
            keep_bare_codes: true,
        }
    }
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

fn default_announce_name() -> bool {
    true
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
            announce_name: true,
            hotkeys: HotkeySettings::default(),
            history: HistorySettings::default(),
        }
    }
}

/// Addresses kept per device this one dials.
pub const MAX_ADDRESSES: usize = 8;

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

    /// Gives a device the name it now goes by, in place. Returns whether it changed.
    pub fn rename(&mut self, id: &DeviceId, name: &str) -> bool {
        let name = sanitize_device_name(name);
        match self.0.iter_mut().find(|device| device.id == *id) {
            Some(device) if device.name != name => {
                device.name = name;
                true
            }
            _ => false,
        }
    }

    /// A device this one dials answered at `address`, which proved its identity: it is dialed
    /// there first from now on, and kept if it was new (the device moved). At most
    /// [`MAX_ADDRESSES`] are kept. Returns whether anything changed.
    pub fn reached(&mut self, id: &DeviceId, address: SocketAddr) -> bool {
        let Some(device) = self.0.iter_mut().find(|device| device.id == *id) else {
            return false;
        };
        if device.addresses.first() == Some(&address) {
            return false;
        }
        device.addresses.retain(|known| *known != address);
        device.addresses.insert(0, address);
        device.addresses.truncate(MAX_ADDRESSES);
        true
    }

    /// Removes a device. Returns it, if it was there.
    pub fn remove(&mut self, id: &DeviceId) -> Option<KnownDevice> {
        let index = self.0.iter().position(|device| device.id == *id)?;
        Some(self.0.remove(index))
    }
}

/// What [`Profile::load_known_devices`] read.
#[derive(Debug, Default)]
pub struct LoadedDevices {
    pub devices: KnownDevices,
    /// Some paired devices couldn't be read, and were set aside in `devices.toml.bad`: a device
    /// missing from `devices` may still be paired, and must not be told it isn't.
    pub set_aside: bool,
}

#[derive(Serialize, Deserialize, Default)]
struct DevicesFile {
    #[serde(default)]
    device: Vec<StoredDevice>,
}

/// `devices.toml` before each entry is checked, so that one bad entry doesn't take the others
/// with it.
#[derive(Deserialize)]
struct UncheckedDevicesFile {
    #[serde(default)]
    device: Vec<toml::Value>,
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

/// The paired devices in `devices.toml`, and how many entries were left out because they don't
/// read as one; or why it isn't such a file.
fn parse_known_devices(bytes: &[u8]) -> Result<(KnownDevices, usize), &'static str> {
    let text = std::str::from_utf8(bytes).map_err(|_| "not UTF-8")?;
    let file: UncheckedDevicesFile =
        toml::from_str(text).map_err(|_| "not a list of paired devices in TOML")?;
    let entries = file.device.len();
    let devices: Vec<KnownDevice> = file
        .device
        .into_iter()
        .filter_map(|entry| entry.try_into::<StoredDevice>().ok())
        .filter_map(|stored| KnownDevice::try_from(stored).ok())
        .collect();
    let left_out = entries - devices.len();
    Ok((KnownDevices(devices), left_out))
}

#[cfg(windows)]
fn restrict_windows_permissions(path: &Path, is_dir: bool) {
    if let Some(username) = std::env::var_os("USERNAME") {
        let perm = if is_dir {
            format!("{}:(OI)(CI)F", username.to_string_lossy())
        } else {
            format!("{}:F", username.to_string_lossy())
        };
        let _ = std::process::Command::new("icacls")
            .arg(path)
            .arg("/inheritance:r")
            .arg("/grant:r")
            .arg(perm)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
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
    #[cfg(windows)]
    {
        restrict_windows_permissions(dir, true);
    }
    Ok(())
}

/// Writes `contents` atomically, readable and writable by the current user only.
///
/// On Windows, files under `%APPDATA%` already inherit a per-user ACL, and explicit
/// DACL restrictions are applied to protect custom PASTEGET_HOME locations.
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
    fs::rename(&temp, path).with_context(|| format!("could not write {}", path.display()))?;
    #[cfg(windows)]
    {
        restrict_windows_permissions(path, false);
    }
    Ok(())
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
    fn the_address_that_worked_goes_first() {
        let address = |s: &str| s.parse::<SocketAddr>().unwrap();
        let joined = KnownDevice {
            id: Identity::generate().unwrap().device_id(),
            name: "pc".into(),
            addresses: vec![address("192.168.1.20:47290"), address("10.0.0.2:47290")],
        };
        let mut known = KnownDevices::default();
        known.insert(joined.clone());

        assert!(known.reached(&joined.id, address("10.0.0.2:47290")));
        assert!(!known.reached(&joined.id, address("10.0.0.2:47290")));
        assert!(known.reached(&joined.id, address("192.168.1.31:47290")));
        assert_eq!(
            known.get(&joined.id).unwrap().addresses,
            [
                address("192.168.1.31:47290"),
                address("10.0.0.2:47290"),
                address("192.168.1.20:47290")
            ]
        );
        // At most MAX_ADDRESSES, the oldest dropped.
        for last in 1..=MAX_ADDRESSES {
            known.reached(&joined.id, address(&format!("10.0.1.{last}:47290")));
        }
        let addresses = &known.get(&joined.id).unwrap().addresses;
        assert_eq!(addresses.len(), MAX_ADDRESSES);
        assert_eq!(
            addresses[0],
            address(&format!("10.0.1.{MAX_ADDRESSES}:47290"))
        );
        // A device it doesn't know.
        let stranger = Identity::generate().unwrap().device_id();
        assert!(!known.reached(&stranger, address("10.0.0.2:47290")));
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

        // The history is off until turned on.
        assert!(!defaults.history.enabled);
        fs::write(
            profile.dir().join(SETTINGS_FILE),
            "[history]\nenabled = true\nkeep_for_days = 0\n",
        )
        .unwrap();
        let history = profile.settings().unwrap().history;
        assert!(history.enabled);
        assert_eq!(history.keep_for_days, 0);
        assert!(history.keep_bare_codes);
    }

    /// With `PASTEGET_HOME` (tests, two profiles side by side), the history goes there too.
    #[test]
    fn the_history_is_in_the_profile_s_data_folder() {
        let (base, profile) = profile();

        assert_eq!(
            profile.history_dir(),
            base.path().join("test").join("history")
        );
    }

    #[test]
    fn default_settings_file_is_valid_toml_with_the_hotkeys_table() {
        let (_base, profile) = profile();
        profile.settings().unwrap();

        let text = fs::read_to_string(profile.dir().join(SETTINGS_FILE)).unwrap();
        assert!(text.contains("[hotkeys]"));
        assert!(text.contains("[history]"));
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
    fn an_unreadable_devices_file_is_moved_aside() {
        let (_base, profile) = profile();
        let path = profile.dir().join(DEVICES_FILE);
        let bad = profile.dir().join("devices.toml.bad");

        for contents in [
            &b"[[device]\nid = "[..],
            &[0xff, 0xfe],
            b"device = \"not a list\"\n",
        ] {
            fs::write(&path, contents).unwrap();

            let loaded = profile.load_known_devices().unwrap();
            assert_eq!(loaded.devices, KnownDevices::default());
            assert!(loaded.set_aside);
            assert!(!path.exists());
            assert_eq!(fs::read(&bad).unwrap(), contents);
        }

        // New pairings are saved as usual.
        let mut devices = KnownDevices::default();
        devices.insert(KnownDevice {
            id: Identity::generate().unwrap().device_id(),
            name: "POCO".into(),
            addresses: vec![],
        });
        profile.save_known_devices(&devices).unwrap();
        assert_eq!(profile.known_devices().unwrap(), devices);
    }

    #[test]
    fn only_the_entries_that_dont_read_are_left_out() {
        let (_base, profile) = profile();
        let path = profile.dir().join(DEVICES_FILE);
        let phone = Identity::generate().unwrap().device_id();
        let contents = format!(
            "[[device]]\nid = \"not an ID\"\nname = \"Old phone\"\n\n\
             [[device]]\nid = \"{phone}\"\nname = \"POCO\"\n\n\
             [[device]]\nid = \"{phone}\"\n"
        );
        fs::write(&path, &contents).unwrap();

        let loaded = profile.load_known_devices().unwrap();

        assert!(loaded.set_aside);
        assert_eq!(loaded.devices.iter().count(), 1);
        assert_eq!(loaded.devices.get(&phone).unwrap().name, "POCO");
        // The whole file is kept aside, and in place until it's saved again.
        let bad = profile.dir().join("devices.toml.bad");
        assert_eq!(fs::read_to_string(&bad).unwrap(), contents);
        assert_eq!(fs::read_to_string(&path).unwrap(), contents);
        // A file that reads in full is not set aside.
        profile.save_known_devices(&loaded.devices).unwrap();
        assert!(!profile.load_known_devices().unwrap().set_aside);
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
    fn renaming_keeps_the_order_and_cleans_the_name() {
        let (first, second) = (
            Identity::generate().unwrap().device_id(),
            Identity::generate().unwrap().device_id(),
        );
        let mut devices = KnownDevices::default();
        for (id, name) in [(first, "First"), (second, "Second")] {
            devices.insert(KnownDevice {
                id,
                name: name.into(),
                addresses: vec![],
            });
        }

        assert!(devices.rename(&first, "  Renamed\n"));
        assert!(!devices.rename(&first, "Renamed"));
        assert!(!devices.rename(&Identity::generate().unwrap().device_id(), "Nobody"));
        let names: Vec<_> = devices.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(names, ["Renamed", "Second"]);
    }

    #[test]
    fn rejects_unsafe_profile_names() {
        let base = tempfile::tempdir().unwrap();

        for name in ["", "../escape", "a/b", &"x".repeat(33)] {
            assert!(Profile::open_in(base.path(), name).is_err(), "{name}");
        }
    }
}
