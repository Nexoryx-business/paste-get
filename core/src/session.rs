// SPDX-License-Identifier: GPL-3.0-or-later

//! Sessions with connected devices, shared by the desktop daemon and the phone app.
//!
//! A session starts with the [`Hello`] exchange, then runs two halves until the connection ends:
//! - a writer task sending queued clips, acks and pings (every [`PING_INTERVAL`]);
//! - a reader loop answering pings, reassembling clips and handing complete ones to
//!   [`SessionEvents`]. [`IDLE_TIMEOUT`] of silence ends the session.
//!
//! Each clip sent is timed from the moment it was queued to the device's [`Ack`]: that is what
//! [`SessionEvents::clip_delivered`] reports, to measure latency.
//!
//! [`Peers`] holds the outgoing queue of every connected device, so the app can send to all of
//! them. What to do with received clips (clipboard, relay, notification) is up to the app.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, watch};
use tokio::time::{Instant, MissedTickBehavior, interval, sleep, sleep_until, timeout};

use crate::clip::{self, Clip, ClipReceiver, Received};
use crate::identity::{DeviceId, Identity};
use crate::proto::envelope::Payload;
use crate::proto::{Ack, AckStatus, Envelope, Hello, Ping, Pong};
use crate::transport::{self, Connection, EnvelopeReader, EnvelopeWriter, TransportError};

/// Time allowed for the TLS handshake, and separately for the Hello exchange.
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
/// Every session sends a ping this often.
pub const PING_INTERVAL: Duration = Duration::from_secs(30);
/// A session that hears nothing for this long is considered dead.
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(90);
/// Delay between reconnection attempts, doubling from the minimum to the maximum.
pub const RECONNECT_MIN: Duration = Duration::from_secs(1);
pub const RECONNECT_MAX: Duration = Duration::from_secs(30);
/// Clips waiting to be written to one device. Beyond that, new clips skip that device.
pub const OUTGOING_QUEUE: usize = 16;
/// Sent clips still waiting for their [`Ack`]. Beyond that, the oldest is no longer timed.
const AWAITING_ACK: usize = 32;

