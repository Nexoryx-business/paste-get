// SPDX-License-Identifier: GPL-3.0-or-later

//! Tests against fake phones that follow AOSP's server side: a pairing server and an adbd.
//! Both check the client's key for real (test keys, not disabled checks).

use std::net::TcpListener;
use std::thread::{self, JoinHandle};

use rcgen::PublicKeyData;
use rustls::DistinguishedName;
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::server::{ParsedCertificate, ServerConfig, ServerConnection};

use super::*;

/// A 2048-bit RSA key made for these tests with `openssl genpkey`.
const TEST_KEY: &[u8] = include_bytes!("test_key.pk8");
const PUBLIC_KEY: &str = "QAAAAHRlc3Q= paste-get@test";

fn spki_of(pkcs8: &[u8]) -> Vec<u8> {
    let der = PrivatePkcs8KeyDer::from(pkcs8.to_vec());
    rcgen::KeyPair::from_pkcs8_der_and_sign_algo(&der, &rcgen::PKCS_RSA_SHA256)
        .unwrap()
        .subject_public_key_info()
}

/// Requires a client certificate for exactly `expected` (a SubjectPublicKeyInfo), as adbd
/// requires a known key.
#[derive(Debug)]
struct OnlyKey {
    expected: Vec<u8>,
    algorithms: WebPkiSupportedAlgorithms,
}

impl ClientCertVerifier for OnlyKey {
    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &[]
    }

    fn verify_client_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _now: UnixTime,
    ) -> Result<ClientCertVerified, rustls::Error> {
        let parsed = ParsedCertificate::try_from(end_entity)?;
        if parsed.subject_public_key_info().as_ref() == self.expected.as_slice() {
            Ok(ClientCertVerified::assertion())
        } else {
            Err(rustls::Error::InvalidCertificate(
                rustls::CertificateError::ApplicationVerificationFailure,
            ))
        }
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

/// adbd's side of TLS: a fresh certificate, and only `client_spki` accepted.
fn server_config(client_spki: Vec<u8>) -> Arc<ServerConfig> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let key = rcgen::KeyPair::generate().unwrap();
    let certificate = rcgen::CertificateParams::new(Vec::new())
        .unwrap()
        .self_signed(&key)
        .unwrap();
    let verifier = OnlyKey {
        expected: client_spki,
        algorithms: provider.signature_verification_algorithms,
    };
    let config = ServerConfig::builder_with_provider(Arc::clone(&provider))
        .with_protocol_versions(&[&rustls::version::TLS13])
        .unwrap()
        .with_client_cert_verifier(Arc::new(verifier))
        .with_single_cert(
            vec![certificate.der().clone()],
            PrivateKeyDer::Pkcs8(key.serialize_der().into()),
        )
        .unwrap();
    Arc::new(config)
}

fn tls_accept(
    socket: TcpStream,
    config: Arc<ServerConfig>,
) -> Option<StreamOwned<ServerConnection, TcpStream>> {
    let mut stream = StreamOwned::new(ServerConnection::new(config).unwrap(), socket);
    while stream.conn.is_handshaking() {
        stream.conn.complete_io(&mut stream.sock).ok()?;
    }
    Some(stream)
}

/// A pairing server like the phone's, with `code`. Returns what the client sent as its key.
fn pairing_server(code: &'static str, client_spki: Vec<u8>) -> (u16, JoinHandle<Option<Vec<u8>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let handle = thread::spawn(move || {
        let (socket, _) = listener.accept().unwrap();
        let mut tls = tls_accept(socket, server_config(client_spki))?;
        let mut exported = [0; EXPORTED_KEY_LEN];
        tls.conn
            .export_keying_material(&mut exported, EXPORTED_KEY_LABEL, None)
            .unwrap();
        let mut password = code.as_bytes().to_vec();
        password.extend_from_slice(&exported);

        let mut spake = Spake2::server().unwrap();
        let ours = spake.generate_message(&password).unwrap();
        let theirs = read_pairing_packet(&mut tls, wire::SPAKE2_MSG).ok()?;
        write_pairing_packet(&mut tls, wire::SPAKE2_MSG, &ours).ok()?;
        let mut cipher = PairingCipher::new(&spake.process_message(&theirs).ok()?).unwrap();

        let encrypted = read_pairing_packet(&mut tls, wire::PEER_INFO).ok()?;
        // A wrong code: hang up, as adbd does.
        let info = cipher.decrypt(encrypted).ok()?;
        let (kind, key) = wire::parse_peer_info(&info).unwrap();
        assert_eq!(kind, wire::PEER_RSA_PUBLIC_KEY);
        let reply = wire::peer_info(wire::PEER_DEVICE_GUID, b"adb-TEST1234-abcdef").unwrap();
        write_pairing_packet(&mut tls, wire::PEER_INFO, &cipher.encrypt(reply).unwrap()).ok()?;
        Some(key)
    });
    (port, handle)
}

