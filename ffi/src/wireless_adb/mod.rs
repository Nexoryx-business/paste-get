// SPDX-License-Identifier: GPL-3.0-or-later

//! A minimal ADB client for Android's **wireless debugging**, so the app can get shell rights on
//! its own phone without Shizuku (the "built-in" mode of the automatic copy).
//!
//! It does two things, both only towards the phone itself:
//! 1. [`adb_pair`]: pairs the app's RSA key with wireless debugging, using the 6-digit code
//!    shown by Settings. After that, Android trusts the key like a paired computer.
//! 2. [`adb_exec`]: connects with that key and runs one shell command (the app uses it to start
//!    its shell helper and to grant itself a permission).
//!
//! The protocols are Android's (AOSP `packages/modules/adb`); [`wire`] has the byte layouts.
//! The cryptography is all from audited libraries: TLS 1.3 from rustls, SPAKE2 from BoringSSL
//! (through `boring-sys`, the same implementation adbd uses), HKDF and AES-GCM from ring. RSA
//! keys are made by the app (Android's own provider) and only used here.
//!
//! SECURITY: adbd's TLS certificate can't be checked: adbd makes a fresh random key each time it
//! starts (AOSP `daemon/auth.cpp`, `adbd_auth_tls_handshake`), and Google's own client accepts
//! any (`transport.cpp`, `SetCertVerifyCallback`). What protects each step instead:
//! - pairing: SPAKE2 over the code mixes in keying material exported from this very TLS session,
//!   so a machine in the middle can't complete it without the code;
//! - connecting: only to addresses of this phone ([`check_local`]); the shell helper the app
//!   starts is then authenticated by Android itself (the app checks its Linux UID).
//!   The handshake still checks that the server holds the key of the certificate it presents.

pub mod wire;

use std::io::{Read, Write};
use std::net::{IpAddr, SocketAddr, TcpStream};
use std::sync::Arc;
use std::time::Duration;

use ring::aead::{AES_128_GCM, Aad, LessSafeKey, Nonce, UnboundKey};
use ring::hkdf;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{WebPkiSupportedAlgorithms, verify_tls13_signature};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, UnixTime};
use rustls::{ClientConfig, ClientConnection, DigitallySignedStruct, SignatureScheme, StreamOwned};

use self::wire::{Message, WireError};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const IO_TIMEOUT: Duration = Duration::from_secs(15);
/// Most command output kept: commands run here print a line or two.
const MAX_OUTPUT: usize = 1024 * 1024;

/// `kExportedKeyLabel` in AOSP's `tls_connection.cpp`, with its final NUL (it uses `sizeof`).
const EXPORTED_KEY_LABEL: &[u8] = b"adb-label\0";
const EXPORTED_KEY_LEN: usize = 64;
/// `kClientName` and `kServerName` in `pairing_auth.cpp`, also with the NUL.
const SPAKE2_CLIENT_NAME: &[u8] = b"adb pair client\0";
const SPAKE2_SERVER_NAME: &[u8] = b"adb pair server\0";
const SPAKE2_MAX_MSG_SIZE: usize = 32;
const SPAKE2_MAX_KEY_SIZE: usize = 64;
/// HKDF info in `aes_128_gcm.cpp`, without NUL (it uses `sizeof - 1`).
const AES_KEY_INFO: &[u8] = b"adb pairing_auth aes-128-gcm key";
/// The banner sent in `CNXN`, as the simplest clients do.
const CONNECT_BANNER: &[u8] = b"host::\0";

#[derive(Debug, thiserror::Error, uniffi::Error)]
#[uniffi(flat_error)]
pub enum AdbError {
    #[error("the pairing code is wrong or has expired")]
    WrongCode,
    #[error("wireless debugging doesn't know this app: pair it again")]
    NotAuthorized,
    #[error("refusing to reach {0}: only this phone's own wireless debugging is used")]
    NotLocal(String),
    #[error("could not reach wireless debugging: {0}")]
    Unreachable(String),
    #[error("unexpected answer from wireless debugging: {0}")]
    Protocol(String),
    #[error("the app's debugging key is not usable: {0}")]
    Key(String),
}

impl From<WireError> for AdbError {
    fn from(e: WireError) -> Self {
        Self::Protocol(e.to_string())
    }
}