#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error(transparent)]
    Transport(#[from] TransportError),
    #[error("{0} timed out")]
    Timeout(&'static str),
    #[error("device broke the protocol: {0}")]
    Protocol(&'static str),
    #[error("the session was closed")]
    Closed,
}

/// What a session reports to the app. Called from session tasks: keep it quick.
pub trait SessionEvents: Send + Sync {
    /// [`keep_connected`] reached `peer` at `address`: the TLS handshake proved its identity.
    /// The place to remember a working address.
    fn reached(&self, _peer: DeviceId, _address: SocketAddr) {}

    fn connected(&self, _peer: DeviceId, _hello: &Hello) {}

    /// The device is no longer connected, also after a cancelled session. Not reported when a
    /// newer session with the same device already replaced this one.
    fn disconnected(&self, _peer: DeviceId) {}

    /// A complete clip whose hash and type were verified.
    fn clip_received(&self, peer: DeviceId, clip: Clip);

    /// `peer` acknowledged a clip this side sent.
    fn clip_delivered(&self, _peer: DeviceId, _delivery: &Delivery) {}
}

/// A clip the device acknowledged.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Delivery {
    pub mime_type: String,
    pub len: usize,
    /// `Ok`, or why the device refused it.
    pub status: AckStatus,
    /// From the moment the clip was queued for this device to its acknowledgement.
    pub elapsed: Duration,
}

/// The devices currently connected, and their outgoing queues.
#[derive(Debug, Default)]
pub struct Peers {
    queues: Mutex<HashMap<DeviceId, PeerQueue>>,
    next_session: AtomicU64,
}

#[derive(Debug)]
struct PeerQueue {
    session: u64,
    sender: mpsc::Sender<Outgoing>,
}

#[derive(Debug)]
enum Outgoing {
    Clip { clip: Clip, queued_at: Instant },
    Envelope(Envelope),
}

/// A clip written to the device and not acknowledged yet.
#[derive(Debug)]
struct Sent {
    mime_type: String,
    len: usize,
    queued_at: Instant,
}

/// Shared by a session's writer, which adds sent clips, and its reader, which matches acks.
type AwaitingAck = Arc<Mutex<HashMap<Vec<u8>, Sent>>>;

impl Peers {
    pub fn new() -> Self {
        Self::default()
    }

    /// Queues `clip` for every connected device except `except`. Returns how many.
    pub fn broadcast(&self, clip: &Clip, except: Option<DeviceId>) -> usize {
        let queues = self.lock();
        let mut queued = 0;
        for (id, queue) in queues.iter() {
            if Some(*id) == except {
                continue;
            }
            let outgoing = Outgoing::Clip {
                clip: clip.clone(),
                queued_at: Instant::now(),
            };
            match queue.sender.try_send(outgoing) {
                Ok(()) => queued += 1,
                Err(_) => tracing::warn!(peer = %id, "device is not keeping up, clip skipped"),
            }
        }
        queued
    }

    pub fn connected(&self) -> Vec<DeviceId> {
        self.lock().keys().copied().collect()
    }

    pub fn is_connected(&self, peer: &DeviceId) -> bool {
        self.lock().contains_key(peer)
    }

    fn register(&self, peer: DeviceId, sender: mpsc::Sender<Outgoing>) -> u64 {
        let session = self.next_session.fetch_add(1, Ordering::Relaxed);
        // A newer session replaces an older one that hasn't noticed it's dead yet.
        self.lock().insert(peer, PeerQueue { session, sender });
        session
    }

    /// Forgets `session`. Returns `false` if a newer session with the device replaced it.
    fn unregister(&self, peer: DeviceId, session: u64) -> bool {
        let mut queues = self.lock();
        let current = queues.get(&peer).is_some_and(|q| q.session == session);
        if current {
            queues.remove(&peer);
        }
        current
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<DeviceId, PeerQueue>> {
        // Poisoning means a panic elsewhere; the map itself is still consistent.
        self.queues.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Runs one session until the connection ends. Returns `Ok` when the peer closed it cleanly.
pub async fn run_session<S>(
    mut connection: Connection<S>,
    hello: Hello,
    peers: &Peers,
    events: &dyn SessionEvents,
) -> Result<(), SessionError>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let peer = connection.peer();
    let their_hello = timeout(HANDSHAKE_TIMEOUT, connection.exchange_hello(hello))
        .await
        .map_err(|_| SessionError::Timeout("Hello exchange"))??;
    let (reader, writer) = connection.into_split();
    let (sender, queue) = mpsc::channel(OUTGOING_QUEUE);
    let awaiting = AwaitingAck::default();
    let mut guard = SessionGuard {
        peers,
        events,
        peer,
        session: peers.register(peer, sender.clone()),
        was_current: None,
        writer: Some(tokio::spawn(write_loop(
            writer,
            queue,
            Arc::clone(&awaiting),
        ))),
    };
    events.connected(peer, &their_hello);

    let result = read_loop(peer, reader, &sender, &awaiting, events).await;

    // Graceful end: once no queue sender is left, the writer flushes and closes the connection.
    guard.unregister();
    drop(sender);
    if let Some(mut writer) = guard.writer.take()
        && timeout(Duration::from_secs(1), &mut writer).await.is_err()
    {
        tracing::debug!(%peer, "writer did not stop in time");
        writer.abort();
    }
    result
}

/// Tears a session down even if `run_session` is cancelled: forgets the device, stops the
/// writer task (which would otherwise keep the connection alive with its pings) and reports the
/// disconnection.
struct SessionGuard<'a> {
    peers: &'a Peers,
    events: &'a dyn SessionEvents,
    peer: DeviceId,
    session: u64,
    /// Set once unregistered: whether this was still the device's current session.
    was_current: Option<bool>,
    writer: Option<tokio::task::JoinHandle<()>>,
}

impl SessionGuard<'_> {
    fn unregister(&mut self) {
        if self.was_current.is_none() {
            self.was_current = Some(self.peers.unregister(self.peer, self.session));
        }
    }
}

impl Drop for SessionGuard<'_> {
    fn drop(&mut self) {
        self.unregister();
        if let Some(writer) = self.writer.take() {
            writer.abort();
        }
        if self.was_current == Some(true) {
            self.events.disconnected(self.peer);
        }
    }
}

