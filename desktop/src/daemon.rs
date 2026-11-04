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

use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use pasteget_core::clip::{Clip, mime};
use pasteget_core::echo::EchoGuard;
use pasteget_core::identity::{DeviceId, Identity};
use pasteget_core::pairing::{self, PairingInvite, PairingWindow};
use pasteget_core::proto::{Hello, PROTOCOL_VERSION, Platform};
use pasteget_core::session::{self, HANDSHAKE_TIMEOUT, Peers, RECONNECT_MIN, SessionEvents};
use pasteget_core::transport;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Mutex as AsyncMutex, broadcast, mpsc, watch};
use tokio::task::JoinSet;
use tokio::time::{sleep, timeout};

use crate::clipboard::{ClipboardBackend, ClipboardWatcher, Content, DEFAULT_POLL_INTERVAL};
use crate::config::{KnownDevice, KnownDevices, NotifyFor, Profile};
use crate::discovery::Advertiser;
use crate::notify::{Notifier, Received};

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
}

impl DaemonOptions {
    pub fn listen_on(port: u16) -> Self {
        Self {
            listen: SocketAddr::from(([0, 0, 0, 0], port)),
            advertised_ips: None,
            poll_interval: DEFAULT_POLL_INTERVAL,
            announce: true,
            notifier: None,
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
    events: broadcast::Sender<Event>,
    tasks: Mutex<JoinSet<()>>,
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
            events,
            tasks: Mutex::new(JoinSet::new()),
        });

        shared.spawn(accept_loop(Arc::clone(&shared), listener));
        shared.spawn(local_changes_loop(Arc::clone(&shared), changes));
        let joined: Vec<KnownDevice> = lock(&shared.known)
            .iter()
            .filter(|device| !device.addresses.is_empty())
            .cloned()
            .collect();
        for device in joined {
            shared.spawn(dial_loop(Arc::clone(&shared), device));
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
        Ok(Self {
            shared,
            local_addr,
            _advertiser: advertiser,
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
                    self.shared
                        .spawn(dial_loop(Arc::clone(&self.shared), device.clone()));
                    return Ok(device);
                }
                Ok(Err(e)) => last_error = e.context(format!("pairing through {address} failed")),
                Err(_) => last_error = anyhow!("{address} did not answer"),
            }
        }
        Err(last_error)
    }

    /// Sends content to every connected device. Returns how many it was queued for.
    pub fn send(&self, content: Content) -> usize {
        let clip = Clip {
            mime_type: content.mime_type,
            data: content.data,
            sensitive: content.sensitive,
            origin: self.device_id(),
        };
        self.shared.peers.broadcast(&clip, None)
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
        }
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        lock(&self.shared.tasks).abort_all();
    }
}

impl Shared {
    fn spawn(&self, task: impl Future<Output = ()> + Send + 'static) {
        let mut tasks = lock(&self.tasks);
        while tasks.try_join_next().is_some() {}
        tasks.spawn(task);
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

    fn remember(&self, device: KnownDevice) -> Result<()> {
        let mut known = lock(&self.known);
        known.insert(device);
        self.profile.save_known_devices(&known)
    }
}

/// Received clips go to the clipboard and on to the other devices.
impl SessionEvents for Shared {
    fn connected(&self, peer: DeviceId, hello: &Hello) {
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
        lock(&self.guard).applied_remote(&clip);

        let received = Received {
            image: clip.mime_type != mime::TEXT,
            from: lock(&self.known)
                .get(&peer)
                .map_or_else(|| "a paired device".to_owned(), |d| d.name.clone()),
        };
        // Sensitive clips stay as quiet as possible.
        let notifier = self
            .notifier
            .clone()
            .filter(|_| !clip.sensitive && received.wanted_by(self.notify_for));
        let (mime_type, len) = (clip.mime_type.clone(), clip.data.len());
        let applied = move |took: Duration| {
            tracing::info!(%mime_type, len, ms = took.as_millis(), "clip applied to the clipboard");
            if let Some(notifier) = notifier {
                notifier.received(received);
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

/// Sends local clipboard changes to every connected device.
async fn local_changes_loop(shared: Arc<Shared>, mut changes: mpsc::Receiver<Content>) {
    while let Some(content) = changes.recv().await {
        if !lock(&shared.guard).should_send(&content.mime_type, &content.data) {
            continue;
        }
        let clip = Clip {
            mime_type: content.mime_type,
            data: content.data,
            sensitive: content.sensitive,
            origin: shared.identity.device_id(),
        };
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
    struct Notifications(Mutex<Vec<Received>>);

    impl Notifier for Notifications {
        fn received(&self, received: Received) {
            self.0.lock().unwrap().push(received);
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
