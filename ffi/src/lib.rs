// SPDX-License-Identifier: GPL-3.0-or-later

//! The phone side of paste-get, exposed to the Android app through UniFFI.
//!
//! [`PhoneClient`] keeps this phone's identity and the computers it paired with, keeps a
//! session open with each of them (the same `core::session` engine as the desktop), and reports
//! connections and received clips through [`PhoneListener`].
//!
//! The app finds computers on the local network itself (Android's `NsdManager`) and passes what
//! it finds to [`PhoneClient::computer_found`].
//!
//! Methods that touch the network block: call them off the main thread.

// UniFFI's generated scaffolding is `extern "C"` glue that needs `unsafe`. Our own code here
// uses none.
#![allow(unsafe_code)]

use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use pasteget_core::clip::{Clip, mime};
use pasteget_core::echo::EchoGuard;
use pasteget_core::identity::{DeviceId, Identity};
use pasteget_core::pairing::{self, PairingInvite, sanitize_device_name};
use pasteget_core::proto::{AckStatus, Hello, PROTOCOL_VERSION, Platform};
use pasteget_core::session::{self, Delivery, HANDSHAKE_TIMEOUT, Peers, SessionEvents};
use serde::{Deserialize, Serialize};
use tokio::net::TcpStream;
use tokio::runtime::Runtime;
use tokio::sync::watch;
use tokio::task::JoinHandle;

uniffi::setup_scaffolding!();

#[cfg(not(windows))]
pub mod wireless_adb;

const IDENTITY_FILE: &str = "identity.key";
const COMPUTERS_FILE: &str = "computers.json";
/// Addresses remembered per computer, and found addresses tried per announcement.
const MAX_ADDRESSES: usize = 8;

#[derive(Debug, thiserror::Error, uniffi::Error)]
#[uniffi(flat_error)]
pub enum PastegetError {
    #[error("this is not a paste-get pairing code")]
    InvalidCode,
    #[error("could not reach the computer: {0}")]
    Unreachable(String),
    #[error("pairing failed: {0}")]
    PairingFailed(String),
    #[error("not a PNG or JPEG image")]
    UnsupportedImage,
    #[error("unknown device")]
    UnknownDevice,
    #[error("storage error: {0}")]
    Storage(String),
}

impl From<std::io::Error> for PastegetError {
    fn from(e: std::io::Error) -> Self {
        Self::Storage(e.to_string())
    }
}

/// A computer this phone paired with.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, uniffi::Record)]
pub struct PairedComputer {
    pub device_id: String,
    pub name: String,
    /// Socket addresses, e.g. `192.168.1.20:47290`: from the pairing code, then the last one
    /// that worked moves to the front.
    pub addresses: Vec<String>,
}

/// A clip a computer acknowledged.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct ClipDelivery {
    pub device_id: String,
    pub mime_type: String,
    pub size: u64,
    /// From sending to the computer's acknowledgement.
    pub millis: u64,
    /// `false` if the computer refused it (too large, unsupported type…).
    pub accepted: bool,
}

/// Implemented by the app to hear what happens. Called from background threads.
#[uniffi::export(with_foreign)]
pub trait PhoneListener: Send + Sync {
    fn on_connection_changed(&self, device_id: String, connected: bool);

    /// A clip arrived from a computer. `sensitive` clips must stay out of history.
    fn on_clip_received(&self, mime_type: String, data: Vec<u8>, sensitive: bool);

    /// A computer acknowledged a clip sent with `send_text` or `send_image`.
    fn on_clip_delivered(&self, delivery: ClipDelivery);
}

#[derive(uniffi::Object)]
pub struct PhoneClient {
    runtime: Runtime,
    state: Arc<State>,
    /// One per paired computer while the client runs.
    dialers: Mutex<HashMap<DeviceId, Dialer>>,
}

/// Keeps a session with one computer.
struct Dialer {
    task: JoinHandle<()>,
    /// Where it dials. Any update also makes it dial now rather than after its backoff.
    addresses: watch::Sender<Vec<SocketAddr>>,
}

struct State {
    identity: Identity,
    device_name: String,
    data_dir: PathBuf,
    computers: Mutex<Vec<PairedComputer>>,
    peers: Peers,
    guard: Mutex<EchoGuard>,
    listener: Mutex<Option<Arc<dyn PhoneListener>>>,
}

