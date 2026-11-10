// SPDX-License-Identifier: GPL-3.0-or-later

//! The daemon: accepts paired devices, keeps a session with each, and moves clips between the
//! local clipboard and every connected device.
//!
//! ```text
//!   clipboard thread ──local copies──▶ broadcast to all sessions
//!   session (per device) ──received clips──▶ clipboard thread + relay to other sessions
//! ```
//!
//! A received clip is passed on once, so that a phone paired with this computer only reaches the
//! others through it; a copy that arrives a second time is dropped by the session. Secrets and
//! 2FA codes are never passed on (`Peers::relay`).
//!
//! Devices that paired with us (phones) connect in. Devices we paired with by scanning their
//! code (`pasteget join`) are dialed out and redialed after disconnects, also at the addresses
//! mDNS finds them at, since their address may change.
//!
//! When `settings.toml` turns it on, the clipboard history ([`History`]) records what goes
//! through the *Recent* list, sensitive clips included (it only counts them).

use std::collections::{HashMap, HashSet};
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, Result, bail};
use pasteget_core::clip::{Clip, Kind, mime};
use pasteget_core::detect;
use pasteget_core::echo::EchoGuard;
use pasteget_core::history::{self as store, ItemId};
use pasteget_core::identity::{DeviceId, Identity};
use pasteget_core::pairing::{
    self, Dial, Opening, PairedDevice, PairingError, PairingInvite, PairingRequest, PairingWindow,
};
use pasteget_core::proto::{Hello, PROTOCOL_VERSION, Platform, RefusalReason};
use pasteget_core::session::{self, HANDSHAKE_TIMEOUT, Peers, RECONNECT_MIN, SessionEvents};
use pasteget_core::transport;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{broadcast, mpsc, watch};
use tokio::task::{AbortHandle, JoinSet};
use tokio::time::{sleep, timeout};

use crate::admission::{Admission, Ticket};
use crate::clipboard::{ClipboardBackend, ClipboardWatcher, Content, DEFAULT_POLL_INTERVAL};
use crate::config::{
    HistorySettings, KnownDevice, KnownDevices, LoadedDevices, MAX_ADDRESSES, NotifyFor, Profile,
    Settings,
};
use crate::discovery::{self, Advertiser, Announced};
use crate::history::History;
use crate::hotkeys::{self, Action, Hotkeys};
use crate::i18n::{french, tr};
use crate::notify::{Notifier, Received};
use crate::recent::{RecentClip, RecentClips};

/// Time an incoming connection has for its TLS handshake, and a device that isn't paired for its
/// pairing request: anyone on the network can open one ([`Admission`]).
const UNKNOWN_DEADLINE: Duration = Duration::from_secs(3);
/// Time a session has to tell a device it was unpaired before its dialer is stopped.
const UNPAIR_FAREWELL: Duration = Duration::from_secs(3);

/// How to run a daemon.
#[derive(Clone, Debug)]
pub struct DaemonOptions {
    /// Address to listen on. Port 0 picks a free port.
    pub listen: SocketAddr,
    /// Addresses put in pairing QR codes. `None` uses this machine's network interfaces.
    pub advertised_ips: Option<Vec<IpAddr>>,
    pub poll_interval: Duration,
    /// Announce the daemon on the local network with mDNS, and look there for the computers it
    /// joined.
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
    /// Set by [`Daemon::request_quit`].
    quit: watch::Sender<bool>,
}

struct Shared {
    identity: Identity,
    device_name: String,
    profile: Profile,
    listen_port: u16,
    advertised_ips: Option<Vec<IpAddr>>,
    known: Mutex<KnownDevices>,
    /// Locked only for quick checks, never across network I/O: status reads it.
    pairing: Mutex<Option<PairingWindow>>,
    /// Incoming connections not known yet to come from a paired device.
    admission: Arc<Admission>,
    peers: Peers,
    guard: Mutex<EchoGuard>,
    clipboard: ClipboardWatcher,
    notifier: Option<Arc<dyn Notifier>>,
    /// Changed by [`Daemon::reload_settings`].
    notify_for: Mutex<NotifyFor>,
    offer_to_open_links: AtomicBool,
    /// What the daemon started with, which only a restart changes: its name, port and hotkeys.
    started_with: Settings,
    paused: AtomicBool,
    recent: Mutex<RecentClips>,
    /// The clipboard history, while `settings.toml` has it on and it could open.
    history: Mutex<Option<Arc<History>>>,
    events: broadcast::Sender<Event>,
    tasks: Mutex<JoinSet<()>>,
    /// The redial loops of devices we joined, to stop when one is unpaired.
    dialers: Mutex<HashMap<DeviceId, Dialer>>,
    /// Devices the user was told run another protocol version, once each while this runs.
    told_other_version: Mutex<HashSet<DeviceId>>,
    /// Some paired devices couldn't be read at start ([`LoadedDevices::set_aside`]): a device
    /// this one doesn't know may be one of them, so it isn't told it was unpaired, unless it was
    /// forgotten while this runs.
    devices_set_aside: bool,
    forgotten: Mutex<HashSet<DeviceId>>,
}

/// Keeps a session with a device we joined.
struct Dialer {
    task: AbortHandle,
    /// Where it dials. Any update also makes it dial now rather than after its backoff.
    addresses: watch::Sender<Vec<SocketAddr>>,
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
        let LoadedDevices {
            devices: known,
            set_aside,
        } = profile.load_known_devices()?;
        let listener = TcpListener::bind(options.listen)
            .await
            .with_context(|| format!("could not listen on {}", options.listen))?;
        let local_addr = listener.local_addr()?;

        let (changes_tx, changes) = mpsc::channel(8);
        let clipboard = ClipboardWatcher::spawn(backend, options.poll_interval, changes_tx)?;
        let (events, _) = broadcast::channel(16);

        // What's copied while a device is away waits for it.
        let peers = Peers::new();
        for device in known.iter() {
            peers.expect(device.id);
        }
        let shared = Arc::new(Shared {
            guard: Mutex::new(EchoGuard::new(identity.device_id())),
            identity,
            device_name: settings.device_name.clone(),
            profile,
            listen_port: local_addr.port(),
            advertised_ips: options.advertised_ips,
            known: Mutex::new(known),
            pairing: Mutex::new(None),
            admission: Arc::default(),
            peers,
            clipboard,
            notifier: options.notifier,
            notify_for: Mutex::new(settings.notify),
            offer_to_open_links: AtomicBool::new(settings.offer_to_open_links),
            started_with: settings.clone(),
            paused: AtomicBool::new(false),
            recent: Mutex::new(RecentClips::default()),
            history: Mutex::new(None),
            events,
            tasks: Mutex::new(JoinSet::new()),
            dialers: Mutex::new(HashMap::new()),
            told_other_version: Mutex::new(HashSet::new()),
            devices_set_aside: set_aside,
            forgotten: Mutex::new(HashSet::new()),
        });
        if set_aside && let Some(notifier) = &shared.notifier {
            notifier.notice(
                tr(
                    "paste-get couldn't read all its paired devices: the file is kept as devices.toml.bad. Pair the missing ones again, or fix it and rename it devices.toml before paste-get restarts",
                    "paste-get n'a pas pu lire tous ses appareils appairés : le fichier est gardé sous le nom devices.toml.bad. Appaire à nouveau ceux qui manquent, ou corrige-le et renomme-le devices.toml avant que paste-get redémarre",
                )
                .to_owned(),
            );
        }