/// Keeps a connection to `peer` open forever: dials its addresses in turn, runs a session, and
/// redials with backoff when it ends. Cancel by dropping or aborting the task.
///
/// `addresses` may change while it runs, for example when the device is found at a new address.
/// Every update, even one with the same addresses, also cuts short the wait before the next
/// attempt: it's the hint that the device or the network just came back.
pub async fn keep_connected(
    identity: &Identity,
    peer: DeviceId,
    mut addresses: watch::Receiver<Vec<SocketAddr>>,
    hello: &Hello,
    peers: &Peers,
    events: &dyn SessionEvents,
) {
    let mut delay = RECONNECT_MIN;
    loop {
        let round = Instant::now();
        let current = addresses.borrow_and_update().clone();
        for address in current {
            let connected = timeout(HANDSHAKE_TIMEOUT, async {
                let socket = TcpStream::connect(address)
                    .await
                    .map_err(TransportError::from)?;
                transport::connect(socket, identity, peer).await
            })
            .await;
            match connected {
                Ok(Ok(connection)) => {
                    delay = RECONNECT_MIN;
                    events.reached(peer, address);
                    if let Err(e) = run_session(connection, hello.clone(), peers, events).await {
                        tracing::info!(%peer, error = %e, "session ended");
                    }
                    break;
                }
                Ok(Err(e)) => tracing::debug!(%peer, %address, error = %e, "dial failed"),
                Err(_) => tracing::debug!(%peer, %address, "dial timed out"),
            }
        }
        tokio::select! {
            () = sleep(delay) => delay = (delay * 2).min(RECONNECT_MAX),
            // A closed channel disables this branch: then only the backoff applies.
            Ok(()) = addresses.changed() => {
                // At most one round per RECONNECT_MIN, however often hints arrive.
                sleep_until(round + RECONNECT_MIN).await;
                delay = RECONNECT_MIN;
            }
        }
    }
}

async fn read_loop<R: AsyncRead + Unpin>(
    peer: DeviceId,
    mut reader: EnvelopeReader<R>,
    replies: &mpsc::Sender<Outgoing>,
    awaiting: &AwaitingAck,
    events: &dyn SessionEvents,
) -> Result<(), SessionError> {
    let mut receiver = ClipReceiver::default();
    // When the offer of the clip being received arrived.
    let mut offered_at = Instant::now();
    loop {
        let Some(envelope) = timeout(IDLE_TIMEOUT, reader.recv())
            .await
            .map_err(|_| SessionError::Timeout("waiting for the device"))??
        else {
            return Ok(());
        };
        let received = match envelope.payload {
            Some(Payload::ClipOffer(offer)) => {
                offered_at = Instant::now();
                receiver.on_offer(offer)
            }
            Some(Payload::ClipChunk(chunk)) => receiver.on_chunk(chunk),
            Some(Payload::Ping(Ping { nonce })) => {
                reply(replies, Payload::Pong(Pong { nonce })).await?;
                continue;
            }
            Some(Payload::Ack(ack)) => {
                acknowledged(peer, &ack, awaiting, events);
                continue;
            }
            // Pongs only prove the peer is alive; unknown payloads come from newer peers.
            Some(Payload::Pong(_)) | None => continue,
            Some(Payload::Hello(_) | Payload::PairRequest(_) | Payload::PairResponse(_)) => {
                return Err(SessionError::Protocol("handshake message mid-session"));
            }
        };
        match received {
            Received::Pending => {}
            Received::Rejected { ack } => {
                tracing::warn!(%peer, status = ?ack.status(), "clip from device rejected");
                reply(replies, Payload::Ack(ack)).await?;
            }
            Received::Clip { clip, ack } => {
                reply(replies, Payload::Ack(ack)).await?;
                tracing::info!(
                    %peer,
                    mime_type = %clip.mime_type,
                    len = clip.data.len(),
                    ms = offered_at.elapsed().as_millis(),
                    "clip received"
                );
                events.clip_received(peer, clip);
            }
        }
    }
}