/// Pairs the app's key with wireless debugging on `host:port` (the pairing port that Settings
/// shows next to the code). `public_key` is the key in adb's format, `<base64> <name>`, which the
/// phone lists under "Paired devices". Returns the phone's GUID.
#[uniffi::export]
pub fn adb_pair(
    host: String,
    port: u16,
    code: String,
    private_key_pkcs8: Vec<u8>,
    public_key: String,
) -> Result<String, AdbError> {
    let key = AppKey::new(private_key_pkcs8)?;
    let socket = connect_local(&host, port)?;
    pair(socket, code.trim().as_bytes(), &key, public_key.as_bytes())
}

/// Runs `command` through `exec:` (no terminal) on `host:port`, wireless debugging's connect
/// port, and returns what it printed.
#[uniffi::export]
pub fn adb_exec(
    host: String,
    port: u16,
    private_key_pkcs8: Vec<u8>,
    command: String,
) -> Result<String, AdbError> {
    let key = AppKey::new(private_key_pkcs8)?;
    let socket = connect_local(&host, port)?;
    let output = exec(socket, &key, &command)?;
    Ok(String::from_utf8_lossy(&output).into_owned())
}

/// The app's RSA key and a self-signed certificate for it.
struct AppKey {
    pkcs8: Vec<u8>,
    certificate: CertificateDer<'static>,
}

impl AppKey {
    fn new(pkcs8: Vec<u8>) -> Result<Self, AdbError> {
        let der = PrivatePkcs8KeyDer::from(pkcs8.clone());
        let key_pair = rcgen::KeyPair::from_pkcs8_der_and_sign_algo(&der, &rcgen::PKCS_RSA_SHA256)
            .map_err(|e| AdbError::Key(e.to_string()))?;
        let params =
            rcgen::CertificateParams::new(Vec::new()).map_err(|e| AdbError::Key(e.to_string()))?;
        let certificate = params
            .self_signed(&key_pair)
            .map_err(|e| AdbError::Key(e.to_string()))?;
        Ok(Self {
            pkcs8,
            certificate: certificate.der().clone(),
        })
    }

    fn tls_config(&self) -> Result<Arc<ClientConfig>, AdbError> {
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let verifier = AdbdCertificate {
            algorithms: provider.signature_verification_algorithms,
        };
        let config = ClientConfig::builder_with_provider(Arc::clone(&provider))
            .with_protocol_versions(&[&rustls::version::TLS13])
            .map_err(|e| AdbError::Protocol(e.to_string()))?
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(verifier))
            .with_client_auth_cert(
                vec![self.certificate.clone()],
                PrivateKeyDer::Pkcs8(self.pkcs8.clone().into()),
            )
            .map_err(|e| AdbError::Key(e.to_string()))?;
        Ok(Arc::new(config))
    }

    fn tls(&self, socket: TcpStream) -> Result<StreamOwned<ClientConnection, TcpStream>, AdbError> {
        // adbd ignores the name; an IP address keeps SNI off the wire.
        let name = ServerName::IpAddress(socket.peer_addr().map_err(io)?.ip().into());
        let mut stream = StreamOwned::new(
            ClientConnection::new(self.tls_config()?, name)
                .map_err(|e| AdbError::Protocol(e.to_string()))?,
            socket,
        );
        while stream.conn.is_handshaking() {
            stream
                .conn
                .complete_io(&mut stream.sock)
                .map_err(handshake_error)?;
        }
        Ok(stream)
    }
}

/// Opens a TCP connection, only to an address of this phone.
fn connect_local(host: &str, port: u16) -> Result<TcpStream, AdbError> {
    let ip: IpAddr = host
        .parse()
        .map_err(|_| AdbError::NotLocal(host.to_owned()))?;
    check_local(ip)?;
    let socket =
        TcpStream::connect_timeout(&SocketAddr::new(ip, port), CONNECT_TIMEOUT).map_err(io)?;
    socket.set_read_timeout(Some(IO_TIMEOUT)).map_err(io)?;
    socket.set_write_timeout(Some(IO_TIMEOUT)).map_err(io)?;
    socket.set_nodelay(true).map_err(io)?;
    Ok(socket)
}