#[test]
fn pairs_with_the_right_code() {
    let (port, server) = pairing_server("482913", spki_of(TEST_KEY));

    let guid = adb_pair(
        "127.0.0.1".into(),
        port,
        " 482913 ".into(),
        TEST_KEY.to_vec(),
        PUBLIC_KEY.into(),
    )
    .unwrap();

    assert_eq!(guid, "adb-TEST1234-abcdef");
    assert_eq!(
        server.join().unwrap().as_deref(),
        Some(PUBLIC_KEY.as_bytes())
    );
}

#[test]
fn a_wrong_code_is_reported_as_such() {
    let (port, server) = pairing_server("482913", spki_of(TEST_KEY));

    let result = adb_pair(
        "127.0.0.1".into(),
        port,
        "000000".into(),
        TEST_KEY.to_vec(),
        PUBLIC_KEY.into(),
    );

    assert!(matches!(result, Err(AdbError::WrongCode)), "{result:?}");
    assert_eq!(server.join().unwrap(), None);
}

/// An adbd with one stream: answers `exec:` with `output`.
fn adbd(client_spki: Vec<u8>, output: &'static [u8]) -> (u16, JoinHandle<Option<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let handle = thread::spawn(move || {
        let (mut socket, _) = listener.accept().unwrap();
        let hello = receive(&mut socket).ok()?;
        assert_eq!(hello.command, wire::A_CNXN);
        assert_eq!(hello.arg0, wire::A_VERSION);
        send(
            &mut socket,
            &Message::new(wire::A_STLS, wire::A_STLS_VERSION, 0, Vec::new()),
        )
        .ok()?;
        let stls = receive(&mut socket).ok()?;
        assert_eq!(stls.command, wire::A_STLS);

        let mut tls = tls_accept(socket, server_config(client_spki))?;
        send(
            &mut tls,
            &Message::new(
                wire::A_CNXN,
                wire::A_VERSION,
                wire::MAX_PAYLOAD,
                b"device::ro.product.name=test;".to_vec(),
            ),
        )
        .ok()?;
        let open = receive(&mut tls).ok()?;
        assert_eq!(open.command, wire::A_OPEN);
        let service = String::from_utf8(open.data).unwrap();
        const REMOTE_ID: u32 = 7;
        send(
            &mut tls,
            &Message::new(wire::A_OKAY, REMOTE_ID, open.arg0, Vec::new()),
        )
        .ok()?;
        send(
            &mut tls,
            &Message::new(wire::A_WRTE, REMOTE_ID, open.arg0, output.to_vec()),
        )
        .ok()?;
        let okay = receive(&mut tls).ok()?;
        assert_eq!((okay.command, okay.arg1), (wire::A_OKAY, REMOTE_ID));
        send(
            &mut tls,
            &Message::new(wire::A_CLSE, REMOTE_ID, open.arg0, Vec::new()),
        )
        .ok()?;
        Some(service)
    });
    (port, handle)
}

#[test]
fn runs_a_command_over_tls() {
    let (port, phone) = adbd(spki_of(TEST_KEY), b"Success\n");

    let output = adb_exec(
        "127.0.0.1".into(),
        port,
        TEST_KEY.to_vec(),
        "pm grant x y".into(),
    )
    .unwrap();

    assert_eq!(output, "Success\n");
    assert_eq!(
        phone.join().unwrap().as_deref(),
        Some("exec:pm grant x y\0")
    );
}

#[test]
fn an_unknown_key_is_refused() {
    let other = rcgen::KeyPair::generate()
        .unwrap()
        .subject_public_key_info();
    let (port, phone) = adbd(other, b"");

    let result = adb_exec("127.0.0.1".into(), port, TEST_KEY.to_vec(), "id".into());

    assert!(matches!(result, Err(AdbError::NotAuthorized)), "{result:?}");
    assert_eq!(phone.join().unwrap(), None);
}

#[test]
fn only_this_phone_is_reached() {
    for host in ["8.8.8.8", "192.0.2.1", "not an address"] {
        let result = adb_exec(host.into(), 5555, TEST_KEY.to_vec(), "id".into());
        assert!(
            matches!(result, Err(AdbError::NotLocal(_))),
            "{host}: {result:?}"
        );
    }
}

#[test]
fn a_bad_key_is_reported() {
    let result = adb_exec("127.0.0.1".into(), 1, b"not a key".to_vec(), "id".into());

    assert!(matches!(result, Err(AdbError::Key(_))), "{result:?}");
}

#[test]
fn the_cipher_uses_a_new_nonce_per_message() {
    let mut cipher = PairingCipher::new(&[7; 64]).unwrap();
    let first = cipher.encrypt(b"same".to_vec()).unwrap();
    let second = cipher.encrypt(b"same".to_vec()).unwrap();
    assert_ne!(first, second);
    assert_eq!(first.len(), 4 + 16);

    // The other side decrypts them in order.
    let mut peer = PairingCipher::new(&[7; 64]).unwrap();
    assert_eq!(peer.decrypt(first).unwrap(), b"same");
    assert_eq!(peer.decrypt(second).unwrap(), b"same");
}