        // Opened once the port is this daemon's: another daemon of the profile would have it.
        shared.apply_history_settings(&settings.history);

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
                let name = if shared.started_with.announce_name {
                    &shared.device_name
                } else {
                    ""
                };
                Advertiser::start(
                    shared.identity.device_id(),
                    name,
                    local_addr.port(),
                )
            })
            .transpose()
            .unwrap_or_else(|e| {
                tracing::warn!(error = %e, "not announcing on the network");
                None
            });
        // The computers this one joined may have moved since.
        if let Some(advertiser) = &advertiser {
            match advertiser.browse() {
                Ok(events) => {
                    shared.spawn(found_loop(Arc::clone(&shared), events));
                }
                Err(e) => tracing::warn!(error = %format!("{e:#}"), "not looking for devices"),
            }
        }

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
            hotkeys::start(&settings.hotkeys.bindings(), shared.profile.name(), pressed)
        } else {
            None
        };

        Ok(Self {
            shared,
            local_addr,
            _advertiser: advertiser,
            _hotkeys: hotkeys,
            quit: watch::channel(false).0,
        })
    }

    /// Asks whatever runs this daemon (`pasteget daemon`, the tray) to stop it and exit: a newer
    /// paste-get takes over. See [`Daemon::quit_signal`].
    pub fn request_quit(&self) {
        tracing::info!("asked to quit");
        self.quit.send_replace(true);
    }

    /// Turns `true` once [`Daemon::request_quit`] was called. Holding it doesn't keep the daemon
    /// alive.
    pub fn quit_signal(&self) -> watch::Receiver<bool> {
        self.quit.subscribe()
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
        *lock(&self.shared.pairing) = Some(window);
        tracing::info!("pairing window opened");
        Ok(invite)
    }

    /// Pairs with the device that shows `invite`, then keeps a connection to it. Only the
    /// invite's addresses on the local network are dialed, or on this computer itself (another
    /// profile).
    pub async fn join(&self, invite: &PairingInvite) -> Result<KnownDevice> {
        let dial = Dial::LocalNetwork;
        let (paired, worked) = pairing::pair_with_any_address(
            &self.shared.identity,
            invite,
            &self.shared.device_name,
            dial,
        )
        .await
        .context("could not pair")?;
        // The address that worked is dialed first.
        let mut addresses = vec![worked];
        addresses.extend(
            invite
                .addresses
                .iter()
                .filter(|a| **a != worked && dial.allows(**a)),
        );
        let device = KnownDevice::joined(paired, addresses);
        self.shared.remember(device.clone())?;
        self.shared.start_dialer(device.clone());
        Ok(device)
    }

    /// Sends content to every connected device, even while paused. Returns how many it was
    /// queued for.
    pub fn send(&self, content: Content) -> usize {
        let clip = Clip {
            mime_type: content.mime_type,
            data: content.data,
            sensitive: content.sensitive,
            origin: self.device_id(),
            kind: Kind::Copy,
            origin_clip_id: None,
        };
        self.shared
            .remember_clip(&clip, store::Context::new(store::Source::SentByHand));
        self.shared.peers.broadcast(&clip, None)
    }

    /// Unpairs a device: it's no longer trusted, its connection ends now, and it can only come
    /// back by pairing again. If it's connected, it's told, and forgets this computer too.
    pub fn forget(&self, id: &DeviceId) -> Result<KnownDevice> {
        let device = {
            let mut known = lock(&self.shared.known);
            let device = known.remove(id).context("no paired device with this ID")?;
            self.shared.profile.save_known_devices(&known)?;
            device
        };
        lock(&self.shared.forgotten).insert(*id);
        self.shared.peers.unexpect(id);
        let dialer = lock(&self.shared.dialers).remove(id);
        let told = self.shared.peers.unpair(id);
        if let Some(dialer) = dialer {
            if told {
                // Its session says so, then ends, and the dialer with it. Stopped anyway in case
                // the session ended first for another reason.
                self.shared.spawn(async move {
                    sleep(UNPAIR_FAREWELL).await;
                    dialer.task.abort();
                });
            } else {
                dialer.task.abort();
            }
        }
        tracing::info!(device = %device.name, id = %device.id, told, "unpaired");
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

    /// The clipboard history, or an error saying it is off.
    pub fn history(&self) -> Result<Arc<History>> {
        self.shared.history().context(
            "the clipboard history is off: set `enabled = true` under `[history]` in \
             settings.toml",
        )
    }

    /// Puts an item of the history back on the clipboard. It isn't sent again: the devices may
    /// be offered it with `send`.
    pub fn copy_from_history(&self, id: ItemId) -> Result<()> {
        let content = self
            .history()?
            .content(id)?
            .context("that item is gone, or wasn't kept whole")?;
        self.shared.clipboard.write(content, |_| {})
    }

    /// Reads `settings.toml` again and applies what can change while running: notifications,
    /// links and the history. Returns whether the rest (name, port, hotkeys) changed, which
    /// takes a restart.
    pub fn reload_settings(&self) -> Result<bool> {
        let settings = self.shared.profile.settings()?;
        self.shared.apply_history_settings(&settings.history);
        *lock(&self.shared.notify_for) = settings.notify;
        self.shared
            .offer_to_open_links
            .store(settings.offer_to_open_links, Ordering::Relaxed);
        let started = &self.shared.started_with;
        let restart_needed = settings.device_name != started.device_name
            || settings.port != started.port
            || settings.hotkeys != started.hotkeys;
        tracing::info!(restart_needed, "settings reloaded");
        Ok(restart_needed)
    }

    /// Never waits on pairing or on the network.
    pub async fn status(&self) -> Status {
        let pairing_open = self.shared.pairing_open();
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
        let (addresses, watched) = watch::channel(device.addresses);
        let task = self.spawn(dial_loop(Arc::clone(self), id, watched));
        if let Some(previous) = lock(&self.dialers).insert(id, Dialer { task, addresses }) {
            previous.task.abort();
        }
    }

    /// `announced` on the network: if it's a computer this one joined, it's dialed there now,
    /// then at the addresses it had. Returns whether it is one.
    ///
    /// Found addresses are only tried: one is remembered once a connection through it proved
    /// the device's identity ([`SessionEvents::reached`]), so a forged announcement can't push
    /// the real addresses out.
    fn found(&self, announced: &Announced) -> bool {
        let Some(known) = lock(&self.known)
            .get(&announced.id)
            .map(|device| device.addresses.clone())
        else {
            return false;
        };
        let found = announced
            .ips
            .iter()
            .take(MAX_ADDRESSES)
            .map(|ip| SocketAddr::new(*ip, announced.port));
        match lock(&self.dialers).get(&announced.id) {
            Some(dialer) => {
                dialer
                    .addresses
                    .send_replace(session::found_first(found, known));
                true
            }
            None => false,
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
        let addresses = if_addrs::get_if_addrs()
            .context("could not list network interfaces")?
            .into_iter()
            .filter(|interface| {
                interface.is_oper_up() && !interface.is_loopback() && !interface.is_link_local()
            })
            // The listener is IPv4-only for now.
            .filter(|interface| interface.ip().is_ipv4())
            .map(|interface| LocalAddress {
                ip: interface.ip(),
                point_to_point: interface.is_p2p,
                interface: interface.name,
            })
            .collect();
        Ok(likeliest_first(addresses))
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
            kind: Kind::Copy,
            origin_clip_id: None,
        };
        self.remember_clip(&clip, store::Context::new(store::Source::SentByHand));
        let sent_to = self.peers.broadcast(&clip, None);
        tracing::info!(mime_type = %clip.mime_type, len = clip.data.len(), sensitive = clip.sensitive, sent_to, "clipboard sent");
        Ok(sent_to)
    }

    async fn perform(&self, action: Action) {
        let summary = match action {
            Action::TogglePause => {
                if self.set_paused(!self.paused.load(Ordering::Relaxed)) {
                    tr("Syncing paused", "Synchro en pause").to_owned()
                } else {
                    tr("Syncing resumed", "Synchro reprise").to_owned()
                }
            }
            Action::SendClipboard => match self.send_clipboard().await {
                Ok(0) => tr("No device is connected", "Aucun appareil connecté").to_owned(),
                Ok(1) => tr(
                    "Clipboard sent to 1 device",
                    "Presse-papiers envoyé à 1 appareil",
                )
                .to_owned(),
                Ok(n) if french() => format!("Presse-papiers envoyé à {n} appareils"),
                Ok(n) => format!("Clipboard sent to {n} devices"),
                Err(e) => {
                    tracing::info!(error = %e, "clipboard not sent");
                    tr("Nothing to send", "Rien à envoyer").to_owned()
                }
            },
        };
        if let Some(notifier) = &self.notifier {
            notifier.notice(summary);
        }
    }

    /// Adds a clip to the recent list and to the history. Sensitive clips are kept by neither:
    /// the history counts them.
    fn remember_clip(&self, clip: &Clip, context: store::Context) {
        let from = context.via.map(|id| self.name_of(&id));
        if let Some(history) = self.history() {
            history.record(clip, context);
        }
        if clip.sensitive {
            return;
        }
        lock(&self.recent).push(&clip.mime_type, &clip.data, from, SystemTime::now());
    }

    fn history(&self) -> Option<Arc<History>> {
        lock(&self.history).clone()
    }

    /// Opens the history, applies new settings to it, or closes it, as `settings` say. A
    /// history that can't open leaves syncing as it was, and says so.
    fn apply_history_settings(&self, settings: &HistorySettings) {
        if !settings.enabled {
            if lock(&self.history).take().is_some() {
                tracing::info!("history: off");
            }
            return;
        }
        if let Some(history) = self.history() {
            if let Err(e) = history.set_settings(settings) {
                tracing::warn!(error = %format!("{e:#}"), "history: new settings not applied");
            }
            return;
        }
        let dir = self.profile.history_dir();
        match History::open(&dir, self.identity.device_id(), settings) {
            Ok(history) => {
                for device in lock(&self.known).iter() {
                    history.name_device(device.id, &device.name, 0);
                }
                tracing::info!(dir = %dir.display(), "history: on");
                *lock(&self.history) = Some(Arc::new(history));
            }
            Err(e) => {
                tracing::warn!(error = %format!("{e:#}"), "history: could not open, running without");
                if let Some(notifier) = &self.notifier {
                    notifier.notice(
                        tr(
                            "paste-get couldn't open its clipboard history: it syncs without it",
                            "paste-get n'a pas pu ouvrir son historique du presse-papiers : il synchronise sans",
                        )
                        .to_owned(),
                    );
                }
            }
        }
    }

    fn name_of(&self, id: &DeviceId) -> String {
        lock(&self.known)
            .get(id)
            .map_or_else(|| "a paired device".to_owned(), |d| d.name.clone())
    }

    fn remember(&self, device: KnownDevice) -> Result<()> {
        self.peers.expect(device.id);
        let mut known = lock(&self.known);
        known.insert(device);
        self.profile.save_known_devices(&known)
    }

    fn pairing_open(&self) -> bool {
        lock(&self.pairing)
            .as_ref()
            .is_some_and(|window| window.is_open(Instant::now()))
    }

    /// Answers a pairing request from the open window. The window is locked only to check the
    /// secret, never across network I/O, so the single use still holds when requests come in at
    /// once.
    async fn answer_pairing<S>(&self, request: PairingRequest<S>) -> Result<()>
    where
        S: AsyncRead + AsyncWrite + Unpin,
    {
        let peer = request.peer();
        let check = |secret: &[u8]| {
            let verdict = check_secret(&mut lock(&self.pairing), secret);
            if verdict.is_ok() {
                // Paired again: a session it may still have here ends, it starts a new one.
                self.peers.disconnect(&peer);
            }
            verdict
        };
        let result = request.answer(check, &self.device_name).await;
        self.paired(result)
    }

    /// Says that a pairing attempt failed, and returns why.
    fn pairing_failed(&self, e: PairingError) -> anyhow::Error {
        // Closed: the code was used meanwhile, maybe by this device through another of its
        // addresses.
        if !matches!(e, PairingError::Closed) {
            let _ = self.events.send(Event::PairingFailed(e.to_string()));
        }
        e.into()
    }

    /// Stores a device that just paired, or paired again, and says so.
    fn paired(&self, result: Result<PairedDevice, PairingError>) -> Result<()> {
        let paired = result.map_err(|e| self.pairing_failed(e))?;
        // Paired again: it keeps its addresses, if this computer dials it.
        let known = lock(&self.known).get(&paired.device_id).cloned();
        let again = known.is_some();
        let device = match known {
            Some(known) => KnownDevice {
                name: paired.device_name,
                ..known
            },
            None => KnownDevice::accepted(paired),
        };
        self.remember(device.clone())?;
        tracing::info!(device = %device.name, id = %device.id, again, "paired");
        let _ = self.events.send(Event::Paired(device));
        Ok(())
    }
}