/// Loopback, or an address one of this phone's interfaces has.
fn check_local(ip: IpAddr) -> Result<(), AdbError> {
    if ip.is_loopback() {
        return Ok(());
    }
    let own = if_addrs::get_if_addrs()
        .map_err(|e| AdbError::Unreachable(format!("could not list network interfaces: {e}")))?
        .iter()
        .any(|interface| interface.ip() == ip);
    if own {
        Ok(())
    } else {
        Err(AdbError::NotLocal(ip.to_string()))
    }
}

// --- Pairing -----------------------------------------------------------------------------------

fn pair(
    socket: TcpStream,
    code: &[u8],
    key: &AppKey,
    public_key: &[u8],
) -> Result<String, AdbError> {
    let mut tls = key.tls(socket)?;

    // The password is the code followed by keying material from this TLS session, so the
    // pairing can't be relayed to another connection.
    let mut password = code.to_vec();
    let mut exported = [0; EXPORTED_KEY_LEN];
    tls.conn
        .export_keying_material(&mut exported, EXPORTED_KEY_LABEL, None)
        .map_err(|e| AdbError::Protocol(e.to_string()))?;
    password.extend_from_slice(&exported);

    let mut spake = Spake2::client()?;
    let our_message = spake.generate_message(&password)?;
    write_pairing_packet(&mut tls, wire::SPAKE2_MSG, &our_message)?;
    let their_message = read_pairing_packet(&mut tls, wire::SPAKE2_MSG)?;
    let mut cipher = PairingCipher::new(&spake.process_message(&their_message)?)?;

    let our_info = wire::peer_info(wire::PEER_RSA_PUBLIC_KEY, public_key)?;
    write_pairing_packet(&mut tls, wire::PEER_INFO, &cipher.encrypt(our_info)?)?;
    // A wrong code gives each side a different key: the phone can't read our info and hangs
    // up, or we can't read its.
    let encrypted = read_pairing_packet(&mut tls, wire::PEER_INFO).map_err(|e| match e {
        AdbError::Unreachable(_) => AdbError::WrongCode,
        other => other,
    })?;
    let their_info = cipher.decrypt(encrypted)?;
    let (kind, guid) = wire::parse_peer_info(&their_info)?;
    if kind != wire::PEER_DEVICE_GUID {
        return Err(AdbError::Protocol(format!(
            "peer info of type {kind}, not a device GUID"
        )));
    }
    Ok(String::from_utf8_lossy(&guid).into_owned())
}

fn write_pairing_packet(stream: &mut impl Write, kind: u8, payload: &[u8]) -> Result<(), AdbError> {
    let len =
        u32::try_from(payload.len()).map_err(|_| AdbError::Protocol("packet too large".into()))?;
    stream
        .write_all(&wire::pairing_header(kind, len))
        .map_err(io)?;
    stream.write_all(payload).map_err(io)?;
    stream.flush().map_err(io)
}

fn read_pairing_packet(stream: &mut impl Read, expected: u8) -> Result<Vec<u8>, AdbError> {
    let mut header = [0; wire::PAIRING_HEADER_LEN];
    stream.read_exact(&mut header).map_err(io)?;
    let (kind, len) = wire::parse_pairing_header(&header)?;
    if kind != expected {
        return Err(AdbError::Protocol(format!(
            "pairing packet of type {kind}, expected {expected}"
        )));
    }
    let mut payload = vec![0; len as usize];
    stream.read_exact(&mut payload).map_err(io)?;
    Ok(payload)
}

/// BoringSSL's SPAKE2, which adbd also uses.
struct Spake2(*mut boring_sys::SPAKE2_CTX);

impl Spake2 {
    fn client() -> Result<Self, AdbError> {
        Self::new(
            boring_sys::spake2_role_t::spake2_role_alice,
            SPAKE2_CLIENT_NAME,
            SPAKE2_SERVER_NAME,
        )
    }

    /// The phone's side, for tests.
    #[cfg(test)]
    fn server() -> Result<Self, AdbError> {
        Self::new(
            boring_sys::spake2_role_t::spake2_role_bob,
            SPAKE2_SERVER_NAME,
            SPAKE2_CLIENT_NAME,
        )
    }

    fn new(role: boring_sys::spake2_role_t, me: &[u8], them: &[u8]) -> Result<Self, AdbError> {
        // SAFETY: the names are valid for their lengths; BoringSSL copies them.
        let ctx = unsafe {
            boring_sys::SPAKE2_CTX_new(role, me.as_ptr(), me.len(), them.as_ptr(), them.len())
        };
        if ctx.is_null() {
            return Err(AdbError::Protocol("could not start SPAKE2".into()));
        }
        Ok(Self(ctx))
    }

