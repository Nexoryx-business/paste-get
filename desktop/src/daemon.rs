// SPDX-License-Identifier: GPL-3.0-or-later

//! The daemon: accepts paired devices, keeps a session with each, and moves clips between the
//! local clipboard and every connected device.
//!
//! ```text
//!   clipboard thread ──local copies──▶ broadcast to all sessions
//!   session (per device) ──received clips──▶ clipboard thread + relay to other sessions
//! ```
//!
//! Devices that paired with us (phones) connect in. Devices we paired with by scanning their
//! code (`pasteget join`) are dialed out and redialed after disconnects.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, Result, anyhow, bail};
use pasteget_core::clip::{Clip, mime};
use pasteget_core::detect;
use pasteget_core::echo::EchoGuard;
use pasteget_core::identity::{DeviceId, Identity};
use pasteget_core::pairing::{self, PairingInvite, PairingWindow};
use pasteget_core::proto::{Hello, PROTOCOL_VERSION, Platform};
use pasteget_core::session::{self, HANDSHAKE_TIMEOUT, Peers, RECONNECT_MIN, SessionEvents};
use pasteget_core::transport;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Mutex as AsyncMutex, broadcast, mpsc, watch};
use tokio::task::{AbortHandle, JoinSet};
use tokio::time::{sleep, timeout};

use crate::clipboard::{ClipboardBackend, ClipboardWatcher, Content, DEFAULT_POLL_INTERVAL};
use crate::config::{KnownDevice, KnownDevices, NotifyFor, Profile};
use crate::discovery::Advertiser;
use crate::hotkeys::{self, Action, Hotkeys};
use crate::notify::{Notifier, Received};
use crate::recent::{RecentClip, RecentClips};

/// Time a not-yet-paired device has to send its pairing request.
const PAIRING_REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// How to run a daemon.
#[derive(Clone, Debug)]
pub struct DaemonOptions {
    /// Address to listen on. Port 0 picks a free port.
    pub listen: SocketAddr,
    /// Addresses put in pairing QR codes. `None` uses this machine's network interfaces.
    pub advertised_ips: Option<Vec<IpAddr>>,
    pub poll_interval: Duration,
    /// Announce the daemon on the local network with mDNS.
    pub announce: bool,
    /// Shows "Image from … ✓" when a clip lands on the clipboard, as `settings.toml` asks.
    pub notifier: Option<Arc<dyn Notifier>>,
    /// Register the global hotkeys of `settings.toml`.
    pub hotkeys: bool,
}

impl DaemonOptions {
    pub fn listen_on(port: u16) -> Self {
        Self {
            listen: SocketAddr::from(([0, 0, 0, 0], port)),
            advertised_ips: None,
            poll_interval: DEFAULT_POLL_INTERVAL,
            announce: true,
            notifier: None,
            hotkeys: false,
        }
    }
}

/// Something that happened, for the command-line tool to report.
#[derive(Clone, Debug)]
pub enum Event {
    Paired(KnownDevice),
    PairingFailed(String),
}

/// What `pasteget status` shows.
#[derive(Clone, Debug)]
pub struct Status {
    pub device_id: DeviceId,
    pub device_name: String,
    pub listening_on: SocketAddr,
    pub devices: Vec<DeviceStatus>,
    pub pairing_open: bool,
    /// Nothing is sent or received while paused, except what is sent explicitly.
    pub paused: bool,
}

#[derive(Clone, Debug)]
pub struct DeviceStatus {
    pub device: KnownDevice,
    pub connected: bool,
}

/// A running daemon. Dropping it stops all its tasks.
pub struct Daemon {
    shared: Arc<Shared>,
    local_addr: SocketAddr,
    _advertiser: Option<Advertiser>,
    _hotkeys: Option<Hotkeys>,
}

struct Shared {
    identity: Identity,
    device_name: String,
    profile: Profile,
    listen_port: u16,
    advertised_ips: Option<Vec<IpAddr>>,
    known: Mutex<KnownDevices>,
    pairing: AsyncMutex<Option<PairingWindow>>,
    peers: Peers,
    guard: Mutex<EchoGuard>,
    clipboard: ClipboardWatcher,
    notifier: Option<Arc<dyn Notifier>>,
    notify_for: NotifyFor,
    offer_to_open_links: bool,
    paused: AtomicBool,
    recent: Mutex<RecentClips>,
    events: broadcast::Sender<Event>,
    tasks: Mutex<JoinSet<()>>,
    /// The redial loops of devices we joined, to stop when one is unpaired.
    dialers: Mutex<HashMap<DeviceId, AbortHandle>>,
}