#[uniffi::export]
impl PhoneClient {
    /// Loads this phone's identity and pairings from `data_dir` (the app's private files
    /// directory), creating the identity on first use.
    #[uniffi::constructor]
    pub fn new(data_dir: String, device_name: String) -> Result<Arc<Self>, PastegetError> {
        let data_dir = PathBuf::from(data_dir);
        fs::create_dir_all(&data_dir)?;
        let identity = load_or_create_identity(&data_dir)?;
        let computers = load_computers(&data_dir)?;
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("pasteget")
            .enable_all()
            .build()?;
        Ok(Arc::new(Self {
            runtime,
            state: Arc::new(State {
                guard: Mutex::new(EchoGuard::new(identity.device_id())),
                identity,
                device_name: sanitize_device_name(&device_name),
                data_dir,
                computers: Mutex::new(computers),
                peers: Peers::new(),
                listener: Mutex::new(None),
            }),
            dialers: Mutex::new(HashMap::new()),
        }))
    }

    pub fn device_id(&self) -> String {
        self.state.identity.device_id().to_string()
    }

    pub fn paired_computers(&self) -> Vec<PairedComputer> {
        lock(&self.state.computers).clone()
    }

    /// Pairs with the computer whose QR code was scanned. Blocks for up to ~10 s per address.
    pub fn pair(&self, invite_uri: String) -> Result<PairedComputer, PastegetError> {
        let invite: PairingInvite = invite_uri
            .trim()
            .parse()
            .map_err(|_| PastegetError::InvalidCode)?;
        let paired = self
            .runtime
            .block_on(pair_with_any_address(&self.state, &invite))?;
        let computer = PairedComputer {
            device_id: paired.device_id.to_string(),
            name: paired.device_name,
            addresses: invite.addresses.iter().map(SocketAddr::to_string).collect(),
        };
        {
            let mut computers = lock(&self.state.computers);
            computers.retain(|c| c.device_id != computer.device_id);
            computers.push(computer.clone());
            save_computers(&self.state.data_dir, &computers)?;
        }
        // Connect right away if the client is running.
        if self.is_running() {
            self.start_dialer(&computer);
        }
        Ok(computer)
    }

    /// Unpairs a computer and drops its connection.
    pub fn forget(&self, device_id: String) -> Result<(), PastegetError> {
        {
            let mut computers = lock(&self.state.computers);
            let before = computers.len();
            computers.retain(|c| c.device_id != device_id);
            if computers.len() == before {
                return Err(PastegetError::UnknownDevice);
            }
            save_computers(&self.state.data_dir, &computers)?;
        }
        if let Ok(id) = device_id.parse::<DeviceId>()
            && let Some(dialer) = lock(&self.dialers).remove(&id)
        {
            dialer.task.abort();
        }
        Ok(())
    }

    /// Connects to every paired computer and keeps reconnecting until [`PhoneClient::stop`].
    pub fn start(&self, listener: Arc<dyn PhoneListener>) {
        *lock(&self.state.listener) = Some(listener);
        let computers = lock(&self.state.computers).clone();
        for computer in &computers {
            self.start_dialer(computer);
        }
    }

    /// Closes every connection.
    pub fn stop(&self) {
        for (_, dialer) in lock(&self.dialers).drain() {
            dialer.task.abort();
        }
        *lock(&self.state.listener) = None;
    }

    /// The computer `device_id` was found on the local network at `ips` and `port`. If it isn't
    /// connected, it's dialed there right away. Returns `false` if it isn't a paired computer.
    ///
    /// Found addresses are only tried: one is remembered once a connection through it proved
    /// the computer's identity, so a forged announcement can't push the real addresses out.
    pub fn computer_found(&self, device_id: String, ips: Vec<String>, port: u16) -> bool {
        let Ok(id) = device_id.parse::<DeviceId>() else {
            return false;
        };
        let Some(known) = lock(&self.state.computers)
            .iter()
            .find(|c| c.device_id == device_id)
            .map(|c| parse_addresses(&c.addresses))
        else {
            return false;
        };
        let found = ips
            .iter()
            .filter_map(|ip| ip.parse::<IpAddr>().ok())
            .map(|ip| SocketAddr::new(ip, port))
            .take(MAX_ADDRESSES);
        if let Some(dialer) = lock(&self.dialers).get(&id) {
            dialer.addresses.send_replace(found_first(found, known));
        }
        true
    }

    /// The phone joined a network: computers that aren't connected are dialed right away
    /// instead of after their reconnection wait.
    pub fn network_changed(&self) {
        for dialer in lock(&self.dialers).values() {
            dialer.addresses.send_modify(|_| {});
        }
    }

    pub fn connected_count(&self) -> u32 {
        u32::try_from(self.state.peers.connected().len()).unwrap_or(u32::MAX)
    }

