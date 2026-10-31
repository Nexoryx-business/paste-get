// SPDX-License-Identifier: GPL-3.0-or-later

//! The phone side of paste-get, exposed to the Android app through UniFFI.
//!
//! [`PhoneClient`] keeps this phone's identity and the computers it paired with, keeps a
//! session open with each of them (the same `core::session` engine as the desktop), and reports
//! connections and received clips through [`PhoneListener`].
//!
//! Methods that touch the network block: call them off the main thread.

// UniFFI's generated scaffolding is `extern "C"` glue that needs `unsafe`. Our own code here
// uses none.
#![allow(unsafe_code)]

use std::fs;
use std::io::Write;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use pasteget_core::clip::{Clip, mime};
use pasteget_core::echo::EchoGuard;
use pasteget_core::identity::{DeviceId, Identity};
use pasteget_core::pairing::{self, PairingInvite, sanitize_device_name};
use pasteget_core::proto::{Hello, PROTOCOL_VERSION, Platform};
use pasteget_core::session::{self, HANDSHAKE_TIMEOUT, Peers, SessionEvents};
use serde::{Deserialize, Serialize};
use tokio::net::TcpStream;
use tokio::runtime::Runtime;
use tokio::task::JoinHandle;

uniffi::setup_scaffolding!();

const IDENTITY_FILE: &str = "identity.key";
const COMPUTERS_FILE: &str = "computers.json";
const PNG_SIGNATURE: &[u8] = b"\x89PNG\r\n\x1a\n";

#[derive(Debug, thiserror::Error, uniffi::Error)]
#[uniffi(flat_error)]
pub enum PastegetError {
    #[error("this is not a paste-get pairing code")]
    InvalidCode,
    #[error("could not reach the computer: {0}")]
    Unreachable(String),
    #[error("pairing failed: {0}")]
    PairingFailed(String),
    #[error("not a PNG image")]
    NotPng,
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
    /// Socket addresses from the pairing code, e.g. `192.168.1.20:47290`.
    pub addresses: Vec<String>,
}

/// Implemented by the app to hear what happens. Called from background threads.
#[uniffi::export(with_foreign)]
pub trait PhoneListener: Send + Sync {
    fn on_connection_changed(&self, device_id: String, connected: bool);

    /// A clip arrived from a computer. `sensitive` clips must stay out of history.
    fn on_clip_received(&self, mime_type: String, data: Vec<u8>, sensitive: bool);
}

#[derive(uniffi::Object)]
pub struct PhoneClient {
    runtime: Runtime,
    state: Arc<State>,
    sessions: Mutex<Vec<JoinHandle<()>>>,
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
            sessions: Mutex::new(Vec::new()),
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
        if lock(&self.state.listener).is_some() {
            self.restart_sessions();
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
        if lock(&self.state.listener).is_some() {
            self.restart_sessions();
        }
        Ok(())
    }

    /// Connects to every paired computer and keeps reconnecting until [`PhoneClient::stop`].
    pub fn start(&self, listener: Arc<dyn PhoneListener>) {
        *lock(&self.state.listener) = Some(listener);
        self.restart_sessions();
    }

    /// Closes every connection.
    pub fn stop(&self) {
        for session in lock(&self.sessions).drain(..) {
            session.abort();
        }
        *lock(&self.state.listener) = None;
    }

    pub fn connected_count(&self) -> u32 {
        u32::try_from(self.state.peers.connected().len()).unwrap_or(u32::MAX)
    }

    /// Sends text to every connected computer. Returns how many.
    pub fn send_text(&self, text: String) -> u32 {
        self.send(mime::TEXT, text.into_bytes(), false)
    }

    /// Sends a PNG image to every connected computer. Returns how many.
    pub fn send_png(&self, png: Vec<u8>) -> Result<u32, PastegetError> {
        if !png.starts_with(PNG_SIGNATURE) {
            return Err(PastegetError::NotPng);
        }
        Ok(self.send(mime::PNG, png, false))
    }
}

impl PhoneClient {
    fn send(&self, mime_type: &str, data: Vec<u8>, sensitive: bool) -> u32 {
        let clip = Clip {
            mime_type: mime_type.to_owned(),
            data,
            sensitive,
            origin: self.state.identity.device_id(),
        };
        // Remember it, so the same content coming back from a computer isn't re-applied.
        lock(&self.state.guard).should_send(&clip.mime_type, &clip.data);
        u32::try_from(self.state.peers.broadcast(&clip, None)).unwrap_or(u32::MAX)
    }

    fn restart_sessions(&self) {
        let mut sessions = lock(&self.sessions);
        for session in sessions.drain(..) {
            session.abort();
        }
        let hello = self.state.hello();
        for computer in lock(&self.state.computers).iter() {
            let Ok(id) = computer.device_id.parse::<DeviceId>() else {
                continue;
            };
            let addresses: Vec<SocketAddr> = computer
                .addresses
                .iter()
                .filter_map(|a| a.parse().ok())
                .collect();
            let state = Arc::clone(&self.state);
            let hello = hello.clone();
            sessions.push(self.runtime.spawn(async move {
                session::keep_connected(
                    &state.identity,
                    id,
                    &addresses,
                    &hello,
                    &state.peers,
                    &*state,
                )
                .await;
            }));
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