impl Daemon {
    /// Binds the listener and starts every task.
    pub async fn start(
        profile: Profile,
        backend: impl ClipboardBackend,
        options: DaemonOptions,
    ) -> Result<Self> {
        let identity = profile.identity()?;
        let settings = profile.settings()?;
        let known = profile.known_devices()?;
        let listener = TcpListener::bind(options.listen)
            .await
            .with_context(|| format!("could not listen on {}", options.listen))?;
        let local_addr = listener.local_addr()?;

        let (changes_tx, changes) = mpsc::channel(8);
        let clipboard = ClipboardWatcher::spawn(backend, options.poll_interval, changes_tx)?;
        let (events, _) = broadcast::channel(16);

        let shared = Arc::new(Shared {
            guard: Mutex::new(EchoGuard::new(identity.device_id())),
            identity,
            device_name: settings.device_name,
            profile,
            listen_port: local_addr.port(),
            advertised_ips: options.advertised_ips,
            known: Mutex::new(known),
            pairing: AsyncMutex::new(None),
            peers: Peers::new(),
            clipboard,
            notifier: options.notifier,
            notify_for: settings.notify,
            offer_to_open_links: settings.offer_to_open_links,
            paused: AtomicBool::new(false),
            recent: Mutex::new(RecentClips::default()),
            events,
            tasks: Mutex::new(JoinSet::new()),
            dialers: Mutex::new(HashMap::new()),
        });

        shared.spawn(accept_loop(Arc::clone(&shared), listener));
        shared.spawn(local_changes_loop(Arc::clone(&shared), changes));
        let joined: Vec<KnownDevice> = lock(&shared.known)
            .iter()
            .filter(|device| !device.addresses.is_empty())
            .cloned()
            .collect();
        for device in joined {
            shared.start_dialer(device);
        }

        // Announcing is a convenience: pairing codes carry addresses anyway.
        let advertiser = options
            .announce
            .then(|| {
                Advertiser::start(
                    shared.identity.device_id(),
                    &shared.device_name,
                    local_addr.port(),
                )
            })
            .transpose()
            .unwrap_or_else(|e| {
                tracing::warn!(error = %e, "not announcing on the network");
                None
            });

        tracing::info!(
            device_id = %shared.identity.device_id(),
            name = %shared.device_name,
            address = %local_addr,
            announced = advertiser.is_some(),
            "daemon started"
        );
        let hotkeys = if options.hotkeys {
            let (pressed, presses) = mpsc::channel(4);
            shared.spawn(hotkey_loop(Arc::clone(&shared), presses));
            hotkeys::start(&settings.hotkeys.bindings(), pressed)
        } else {
            None
        };

        Ok(Self {
            shared,
            local_addr,
            _advertiser: advertiser,
            _hotkeys: hotkeys,
        })
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    pub fn device_id(&self) -> DeviceId {
        self.shared.identity.device_id()
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.shared.events.subscribe()
    }

    /// Opens a pairing window and returns the invite to show as a QR code.
    pub async fn open_pairing(&self) -> Result<PairingInvite> {
        let window = PairingWindow::open(Instant::now())?;
        let addresses: Vec<SocketAddr> = self
            .shared
            .ips_to_advertise()?
            .into_iter()
            .map(|ip| SocketAddr::new(ip, self.shared.listen_port))
            .collect();
        if addresses.is_empty() {
            bail!("no network address to put in the pairing code: is this PC on a network?");
        }
        let invite = window.invite(&self.shared.identity, &self.shared.device_name, addresses);
        *self.shared.pairing.lock().await = Some(window);
        tracing::info!("pairing window opened");
        Ok(invite)
    }

    /// Pairs with the device that shows `invite`, then keeps a connection to it.
    pub async fn join(&self, invite: &PairingInvite) -> Result<KnownDevice> {
        let mut last_error = anyhow!("the pairing code has no address");
        for address in &invite.addresses {
            let attempt = async {
                let socket = TcpStream::connect(address).await?;
                pairing::pair_with(
                    socket,
                    &self.shared.identity,
                    invite,
                    &self.shared.device_name,
                )
                .await
                .map_err(anyhow::Error::from)
            };
            match timeout(HANDSHAKE_TIMEOUT, attempt).await {
                Ok(Ok(paired)) => {
                    let device = KnownDevice::joined(paired, invite.addresses.clone());
                    self.shared.remember(device.clone())?;
                    self.shared.start_dialer(device.clone());
                    return Ok(device);
                }
                Ok(Err(e)) => last_error = e.context(format!("pairing through {address} failed")),
                Err(_) => last_error = anyhow!("{address} did not answer"),
            }
        }
        Err(last_error)
    }

    /// Sends content to every connected device, even while paused. Returns how many it was
    /// queued for.
    pub fn send(&self, content: Content) -> usize {
        let clip = Clip {
            mime_type: content.mime_type,
            data: content.data,
            sensitive: content.sensitive,
            origin: self.device_id(),
        };
        self.shared.remember_clip(&clip, None);
        self.shared.peers.broadcast(&clip, None)
    }

    /// Unpairs a device: it's no longer trusted, its connection ends now, and it can only come
    /// back by pairing again.
    pub fn forget(&self, id: &DeviceId) -> Result<KnownDevice> {
        let device = {
            let mut known = lock(&self.shared.known);
            let device = known.remove(id).context("no paired device with this ID")?;
            self.shared.profile.save_known_devices(&known)?;
            device
        };
        if let Some(dialer) = lock(&self.shared.dialers).remove(id) {
            dialer.abort();
        }
        self.shared.peers.disconnect(id);
        tracing::info!(device = %device.name, id = %device.id, "unpaired");
        Ok(device)
    }

    /// Sends what's on the clipboard now, even while paused. Returns how many devices it was
    /// queued for.
    pub async fn send_clipboard(&self) -> Result<usize> {
        self.shared.send_clipboard().await
    }

    /// Pauses or resumes syncing. While paused, local copies aren't sent and clips from devices
    /// aren't applied. Returns the new state.
    pub fn set_paused(&self, paused: bool) -> bool {
        self.shared.set_paused(paused)
    }

    pub fn is_paused(&self) -> bool {
        self.shared.paused.load(Ordering::Relaxed)
    }

    /// Does what a hotkey does, and says so in a notification.
    pub async fn perform(&self, action: Action) {
        self.shared.perform(action).await;
    }

    /// The last clips that went through, newest first.
    pub fn recent(&self) -> Vec<RecentClip> {
        lock(&self.shared.recent).list()
    }

    /// Puts a recent clip back on the clipboard. It isn't sent again: the devices had it.
    pub fn copy_recent(&self, id: u64) -> Result<()> {
        let clip = lock(&self.shared.recent)
            .get(id)
            .context("that clip is no longer in the recent list")?;
        let content = Content {
            mime_type: clip.mime_type,
            data: clip.data.to_vec(),
            sensitive: false,
        };
        self.shared.clipboard.write(content, |_| {})
    }

    pub async fn status(&self) -> Status {
        let pairing_open = self
            .shared
            .pairing
            .lock()
            .await
            .as_ref()
            .is_some_and(|window| window.is_open(Instant::now()));
        let connected = self.shared.peers.connected();
        let devices = lock(&self.shared.known)
            .iter()
            .map(|device| DeviceStatus {
                device: device.clone(),
                connected: connected.contains(&device.id),
            })
            .collect();
        Status {
            device_id: self.device_id(),
            device_name: self.shared.device_name.clone(),
            listening_on: self.local_addr,
            devices,
            pairing_open,
            paused: self.is_paused(),
        }
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        lock(&self.shared.tasks).abort_all();
    }
}

impl Shared {
    fn spawn(&self, task: impl Future<Output = ()> + Send + 'static) -> AbortHandle {
        let mut tasks = lock(&self.tasks);
        while tasks.try_join_next().is_some() {}
        tasks.spawn(task)
    }