    /// Sends text to every connected computer. Returns how many.
    pub fn send_text(&self, text: String) -> u32 {
        self.send(mime::TEXT, text.into_bytes(), false)
    }

    /// Sends a PNG or JPEG image, as it is, to every connected computer. Returns how many.
    /// Other formats must be converted to PNG first.
    pub fn send_image(&self, image: Vec<u8>) -> Result<u32, PastegetError> {
        let mime_type = mime::of_image(&image).ok_or(PastegetError::UnsupportedImage)?;
        Ok(self.send(mime_type, image, false))
    }

    /// Text the phone's clipboard now holds, seen by the automatic copy rather than shared by
    /// the user. Sent to every connected computer unless the clipboard already held it: a clip
    /// from a computer, or a copy reported twice. Returns how many computers it went to, or
    /// `None` if it wasn't new.
    ///
    /// `sensitive` copies (passwords, codes) are sent marked as such, so computers keep them out
    /// of history and clipboard managers.
    pub fn copied_text(&self, text: String, sensitive: bool) -> Option<u32> {
        let data = text.into_bytes();
        if !lock(&self.state.guard).should_send(mime::TEXT, &data) {
            return None;
        }
        Some(self.broadcast(mime::TEXT, data, sensitive))
    }
}

impl PhoneClient {
    /// Sends what the user shared, even if it was sent before.
    fn send(&self, mime_type: &str, data: Vec<u8>, sensitive: bool) -> u32 {
        // Remember it, so the automatic copy doesn't send it a second time.
        lock(&self.state.guard).should_send(mime_type, &data);
        self.broadcast(mime_type, data, sensitive)
    }

    fn broadcast(&self, mime_type: &str, data: Vec<u8>, sensitive: bool) -> u32 {
        let clip = Clip {
            mime_type: mime_type.to_owned(),
            data,
            sensitive,
            origin: self.state.identity.device_id(),
        };
        u32::try_from(self.state.peers.broadcast(&clip, None)).unwrap_or(u32::MAX)
    }

    fn is_running(&self) -> bool {
        lock(&self.state.listener).is_some()
    }

    /// Starts keeping a session with `computer`, replacing any earlier dialer for it.
    fn start_dialer(&self, computer: &PairedComputer) {
        let Ok(id) = computer.device_id.parse::<DeviceId>() else {
            return;
        };
        let (addresses, watched) = watch::channel(parse_addresses(&computer.addresses));
        let state = Arc::clone(&self.state);
        let task = self.runtime.spawn(async move {
            let hello = state.hello();
            session::keep_connected(&state.identity, id, watched, &hello, &state.peers, &*state)
                .await;
        });
        if let Some(old) = lock(&self.dialers).insert(id, Dialer { task, addresses }) {
            old.task.abort();
        }
    }
}

impl Drop for PhoneClient {
    fn drop(&mut self) {
        self.stop();
    }
}

impl State {
    fn hello(&self) -> Hello {
        Hello {
            protocol_version: PROTOCOL_VERSION,
            device_name: self.device_name.clone(),
            app_version: env!("CARGO_PKG_VERSION").to_owned(),
            platform: Platform::Android.into(),
        }
    }

    fn listener(&self) -> Option<Arc<dyn PhoneListener>> {
        lock(&self.listener).clone()
    }
}

impl SessionEvents for State {
    /// Moves the address that worked to the front, so it's dialed first next time.
    fn reached(&self, peer: DeviceId, address: SocketAddr) {
        let id = peer.to_string();
        let address = address.to_string();
        let mut computers = lock(&self.computers);
        let Some(computer) = computers.iter_mut().find(|c| c.device_id == id) else {
            return;
        };
        if computer.addresses.first() == Some(&address) {
            return;
        }
        computer.addresses.retain(|a| *a != address);
        computer.addresses.insert(0, address);
        computer.addresses.truncate(MAX_ADDRESSES);
        // Best effort: if saving fails, the next connection tries again.
        let _ = save_computers(&self.data_dir, &computers);
    }

    fn connected(&self, peer: DeviceId, _hello: &Hello) {
        if let Some(listener) = self.listener() {
            listener.on_connection_changed(peer.to_string(), true);
        }
    }

    fn disconnected(&self, peer: DeviceId) {
        if let Some(listener) = self.listener() {
            listener.on_connection_changed(peer.to_string(), false);
        }
    }

