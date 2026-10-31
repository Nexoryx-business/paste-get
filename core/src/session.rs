// SPDX-License-Identifier: GPL-3.0-or-later

//! Sessions with connected devices, shared by the desktop daemon and the phone app.
//!
//! A session starts with the [`Hello`] exchange, then runs two halves until the connection ends:
//! - a writer task sending queued clips, acks and pings (every [`PING_INTERVAL`]);
//! - a reader loop answering pings, reassembling clips and handing complete ones to
//!   [`SessionEvents`]. [`IDLE_TIMEOUT`] of silence ends the session.
//!
//! [`Peers`] holds the outgoing queue of every connected device, so the app can send to all of
//! them. What to do with received clips (clipboard, relay, notification) is up to the app.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio::time::{MissedTickBehavior, interval, sleep, timeout};

use crate::clip::{self, Clip, ClipReceiver, Received};
use crate::identity::{DeviceId, Identity};
use crate::proto::envelope::Payload;
use crate::proto::{AckStatus, Envelope, Hello, Ping, Pong};
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
    fn connected(&self, _peer: DeviceId, _hello: &Hello) {}

    fn disconnected(&self, _peer: DeviceId) {}

    /// A complete clip whose hash and type were verified.
    fn clip_received(&self, peer: DeviceId, clip: Clip);
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
    Clip(Clip),
    Envelope(Envelope),
}

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
            match queue.sender.try_send(Outgoing::Clip(clip.clone())) {
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

    fn unregister(&self, peer: DeviceId, session: u64) {
        let mut queues = self.lock();
        if queues.get(&peer).is_some_and(|q| q.session == session) {
            queues.remove(&peer);
        }
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
    let mut guard = SessionGuard {
        peers,
        peer,
        session: peers.register(peer, sender.clone()),
        writer: Some(tokio::spawn(write_loop(writer, queue))),
    };
    events.connected(peer, &their_hello);

    let result = read_loop(peer, reader, &sender, events).await;

    // Graceful end: once no queue sender is left, the writer flushes and closes the connection.
    peers.unregister(peer, guard.session);
    drop(sender);
    if let Some(mut writer) = guard.writer.take()
        && timeout(Duration::from_secs(1), &mut writer).await.is_err()
    {
        tracing::debug!(%peer, "writer did not stop in time");
        writer.abort();
    }
    events.disconnected(peer);
    result
}

/// Tears a session down even if `run_session` is cancelled: forgets the device and stops the
/// writer task, which would otherwise keep the connection alive with its pings.
struct SessionGuard<'a> {
    peers: &'a Peers,
    peer: DeviceId,
    session: u64,
    writer: Option<tokio::task::JoinHandle<()>>,
}

impl Drop for SessionGuard<'_> {
    fn drop(&mut self) {
        self.peers.unregister(self.peer, self.session);
        if let Some(writer) = self.writer.take() {
            writer.abort();
        }
    }
}

/// Keeps a connection to `peer` open forever: dials `addresses` in turn, runs a session, and
/// redials with backoff when it ends. Cancel by dropping or aborting the task.
pub async fn keep_connected(
    identity: &Identity,
    peer: DeviceId,
    addresses: &[SocketAddr],
    hello: &Hello,
    peers: &Peers,
    events: &dyn SessionEvents,
) {
    let mut delay = RECONNECT_MIN;
    loop {
        for address in addresses {
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
                    if let Err(e) = run_session(connection, hello.clone(), peers, events).await {
                        tracing::info!(%peer, error = %e, "session ended");
                    }
                    break;
                }
                Ok(Err(e)) => tracing::debug!(%peer, %address, error = %e, "dial failed"),
                Err(_) => tracing::debug!(%peer, %address, "dial timed out"),
            }
        }
        sleep(delay).await;
        delay = (delay * 2).min(RECONNECT_MAX);
    }
}

async fn read_loop<R: AsyncRead + Unpin>(
    peer: DeviceId,
    mut reader: EnvelopeReader<R>,
    replies: &mpsc::Sender<Outgoing>,
    events: &dyn SessionEvents,
) -> Result<(), SessionError> {
    let mut receiver = ClipReceiver::default();
    loop {
        let Some(envelope) = timeout(IDLE_TIMEOUT, reader.recv())
            .await
            .map_err(|_| SessionError::Timeout("waiting for the device"))??
        else {
            return Ok(());
        };
        let received = match envelope.payload {
            Some(Payload::ClipOffer(offer)) => receiver.on_offer(offer),
            Some(Payload::ClipChunk(chunk)) => receiver.on_chunk(chunk),
            Some(Payload::Ping(Ping { nonce })) => {
                reply(replies, Payload::Pong(Pong { nonce })).await?;
                continue;
            }
            Some(Payload::Ack(ack)) => {
                if ack.status() != AckStatus::Ok {
                    tracing::warn!(%peer, status = ?ack.status(), "device refused a clip");
                }
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
                tracing::info!(%peer, mime_type = %clip.mime_type, len = clip.data.len(), "clip received");
                events.clip_received(peer, clip);
            }
        }
    }
}

async fn reply(replies: &mpsc::Sender<Outgoing>, payload: Payload) -> Result<(), SessionError> {
    replies
        .send(Outgoing::Envelope(Envelope {
            payload: Some(payload),
        }))
        .await
        .map_err(|_| SessionError::Closed)
}

async fn write_loop<W: AsyncWrite + Unpin>(
    mut writer: EnvelopeWriter<W>,
    mut queue: mpsc::Receiver<Outgoing>,
) {
    let mut ping = interval(PING_INTERVAL);
    ping.set_missed_tick_behavior(MissedTickBehavior::Delay);
    ping.tick().await;
    let mut nonce = 0u64;

    let result: Result<(), SessionError> = async {
        loop {
            tokio::select! {
                next = queue.recv() => match next {
                    Some(Outgoing::Clip(clip)) => {
                        let envelopes = clip::outgoing(&clip)
                            .map_err(|_| SessionError::Protocol("no randomness for a clip ID"))?;
                        for envelope in envelopes {
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
    use std::sync::Arc;
    use tokio::io::duplex;

    #[derive(Default)]
    struct Recorder {
        clips: Mutex<Vec<(DeviceId, Clip)>>,
        connected: Mutex<Vec<String>>,
    }

    impl SessionEvents for Recorder {
        fn connected(&self, _peer: DeviceId, hello: &Hello) {
            self.connected
                .lock()
                .unwrap()
                .push(hello.device_name.clone());
        }

        fn clip_received(&self, peer: DeviceId, clip: Clip) {
            self.clips.lock().unwrap().push((peer, clip));
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
        let events = Arc::new(Recorder::default());

        let phone_session = tokio::spawn({
            let (peers, events) = (Arc::clone(&phone_peers), Arc::clone(&events));
            async move { run_session(client.unwrap(), hello("phone"), &peers, &*events).await }
        });
        let pc_session = tokio::spawn({
            let (peers, events) = (Arc::clone(&pc_peers), Arc::clone(&events));
            async move { run_session(server.unwrap(), hello("pc"), &peers, &*events).await }
        });
        until(|| pc_peers.is_connected(&phone.device_id())).await;

        phone_session.abort();

        assert!(timeout(Duration::from_secs(5), pc_session).await.is_ok());
        assert!(phone_peers.connected().is_empty());
        assert!(pc_peers.connected().is_empty());
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
        peers.register(id, new);
        peers.unregister(id, old_session);

        assert!(peers.is_connected(&id));
    }
}