    fn generate_message(&mut self, password: &[u8]) -> Result<Vec<u8>, AdbError> {
        let mut out = [0; SPAKE2_MAX_MSG_SIZE];
        let mut len = 0;
        // SAFETY: `self.0` is a live context; the buffers are valid for the lengths given.
        let ok = unsafe {
            boring_sys::SPAKE2_generate_msg(
                self.0,
                out.as_mut_ptr(),
                &mut len,
                out.len(),
                password.as_ptr(),
                password.len(),
            )
        };
        if ok != 1 || len == 0 {
            return Err(AdbError::Protocol("SPAKE2 failed".into()));
        }
        Ok(out[..len].to_vec())
    }

    /// The shared key material. Only callable once per context, as BoringSSL requires.
    fn process_message(self, theirs: &[u8]) -> Result<Vec<u8>, AdbError> {
        if theirs.is_empty() || theirs.len() > SPAKE2_MAX_MSG_SIZE {
            return Err(AdbError::Protocol("bad SPAKE2 message size".into()));
        }
        let mut key = [0; SPAKE2_MAX_KEY_SIZE];
        let mut len = 0;
        // SAFETY: as in `generate_message`; consuming `self` keeps this to one call.
        let ok = unsafe {
            boring_sys::SPAKE2_process_msg(
                self.0,
                key.as_mut_ptr(),
                &mut len,
                key.len(),
                theirs.as_ptr(),
                theirs.len(),
            )
        };
        if ok != 1 {
            return Err(AdbError::WrongCode);
        }
        Ok(key[..len].to_vec())
    }
}

impl Drop for Spake2 {
    fn drop(&mut self) {
        // SAFETY: `self.0` came from SPAKE2_CTX_new and is freed only here.
        unsafe { boring_sys::SPAKE2_CTX_free(self.0) }
    }
}

/// AES-128-GCM keyed by HKDF over the SPAKE2 key, with a counter nonce per direction
/// (`aes_128_gcm.cpp`: the 64-bit counter in the first 8 bytes, in memory order).
struct PairingCipher {
    key: LessSafeKey,
    sent: u64,
    received: u64,
}

impl PairingCipher {
    fn new(key_material: &[u8]) -> Result<Self, AdbError> {
        let prk = hkdf::Salt::new(hkdf::HKDF_SHA256, &[]).extract(key_material);
        let mut key = [0; 16];
        prk.expand(&[AES_KEY_INFO], AesKeyLen)
            .and_then(|okm| okm.fill(&mut key))
            .map_err(|_| AdbError::Protocol("HKDF failed".into()))?;
        let key = UnboundKey::new(&AES_128_GCM, &key)
            .map_err(|_| AdbError::Protocol("AES key".into()))?;
        Ok(Self {
            key: LessSafeKey::new(key),
            sent: 0,
            received: 0,
        })
    }

    fn nonce(counter: u64) -> Nonce {
        let mut nonce = [0; 12];
        nonce[..8].copy_from_slice(&counter.to_le_bytes());
        Nonce::assume_unique_for_key(nonce)
    }

    fn encrypt(&mut self, mut data: Vec<u8>) -> Result<Vec<u8>, AdbError> {
        self.key
            .seal_in_place_append_tag(Self::nonce(self.sent), Aad::empty(), &mut data)
            .map_err(|_| AdbError::Protocol("encryption failed".into()))?;
        self.sent += 1;
        Ok(data)
    }

    fn decrypt(&mut self, mut data: Vec<u8>) -> Result<Vec<u8>, AdbError> {
        let plain = self
            .key
            .open_in_place(Self::nonce(self.received), Aad::empty(), &mut data)
            .map_err(|_| AdbError::WrongCode)?;
        let len = plain.len();
        self.received += 1;
        data.truncate(len);
        Ok(data)
    }
}

struct AesKeyLen;

impl hkdf::KeyType for AesKeyLen {
    fn len(&self) -> usize {
        16
    }
}

// --- Running a command -------------------------------------------------------------------------

trait Stream: Read + Write {}
impl<T: Read + Write> Stream for T {}