/// Reports the delivery of the clip `ack` answers. Acks for unknown clips are ignored.
fn acknowledged(peer: DeviceId, ack: &Ack, awaiting: &AwaitingAck, events: &dyn SessionEvents) {
    let Some(sent) = lock(awaiting).remove(&ack.clip_id) else {
        tracing::debug!(%peer, "ack for a clip that isn't waiting for one");
        return;
    };
    let delivery = Delivery {
        mime_type: sent.mime_type,
        len: sent.len,
        status: ack.status(),
        elapsed: sent.queued_at.elapsed(),
    };
    if delivery.status == AckStatus::Ok {
        tracing::info!(
            %peer,
            mime_type = %delivery.mime_type,
            len = delivery.len,
            ms = delivery.elapsed.as_millis(),
            "clip delivered"
        );
    } else {
        tracing::warn!(%peer, status = ?delivery.status, "device refused a clip");
    }
    events.clip_delivered(peer, &delivery);
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    // Poisoning means a panic elsewhere; the data is still consistent.
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

async fn reply(replies: &mpsc::Sender<Outgoing>, payload: Payload) -> Result<(), SessionError> {
    replies
        .send(Outgoing::Envelope(Envelope {
            payload: Some(payload),
        }))
        .await
        .map_err(|_| SessionError::Closed)
}

fn await_ack(awaiting: &AwaitingAck, clip_id: Vec<u8>, clip: &Clip, queued_at: Instant) {
    let mut awaiting = lock(awaiting);
    if awaiting.len() >= AWAITING_ACK
        && let Some(oldest) = awaiting
            .iter()
            .min_by_key(|(_, sent)| sent.queued_at)
            .map(|(id, _)| id.clone())
    {
        awaiting.remove(&oldest);
    }
    awaiting.insert(
        clip_id,
        Sent {
            mime_type: clip.mime_type.clone(),
            len: clip.data.len(),
            queued_at,
        },
    );
}

async fn write_loop<W: AsyncWrite + Unpin>(
    mut writer: EnvelopeWriter<W>,
    mut queue: mpsc::Receiver<Outgoing>,
    awaiting: AwaitingAck,
) {
    let mut ping = interval(PING_INTERVAL);
    ping.set_missed_tick_behavior(MissedTickBehavior::Delay);
    ping.tick().await;
    let mut nonce = 0u64;

    let result: Result<(), SessionError> = async {
        loop {
            tokio::select! {
                next = queue.recv() => match next {
                    Some(Outgoing::Clip { clip, queued_at }) => {
                        let envelopes = clip::outgoing(&clip)
                            .map_err(|_| SessionError::Protocol("no randomness for a clip ID"))?;
                        for envelope in envelopes {
                            // Recorded before the offer leaves, so its ack can't come first.
                            if let Some(Payload::ClipOffer(offer)) = &envelope.payload {
                                await_ack(&awaiting, offer.clip_id.clone(), &clip, queued_at);
                            }
                            writer.send(&envelope).await?;
                        }
                    }
                    Some(Outgoing::Envelope(envelope)) => writer.send(&envelope).await?,
                    None => return Ok(()),
                },
                _ = ping.tick() => {
                    nonce = nonce.wrapping_add(1);
                    let ping = Envelope { payload: Some(Payload::Ping(Ping { nonce })) };
                    writer.send(&ping).await?;
                }
            }
        }
    }
    .await;
    if let Err(e) = result {
        tracing::debug!(error = %e, "writer stopped");
    }
    let _ = writer.close().await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::{PROTOCOL_VERSION, Platform};
    use std::net::Ipv4Addr;
    use std::sync::Arc;
    use tokio::io::duplex;
    use tokio::net::TcpListener;

    #[derive(Default)]
    struct Recorder {
        clips: Mutex<Vec<(DeviceId, Clip)>>,
        connected: Mutex<Vec<String>>,
        disconnected: Mutex<Vec<DeviceId>>,
        deliveries: Mutex<Vec<(DeviceId, Delivery)>>,
    }

    impl SessionEvents for Recorder {
        fn connected(&self, _peer: DeviceId, hello: &Hello) {
            self.connected
                .lock()
                .unwrap()
                .push(hello.device_name.clone());
        }

        fn disconnected(&self, peer: DeviceId) {
            self.disconnected.lock().unwrap().push(peer);
        }

        fn clip_received(&self, peer: DeviceId, clip: Clip) {
            self.clips.lock().unwrap().push((peer, clip));
        }

        fn clip_delivered(&self, peer: DeviceId, delivery: &Delivery) {
            self.deliveries
                .lock()
                .unwrap()
                .push((peer, delivery.clone()));
        }
    }

    fn hello(name: &str) -> Hello {
        Hello {
            protocol_version: PROTOCOL_VERSION,
            device_name: name.into(),
            app_version: "test".into(),
            platform: Platform::Linux.into(),
        }
    }

    async fn until(condition: impl Fn() -> bool) {
        for _ in 0..500 {
            if condition() {
                return;
            }
            sleep(Duration::from_millis(5)).await;
        }
        panic!("condition not met in time");
    }

    #[tokio::test]
    async fn sessions_deliver_broadcast_clips_both_ways() {
        let (phone, pc) = (Identity::generate().unwrap(), Identity::generate().unwrap());
        let (phone_io, pc_io) = duplex(64 * 1024);
        let (client, server) = tokio::join!(
            transport::connect(phone_io, &phone, pc.device_id()),
            async { transport::accept(pc_io, &pc).await?.authorize(|_| true) }
        );
        let (phone_peers, pc_peers) = (Arc::new(Peers::new()), Arc::new(Peers::new()));
        let (phone_events, pc_events) =
            (Arc::new(Recorder::default()), Arc::new(Recorder::default()));

        let phone_session = tokio::spawn({
            let (peers, events) = (Arc::clone(&phone_peers), Arc::clone(&phone_events));
            async move { run_session(client.unwrap(), hello("phone"), &peers, &*events).await }
        });
        let pc_session = tokio::spawn({
            let (peers, events) = (Arc::clone(&pc_peers), Arc::clone(&pc_events));
            async move { run_session(server.unwrap(), hello("pc"), &peers, &*events).await }
        });
        until(|| {
            phone_peers.is_connected(&pc.device_id()) && pc_peers.is_connected(&phone.device_id())
        })
        .await;
        assert_eq!(*pc_events.connected.lock().unwrap(), ["phone"]);

        let text = Clip::text("from the phone", phone.device_id());
        assert_eq!(phone_peers.broadcast(&text, None), 1);
        let image = Clip::png(b"\x89PNG\r\n\x1a\nrest".to_vec(), pc.device_id());
        assert_eq!(pc_peers.broadcast(&image, None), 1);

        until(|| !pc_events.clips.lock().unwrap().is_empty()).await;
        until(|| !phone_events.clips.lock().unwrap().is_empty()).await;
        assert_eq!(
            pc_events.clips.lock().unwrap()[0],
            (phone.device_id(), text)
        );
        assert_eq!(
            phone_events.clips.lock().unwrap()[0],
            (pc.device_id(), image)
        );

        // Each sender hears that its clip was delivered, and how long it took.
        until(|| !phone_events.deliveries.lock().unwrap().is_empty()).await;
        let (to, delivery) = phone_events.deliveries.lock().unwrap()[0].clone();
        assert_eq!(to, pc.device_id());
        assert_eq!(delivery.mime_type, clip::mime::TEXT);
        assert_eq!(delivery.len, "from the phone".len());
        assert_eq!(delivery.status, AckStatus::Ok);
        assert!(delivery.elapsed < Duration::from_secs(5));

        // When one side goes away, the other ends its session and forgets the device.
        phone_session.abort();
        let _ = pc_session.await;
        assert!(pc_peers.connected().is_empty());
    }

    #[tokio::test]
    async fn cancelling_a_session_closes_its_connection() {
        let (phone, pc) = (Identity::generate().unwrap(), Identity::generate().unwrap());
        let (phone_io, pc_io) = duplex(64 * 1024);
        let (client, server) = tokio::join!(
            transport::connect(phone_io, &phone, pc.device_id()),
            async { transport::accept(pc_io, &pc).await?.authorize(|_| true) }
        );
        let (phone_peers, pc_peers) = (Arc::new(Peers::new()), Arc::new(Peers::new()));
        let (phone_events, pc_events) =
            (Arc::new(Recorder::default()), Arc::new(Recorder::default()));

        let phone_session = tokio::spawn({
            let (peers, events) = (Arc::clone(&phone_peers), Arc::clone(&phone_events));
            async move { run_session(client.unwrap(), hello("phone"), &peers, &*events).await }
        });
        let pc_session = tokio::spawn({
            let (peers, events) = (Arc::clone(&pc_peers), Arc::clone(&pc_events));
            async move { run_session(server.unwrap(), hello("pc"), &peers, &*events).await }
        });
        until(|| pc_peers.is_connected(&phone.device_id())).await;

        phone_session.abort();

        assert!(timeout(Duration::from_secs(5), pc_session).await.is_ok());
        assert!(phone_peers.connected().is_empty());
        assert!(pc_peers.connected().is_empty());
        // Both sides report it, the cancelled one included.
        until(|| !phone_events.disconnected.lock().unwrap().is_empty()).await;
        assert_eq!(*phone_events.disconnected.lock().unwrap(), [pc.device_id()]);
        assert_eq!(*pc_events.disconnected.lock().unwrap(), [phone.device_id()]);
    }

    #[tokio::test]
    async fn new_addresses_cut_the_reconnect_wait() {
        let (phone, pc) = (Identity::generate().unwrap(), Identity::generate().unwrap());
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let (addresses, watched) = watch::channel(Vec::new());
        let dialer = tokio::spawn(async move {
            let (peers, events) = (Peers::new(), Recorder::default());
            keep_connected(
                &phone,
                pc.device_id(),
                watched,
                &hello("phone"),
                &peers,
                &events,
            )
            .await;
        });

        // With no address to dial, the waits grow: 1 s, 2 s, then 4 s from t = 3 s.
        sleep(Duration::from_millis(3500)).await;
        addresses.send_replace(vec![listener.local_addr().unwrap()]);

        // Without the update, the next attempt would come at t = 7 s, and to no address.
        let dialed = timeout(Duration::from_secs(2), listener.accept()).await;
        dialer.abort();
        assert!(dialed.is_ok(), "the new address was not dialed in time");
    }

    #[tokio::test]
    async fn broadcast_skips_the_excluded_device() {
        let peers = Peers::new();
        let (sender, _queue) = mpsc::channel(1);
        let id = Identity::generate().unwrap().device_id();
        peers.register(id, sender);

        let clip = Clip::text("x", id);

        assert_eq!(peers.broadcast(&clip, Some(id)), 0);
        assert_eq!(peers.broadcast(&clip, None), 1);
        // The queue holds one clip: a slow device skips the next one instead of blocking.
        assert_eq!(peers.broadcast(&clip, None), 0);
    }

    #[tokio::test]
    async fn a_stale_session_does_not_unregister_its_replacement() {
        let peers = Peers::new();
        let id = Identity::generate().unwrap().device_id();
        let (old, _a) = mpsc::channel(1);
        let (new, _b) = mpsc::channel(1);

        let old_session = peers.register(id, old);
        let new_session = peers.register(id, new);

        assert!(!peers.unregister(id, old_session));
        assert!(peers.is_connected(&id));
        assert!(peers.unregister(id, new_session));
        assert!(!peers.is_connected(&id));
    }
}