    fn start_dialer(self: &Arc<Self>, device: KnownDevice) {
        let id = device.id;
        let handle = self.spawn(dial_loop(Arc::clone(self), device));
        if let Some(previous) = lock(&self.dialers).insert(id, handle) {
            previous.abort();
        }
    }

    fn hello(&self) -> Hello {
        let platform = match std::env::consts::OS {
            "windows" => Platform::Windows,
            "linux" => Platform::Linux,
            "macos" => Platform::Macos,
            _ => Platform::Unspecified,
        };
        Hello {
            protocol_version: PROTOCOL_VERSION,
            device_name: self.device_name.clone(),
            app_version: env!("CARGO_PKG_VERSION").to_owned(),
            platform: platform.into(),
        }
    }

    fn ips_to_advertise(&self) -> Result<Vec<IpAddr>> {
        if let Some(ips) = &self.advertised_ips {
            return Ok(ips.clone());
        }
        let mut ips: Vec<IpAddr> = if_addrs::get_if_addrs()
            .context("could not list network interfaces")?
            .into_iter()
            .filter(|interface| {
                interface.is_oper_up() && !interface.is_loopback() && !interface.is_link_local()
            })
            .map(|interface| interface.ip())
            // The listener is IPv4-only for now.
            .filter(IpAddr::is_ipv4)
            .collect();
        ips.sort();
        ips.dedup();
        Ok(ips)
    }

