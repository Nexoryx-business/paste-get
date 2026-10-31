// SPDX-License-Identifier: GPL-3.0-or-later

//! Roadmap M1, "done when": two peers pair, then exchange text and a PNG, over real TCP and
//! using only the public API.

// Helpers here are test code: a panic is a test failure.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::net::Ipv4Addr;
use std::time::Instant;

use data_encoding::BASE64;
use pasteget_core::clip::{self, Clip, ClipReceiver, Received, mime};
use pasteget_core::echo::EchoGuard;
use pasteget_core::identity::Identity;
use pasteget_core::pairing::{self, PairingError, PairingInvite, PairingWindow};
use pasteget_core::proto::envelope::Payload;
use pasteget_core::proto::{Ack, AckStatus, Envelope, Hello, PROTOCOL_VERSION, Platform};
use pasteget_core::transport::{self, Connection, TransportError};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::{TcpListener, TcpStream};

/// A real 1×1 PNG.
const PNG_1X1: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNkYPhfDwAChwGA60e6kgAAAABJRU5ErkJggg==";

fn hello(device_name: &str, platform: Platform) -> Hello {
    Hello {
        protocol_version: PROTOCOL_VERSION,
        device_name: device_name.into(),
        app_version: pasteget_core::VERSION.into(),
        platform: platform.into(),
    }
}

/// Sends a clip and returns the peer's verdict.
async fn send_clip<S>(connection: &mut Connection<S>, clip: &Clip) -> AckStatus
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    for envelope in clip::outgoing(clip).unwrap() {
        connection.send(&envelope).await.unwrap();
    }
    match connection.recv().await.unwrap() {
        Some(Envelope {
            payload: Some(Payload::Ack(ack)),
        }) => ack.status(),
        other => panic!("expected an Ack, got {other:?}"),
    }
}

/// Receives one clip, acknowledges it, and returns it.
async fn receive_clip<S>(
    connection: &mut Connection<S>,
    receiver: &mut ClipReceiver,
    guard: &EchoGuard,
) -> Clip
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    loop {
        let envelope = connection.recv().await.unwrap().expect("connection closed");
        let received = match envelope.payload {
            Some(Payload::ClipOffer(offer)) => {
                let origin = offer.origin_device.as_slice().try_into().unwrap();
                assert!(guard.accepts_origin(&origin));
                receiver.on_offer(offer)
            }
            Some(Payload::ClipChunk(chunk)) => receiver.on_chunk(chunk),
            other => panic!("unexpected payload {other:?}"),
        };
        match received {
            Received::Pending => {}
            Received::Clip { clip, ack } => {
                reply(connection, ack).await;
                return clip;
            }
            Received::Rejected { ack } => panic!("clip rejected: {:?}", ack.status()),
        }
    }
}

async fn reply<S: AsyncRead + AsyncWrite + Unpin>(connection: &mut Connection<S>, ack: Ack) {
    connection
        .send(&Envelope {
            payload: Some(Payload::Ack(ack)),
        })
        .await
        .unwrap();
}