fn exec(socket: TcpStream, key: &AppKey, command: &str) -> Result<Vec<u8>, AdbError> {
    let mut plain = socket;
    send(
        &mut plain,
        &Message::new(
            wire::A_CNXN,
            wire::A_VERSION,
            wire::MAX_PAYLOAD,
            CONNECT_BANNER,
        ),
    )?;
    let first = receive(&mut plain)?;
    let mut stream: Box<dyn Stream> = match first.command {
        // Wireless debugging always asks for TLS.
        wire::A_STLS => {
            send(
                &mut plain,
                &Message::new(wire::A_STLS, wire::A_STLS_VERSION, 0, Vec::new()),
            )?;
            let mut tls = key.tls(plain)?;
            // TLS 1.3: a key adbd doesn't know shows only on the first read.
            let connected = receive(&mut tls).map_err(|e| match e {
                AdbError::Unreachable(_) => AdbError::NotAuthorized,
                other => other,
            })?;
            expect(&connected, wire::A_CNXN)?;
            Box::new(tls)
        }
        // A device that doesn't ask for authentication (some emulators).
        wire::A_CNXN => Box::new(plain),
        wire::A_AUTH => return Err(AdbError::NotAuthorized),
        other => return Err(unexpected(other)),
    };

    const LOCAL_ID: u32 = 1;
    let mut service = format!("exec:{command}").into_bytes();
    service.push(0);
    send(
        &mut stream,
        &Message::new(wire::A_OPEN, LOCAL_ID, 0, service),
    )?;
    let mut output = Vec::new();
    let mut remote_id = 0;
    loop {
        let message = receive(&mut stream)?;
        match message.command {
            wire::A_OKAY => remote_id = message.arg0,
            wire::A_WRTE => {
                remote_id = message.arg0;
                if output.len() + message.data.len() <= MAX_OUTPUT {
                    output.extend_from_slice(&message.data);
                }
                send(
                    &mut stream,
                    &Message::new(wire::A_OKAY, LOCAL_ID, remote_id, Vec::new()),
                )?;
            }
            wire::A_CLSE => {
                // Best effort: the stream is done either way.
                let _ = send(
                    &mut stream,
                    &Message::new(wire::A_CLSE, LOCAL_ID, remote_id, Vec::new()),
                );
                return Ok(output);
            }
            other => return Err(unexpected(other)),
        }
    }
}

fn send(stream: &mut impl Write, message: &Message) -> Result<(), AdbError> {
    stream.write_all(&message.encode()).map_err(io)?;
    stream.flush().map_err(io)
}

fn receive(stream: &mut (impl Read + ?Sized)) -> Result<Message, AdbError> {
    let mut header = [0; wire::HEADER_LEN];
    stream.read_exact(&mut header).map_err(io)?;
    let header = wire::parse_header(&header)?;
    let mut data = vec![0; header.data_length as usize];
    stream.read_exact(&mut data).map_err(io)?;
    Ok(Message::new(header.command, header.arg0, header.arg1, data))
}

fn expect(message: &Message, command: u32) -> Result<(), AdbError> {
    if message.command == command {
        Ok(())
    } else {
        Err(unexpected(message.command))
    }
}

fn unexpected(command: u32) -> AdbError {
    AdbError::Protocol(format!(
        "unexpected {} message",
        wire::command_name(command)
    ))
}

fn io(e: std::io::Error) -> AdbError {
    AdbError::Unreachable(e.to_string())
}

/// An alert during the handshake usually means adbd refused the key.
fn handshake_error(e: std::io::Error) -> AdbError {
    let refused = e
        .get_ref()
        .and_then(|inner| inner.downcast_ref::<rustls::Error>())
        .is_some_and(|tls| matches!(tls, rustls::Error::AlertReceived(_)));
    if refused {
        AdbError::NotAuthorized
    } else {
        io(e)
    }
}

/// Accepts adbd's certificate, whatever its key: see the module's SECURITY note. The handshake
/// signature is still checked, so the server holds the key it presents.
#[derive(Debug)]
struct AdbdCertificate {
    algorithms: WebPkiSupportedAlgorithms,
}

impl ServerCertVerifier for AdbdCertificate {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Err(rustls::Error::General("TLS 1.2 is disabled".into()))
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

#[cfg(test)]
mod tests;
