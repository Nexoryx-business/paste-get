// SPDX-License-Identifier: GPL-3.0-or-later

//! Mutually authenticated TLS 1.3 connections carrying framed [`Envelope`]s.
//!
//! Trust model (see `docs/PROTOCOL.md`, section 2):
//! - The connecting side pins the peer's [`DeviceId`]: any other key fails the handshake.
//! - The accepting side requires a client certificate but does not know in advance who connects
//!   (a new device may be pairing). [`accept`] therefore returns an [`UnverifiedConnection`] that
//!   can't send or receive until the caller has decided whether the peer is trusted.
//!
//! In both directions the TLS handshake proves that the peer holds the private key of the
//! presented certificate; the signature checks are done by rustls.

use std::net::Ipv4Addr;
use std::sync::Arc;

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{CryptoProvider, WebPkiSupportedAlgorithms, verify_tls13_signature};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::{
    CertificateError, ClientConfig, DigitallySignedStruct, DistinguishedName, ServerConfig,
    SignatureScheme,
};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadHalf, WriteHalf};
use tokio_rustls::{TlsAcceptor, TlsConnector, client, server};

use crate::framing::{FrameDecoder, FrameError, encode};
use crate::identity::{DeviceId, Identity};
use crate::pairing::sanitize_device_name;
use crate::proto::{Envelope, Hello, PROTOCOL_VERSION, envelope::Payload};

/// ALPN protocol name, also the protocol version marker on the wire.
pub const ALPN: &[u8] = b"pasteget/0";