#[tokio::test]
async fn phone_and_pc_pair_then_exchange_text_and_png() {
    let pc = Identity::generate().unwrap();
    let phone = Identity::generate().unwrap();
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();

    // 1. Pairing. The PC shows a QR code; the phone scans and parses it.
    let mut window = PairingWindow::open(Instant::now()).unwrap();
    let qr_code = window
        .invite(
            &pc,
            "HP-AIO (Windows)",
            vec![listener.local_addr().unwrap()],
        )
        .to_uri();
    let invite: PairingInvite = qr_code.parse().unwrap();

    let (paired_pc, paired_phone) = tokio::join!(
        async {
            let socket = TcpStream::connect(invite.addresses[0])
                .await
                .map_err(TransportError::from)?;
            pairing::pair_with(socket, &phone, &invite, "POCO M8 5G").await
        },
        async {
            let (socket, _) = listener.accept().await.map_err(TransportError::from)?;
            let incoming = transport::accept(socket, &pc).await?;
            pairing::accept_pairing(incoming, &mut window, "HP-AIO (Windows)").await
        }
    );
    let (paired_pc, paired_phone) = (paired_pc.unwrap(), paired_phone.unwrap());
    assert_eq!(paired_pc.device_id, pc.device_id());
    assert_eq!(paired_phone.device_id, phone.device_id());

    // 2. A normal session: pinned TLS, authorized from the stored pairing.
    let (phone_side, pc_side) = tokio::join!(
        async {
            let socket = TcpStream::connect(invite.addresses[0]).await?;
            transport::connect(socket, &phone, paired_pc.device_id).await
        },
        async {
            let (socket, _) = listener.accept().await?;
            transport::accept(socket, &pc)
                .await?
                .authorize(|id| *id == paired_phone.device_id)
        }
    );
    let (mut phone_connection, mut pc_connection) = (phone_side.unwrap(), pc_side.unwrap());

    let (pc_hello, phone_hello) = tokio::join!(
        phone_connection.exchange_hello(hello("POCO M8 5G", Platform::Android)),
        pc_connection.exchange_hello(hello("HP-AIO (Windows)", Platform::Windows))
    );
    assert_eq!(pc_hello.unwrap().platform(), Platform::Windows);
    assert_eq!(phone_hello.unwrap().platform(), Platform::Android);

    let mut pc_receiver = ClipReceiver::default();
    let mut pc_guard = EchoGuard::new(pc.device_id());
    let mut phone_receiver = ClipReceiver::default();
    let phone_guard = EchoGuard::new(phone.device_id());

    // 3. Text copied on the phone reaches the PC.
    let text = Clip::text("Bonjour depuis le téléphone", phone.device_id());
    let (status, received) = tokio::join!(
        send_clip(&mut phone_connection, &text),
        receive_clip(&mut pc_connection, &mut pc_receiver, &pc_guard)
    );
    assert_eq!(status, AckStatus::Ok);
    assert_eq!(received, text);

    // The PC puts it on its clipboard; its watcher then reports it. It must not go back.
    pc_guard.applied_remote(&received);
    assert!(!pc_guard.should_send(mime::TEXT, &received.data));

    // 4. A screenshot (PNG) taken on the phone reaches the PC.
    let screenshot = Clip::png(
        BASE64.decode(PNG_1X1.as_bytes()).unwrap(),
        phone.device_id(),
    );
    let (status, received) = tokio::join!(
        send_clip(&mut phone_connection, &screenshot),
        receive_clip(&mut pc_connection, &mut pc_receiver, &pc_guard)
    );
    assert_eq!(status, AckStatus::Ok);
    assert_eq!(received, screenshot);

    // 5. Something copied on the PC reaches the phone.
    assert!(pc_guard.should_send(mime::TEXT, b"copied on the PC"));
    let from_pc = Clip::text("copied on the PC", pc.device_id());
    let (status, received) = tokio::join!(
        send_clip(&mut pc_connection, &from_pc),
        receive_clip(&mut phone_connection, &mut phone_receiver, &phone_guard)
    );
    assert_eq!(status, AckStatus::Ok);
    assert_eq!(received.as_text(), Some("copied on the PC"));

    // 6. Clean shutdown.
    phone_connection.close().await.unwrap();
    assert!(pc_connection.recv().await.unwrap().is_none());
}

#[tokio::test]
async fn a_used_qr_code_cannot_pair_a_second_device() {
    let pc = Identity::generate().unwrap();
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let mut window = PairingWindow::open(Instant::now()).unwrap();
    let invite: PairingInvite = window
        .invite(&pc, "PC", vec![listener.local_addr().unwrap()])
        .to_uri()
        .parse()
        .unwrap();

    let mut results = Vec::new();
    for name in ["Phone", "Stranger with a photo of the QR code"] {
        let device = Identity::generate().unwrap();
        let (device_side, pc_side) = tokio::join!(
            async {
                let socket = TcpStream::connect(invite.addresses[0])
                    .await
                    .map_err(TransportError::from)?;
                pairing::pair_with(socket, &device, &invite, name).await
            },
            async {
                let (socket, _) = listener.accept().await.map_err(TransportError::from)?;
                let incoming = transport::accept(socket, &pc).await?;
                pairing::accept_pairing(incoming, &mut window, "PC").await
            }
        );
        results.push((device_side, pc_side));
    }

    assert!(results[0].0.is_ok() && results[0].1.is_ok());
    assert!(matches!(results[1].0, Err(PairingError::Rejected)));
    assert!(matches!(results[1].1, Err(PairingError::Closed)));
}