    fn clip_received(&self, _peer: DeviceId, clip: Clip) {
        {
            let mut guard = lock(&self.guard);
            if !guard.accepts_origin(&clip.origin) {
                return;
            }
            guard.applied_remote(&clip);
        }
        if let Some(listener) = self.listener() {
            listener.on_clip_received(clip.mime_type, clip.data, clip.sensitive);
        }
    }

    fn clip_delivered(&self, peer: DeviceId, delivery: &Delivery) {
        if let Some(listener) = self.listener() {
            listener.on_clip_delivered(ClipDelivery {
                device_id: peer.to_string(),
                mime_type: delivery.mime_type.clone(),
                size: delivery.len as u64,
                millis: u64::try_from(delivery.elapsed.as_millis()).unwrap_or(u64::MAX),
                accepted: delivery.status == AckStatus::Ok,
            });
        }
    }
}

async fn pair_with_any_address(
    state: &State,
    invite: &PairingInvite,
) -> Result<pairing::PairedDevice, PastegetError> {
    let mut last_error = PastegetError::Unreachable("the code has no address".into());
    for address in &invite.addresses {
        let attempt = async {
            let socket = TcpStream::connect(address)
                .await
                .map_err(|e| PastegetError::Unreachable(format!("{address}: {e}")))?;
            pairing::pair_with(socket, &state.identity, invite, &state.device_name)
                .await
                .map_err(|e| PastegetError::PairingFailed(e.to_string()))
        };
        match tokio::time::timeout(HANDSHAKE_TIMEOUT, attempt).await {
            Ok(Ok(paired)) => return Ok(paired),
            Ok(Err(e)) => last_error = e,
            Err(_) => last_error = PastegetError::Unreachable(format!("{address} did not answer")),
        }
    }
    Err(last_error)
}

fn parse_addresses(addresses: &[String]) -> Vec<SocketAddr> {
    addresses.iter().filter_map(|a| a.parse().ok()).collect()
}

/// `first`, then the addresses of `then` that aren't in it.
fn found_first(
    first: impl IntoIterator<Item = SocketAddr>,
    then: impl IntoIterator<Item = SocketAddr>,
) -> Vec<SocketAddr> {
    let mut merged: Vec<SocketAddr> = Vec::new();
    for address in first.into_iter().chain(then) {
        if !merged.contains(&address) {
            merged.push(address);
        }
    }
    merged
}

fn load_or_create_identity(dir: &Path) -> Result<Identity, PastegetError> {
    let path = dir.join(IDENTITY_FILE);
    match fs::read(&path) {
        Ok(der) => {
            Identity::from_pkcs8_der(&der).map_err(|e| PastegetError::Storage(e.to_string()))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let identity =
                Identity::generate().map_err(|e| PastegetError::Storage(e.to_string()))?;
            write_atomically(&path, identity.pkcs8_der())?;
            Ok(identity)
        }
        Err(e) => Err(e.into()),
    }
}

fn load_computers(dir: &Path) -> Result<Vec<PairedComputer>, PastegetError> {
    match fs::read(dir.join(COMPUTERS_FILE)) {
        Ok(json) => {
            serde_json::from_slice(&json).map_err(|e| PastegetError::Storage(e.to_string()))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(e.into()),
    }
}

fn save_computers(dir: &Path, computers: &[PairedComputer]) -> Result<(), PastegetError> {
    let json =
        serde_json::to_vec_pretty(computers).map_err(|e| PastegetError::Storage(e.to_string()))?;
    write_atomically(&dir.join(COMPUTERS_FILE), &json)
}

/// The app's files directory is private to the app on Android, so no extra permissions needed.
fn write_atomically(path: &Path, contents: &[u8]) -> Result<(), PastegetError> {
    let temp = path.with_extension("tmp");
    let mut file = fs::File::create(&temp)?;
    file.write_all(contents)?;
    file.sync_all()?;
    drop(file);
    fs::rename(&temp, path)?;
    Ok(())
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(s: &str) -> SocketAddr {
        s.parse().unwrap()
    }

    #[test]
    fn found_addresses_come_first_without_duplicates() {
        let found = [addr("192.168.1.30:47290"), addr("10.0.0.2:47290")];
        let known = [addr("192.168.1.20:47290"), addr("10.0.0.2:47290")];

        assert_eq!(
            found_first(found, known),
            [
                addr("192.168.1.30:47290"),
                addr("10.0.0.2:47290"),
                addr("192.168.1.20:47290")
            ]
        );
    }

    #[test]
    fn unparsable_addresses_are_skipped() {
        let stored = ["192.168.1.20:47290".to_owned(), "not an address".to_owned()];
        assert_eq!(parse_addresses(&stored), [addr("192.168.1.20:47290")]);
    }
}