/// Bytes read from the socket at a time.
const READ_BUF_LEN: usize = 16 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum TransportError {
    #[error("TLS error: {0}")]
    Tls(#[from] rustls::Error),
    #[error("connection error: {0}")]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Frame(#[from] FrameError),
    #[error("peer did not present a certificate")]
    NoPeerCertificate,
    #[error("peer does not speak {}", String::from_utf8_lossy(ALPN))]
    WrongProtocol,
    #[error("device {0} is not trusted")]
    UntrustedPeer(DeviceId),
    #[error("peer sent another message where {expected} was expected")]
    UnexpectedMessage { expected: &'static str },
    #[error("peer speaks protocol version {0}, this device speaks {PROTOCOL_VERSION}")]
    VersionMismatch(u32),
    #[error("peer closed the connection")]
    Closed,
}

/// Connects to the device `expected_peer` over `io` (usually a `TcpStream`).
pub async fn connect<IO>(
    io: IO,
    identity: &Identity,
    expected_peer: DeviceId,
) -> Result<Connection<client::TlsStream<IO>>, TransportError>
where
    IO: AsyncRead + AsyncWrite + Unpin,
{
    let provider = provider();
    let verifier = PinnedServerVerifier {
        expected: expected_peer,
        algorithms: provider.signature_verification_algorithms,
    };
    let mut config = ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(verifier))
        .with_client_auth_cert(vec![identity.certificate()], identity.private_key())?;
    config.alpn_protocols = vec![ALPN.to_vec()];

    // The pinned verifier ignores names. An IP address also keeps SNI off the wire.
    let server_name = ServerName::IpAddress(Ipv4Addr::UNSPECIFIED.into());
    let stream = TlsConnector::from(Arc::new(config))
        .connect(server_name, io)
        .await?;

    let (_, session) = stream.get_ref();
    check_alpn(session.alpn_protocol())?;
    let peer = peer_id(session.peer_certificates())?;
    Ok(Connection::new(stream, peer))
}

/// Accepts a connection from any device over `io`. See [`UnverifiedConnection`].
pub async fn accept<IO>(
    io: IO,
    identity: &Identity,
) -> Result<UnverifiedConnection<server::TlsStream<IO>>, TransportError>
where
    IO: AsyncRead + AsyncWrite + Unpin,
{
    let provider = provider();
    let verifier = AnyClientCertificate {
        algorithms: provider.signature_verification_algorithms,
    };
    let mut config = ServerConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .with_client_cert_verifier(Arc::new(verifier))
        .with_single_cert(vec![identity.certificate()], identity.private_key())?;
    config.alpn_protocols = vec![ALPN.to_vec()];

    let stream = TlsAcceptor::from(Arc::new(config)).accept(io).await?;

    let (_, session) = stream.get_ref();
    check_alpn(session.alpn_protocol())?;
    let peer = peer_id(session.peer_certificates())?;
    Ok(UnverifiedConnection(Connection::new(stream, peer)))
}

/// An accepted connection whose peer has proven its key but is not yet known to be trusted.
#[derive(Debug)]
pub struct UnverifiedConnection<S>(Connection<S>);

impl<S> UnverifiedConnection<S> {
    /// The device ID the peer proved it owns.
    pub fn peer(&self) -> DeviceId {
        self.0.peer
    }

    /// Unlocks the connection if `is_trusted` accepts the peer's device ID.
    pub fn authorize(
        self,
        is_trusted: impl FnOnce(&DeviceId) -> bool,
    ) -> Result<Connection<S>, TransportError> {
        if is_trusted(&self.0.peer) {
            Ok(self.0)
        } else {
            Err(TransportError::UntrustedPeer(self.0.peer))
        }
    }

    /// For the pairing handshake only, which authenticates the peer by its one-time secret.
    pub(crate) fn into_pairing(self) -> Connection<S> {
        self.0
    }
}

/// An authenticated connection to a known device.
#[derive(Debug)]
pub struct Connection<S> {
    io: EnvelopeReader<S>,
    peer: DeviceId,
}

impl<S> Connection<S> {
    fn new(stream: S, peer: DeviceId) -> Self {
        Self {
            io: EnvelopeReader::new(stream),
            peer,
        }
    }

    pub fn peer(&self) -> DeviceId {
        self.peer
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin> Connection<S> {
    pub async fn send(&mut self, envelope: &Envelope) -> Result<(), TransportError> {
        write_frame(&mut self.io.inner, envelope).await
    }

    /// Next envelope from the peer, or `None` once the peer has closed the connection cleanly.
    pub async fn recv(&mut self) -> Result<Option<Envelope>, TransportError> {
        self.io.recv().await
    }

    /// Splits the connection so reading and writing can happen in different tasks.
    ///
    /// A long session must keep reading while it sends: if both peers send large clips at once
    /// and neither reads, both block forever once the TCP buffers are full.
    pub fn into_split(self) -> (EnvelopeReader<ReadHalf<S>>, EnvelopeWriter<WriteHalf<S>>) {
        let (read_half, write_half) = tokio::io::split(self.io.inner);
        let reader = EnvelopeReader {
            inner: read_half,
            decoder: self.io.decoder,
            read_buf: self.io.read_buf,
        };
        (reader, EnvelopeWriter { inner: write_half })
    }

    /// Sends our [`Hello`] and returns the peer's. Must be the first exchange of a session.
    pub async fn exchange_hello(&mut self, ours: Hello) -> Result<Hello, TransportError> {
        self.send(&Envelope {
            payload: Some(Payload::Hello(ours)),
        })
        .await?;
        match self.recv().await? {
            Some(Envelope {
                payload: Some(Payload::Hello(mut theirs)),
            }) => {
                if theirs.protocol_version != PROTOCOL_VERSION {
                    return Err(TransportError::VersionMismatch(theirs.protocol_version));
                }
                theirs.device_name = sanitize_device_name(&theirs.device_name);
                Ok(theirs)
            }
            Some(_) => Err(TransportError::UnexpectedMessage { expected: "Hello" }),
            None => Err(TransportError::Closed),
        }
    }

    /// Closes the connection, telling the peer it was intentional.
    pub async fn close(mut self) -> Result<(), TransportError> {
        self.io.inner.shutdown().await?;
        Ok(())
    }
}

/// Receiving side of a connection: decodes frames from a byte stream.
#[derive(Debug)]
pub struct EnvelopeReader<R> {
    inner: R,
    decoder: FrameDecoder,
    read_buf: Vec<u8>,
}

impl<R> EnvelopeReader<R> {
    fn new(inner: R) -> Self {
        Self {
            inner,
            decoder: FrameDecoder::new(),
            read_buf: vec![0; READ_BUF_LEN],
        }
    }
}

impl<R: AsyncRead + Unpin> EnvelopeReader<R> {
    /// Next envelope from the peer, or `None` once the peer has closed the connection cleanly.
    ///
    /// Cancel-safe: dropping the future loses no data, so it can be used in `tokio::select!`.
    pub async fn recv(&mut self) -> Result<Option<Envelope>, TransportError> {
        loop {
            if let Some(envelope) = self.decoder.next_envelope()? {
                return Ok(Some(envelope));
            }
            let read = self.inner.read(&mut self.read_buf).await?;
            if read == 0 {
                if self.decoder.buffered_len() == 0 {
                    return Ok(None);
                }
                return Err(std::io::Error::from(std::io::ErrorKind::UnexpectedEof).into());
            }
            self.decoder.push(&self.read_buf[..read]);
        }
    }
}

/// Sending side of a connection.
#[derive(Debug)]
pub struct EnvelopeWriter<W> {
    inner: W,
}

impl<W: AsyncWrite + Unpin> EnvelopeWriter<W> {
    pub async fn send(&mut self, envelope: &Envelope) -> Result<(), TransportError> {
        write_frame(&mut self.inner, envelope).await
    }

    /// Closes the connection, telling the peer it was intentional.
    pub async fn close(mut self) -> Result<(), TransportError> {
        self.inner.shutdown().await?;
        Ok(())
    }
}

async fn write_frame<W: AsyncWrite + Unpin>(
    writer: &mut W,
    envelope: &Envelope,
) -> Result<(), TransportError> {
    let frame = encode(envelope)?;
    writer.write_all(&frame).await?;
    writer.flush().await?;
    Ok(())
}

fn provider() -> Arc<CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

fn check_alpn(negotiated: Option<&[u8]>) -> Result<(), TransportError> {
    if negotiated == Some(ALPN) {
        Ok(())
    } else {
        Err(TransportError::WrongProtocol)
    }
}

fn peer_id(certificates: Option<&[CertificateDer<'_>]>) -> Result<DeviceId, TransportError> {
    let end_entity = certificates
        .and_then(<[_]>::first)
        .ok_or(TransportError::NoPeerCertificate)?;
    Ok(DeviceId::of_certificate(end_entity)?)
}

/// Accepts exactly one server key: the pinned device ID.
///
/// SECURITY: trust comes from the device ID exchanged during pairing, not from a certificate
/// authority. Names, validity dates and intermediates are deliberately ignored. The server's
/// proof of key possession is checked in `verify_tls13_signature`, delegated to rustls.
#[derive(Debug)]
struct PinnedServerVerifier {
    expected: DeviceId,
    algorithms: WebPkiSupportedAlgorithms,
}

impl ServerCertVerifier for PinnedServerVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        if DeviceId::of_certificate(end_entity)? == self.expected {
            Ok(ServerCertVerified::assertion())
        } else {
            Err(rustls::Error::InvalidCertificate(
                CertificateError::ApplicationVerificationFailure,
            ))
        }
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Err(tls12_disabled())
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(message, cert, dss, &self.algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.algorithms.supported_schemes()
    }
}

/// Requires a well-formed client certificate without deciding whether its key is trusted.
///
/// SECURITY: the handshake still proves the client holds the key (`verify_tls13_signature`,
/// delegated to rustls). Authorization happens right after, in [`UnverifiedConnection`].
#[derive(Debug)]
struct AnyClientCertificate {
    algorithms: WebPkiSupportedAlgorithms,
}

impl ClientCertVerifier for AnyClientCertificate {
    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &[]
    }

    fn verify_client_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _now: UnixTime,
    ) -> Result<ClientCertVerified, rustls::Error> {
        DeviceId::of_certificate(end_entity)?;
        Ok(ClientCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Err(tls12_disabled())
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(message, cert, dss, &self.algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.algorithms.supported_schemes()
    }
}

/// Only TLS 1.3 is enabled, so the TLS 1.2 callbacks are unreachable.
fn tls12_disabled() -> rustls::Error {
    rustls::Error::General("TLS 1.2 is disabled".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::{Ping, Pong, envelope::Payload};
    use tokio::io::duplex;
    use tokio::net::{TcpListener, TcpStream};

    fn ping(nonce: u64) -> Envelope {
        Envelope {
            payload: Some(Payload::Ping(Ping { nonce })),
        }
    }

    fn pong(nonce: u64) -> Envelope {
        Envelope {
            payload: Some(Payload::Pong(Pong { nonce })),
        }
    }

    #[tokio::test]
    async fn pinned_peers_exchange_envelopes() {
        let (phone, pc) = (Identity::generate().unwrap(), Identity::generate().unwrap());
        let (phone_io, pc_io) = duplex(64 * 1024);
        let phone_id = phone.device_id();

        let (client, server) = tokio::join!(connect(phone_io, &phone, pc.device_id()), async {
            accept(pc_io, &pc)
                .await?
                .authorize(|peer| *peer == phone_id)
        });
        let (mut client, mut server) = (client.unwrap(), server.unwrap());

        assert_eq!(client.peer(), pc.device_id());
        assert_eq!(server.peer(), phone.device_id());

        client.send(&ping(7)).await.unwrap();
        assert_eq!(server.recv().await.unwrap(), Some(ping(7)));
        server.send(&pong(7)).await.unwrap();
        assert_eq!(client.recv().await.unwrap(), Some(pong(7)));

        client.close().await.unwrap();
        assert_eq!(server.recv().await.unwrap(), None);
    }

    #[tokio::test]
    async fn client_rejects_a_server_with_another_key() {
        let (phone, pc, impostor) = (
            Identity::generate().unwrap(),
            Identity::generate().unwrap(),
            Identity::generate().unwrap(),
        );
        let (phone_io, impostor_io) = duplex(64 * 1024);

        let (client, server) = tokio::join!(
            connect(phone_io, &phone, pc.device_id()),
            accept(impostor_io, &impostor)
        );

        assert!(matches!(
            client,
            Err(TransportError::Io(e)) if e.to_string().contains("ApplicationVerificationFailure")
        ));
        assert!(server.is_err());
    }

    #[tokio::test]
    async fn server_refuses_untrusted_devices() {
        let (stranger, pc) = (Identity::generate().unwrap(), Identity::generate().unwrap());
        let (stranger_io, pc_io) = duplex(64 * 1024);

        let (_client, server) = tokio::join!(
            connect(stranger_io, &stranger, pc.device_id()),
            accept(pc_io, &pc)
        );
        let unverified = server.unwrap();

        assert_eq!(unverified.peer(), stranger.device_id());
        assert!(matches!(
            unverified.authorize(|_| false),
            Err(TransportError::UntrustedPeer(id)) if id == stranger.device_id()
        ));
    }

    fn hello(version: u32, name: &str) -> Hello {
        Hello {
            protocol_version: version,
            device_name: name.into(),
            app_version: "test".into(),
            platform: crate::proto::Platform::Linux.into(),
        }
    }

    async fn connected_pair() -> (
        Connection<client::TlsStream<tokio::io::DuplexStream>>,
        Connection<server::TlsStream<tokio::io::DuplexStream>>,
    ) {
        let (phone, pc) = (Identity::generate().unwrap(), Identity::generate().unwrap());
        let (phone_io, pc_io) = duplex(64 * 1024);
        let (client, server) = tokio::join!(connect(phone_io, &phone, pc.device_id()), async {
            accept(pc_io, &pc).await?.authorize(|_| true)
        });
        (client.unwrap(), server.unwrap())
    }

    #[tokio::test]
    async fn hello_exchange_returns_the_peer_hello() {
        let (mut client, mut server) = connected_pair().await;

        let (from_server, from_client) = tokio::join!(
            client.exchange_hello(hello(PROTOCOL_VERSION, "POCO")),
            server.exchange_hello(hello(PROTOCOL_VERSION, "PC\u{7}"))
        );

        assert_eq!(from_server.unwrap().device_name, "PC");
        assert_eq!(from_client.unwrap().device_name, "POCO");
    }

    #[tokio::test]
    async fn hello_exchange_refuses_other_versions() {
        let (mut client, mut server) = connected_pair().await;

        let (from_server, _) = tokio::join!(
            client.exchange_hello(hello(PROTOCOL_VERSION, "POCO")),
            server.exchange_hello(hello(PROTOCOL_VERSION + 1, "Future PC"))
        );

        assert!(matches!(
            from_server,
            Err(TransportError::VersionMismatch(v)) if v == PROTOCOL_VERSION + 1
        ));
    }

    #[tokio::test]
    async fn split_halves_send_large_frames_both_ways_at_once() {
        use crate::proto::ClipChunk;

        let (client, server) = connected_pair().await;
        let (mut client_rx, mut client_tx) = client.into_split();
        let (mut server_rx, mut server_tx) = server.into_split();
        let big = |byte| Envelope {
            payload: Some(Payload::ClipChunk(ClipChunk {
                clip_id: vec![byte; 16],
                offset: 0,
                data: vec![byte; 512 * 1024],
            })),
        };
        let count = 8; // Far more than the 64 KiB duplex buffer holds.

        let (sent_a, sent_b, got_a, got_b) = tokio::join!(
            async {
                for _ in 0..count {
                    client_tx.send(&big(1)).await?;
                }
                Ok::<_, TransportError>(())
            },
            async {
                for _ in 0..count {
                    server_tx.send(&big(2)).await?;
                }
                Ok::<_, TransportError>(())
            },
            async {
                let mut received = 0;
                while received < count {
                    if server_rx.recv().await?.is_some() {
                        received += 1;
                    }
                }
                Ok::<_, TransportError>(received)
            },
            async {
                let mut received = 0;
                while received < count {
                    if client_rx.recv().await?.is_some() {
                        received += 1;
                    }
                }
                Ok::<_, TransportError>(received)
            }
        );

        sent_a.unwrap();
        sent_b.unwrap();
        assert_eq!(got_a.unwrap(), count);
        assert_eq!(got_b.unwrap(), count);
    }

    #[tokio::test]
    async fn works_over_real_tcp() {
        let (phone, pc) = (Identity::generate().unwrap(), Identity::generate().unwrap());
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let address = listener.local_addr().unwrap();
        let phone_id = phone.device_id();

        let server = async {
            let (socket, _) = listener.accept().await?;
            let mut connection = accept(socket, &pc)
                .await?
                .authorize(|peer| *peer == phone_id)?;
            let received = connection.recv().await?;
            Ok::<_, TransportError>(received)
        };
        let client = async {
            let socket = TcpStream::connect(address).await?;
            let mut connection = connect(socket, &phone, pc.device_id()).await?;
            connection.send(&ping(99)).await?;
            Ok::<_, TransportError>(connection)
        };

        let (received, _connection) = tokio::join!(server, client);
        assert_eq!(received.unwrap(), Some(ping(99)));
    }
}