    fn set_paused(&self, paused: bool) -> bool {
        self.paused.store(paused, Ordering::Relaxed);
        tracing::info!(
            paused,
            "syncing {}",
            if paused { "paused" } else { "resumed" }
        );
        paused
    }

    async fn send_clipboard(&self) -> Result<usize> {
        let content = self
            .clipboard
            .read()
            .await?
            .context("the clipboard is empty, or holds something that can't be sent")?;
        let clip = Clip {
            mime_type: content.mime_type,
            data: content.data,
            sensitive: content.sensitive,
            origin: self.identity.device_id(),
        };
        self.remember_clip(&clip, None);
        let sent_to = self.peers.broadcast(&clip, None);
        tracing::info!(mime_type = %clip.mime_type, len = clip.data.len(), sent_to, "clipboard sent");
        Ok(sent_to)
    }

    async fn perform(&self, action: Action) {
        let summary = match action {
            Action::TogglePause => {
                if self.set_paused(!self.paused.load(Ordering::Relaxed)) {
                    "Syncing paused".to_owned()
                } else {
                    "Syncing resumed".to_owned()
                }
            }
            Action::SendClipboard => match self.send_clipboard().await {
                Ok(0) => "No device is connected".to_owned(),
                Ok(1) => "Clipboard sent to 1 device".to_owned(),
                Ok(n) => format!("Clipboard sent to {n} devices"),
                Err(e) => {
                    tracing::info!(error = %e, "clipboard not sent");
                    "Nothing to send".to_owned()
                }
            },
        };
        if let Some(notifier) = &self.notifier {
            notifier.notice(summary);
        }
    }

    /// Adds a clip to the recent list. Sensitive clips are not kept.
    fn remember_clip(&self, clip: &Clip, from: Option<DeviceId>) {
        if clip.sensitive {
            return;
        }
        let from = from.map(|id| self.name_of(&id));
        lock(&self.recent).push(&clip.mime_type, &clip.data, from, SystemTime::now());
    }

    fn name_of(&self, id: &DeviceId) -> String {
        lock(&self.known)
            .get(id)
            .map_or_else(|| "a paired device".to_owned(), |d| d.name.clone())
    }

    fn remember(&self, device: KnownDevice) -> Result<()> {
        let mut known = lock(&self.known);
        known.insert(device);
        self.profile.save_known_devices(&known)
    }
}

/// Received clips go to the clipboard and on to the other devices.
impl SessionEvents for Shared {
    fn connected(&self, peer: DeviceId, hello: &Hello) {
        // Unpaired while this connection was being set up: [`Daemon::forget`] came too early to
        // end it.
        if !lock(&self.known).contains(&peer) {
            self.peers.disconnect(&peer);
            return;
        }
        tracing::info!(device = %hello.device_name, id = %peer, "connected");
    }

    fn disconnected(&self, peer: DeviceId) {
        tracing::info!(id = %peer, "disconnected");
    }