/// Received clips go to the clipboard, and on to the other devices unless they are secrets.
impl SessionEvents for Shared {
    /// A computer this one joined answered at `address`: it's dialed there first from now on,
    /// also after a restart.
    fn reached(&self, peer: DeviceId, address: SocketAddr) {
        let mut known = lock(&self.known);
        if known.reached(&peer, address) {
            tracing::info!(id = %peer, %address, "reached at a new address");
            if let Err(e) = self.profile.save_known_devices(&known) {
                tracing::warn!(error = %format!("{e:#}"), "could not save the paired devices");
            }
        }
    }

    fn connected(&self, peer: DeviceId, hello: &Hello) {
        {
            let mut known = lock(&self.known);
            // Unpaired while this connection was being set up: [`Daemon::forget`] came too early
            // to end it.
            if !known.contains(&peer) {
                drop(known);
                self.peers.unpair(&peer);
                return;
            }
            // Renamed there since it last connected.
            if known.rename(&peer, &hello.device_name) {
                tracing::info!(device = %hello.device_name, id = %peer, "device renamed");
                if let Err(e) = self.profile.save_known_devices(&known) {
                    tracing::warn!(error = %format!("{e:#}"), "could not save the paired devices");
                }
            }
        }
        if let Some(history) = self.history() {
            history.name_device(peer, &hello.device_name, hello.platform);
        }
        tracing::info!(device = %hello.device_name, id = %peer, "connected");
    }

    fn disconnected(&self, peer: DeviceId) {
        tracing::info!(id = %peer, "disconnected");
    }

    /// Says so once per device: it retries every few seconds, and nothing changes until one of
    /// them is updated.
    fn version_mismatch(&self, peer: DeviceId, _version: u32) {
        if !lock(&self.told_other_version).insert(peer) {
            return;
        }
        let name = self.name_of(&peer);
        if let Some(notifier) = &self.notifier {
            notifier.notice(if french() {
                format!("{name} utilise une autre version de paste-get : mets les deux à jour")
            } else {
                format!("{name} runs another version of paste-get: update both")
            });
        }
    }

    /// A device unpaired this computer: forget it too. A phone or a computer that joined this one
    /// says so in its session; a computer this one joined refuses its dialer, or says so too. Its
    /// dialer, if any, ends by itself. The tray shows the device gone at its next refresh.
    fn unpaired_by(&self, peer: DeviceId) {
        let removed = {
            let mut known = lock(&self.known);
            let removed = known.remove(&peer);
            if removed.is_some()
                && let Err(e) = self.profile.save_known_devices(&known)
            {
                tracing::warn!(error = %format!("{e:#}"), "could not save the paired devices");
            }
            removed
        };
        lock(&self.dialers).remove(&peer);
        self.peers.unexpect(&peer);
        if let Some(device) = removed {
            tracing::info!(device = %device.name, id = %device.id, "unpaired by the device");
            if let Some(notifier) = &self.notifier {
                notifier.notice(if french() {
                    format!("{} a désappairé cet ordinateur", device.name)
                } else {
                    format!("{} unpaired this computer", device.name)
                });
            }
        }
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
        self.remember_clip(&clip, store::Context::received_from(peer));

        let received = Received {
            image: clip.mime_type != mime::TEXT,
            from: self.name_of(&peer),
            code_from: match &clip.kind {
                Kind::OneTimeCode { app } => Some(app.clone()),
                Kind::Copy => None,
            },
        };
        let notify_for = *lock(&self.notify_for);
        // A link gets a notification of its own, which opens it when clicked.
        let offer_link = self.offer_to_open_links.load(Ordering::Relaxed)
            && notify_for != NotifyFor::Off
            && !clip.sensitive;
        let link = clip.as_text().filter(|_| offer_link).and_then(detect::link);
        // Sensitive clips stay as quiet as possible: a one-time code only says where it's from.
        let quiet = clip.sensitive && received.code_from.is_none();
        let notifier = self
            .notifier
            .clone()
            .filter(|_| !quiet && (link.is_some() || received.wanted_by(notify_for)));
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
        let relayed = self.peers.relay(&clip, peer);
        if relayed > 0 {
            tracing::debug!(relayed, "clip passed on");
        }
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
                // Closed at once.
                let Some(ticket) = shared.admission.admit(address.ip()) else {
                    tracing::debug!(%address, "connection refused: too many from this address");
                    continue;
                };
                let shared_for_task = Arc::clone(&shared);
                shared.spawn(async move {
                    let shared = Arc::clone(&shared_for_task);
                    if let Err(e) = handle_incoming(shared, socket, ticket).await {
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

/// What an incoming connection is, once past the part anyone on the network can reach.
enum Opened<C, R> {
    /// A paired device: its connection, past the TLS handshake.
    Paired(C),
    /// A device that isn't paired sent its pairing request, to answer.
    Pairing(R),
}

async fn handle_incoming(shared: Arc<Shared>, socket: TcpStream, mut ticket: Ticket) -> Result<()> {
    // Anyone can get this far: the TLS handshake, then, for a device that isn't paired, its
    // pairing request or a refusal. It's short, and cut shorter when newer connections need the
    // room. Nothing is locked while waiting: a device that stays silent holds up no other.
    let reached = async {
        let unverified = timeout(
            UNKNOWN_DEADLINE,
            transport::accept(socket, &shared.identity),
        )
        .await
        .context("TLS handshake timed out")??;
        let peer = unverified.peer();
        if lock(&shared.known).contains(&peer) {
            let connection = unverified.authorize(|id| lock(&shared.known).contains(id))?;
            return Ok(Opened::Paired(connection));
        }
        if !shared.pairing_open() {
            // Said, so that a phone unpaired here stops dialing and tells its user. Not to one
            // that may be among the paired devices that couldn't be read: it would forget this
            // computer.
            let reason = if shared.devices_set_aside && !lock(&shared.forgotten).contains(&peer) {
                RefusalReason::Unspecified
            } else {
                RefusalReason::NotPaired
            };
            let _ = timeout(UNKNOWN_DEADLINE, unverified.refuse(reason)).await;
            bail!("refused device {peer}: not paired and no pairing code is shown");
        }
        match timeout(UNKNOWN_DEADLINE, pairing::read_request(unverified)).await {
            Ok(Ok(request)) => Ok(Opened::Pairing(request)),
            Ok(Err(e)) => Err(shared.pairing_failed(e)),
            Err(_) => bail!("device {peer} did not send its pairing request in time"),
        }
    };
    let opened = tokio::select! {
        opened = reached => opened?,
        () = ticket.closed() => bail!("closed to make room for newer connections"),
    };

    let connection = match opened {
        Opened::Paired(connection) => connection,
        // Not cut short any more: once its secret is checked, the device must hear the answer.
        Opened::Pairing(request) => return shared.answer_pairing(request).await,
    };
    // Proven paired: no longer limited, and places are kept for its address.
    ticket.proven_paired();
    let opening = timeout(HANDSHAKE_TIMEOUT, pairing::read_opening(connection))
        .await
        .context("the device sent nothing in time")??;
    match opening {
        Opening::Hello(connection, hello) => {
            session::answer_session(connection, hello, shared.hello(), &shared.peers, &*shared)
                .await?;
            Ok(())
        }
        // It forgot this computer and scanned its pairing code: it pairs again.
        Opening::PairRequest(request) => shared.answer_pairing(request).await,
    }
}

/// An address of this PC, and the network interface it is on.
#[derive(Debug)]
struct LocalAddress {
    interface: String,
    /// VPN tunnels are point-to-point.
    point_to_point: bool,
    ip: IpAddr,
}

/// The addresses for a pairing code, the likeliest to reach this PC from a phone on the same
/// network first: home networks (192.168.x, then 10.x), then 172.16-31.x (often Docker or WSL),
/// then the rest, and interfaces that look virtual (VMs, containers, VPNs) last. Only addresses
/// on the local network: the phone dials no other.
fn likeliest_first(mut addresses: Vec<LocalAddress>) -> Vec<IpAddr> {
    addresses.retain(|address| pairing::on_local_network(address.ip));
    addresses.sort_by_key(|address| (likelihood_rank(address), address.ip));
    let mut ips = Vec::new();
    for address in addresses {
        if !ips.contains(&address.ip) {
            ips.push(address.ip);
        }
    }
    ips
}

/// Lower is likelier.
fn likelihood_rank(address: &LocalAddress) -> u8 {
    if address.point_to_point || looks_virtual(&address.interface) {
        return 4;
    }
    match address.ip {
        IpAddr::V4(ip) => match ip.octets() {
            [192, 168, ..] => 0,
            [10, ..] => 1,
            [172, 16..=31, ..] => 2,
            _ => 3,
        },
        IpAddr::V6(_) => 3,
    }
}

/// Interfaces of virtual machines, containers and VPNs, by the names they get on Linux
/// (`docker0`, `virbr0`, `wg0`…) and on Windows (`vEthernet (WSL)`, `VMware Network Adapter
/// VMnet8`…).
pub(crate) fn looks_virtual(interface: &str) -> bool {
    const PREFIXES: &[&str] = &[
        "docker",
        "br-",
        "veth",
        "virbr",
        "vboxnet",
        "vmnet",
        "vnet",
        "tun",
        "tap",
        "wg",
        "tailscale",
        "zt",
        "lxc",
        "lxd",
        "podman",
        "cni",
        "utun",
    ];
    const PARTS: &[&str] = &[
        "vethernet",
        "hyper-v",
        "wsl",
        "virtualbox",
        "vmware",
        "docker",
        "vpn",
        "wireguard",
        "tailscale",
        "zerotier",
        "tap-windows",
    ];
    let name = interface.to_lowercase();
    PREFIXES.iter().any(|prefix| name.starts_with(prefix))
        || PARTS.iter().any(|part| name.contains(part))
}

/// Checks a pairing secret against the window, which closes once used or expired.
fn check_secret(slot: &mut Option<PairingWindow>, secret: &[u8]) -> Result<(), PairingError> {
    let now = Instant::now();
    let verdict = match slot.as_mut() {
        Some(window) => window.check(secret, now),
        None => Err(PairingError::Closed),
    };
    if !slot.as_ref().is_some_and(|window| window.is_open(now)) {
        *slot = None;
    }
    verdict
}

/// Keeps a connection to a device we joined, redialing with backoff, at `addresses`.
async fn dial_loop(shared: Arc<Shared>, id: DeviceId, addresses: watch::Receiver<Vec<SocketAddr>>) {
    let hello = shared.hello();
    session::keep_connected(
        &shared.identity,
        id,
        addresses,
        &hello,
        &shared.peers,
        &*shared,
    )
    .await;
}

/// Tells the dialers where mDNS finds their computers.
async fn found_loop(shared: Arc<Shared>, events: mdns_sd::Receiver<mdns_sd::ServiceEvent>) {
    while let Some(announced) = discovery::next_announced(&events).await {
        if shared.found(&announced) {
            tracing::debug!(id = %announced.id, "found on the network");
        }
    }
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
            kind: Kind::Copy,
            origin_clip_id: None,
        };
        shared.remember_clip(&clip, store::Context::new(store::Source::CopiedHere));
        let sent_to = shared.peers.broadcast(&clip, None);
        tracing::info!(mime_type = %clip.mime_type, len = clip.data.len(), sensitive = clip.sensitive, sent_to, "local copy");
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
        start_in(home, profile).await
    }

    /// Stops the daemon and starts it again from the same profile, on a new port.
    async fn restart(test: TestDaemon) -> TestDaemon {
        let profile = test.daemon.shared.profile.clone();
        let home = test._home;
        drop(test.daemon);
        start_in(home, profile).await
    }

    async fn start_in(home: tempfile::TempDir, profile: Profile) -> TestDaemon {
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
                from: "pc".into(),
                code_from: None,
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

    /// The joined computer comes back at another address, as after its IP address changed, and
    /// mDNS finds it there.
    #[tokio::test]
    async fn a_joined_computer_found_at_a_new_address_is_redialed_and_remembered() {
        let pc = start("pc").await;
        let laptop = start("laptop").await;
        let pc_id = pc.daemon.device_id();
        let invite = pc.daemon.open_pairing().await.unwrap();
        laptop.daemon.join(&invite).await.unwrap();
        assert!(wait_for_async(|| connected(&laptop.daemon)).await);
        let old = pc.daemon.local_addr();

        let pc = restart(pc).await;
        let new = pc.daemon.local_addr();
        assert_ne!(new, old);
        assert!(wait_for_async(|| async { !connected(&laptop.daemon).await }).await);
        let at_new_address = Announced {
            id: pc_id,
            ips: vec![IpAddr::V4(Ipv4Addr::LOCALHOST)],
            port: new.port(),
        };
        assert!(laptop.daemon.shared.found(&at_new_address));

        assert!(wait_for_async(|| connected(&laptop.daemon)).await);
        // The address that worked goes first, also in the file, for the next start.
        let saved = laptop.daemon.shared.profile.known_devices().unwrap();
        assert_eq!(saved.get(&pc_id).unwrap().addresses, [new, old]);
        assert_eq!(
            laptop.daemon.status().await.devices[0].device.addresses,
            [new, old]
        );
        // Announcements of devices it doesn't dial change nothing.
        let phone = Identity::generate().unwrap().device_id();
        assert!(!laptop.daemon.shared.found(&Announced {
            id: phone,
            ..at_new_address.clone()
        }));
        assert!(!pc.daemon.shared.found(&Announced {
            id: laptop.daemon.device_id(),
            ..at_new_address
        }));
    }

    /// A forged announcement is tried, but doesn't replace the addresses that work.
    #[tokio::test]
    async fn a_forged_announcement_is_not_remembered() {
        let pc = start("pc").await;
        let laptop = start("laptop").await;
        let pc_id = pc.daemon.device_id();
        let invite = pc.daemon.open_pairing().await.unwrap();
        laptop.daemon.join(&invite).await.unwrap();
        assert!(wait_for_async(|| connected(&laptop.daemon)).await);
        // Another paste-get answers there: the TLS handshake fails, as it isn't the PC.
        let impostor = start("impostor").await;

        assert!(laptop.daemon.shared.found(&Announced {
            id: pc_id,
            ips: vec![IpAddr::V4(Ipv4Addr::LOCALHOST)],
            port: impostor.daemon.local_addr().port(),
        }));
        pc.daemon
            .shared
            .peers
            .disconnect(&laptop.daemon.device_id());

        assert!(wait_for_async(|| connected(&laptop.daemon)).await);
        sleep(Duration::from_millis(100)).await;
        let saved = laptop.daemon.shared.profile.known_devices().unwrap();
        assert_eq!(
            saved.get(&pc_id).unwrap().addresses,
            [pc.daemon.local_addr()]
        );
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

        // The laptop redials, is told it was unpaired, forgets the PC too and says so.
        assert!(wait_for_async(|| async { laptop.daemon.status().await.devices.is_empty() }).await);
        assert!(
            wait_for(|| {
                laptop
                    .notifications
                    .1
                    .lock()
                    .unwrap()
                    .contains(&"pc unpaired this computer".to_owned())
            })
            .await
        );
        let known = laptop.daemon.shared.profile.known_devices().unwrap();
        assert!(!known.contains(&pc.daemon.device_id()));
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

        // The PC is told: it forgets the laptop too, and says so.
        assert!(wait_for_async(|| async { pc.daemon.status().await.devices.is_empty() }).await);
        assert!(
            pc.notifications
                .1
                .lock()
                .unwrap()
                .contains(&"laptop unpaired this computer".to_owned())
        );
        let known = pc.daemon.shared.profile.known_devices().unwrap();
        assert!(!known.contains(&laptop.daemon.device_id()));
        // Its dialer stopped: no connection comes back.
        sleep(Duration::from_millis(1500)).await;
        assert!(!connected(&pc.daemon).await);
        assert!(laptop.daemon.status().await.devices.is_empty());
        assert!(laptop.notifications.1.lock().unwrap().is_empty());
    }

    async fn pair_as(
        pc: &Daemon,
        device: &Identity,
        invite: &PairingInvite,
        name: &str,
    ) -> Result<PairedDevice, PairingError> {
        let socket = TcpStream::connect(pc.local_addr()).await.unwrap();
        pairing::pair_with(socket, device, invite, name).await
    }

    struct NoEvents;

    impl SessionEvents for NoEvents {
        fn clip_received(&self, _peer: DeviceId, _clip: Clip) {}
    }

    /// Records who unpaired this side.
    #[derive(Default)]
    struct UnpairedBy(Mutex<Vec<DeviceId>>);

    impl SessionEvents for UnpairedBy {
        fn clip_received(&self, _peer: DeviceId, _clip: Clip) {}

        fn unpaired_by(&self, peer: DeviceId) {
            self.0.lock().unwrap().push(peer);
        }
    }

    /// Forgotten while its connection was being set up (after the TLS handshake checked it was
    /// paired, before the session started), where [`Daemon::forget`] can't end it yet: the
    /// session tells the device itself.
    #[tokio::test]
    async fn a_device_forgotten_while_it_connects_is_told() {
        let pc = start("pc").await;
        let shared = Arc::clone(&pc.daemon.shared);
        let phone = Identity::generate().unwrap();
        let (phone_io, pc_io) = tokio::io::duplex(64 * 1024);
        let (client, server) = tokio::join!(
            transport::connect(phone_io, &phone, pc.daemon.device_id()),
            async {
                // Still paired when its key was checked.
                transport::accept(pc_io, &shared.identity)
                    .await?
                    .authorize(|_| true)
            }
        );
        let phone_hello = Hello {
            protocol_version: PROTOCOL_VERSION,
            device_name: "POCO".into(),
            ..Hello::default()
        };
        let (phone_peers, phone_events) = (Peers::new(), UnpairedBy::default());

        let phone_side =
            session::run_session(client.unwrap(), phone_hello, &phone_peers, &phone_events);
        let pc_side = async {
            let Ok(Opening::Hello(connection, hello)) =
                pairing::read_opening(server.unwrap()).await
            else {
                panic!("expected a Hello");
            };
            session::answer_session(connection, hello, shared.hello(), &shared.peers, &*shared)
                .await
        };
        let sessions = async { tokio::join!(phone_side, pc_side) };
        let (phone_ended, pc_ended) = timeout(Duration::from_secs(5), sessions)
            .await
            .expect("both sessions should end");

        assert!(matches!(pc_ended, Err(session::SessionError::Unpaired)));
        assert!(matches!(
            phone_ended,
            Err(session::SessionError::UnpairedByPeer)
        ));
        assert_eq!(*phone_events.0.lock().unwrap(), [pc.daemon.device_id()]);
        // It never counted as connected, nor came back as a device.
        assert!(!shared.peers.is_connected(&phone.device_id()));
        assert!(pc.daemon.status().await.devices.is_empty());
        assert!(pc.notifications.1.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_device_that_forgot_this_one_pairs_again() {
        let pc = start("pc").await;
        let phone = Identity::generate().unwrap();
        let invite = pc.daemon.open_pairing().await.unwrap();
        pair_as(&pc.daemon, &phone, &invite, "Old name")
            .await
            .unwrap();
        let socket = TcpStream::connect(pc.daemon.local_addr()).await.unwrap();
        let connection = transport::connect(socket, &phone, pc.daemon.device_id())
            .await
            .unwrap();
        let old_session = tokio::spawn(async move {
            let hello = Hello {
                protocol_version: PROTOCOL_VERSION,
                device_name: "Old name".into(),
                ..Hello::default()
            };
            session::run_session(connection, hello, &Peers::new(), &NoEvents).await
        });
        assert!(wait_for_async(|| connected(&pc.daemon)).await);

        // The phone forgot the PC, which still knows it, and scans a new code.
        let mut events = pc.daemon.subscribe();
        let invite = pc.daemon.open_pairing().await.unwrap();
        let paired = pair_as(&pc.daemon, &phone, &invite, "New name").await;

        assert_eq!(paired.unwrap().device_id, pc.daemon.device_id());
        assert!(matches!(events.recv().await, Ok(Event::Paired(d)) if d.name == "New name"));
        let status = pc.daemon.status().await;
        assert_eq!(status.devices.len(), 1);
        assert_eq!(status.devices[0].device.name, "New name");
        assert!(!status.pairing_open);
        // Its old session ended.
        assert!(timeout(Duration::from_secs(5), old_session).await.is_ok());
        // The code was used: it doesn't pair it a third time.
        assert!(matches!(
            pair_as(&pc.daemon, &phone, &invite, "Third").await,
            Err(PairingError::Rejected)
        ));
    }

    #[tokio::test]
    async fn a_device_renamed_there_is_renamed_here() {
        let pc = start("pc").await;
        let phone = Identity::generate().unwrap();
        let invite = pc.daemon.open_pairing().await.unwrap();
        pair_as(&pc.daemon, &phone, &invite, "POCO").await.unwrap();

        // It connects with the name it was given since.
        let socket = TcpStream::connect(pc.daemon.local_addr()).await.unwrap();
        let connection = transport::connect(socket, &phone, pc.daemon.device_id())
            .await
            .unwrap();
        let _session = tokio::spawn(async move {
            let hello = Hello {
                protocol_version: PROTOCOL_VERSION,
                device_name: "Mon téléphone\u{7}".into(),
                ..Hello::default()
            };
            session::run_session(connection, hello, &Peers::new(), &NoEvents).await
        });

        assert!(wait_for_async(|| connected(&pc.daemon)).await);
        assert_eq!(
            pc.daemon.status().await.devices[0].device.name,
            "Mon téléphone"
        );
        let saved = pc.daemon.shared.profile.known_devices().unwrap();
        assert_eq!(saved.get(&phone.device_id()).unwrap().name, "Mon téléphone");
    }

    #[tokio::test]
    async fn a_device_of_another_protocol_version_is_shown_once() {
        let pc = start("pc").await;
        let phone = Identity::generate().unwrap();
        let invite = pc.daemon.open_pairing().await.unwrap();
        pair_as(&pc.daemon, &phone, &invite, "POCO").await.unwrap();

        // It retries, as phones do.
        for _ in 0..3 {
            let socket = TcpStream::connect(pc.daemon.local_addr()).await.unwrap();
            let mut connection = transport::connect(socket, &phone, pc.daemon.device_id())
                .await
                .unwrap();
            let newer = Hello {
                protocol_version: PROTOCOL_VERSION + 1,
                device_name: "POCO".into(),
                ..Hello::default()
            };
            let theirs = connection.exchange_hello(newer).await.unwrap();
            assert_eq!(theirs.protocol_version, PROTOCOL_VERSION);
            // The PC closes the connection.
            assert!(
                timeout(Duration::from_secs(5), connection.recv())
                    .await
                    .is_ok()
            );
        }

        assert!(
            wait_for(|| !pc.notifications.1.lock().unwrap().is_empty()).await,
            "no notice"
        );
        sleep(Duration::from_millis(100)).await;
        assert_eq!(
            *pc.notifications.1.lock().unwrap(),
            ["POCO runs another version of paste-get: update both"]
        );
        assert!(!connected(&pc.daemon).await);
    }

    #[tokio::test]
    async fn a_used_code_says_to_show_a_new_one() {
        let pc = start("pc").await;
        let invite = pc.daemon.open_pairing().await.unwrap();
        pair_as(&pc.daemon, &Identity::generate().unwrap(), &invite, "first")
            .await
            .unwrap();

        let second = pair_as(
            &pc.daemon,
            &Identity::generate().unwrap(),
            &invite,
            "second",
        )
        .await;

        assert!(matches!(second, Err(PairingError::NoLongerShown)));
    }

    #[tokio::test]
    async fn a_silent_client_blocks_neither_status_nor_pairing() {
        let pc = start("pc").await;
        let laptop = start("laptop").await;
        let invite = pc.daemon.open_pairing().await.unwrap();
        // Something on the network opens a TLS connection, then says nothing.
        let socket = TcpStream::connect(pc.daemon.local_addr()).await.unwrap();
        let _silent = transport::connect(socket, &Identity::generate().unwrap(), invite.device_id)
            .await
            .unwrap();
        sleep(Duration::from_millis(100)).await;

        let status = timeout(Duration::from_secs(1), pc.daemon.status())
            .await
            .expect("status waited for the silent client");
        assert!(status.pairing_open);
        let joined = timeout(Duration::from_secs(3), laptop.daemon.join(&invite))
            .await
            .expect("pairing waited for the silent client");
        assert_eq!(joined.unwrap().name, "pc");
    }

    /// A TCP connection from `ip` to `pc` that sends nothing, once the daemon counts it.
    async fn silent_connection(pc: &Daemon, ip: Ipv4Addr) -> TcpStream {
        let before = pc.shared.admission.len();
        let socket = tokio::net::TcpSocket::new_v4().unwrap();
        socket.bind(SocketAddr::from((ip, 0))).unwrap();
        let stream = socket.connect(pc.local_addr()).await.unwrap();
        assert!(wait_for(|| pc.shared.admission.len() > before).await);
        stream
    }

    /// Whether the daemon closed `stream` within `within`.
    async fn closed_within(stream: &mut TcpStream, within: Duration) -> bool {
        use tokio::io::AsyncReadExt;
        let mut buf = [0; 64];
        matches!(
            timeout(within, stream.read(&mut buf)).await,
            Ok(Ok(0) | Err(_))
        )
    }

    /// A paired phone opens a session with `pc`, from `ip`.
    async fn connects_from(pc: &Daemon, phone: &Identity, ip: Ipv4Addr) -> bool {
        let socket = tokio::net::TcpSocket::new_v4().unwrap();
        socket.bind(SocketAddr::from((ip, 0))).unwrap();
        let socket = socket.connect(pc.local_addr()).await.unwrap();
        let mut connection = transport::connect(socket, phone, pc.device_id())
            .await
            .unwrap();
        let hello = Hello {
            protocol_version: PROTOCOL_VERSION,
            device_name: "POCO".into(),
            ..Hello::default()
        };
        connection.exchange_hello(hello).await.is_ok()
    }

    #[tokio::test]
    async fn a_silent_connection_is_closed_after_a_few_seconds() {
        let pc = start("pc").await;
        let started = Instant::now();
        let mut silent = silent_connection(&pc.daemon, Ipv4Addr::LOCALHOST).await;

        assert!(closed_within(&mut silent, UNKNOWN_DEADLINE + Duration::from_secs(2)).await);
        assert!(started.elapsed() >= UNKNOWN_DEADLINE - Duration::from_millis(500));
        assert!(wait_for(|| pc.daemon.shared.admission.is_empty()).await);
    }

    #[tokio::test]
    async fn one_address_gets_eight_connections_at_once() {
        let pc = start("pc").await;
        let mut silent = Vec::new();
        for _ in 0..crate::admission::PER_IP {
            silent.push(silent_connection(&pc.daemon, Ipv4Addr::LOCALHOST).await);
        }

        // The ninth is closed at once; the others are still open.
        let mut ninth = TcpStream::connect(pc.daemon.local_addr()).await.unwrap();
        assert!(closed_within(&mut ninth, Duration::from_secs(1)).await);
        for stream in &mut silent {
            assert!(!closed_within(stream, Duration::from_millis(10)).await);
        }

        // Once they're gone, a paired device from that address connects.
        drop(silent);
        assert!(wait_for(|| pc.daemon.shared.admission.is_empty()).await);
        let phone = Identity::generate().unwrap();
        let invite = pc.daemon.open_pairing().await.unwrap();
        pair_as(&pc.daemon, &phone, &invite, "POCO").await.unwrap();
        assert!(connects_from(&pc.daemon, &phone, Ipv4Addr::LOCALHOST).await);
    }

    /// Silent connections from 8 addresses fill every place: the oldest of theirs makes room,
    /// and a paired device still connects. Linux only: it needs several loopback addresses.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn a_paired_device_still_connects_during_a_flood() {
        use crate::admission::{AT_ONCE, PER_IP};
        let pc = start("pc").await;
        let phone = Identity::generate().unwrap();
        let invite = pc.daemon.open_pairing().await.unwrap();
        pair_as(&pc.daemon, &phone, &invite, "POCO").await.unwrap();
        let mut flood = Vec::new();
        for n in 0..AT_ONCE {
            let ip = Ipv4Addr::new(127, 0, 0, 2 + u8::try_from(n / PER_IP).unwrap());
            flood.push(silent_connection(&pc.daemon, ip).await);
        }
        assert_eq!(pc.daemon.shared.admission.len(), AT_ONCE);

        assert!(connects_from(&pc.daemon, &phone, Ipv4Addr::LOCALHOST).await);

        assert!(wait_for_async(|| connected(&pc.daemon)).await);
        assert!(closed_within(&mut flood[0], Duration::from_secs(1)).await);
        assert!(!closed_within(&mut flood[1], Duration::from_millis(10)).await);
    }

    /// Once a paired device connected from an address, a connection from it is never closed to
    /// make room for others, however many addresses they come from: here 64 more, one
    /// connection each, while the device is slow to start its TLS handshake. Linux only.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn a_paired_device_slow_to_connect_is_not_closed_by_a_flood() {
        use crate::admission::AT_ONCE;
        let pc = start("pc").await;
        let phone = Identity::generate().unwrap();
        let invite = pc.daemon.open_pairing().await.unwrap();
        pair_as(&pc.daemon, &phone, &invite, "POCO").await.unwrap();
        assert!(connects_from(&pc.daemon, &phone, Ipv4Addr::LOCALHOST).await);
        assert!(wait_for(|| pc.daemon.shared.admission.is_empty()).await);

        let mut slow = silent_connection(&pc.daemon, Ipv4Addr::LOCALHOST).await;
        let mut flood = Vec::new();
        for n in 0..AT_ONCE {
            let ip = Ipv4Addr::new(127, 0, 1, u8::try_from(n + 1).unwrap());
            let socket = tokio::net::TcpSocket::new_v4().unwrap();
            socket.bind(SocketAddr::from((ip, 0))).unwrap();
            flood.push(socket.connect(pc.daemon.local_addr()).await.unwrap());
        }

        // The 64th of them made the first close, not the device's.
        assert!(closed_within(&mut flood[0], Duration::from_secs(1)).await);
        assert!(!closed_within(&mut slow, Duration::from_millis(10)).await);
        assert_eq!(pc.daemon.shared.admission.len(), AT_ONCE);
    }

    #[tokio::test]
    async fn wrong_secrets_leave_the_window_open_and_a_code_pairs_once() {
        let pc = start("pc").await;
        let invite = pc.daemon.open_pairing().await.unwrap();
        let mut guess = PairingWindow::open(Instant::now()).unwrap().invite(
            &Identity::generate().unwrap(),
            "pc",
            invite.addresses.clone(),
        );
        guess.device_id = invite.device_id;

        // Three wrong secrets at once, as anyone on the network could send: all refused, and
        // the window stays open.
        let guesser = Identity::generate().unwrap();
        let (a, b, c) = tokio::join!(
            pair_as(&pc.daemon, &guesser, &guess, "guess"),
            pair_as(&pc.daemon, &guesser, &guess, "guess"),
            pair_as(&pc.daemon, &guesser, &guess, "guess"),
        );
        for result in [a, b, c] {
            assert!(matches!(result, Err(PairingError::Rejected)), "{result:?}");
        }
        assert!(pc.daemon.status().await.pairing_open);

        // Then two devices with the right secret at once: one pairs.
        let (first, second) = (Identity::generate().unwrap(), Identity::generate().unwrap());
        let (a, b) = tokio::join!(
            pair_as(&pc.daemon, &first, &invite, "first"),
            pair_as(&pc.daemon, &second, &invite, "second"),
        );
        assert_eq!(u8::from(a.is_ok()) + u8::from(b.is_ok()), 1, "{a:?} {b:?}");
        assert_eq!(pc.daemon.status().await.devices.len(), 1);
    }

    #[tokio::test]
    async fn joining_says_why_each_address_failed() {
        let pc = start("pc").await;
        let laptop = start("laptop").await;
        let mut invite = pc.daemon.open_pairing().await.unwrap();
        let closed = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap()
            .local_addr()
            .unwrap();
        invite.addresses = vec![closed];

        let error = laptop.daemon.join(&invite).await.unwrap_err();
        assert_eq!(
            format!("{error:#}"),
            format!("could not pair: {closed}: connection refused")
        );

        // With the PC's own address after it, it pairs, and dials that address first.
        invite.addresses.push(pc.daemon.local_addr());
        let joined = laptop.daemon.join(&invite).await.unwrap();
        assert_eq!(joined.addresses, [pc.daemon.local_addr(), closed]);
        assert!(wait_for_async(|| connected(&laptop.daemon)).await);
    }

    /// The code also names an address on the Internet: it isn't dialed, nor remembered.
    #[tokio::test]
    async fn joining_dials_no_address_on_the_internet() {
        let pc = start("pc").await;
        let laptop = start("laptop").await;
        let mut invite = pc.daemon.open_pairing().await.unwrap();
        let internet: SocketAddr = "203.0.113.9:47290".parse().unwrap();
        invite.addresses.insert(0, internet);

        let joined = timeout(Duration::from_secs(3), laptop.daemon.join(&invite))
            .await
            .expect("the address on the Internet was dialed")
            .unwrap();

        assert_eq!(joined.addresses, [pc.daemon.local_addr()]);
        invite.addresses = vec![internet];
        let error = laptop.daemon.join(&invite).await.unwrap_err();
        assert_eq!(
            format!("{error:#}"),
            "could not pair: 203.0.113.9:47290: not on the local network, not tried"
        );
    }

    #[test]
    fn pairing_codes_list_the_likeliest_addresses_first() {
        let address = |interface: &str, ip: &str| LocalAddress {
            interface: interface.into(),
            point_to_point: false,
            ip: ip.parse().unwrap(),
        };
        let mut vpn = address("ppp0", "100.64.0.7");
        vpn.point_to_point = true;

        let ordered = likeliest_first(vec![
            address("vEthernet (WSL (Hyper-V firewall))", "172.25.80.1"),
            address("docker0", "172.17.0.1"),
            address("wg0", "10.8.0.2"),
            vpn,
            address("eth1", "172.20.1.5"),
            address("Ethernet", "10.0.0.12"),
            address("VirtualBox Host-Only Network", "192.168.56.1"),
            address("Wi-Fi", "192.168.1.20"),
            address("enp3s0", "192.168.1.21"),
            address("eth0", "203.0.113.9"),
            address("Wi-Fi 2", "192.168.1.20"),
        ]);

        let expected = [
            "192.168.1.20",
            "192.168.1.21",
            "10.0.0.12",
            "172.20.1.5",
            // Virtual: last.
            "10.8.0.2",
            "172.17.0.1",
            "172.25.80.1",
            "192.168.56.1",
            // Not on the local network: 203.0.113.9 and 100.64.0.7, left out.
        ];
        assert_eq!(ordered, expected.map(|ip| ip.parse::<IpAddr>().unwrap()));
    }

    #[test]
    fn virtual_interfaces_are_recognized_by_name() {
        for name in [
            "docker0",
            "br-3f2a9c1d",
            "virbr0",
            "vboxnet0",
            "vmnet8",
            "tun0",
            "tap0",
            "wg0",
            "tailscale0",
            "vEthernet (Default Switch)",
            "VMware Network Adapter VMnet1",
            "VirtualBox Host-Only Network",
            "ZeroTier One [8056c2e21c000001]",
            "OpenVPN TAP-Windows6",
        ] {
            assert!(looks_virtual(name), "{name}");
        }
        for name in [
            "Wi-Fi",
            "Ethernet",
            "Ethernet 2",
            "eth0",
            "enp3s0",
            "wlp2s0",
            "wlan0",
            "br0",
        ] {
            assert!(!looks_virtual(name), "{name}");
        }
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

        // Told, so that it stops dialing.
        assert!(matches!(result, Err(transport::TransportError::NotPaired)));
        assert!(pc.daemon.status().await.devices.is_empty());
    }

    /// Whether `device` is refused as not paired, or only refused.
    async fn refusal(pc: &Daemon, device: &Identity) -> transport::TransportError {
        let socket = TcpStream::connect(pc.local_addr()).await.unwrap();
        let mut connection = transport::connect(socket, device, pc.device_id())
            .await
            .unwrap();
        connection
            .exchange_hello(Hello::default())
            .await
            .unwrap_err()
    }

    /// The paired devices couldn't be read: a device it doesn't know may be one of them, and is
    /// refused without being told it was unpaired, so that it doesn't forget this computer.
    #[tokio::test]
    async fn unknown_devices_are_not_told_they_were_unpaired_while_pairings_are_set_aside() {
        let home = tempfile::tempdir().unwrap();
        let profile = Profile::open_in(home.path(), "pc").unwrap();
        std::fs::write(profile.dir().join("devices.toml"), "[[device]\nid = ").unwrap();
        let pc = start_in(home, profile).await;

        assert_eq!(pc.notifications.1.lock().unwrap().len(), 1);
        let phone = Identity::generate().unwrap();
        assert!(matches!(
            refusal(&pc.daemon, &phone).await,
            transport::TransportError::Refused
        ));

        // One forgotten while it runs is told.
        let invite = pc.daemon.open_pairing().await.unwrap();
        pair_as(&pc.daemon, &phone, &invite, "POCO").await.unwrap();
        pc.daemon.forget(&phone.device_id()).unwrap();
        assert!(matches!(
            refusal(&pc.daemon, &phone).await,
            transport::TransportError::NotPaired
        ));
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

    /// Pairs `joiner` with `host`, as `pasteget join` does, and waits for their session.
    async fn link(joiner: &TestDaemon, host: &TestDaemon) {
        let invite = host.daemon.open_pairing().await.unwrap();
        joiner.daemon.join(&invite).await.unwrap();
        let (joiner_id, host_id) = (joiner.daemon.device_id(), host.daemon.device_id());
        assert!(wait_for(|| host.daemon.shared.peers.is_connected(&joiner_id)).await);
        assert!(wait_for(|| joiner.daemon.shared.peers.is_connected(&host_id)).await);
    }

    /// A daemon that shows a notification for every clip it puts on its clipboard.
    async fn start_notifying(name: &str) -> TestDaemon {
        let home = tempfile::tempdir().unwrap();
        let profile = Profile::open_in(home.path(), name).unwrap();
        std::fs::write(
            profile.dir().join("settings.toml"),
            format!("device_name = \"{name}\"\nnotify = \"all\"\n"),
        )
        .unwrap();
        start_in(home, profile).await
    }

    /// How many clips from other devices it put on its clipboard.
    fn applied(test: &TestDaemon) -> usize {
        test.notifications.0.lock().unwrap().len()
    }

    /// Three computers linked in a ring, and a fourth that joined one of them: a copy made on
    /// the fourth reaches each of the three once, through every path, and nothing loops.
    #[tokio::test]
    async fn computers_in_a_ring_take_each_copy_once() {
        let [a, b, c, outside] = [
            start_notifying("a").await,
            start_notifying("b").await,
            start_notifying("c").await,
            start_notifying("outside").await,
        ];
        link(&b, &a).await;
        link(&c, &b).await;
        link(&a, &c).await;
        link(&outside, &a).await;
        let ring = [&a, &b, &c];
        let all = [&a, &b, &c, &outside];
        let everywhere = |text: &str| {
            let text = Raw::Text(text.into());
            move || {
                all.iter()
                    .all(|pc| pc.clipboard.get() == Some(text.clone()))
            }
        };

        outside.clipboard.set(Raw::Text("copied outside".into()));
        assert!(wait_for(everywhere("copied outside")).await);
        sleep(Duration::from_millis(300)).await;
        assert_eq!(ring.map(applied), [1, 1, 1]);
        assert_eq!(applied(&outside), 0);

        // The same text copied again later is a new copy: it arrives again.
        outside.clipboard.set(Raw::Text("something else".into()));
        assert!(wait_for(everywhere("something else")).await);
        outside.clipboard.set(Raw::Text("copied outside".into()));
        assert!(wait_for(everywhere("copied outside")).await);
        sleep(Duration::from_millis(300)).await;
        assert_eq!(ring.map(applied), [3, 3, 3]);

        // A copy made in the ring reaches the others once too.
        b.clipboard.set(Raw::Text("copied on b".into()));
        assert!(wait_for(everywhere("copied on b")).await);
        sleep(Duration::from_millis(300)).await;
        assert_eq!(ring.map(applied), [4, 3, 4]);
        assert_eq!(applied(&outside), 1);
    }

    /// `outside` joined `a`, which joined `b`: `a` passes copies on to `b`, but not secrets.
    #[tokio::test]
    async fn secrets_are_not_passed_on() {
        let [a, b, outside] = [start("a").await, start("b").await, start("outside").await];
        link(&a, &b).await;
        link(&outside, &a).await;

        let content = |text: &str, sensitive| Content {
            mime_type: mime::TEXT.into(),
            data: text.as_bytes().to_vec(),
            sensitive,
        };
        assert_eq!(outside.daemon.send(content("hunter2", true)), 1);
        assert!(wait_for(|| a.clipboard.get() == Some(Raw::Text("hunter2".into()))).await);
        sleep(Duration::from_millis(300)).await;
        assert_eq!(b.clipboard.get(), None);

        // The next copy, which isn't secret, is passed on.
        assert_eq!(outside.daemon.send(content("not a secret", false)), 1);
        assert!(wait_for(|| b.clipboard.get() == Some(Raw::Text("not a secret".into()))).await);
    }

    #[tokio::test]
    async fn edited_settings_apply_at_once_or_ask_for_a_restart() {
        let pc = start("pc").await;
        let laptop = start("laptop").await;
        let invite = pc.daemon.open_pairing().await.unwrap();
        laptop.daemon.join(&invite).await.unwrap();
        assert!(wait_for_async(|| connected(&pc.daemon)).await);

        // Links offered by default; turned off without a restart.
        let profile = &pc.daemon.shared.profile;
        let mut settings = profile.settings().unwrap();
        settings.offer_to_open_links = false;
        settings.notify = NotifyFor::All;
        profile.save_settings(&settings).unwrap();
        assert!(!pc.daemon.reload_settings().unwrap());

        laptop
            .clipboard
            .set(Raw::Text("https://example.com".into()));
        assert!(wait_for(|| !pc.notifications.0.lock().unwrap().is_empty()).await);
        assert!(pc.notifications.2.lock().unwrap().is_empty());

        // A new name takes a restart.
        settings.device_name = "  renamed\n".into();
        profile.save_settings(&settings).unwrap();
        assert_eq!(profile.settings().unwrap().device_name, "renamed");
        assert!(pc.daemon.reload_settings().unwrap());
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

    /// A daemon with the clipboard history on.
    async fn start_with_history(name: &str) -> TestDaemon {
        let home = tempfile::tempdir().unwrap();
        let profile = Profile::open_in(home.path(), name).unwrap();
        std::fs::write(
            profile.dir().join("settings.toml"),
            format!("device_name = \"{name}\"\n[history]\nenabled = true\n"),
        )
        .unwrap();
        start_in(home, profile).await
    }

    /// Every item of the history, newest first, once what was queued is recorded.
    fn history_items(daemon: &Daemon) -> Vec<store::Item> {
        let history = daemon.history().unwrap();
        history.wait_for_recorder();
        history.page(&store::Query::default()).unwrap().items
    }

    /// Steps 51-52: what's copied here, sent and received is kept, also after a restart.
    #[tokio::test]
    async fn the_history_keeps_what_goes_through_and_survives_a_restart() {
        let pc = start_with_history("pc").await;
        let laptop = start("laptop").await;
        let invite = pc.daemon.open_pairing().await.unwrap();
        laptop.daemon.join(&invite).await.unwrap();
        assert!(wait_for_async(|| connected(&pc.daemon)).await);

        pc.clipboard.set(Raw::Text("copié ici".into()));
        assert!(wait_for(|| laptop.clipboard.get() == Some(Raw::Text("copié ici".into()))).await);
        laptop.clipboard.set(Raw::Text("du portable".into()));
        assert!(wait_for(|| pc.clipboard.get() == Some(Raw::Text("du portable".into()))).await);
        pc.daemon.send(Content {
            mime_type: mime::TEXT.into(),
            data: b"pasteget send".to_vec(),
            sensitive: false,
        });

        let items = history_items(&pc.daemon);
        let summary: Vec<_> = items
            .iter()
            .map(|item| (item.preview.as_str(), item.source))
            .collect();
        assert_eq!(
            summary,
            [
                ("pasteget send", store::Source::SentByHand),
                ("du portable", store::Source::Received),
                ("copié ici", store::Source::CopiedHere),
            ]
        );
        let laptop_device = items[1].via.as_ref().unwrap();
        assert_eq!(laptop_device.id, laptop.daemon.device_id());
        assert_eq!(laptop_device.name, "laptop");
        assert_eq!(items[1].origin, items[1].via);
        assert_eq!(items[2].origin, None);

        let pc = restart(pc).await;
        let again = history_items(&pc.daemon);
        assert_eq!(again, items);
        // And an item still goes back on the clipboard.
        pc.daemon.copy_from_history(items[2].id).unwrap();
        assert!(wait_for(|| pc.clipboard.get() == Some(Raw::Text("copié ici".into()))).await);
    }

    /// A sensitive clip, sent or received, adds no row: it is only counted.
    #[tokio::test]
    async fn a_sensitive_clip_adds_no_row() {
        let pc = start_with_history("pc").await;
        let laptop = start("laptop").await;
        let invite = pc.daemon.open_pairing().await.unwrap();
        laptop.daemon.join(&invite).await.unwrap();
        assert!(wait_for_async(|| connected(&laptop.daemon)).await);

        laptop.daemon.send(Content {
            mime_type: mime::TEXT.into(),
            data: b"hunter2 from KeePassXC".to_vec(),
            sensitive: true,
        });
        assert!(
            wait_for(|| pc.clipboard.get() == Some(Raw::Text("hunter2 from KeePassXC".into())))
                .await
        );
        pc.daemon.send(Content {
            mime_type: mime::TEXT.into(),
            data: b"hunter2 from Bitwarden".to_vec(),
            sensitive: true,
        });

        assert!(history_items(&pc.daemon).is_empty());
        let history = pc.daemon.history().unwrap();
        assert_eq!(history.stats().unwrap().items, 0);
        assert_eq!(
            history.kept_out(0).unwrap(),
            [(store::Reason::Sensitive, 2)]
        );
    }

    /// An item put back on the clipboard isn't sent: the other device keeps what it has.
    #[tokio::test]
    async fn an_item_copied_back_is_not_sent_again() {
        let pc = start_with_history("pc").await;
        let laptop = start("laptop").await;
        let invite = pc.daemon.open_pairing().await.unwrap();
        laptop.daemon.join(&invite).await.unwrap();
        assert!(wait_for_async(|| connected(&pc.daemon)).await);
        pc.clipboard.set(Raw::Text("un".into()));
        assert!(wait_for(|| laptop.clipboard.get() == Some(Raw::Text("un".into()))).await);
        laptop.clipboard.set(Raw::Text("deux".into()));
        assert!(wait_for(|| pc.clipboard.get() == Some(Raw::Text("deux".into()))).await);

        let first = history_items(&pc.daemon)[1].id;
        pc.daemon.copy_from_history(first).unwrap();

        assert!(wait_for(|| pc.clipboard.get() == Some(Raw::Text("un".into()))).await);
        sleep(Duration::from_millis(200)).await;
        assert_eq!(laptop.clipboard.get(), Some(Raw::Text("deux".into())));
        assert!(pc.daemon.copy_from_history(ItemId(999)).is_err());
    }

    /// Off by default; turned on and off again by editing settings.toml, without a restart.
    #[tokio::test]
    async fn the_history_is_turned_on_and_off_by_the_settings() {
        let pc = start("pc").await;
        let error = pc.daemon.history().unwrap_err().to_string();
        assert!(error.contains("off"), "{error}");
        pc.clipboard.set(Raw::Text("before".into()));
        sleep(Duration::from_millis(100)).await;

        let profile = pc.daemon.shared.profile.clone();
        let mut settings = profile.settings().unwrap();
        settings.history.enabled = true;
        profile.save_settings(&settings).unwrap();
        assert!(!pc.daemon.reload_settings().unwrap());
        sleep(Duration::from_millis(100)).await;
        pc.clipboard.set(Raw::Text("after".into()));
        assert!(wait_for(|| history_items(&pc.daemon).len() == 1).await);
        assert_eq!(history_items(&pc.daemon)[0].preview, "after");
        assert!(profile.history_dir().join(store::DATABASE_FILE).is_file());

        settings.history.enabled = false;
        profile.save_settings(&settings).unwrap();
        pc.daemon.reload_settings().unwrap();
        assert!(pc.daemon.history().is_err());
    }

    /// A history that can't open leaves the daemon syncing, and says so.
    #[tokio::test]
    async fn the_daemon_runs_without_a_history_that_cant_open() {
        let home = tempfile::tempdir().unwrap();
        let profile = Profile::open_in(home.path(), "pc").unwrap();
        std::fs::write(
            profile.dir().join("settings.toml"),
            "device_name = \"pc\"\n[history]\nenabled = true\n",
        )
        .unwrap();
        std::fs::write(profile.history_dir(), b"not a folder").unwrap();

        let pc = start_in(home, profile).await;

        assert!(pc.daemon.history().is_err());
        assert_eq!(
            *pc.notifications.1.lock().unwrap(),
            ["paste-get couldn't open its clipboard history: it syncs without it"]
        );
        let laptop = start("laptop").await;
        let invite = pc.daemon.open_pairing().await.unwrap();
        laptop.daemon.join(&invite).await.unwrap();
        assert!(wait_for_async(|| connected(&pc.daemon)).await);
        pc.clipboard.set(Raw::Text("still syncing".into()));
        assert!(
            wait_for(|| laptop.clipboard.get() == Some(Raw::Text("still syncing".into()))).await
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