    fn clip_received(&self, peer: DeviceId, clip: Clip) {
        // A clip that started here and came back through another device: drop it.
        if !lock(&self.guard).accepts_origin(&clip.origin) {
            return;
        }
        if self.paused.load(Ordering::Relaxed) {
            tracing::info!(mime_type = %clip.mime_type, len = clip.data.len(), "clip dropped: paused");
            return;
        }
        lock(&self.guard).applied_remote(&clip);
        self.remember_clip(&clip, Some(peer));

        let received = Received {
            image: clip.mime_type != mime::TEXT,
            from: self.name_of(&peer),
        };
        // A link gets a notification of its own, which opens it when clicked.
        let link = clip
            .as_text()
            .filter(|_| {
                self.offer_to_open_links && self.notify_for != NotifyFor::Off && !clip.sensitive
            })
            .and_then(detect::link);
        // Sensitive clips stay as quiet as possible.
        let notifier = self
            .notifier
            .clone()
            .filter(|_| !clip.sensitive && (link.is_some() || received.wanted_by(self.notify_for)));
        let (mime_type, len) = (clip.mime_type.clone(), clip.data.len());
        let applied = move |took: Duration| {
            tracing::info!(%mime_type, len, ms = took.as_millis(), "clip applied to the clipboard");
            match (notifier, link) {
                (Some(notifier), Some(link)) => notifier.link(received.from, link),
                (Some(notifier), None) => notifier.received(received),
                (None, _) => {}
            }
        };
        let content = Content {
            mime_type: clip.mime_type.clone(),
            data: clip.data.clone(),
            sensitive: clip.sensitive,
        };
        if let Err(e) = self.clipboard.write(content, applied) {
            tracing::warn!(error = %e, "received clip not applied");
        }
        self.peers.broadcast(&clip, Some(peer));
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    // Poisoning means a panic elsewhere; the data is still usable for this daemon's purposes.
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

async fn accept_loop(shared: Arc<Shared>, listener: TcpListener) {
    loop {
        match listener.accept().await {
            Ok((socket, address)) => {
                let shared_for_task = Arc::clone(&shared);
                shared.spawn(async move {
                    if let Err(e) = handle_incoming(Arc::clone(&shared_for_task), socket).await {
                        tracing::info!(%address, error = %e, "incoming connection ended");
                    }
                });
            }
            Err(e) => {
                tracing::warn!(error = %e, "could not accept a connection");
                sleep(RECONNECT_MIN).await;
            }
        }
    }
}

async fn handle_incoming(shared: Arc<Shared>, socket: TcpStream) -> Result<()> {
    let unverified = timeout(
        HANDSHAKE_TIMEOUT,
        transport::accept(socket, &shared.identity),
    )
    .await
    .context("TLS handshake timed out")??;
    let peer = unverified.peer();

    if lock(&shared.known).contains(&peer) {
        let connection = unverified.authorize(|id| lock(&shared.known).contains(id))?;
        session::run_session(connection, shared.hello(), &shared.peers, &*shared).await?;
        return Ok(());
    }

    let mut slot = shared.pairing.lock().await;
    let Some(window) = slot.as_mut().filter(|w| w.is_open(Instant::now())) else {
        bail!("refused device {peer}: not paired and no pairing code is shown");
    };
    let result = timeout(
        PAIRING_REQUEST_TIMEOUT,
        pairing::accept_pairing(unverified, window, &shared.device_name),
    )
    .await;
    if !window.is_open(Instant::now()) {
        *slot = None;
    }
    drop(slot);

    match result {
        Ok(Ok(paired)) => {
            let device = KnownDevice::accepted(paired);
            shared.remember(device.clone())?;
            tracing::info!(device = %device.name, id = %device.id, "paired");
            let _ = shared.events.send(Event::Paired(device));
            Ok(())
        }
        Ok(Err(e)) => {
            let _ = shared.events.send(Event::PairingFailed(e.to_string()));
            Err(e.into())
        }
        Err(_) => bail!("device {peer} did not send its pairing request in time"),
    }
}

/// Keeps a connection to a device we joined, redialing with backoff.
async fn dial_loop(shared: Arc<Shared>, device: KnownDevice) {
    let hello = shared.hello();
    // The addresses from the pairing code, which never change here.
    let (_, addresses) = watch::channel(device.addresses.clone());
    session::keep_connected(
        &shared.identity,
        device.id,
        addresses,
        &hello,
        &shared.peers,
        &*shared,
    )
    .await;
}

async fn hotkey_loop(shared: Arc<Shared>, mut presses: mpsc::Receiver<Action>) {
    while let Some(action) = presses.recv().await {
        tracing::info!(action = action.id(), "hotkey pressed");
        shared.perform(action).await;
    }
}

/// Sends local clipboard changes to every connected device.
async fn local_changes_loop(shared: Arc<Shared>, mut changes: mpsc::Receiver<Content>) {
    while let Some(content) = changes.recv().await {
        if !lock(&shared.guard).should_send(&content.mime_type, &content.data) {
            continue;
        }
        if shared.paused.load(Ordering::Relaxed) {
            tracing::info!(mime_type = %content.mime_type, len = content.data.len(), "local copy not sent: paused");
            continue;
        }
        let clip = Clip {
            mime_type: content.mime_type,
            data: content.data,
            sensitive: content.sensitive,
            origin: shared.identity.device_id(),
        };
        shared.remember_clip(&clip, None);
        let sent_to = shared.peers.broadcast(&clip, None);
        tracing::info!(mime_type = %clip.mime_type, len = clip.data.len(), sent_to, "local copy");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clipboard::{MemoryClipboard, Raw};
    use std::net::Ipv4Addr;

    struct TestDaemon {
        daemon: Daemon,
        clipboard: MemoryClipboard,
        notifications: Arc<Notifications>,
        _home: tempfile::TempDir,
    }

    #[derive(Debug, Default)]
    struct Notifications(
        Mutex<Vec<Received>>,
        Mutex<Vec<String>>,
        Mutex<Vec<(String, detect::Link)>>,
    );

    impl Notifier for Notifications {
        fn received(&self, received: Received) {
            self.0.lock().unwrap().push(received);
        }

        fn notice(&self, summary: String) {
            self.1.lock().unwrap().push(summary);
        }

        fn link(&self, from: String, link: detect::Link) {
            self.2.lock().unwrap().push((from, link));
        }
    }

    async fn start(name: &str) -> TestDaemon {
        let home = tempfile::tempdir().unwrap();
        let profile = Profile::open_in(home.path(), name).unwrap();
        std::fs::write(
            profile.dir().join("settings.toml"),
            format!("device_name = \"{name}\"\n"),
        )
        .unwrap();
        let clipboard = MemoryClipboard::default();
        let notifications = Arc::new(Notifications::default());
        let options = DaemonOptions {
            listen: SocketAddr::from((Ipv4Addr::LOCALHOST, 0)),
            advertised_ips: Some(vec![IpAddr::V4(Ipv4Addr::LOCALHOST)]),
            poll_interval: Duration::from_millis(10),
            announce: false,
            notifier: Some(Arc::clone(&notifications) as Arc<dyn Notifier>),
            hotkeys: false,
        };
        let daemon = Daemon::start(profile, clipboard.clone(), options)
            .await
            .unwrap();
        TestDaemon {
            daemon,
            clipboard,
            notifications,
            _home: home,
        }
    }

    async fn wait_for(mut condition: impl FnMut() -> bool) -> bool {
        for _ in 0..300 {
            if condition() {
                return true;
            }
            sleep(Duration::from_millis(10)).await;
        }
        false
    }

    async fn connected(daemon: &Daemon) -> bool {
        daemon.status().await.devices.iter().any(|d| d.connected)
    }

    #[tokio::test]
    async fn joined_daemons_sync_text_and_images_both_ways() {
        let pc = start("pc").await;
        let laptop = start("laptop").await;

        let invite = pc.daemon.open_pairing().await.unwrap();
        let joined = laptop.daemon.join(&invite).await.unwrap();
        assert_eq!(joined.name, "pc");
        assert!(wait_for_async(|| connected(&pc.daemon)).await);
        assert!(wait_for_async(|| connected(&laptop.daemon)).await);

        laptop
            .clipboard
            .set(Raw::Text("copied on the laptop".into()));
        assert!(
            wait_for(|| pc.clipboard.get() == Some(Raw::Text("copied on the laptop".into()))).await
        );

        let image = Raw::Image(crate::clipboard::RgbaImage {
            width: 2,
            height: 1,
            pixels: vec![255, 0, 0, 255, 0, 0, 255, 128],
        });
        pc.clipboard.set(image.clone());
        assert!(wait_for(|| laptop.clipboard.get() == Some(image.clone())).await);

        // Nothing bounces back: the laptop's clipboard keeps the image.
        sleep(Duration::from_millis(100)).await;
        assert_eq!(laptop.clipboard.get(), Some(image));

        // By default, only images show a notification.
        assert_eq!(
            *laptop.notifications.0.lock().unwrap(),
            [Received {
                image: true,
                from: "pc".into()
            }]
        );
        assert!(pc.notifications.0.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_link_from_another_device_is_offered_to_open() {
        let pc = start("pc").await;
        let laptop = start("laptop").await;
        let invite = pc.daemon.open_pairing().await.unwrap();
        laptop.daemon.join(&invite).await.unwrap();
        assert!(wait_for_async(|| connected(&pc.daemon)).await);

        laptop
            .clipboard
            .set(Raw::Text("https://example.com/article?id=7".into()));
        assert!(wait_for(|| !pc.notifications.2.lock().unwrap().is_empty()).await);
        let (from, link) = pc.notifications.2.lock().unwrap()[0].clone();
        assert_eq!(from, "laptop");
        assert_eq!(link.host, "example.com");
        assert_eq!(link.url, "https://example.com/article?id=7");
        // Instead of the plain notification, which text doesn't get by default anyway.
        assert!(pc.notifications.0.lock().unwrap().is_empty());

        // Text with a link in it is just text.
        laptop
            .clipboard
            .set(Raw::Text("read https://example.org later".into()));
        assert!(
            wait_for(
                || pc.clipboard.get() == Some(Raw::Text("read https://example.org later".into()))
            )
            .await
        );
        sleep(Duration::from_millis(50)).await;
        assert_eq!(pc.notifications.2.lock().unwrap().len(), 1);
        // The laptop, where it was copied, offers nothing.
        assert!(laptop.notifications.2.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn pairing_persists_and_status_reports_it() {
        let pc = start("pc").await;
        let laptop = start("laptop").await;

        let invite = pc.daemon.open_pairing().await.unwrap();
        let mut events = pc.daemon.subscribe();
        laptop.daemon.join(&invite).await.unwrap();

        let Ok(Event::Paired(device)) = events.recv().await else {
            panic!("expected a Paired event");
        };
        assert_eq!(device.name, "laptop");
        let status = pc.daemon.status().await;
        assert!(!status.pairing_open);
        assert_eq!(status.devices.len(), 1);
        assert_eq!(status.devices[0].device.id, laptop.daemon.device_id());
    }

    #[tokio::test]
    async fn a_forgotten_device_is_disconnected_and_cannot_come_back() {
        let pc = start("pc").await;
        let laptop = start("laptop").await;
        let invite = pc.daemon.open_pairing().await.unwrap();
        laptop.daemon.join(&invite).await.unwrap();
        assert!(wait_for_async(|| connected(&pc.daemon)).await);

        let forgotten = pc.daemon.forget(&laptop.daemon.device_id()).unwrap();
        assert_eq!(forgotten.name, "laptop");
        assert!(pc.daemon.status().await.devices.is_empty());
        assert!(pc.daemon.forget(&laptop.daemon.device_id()).is_err());

        // The laptop still trusts the PC and keeps redialing; the PC refuses it.
        assert!(wait_for_async(|| async { !connected(&laptop.daemon).await }).await);
        sleep(Duration::from_millis(300)).await;
        assert!(!connected(&laptop.daemon).await);
        laptop.clipboard.set(Raw::Text("after unpairing".into()));
        sleep(Duration::from_millis(100)).await;
        assert_ne!(
            pc.clipboard.get(),
            Some(Raw::Text("after unpairing".into()))
        );

        // It survives a restart: the device is gone from the file too.
        let known = pc.daemon.shared.profile.known_devices().unwrap();
        assert!(!known.contains(&laptop.daemon.device_id()));
    }

    #[tokio::test]
    async fn the_pc_can_forget_a_device_it_joined() {
        let pc = start("pc").await;
        let laptop = start("laptop").await;
        let invite = pc.daemon.open_pairing().await.unwrap();
        laptop.daemon.join(&invite).await.unwrap();
        assert!(wait_for_async(|| connected(&laptop.daemon)).await);

        laptop.daemon.forget(&pc.daemon.device_id()).unwrap();

        // Its dialer stopped: the PC sees no connection coming back.
        assert!(wait_for_async(|| async { !connected(&pc.daemon).await }).await);
        sleep(Duration::from_millis(300)).await;
        assert!(!connected(&pc.daemon).await);
        assert!(laptop.daemon.status().await.devices.is_empty());
    }

    #[tokio::test]
    async fn unknown_devices_cannot_connect() {
        let pc = start("pc").await;
        let stranger = Identity::generate().unwrap();

        let socket = TcpStream::connect(pc.daemon.local_addr()).await.unwrap();
        let mut connection = transport::connect(socket, &stranger, pc.daemon.device_id())
            .await
            .unwrap();
        let result = connection.exchange_hello(Hello::default()).await;

        assert!(result.is_err());
        assert!(pc.daemon.status().await.devices.is_empty());
    }

    #[tokio::test]
    async fn send_reaches_connected_devices() {
        let pc = start("pc").await;
        let laptop = start("laptop").await;
        let invite = pc.daemon.open_pairing().await.unwrap();
        laptop.daemon.join(&invite).await.unwrap();
        assert!(wait_for_async(|| connected(&pc.daemon)).await);

        let sent_to = pc.daemon.send(Content {
            mime_type: pasteget_core::clip::mime::TEXT.into(),
            data: b"sent with pasteget send".to_vec(),
            sensitive: false,
        });

        assert_eq!(sent_to, 1);
        assert!(
            wait_for(|| laptop.clipboard.get()
                == Some(Raw::Text("sent with pasteget send".into())))
            .await
        );
    }

    #[tokio::test]
    async fn nothing_syncs_while_paused() {
        let pc = start("pc").await;
        let laptop = start("laptop").await;
        let invite = pc.daemon.open_pairing().await.unwrap();
        laptop.daemon.join(&invite).await.unwrap();
        assert!(wait_for_async(|| connected(&pc.daemon)).await);
        assert!(wait_for_async(|| connected(&laptop.daemon)).await);

        assert!(pc.daemon.set_paused(true));
        assert!(pc.daemon.status().await.paused);

        // Copies on the paused PC stay there.
        pc.clipboard.set(Raw::Text("private".into()));
        sleep(Duration::from_millis(200)).await;
        assert_eq!(laptop.clipboard.get(), None);

        // Clips sent to the paused PC are dropped.
        laptop.clipboard.set(Raw::Text("from the laptop".into()));
        sleep(Duration::from_millis(200)).await;
        assert_eq!(pc.clipboard.get(), Some(Raw::Text("private".into())));
        assert!(pc.daemon.recent().is_empty());

        // An explicit send still goes out.
        assert_eq!(
            pc.daemon.send(Content {
                mime_type: mime::TEXT.into(),
                data: b"on purpose".to_vec(),
                sensitive: false,
            }),
            1
        );
        assert!(wait_for(|| laptop.clipboard.get() == Some(Raw::Text("on purpose".into()))).await);

        // Resumed: copies flow again.
        assert!(!pc.daemon.set_paused(false));
        pc.clipboard.set(Raw::Text("public".into()));
        assert!(wait_for(|| laptop.clipboard.get() == Some(Raw::Text("public".into()))).await);
    }

    #[tokio::test]
    async fn recent_clips_can_be_copied_again() {
        let pc = start("pc").await;
        let laptop = start("laptop").await;
        let invite = pc.daemon.open_pairing().await.unwrap();
        laptop.daemon.join(&invite).await.unwrap();
        assert!(wait_for_async(|| connected(&pc.daemon)).await);
        assert!(wait_for_async(|| connected(&laptop.daemon)).await);

        pc.clipboard.set(Raw::Text("copied here".into()));
        assert!(wait_for(|| laptop.clipboard.get() == Some(Raw::Text("copied here".into()))).await);
        laptop.clipboard.set(Raw::Text("from the laptop".into()));
        assert!(wait_for(|| pc.clipboard.get() == Some(Raw::Text("from the laptop".into()))).await);

        let recent = pc.daemon.recent();
        assert_eq!(recent.len(), 2);
        assert_eq!(&*recent[0].data, b"from the laptop");
        assert_eq!(recent[0].from.as_deref(), Some("laptop"));
        assert_eq!(&*recent[1].data, b"copied here");
        assert_eq!(recent[1].from, None);

        pc.daemon.copy_recent(recent[1].id).unwrap();
        assert!(wait_for(|| pc.clipboard.get() == Some(Raw::Text("copied here".into()))).await);
        // Not sent again: the laptop keeps its own copy.
        sleep(Duration::from_millis(100)).await;
        assert_eq!(
            laptop.clipboard.get(),
            Some(Raw::Text("from the laptop".into()))
        );

        assert!(pc.daemon.copy_recent(999).is_err());
    }

    #[tokio::test]
    async fn hotkey_actions_pause_and_send_the_clipboard() {
        let pc = start("pc").await;
        let laptop = start("laptop").await;
        let invite = pc.daemon.open_pairing().await.unwrap();
        laptop.daemon.join(&invite).await.unwrap();
        assert!(wait_for_async(|| connected(&pc.daemon)).await);

        pc.daemon.perform(Action::TogglePause).await;
        assert!(pc.daemon.is_paused());

        // Paused: copies stay here until sent on purpose.
        pc.clipboard.set(Raw::Text("only when I say so".into()));
        sleep(Duration::from_millis(100)).await;
        assert_eq!(laptop.clipboard.get(), None);
        pc.daemon.perform(Action::SendClipboard).await;
        assert!(
            wait_for(|| laptop.clipboard.get() == Some(Raw::Text("only when I say so".into())))
                .await
        );

        pc.daemon.perform(Action::TogglePause).await;
        assert!(!pc.daemon.is_paused());
        assert_eq!(
            *pc.notifications.1.lock().unwrap(),
            [
                "Syncing paused",
                "Clipboard sent to 1 device",
                "Syncing resumed"
            ]
        );
    }

    #[tokio::test]
    async fn sending_an_empty_clipboard_fails() {
        let pc = start("pc").await;

        assert!(pc.daemon.send_clipboard().await.is_err());
        pc.daemon.perform(Action::SendClipboard).await;
        assert_eq!(*pc.notifications.1.lock().unwrap(), ["Nothing to send"]);
    }

    async fn wait_for_async<F: Future<Output = bool>>(mut condition: impl FnMut() -> F) -> bool {
        for _ in 0..300 {
            if condition().await {
                return true;
            }
            sleep(Duration::from_millis(10)).await;
        }
        false
    }
}
