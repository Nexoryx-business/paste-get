// SPDX-License-Identifier: GPL-3.0-or-later

//! The phone API against a real desktop daemon (with an in-memory clipboard): what the roadmap
//! asks of the Android app (M3, M4, M6), minus its UI and the phone's own clipboard.

// Test code: a panic is a test failure.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use pasteget_core::clip::mime;
use pasteget_core::identity::Identity;
use pasteget_desktop::clipboard::{Content, MemoryClipboard, Raw, RgbaImage};
use pasteget_desktop::config::Profile;
use pasteget_desktop::daemon::{Daemon, DaemonOptions};
use pasteget_ffi::{ClipDelivery, PastegetError, PhoneClient, PhoneListener};

/// A real 1×1 PNG.
const PNG_1X1: &[u8] = &[
    0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1f, 0x15, 0xc4,
    0x89, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x44, 0x41, 0x54, 0x78, 0xda, 0x63, 0x64, 0x60, 0xf8, 0x5f,
    0x0f, 0x00, 0x02, 0x87, 0x01, 0x80, 0xeb, 0x47, 0xba, 0x92, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45,
    0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
];

fn jpeg(width: u32, height: u32) -> Vec<u8> {
    use image::ImageEncoder;
    let pixels = vec![128; (width * height * 3) as usize];
    let mut jpeg = Vec::new();
    image::codecs::jpeg::JpegEncoder::new(&mut jpeg)
        .write_image(&pixels, width, height, image::ExtendedColorType::Rgb8)
        .unwrap();
    jpeg
}

#[derive(Default)]
struct Recorder {
    connections: Mutex<Vec<bool>>,
    clips: Mutex<Vec<(String, Vec<u8>)>>,
    deliveries: Mutex<Vec<ClipDelivery>>,
}

impl PhoneListener for Recorder {
    fn on_connection_changed(&self, _device_id: String, connected: bool) {
        self.connections.lock().unwrap().push(connected);
    }

    fn on_clip_received(&self, mime_type: String, data: Vec<u8>, _sensitive: bool) {
        self.clips.lock().unwrap().push((mime_type, data));
    }

    fn on_clip_delivered(&self, delivery: ClipDelivery) {
        self.deliveries.lock().unwrap().push(delivery);
    }
}

fn wait_until(what: &str, condition: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !condition() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

struct Pc {
    runtime: tokio::runtime::Runtime,
    daemon: Daemon,
    clipboard: MemoryClipboard,
    _home: tempfile::TempDir,
}

fn start_pc() -> Pc {
    start_pc_in(tempfile::tempdir().unwrap())
}

fn start_pc_in(home: tempfile::TempDir) -> Pc {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let profile = Profile::open_in(home.path(), "pc").unwrap();
    let clipboard = MemoryClipboard::default();
    let options = DaemonOptions {
        listen: SocketAddr::from((Ipv4Addr::LOCALHOST, 0)),
        advertised_ips: Some(vec![IpAddr::V4(Ipv4Addr::LOCALHOST)]),
        poll_interval: Duration::from_millis(10),
        announce: false,
        notifier: None,
    };
    let daemon = runtime
        .block_on(Daemon::start(profile, clipboard.clone(), options))
        .unwrap();
    Pc {
        runtime,
        daemon,
        clipboard,
        _home: home,
    }
}

/// Stops the daemon and starts it again from the same profile, on a new port.
fn restart(pc: Pc) -> Pc {
    let Pc {
        runtime,
        daemon,
        _home: home,
        ..
    } = pc;
    drop(daemon);
    drop(runtime);
    start_pc_in(home)
}

fn path_of(dir: &tempfile::TempDir) -> String {
    dir.path().to_str().unwrap().to_owned()
}

#[test]
fn phone_pairs_then_shares_text_and_screenshots_with_the_pc() {
    let pc = start_pc();
    let qr_code = pc
        .runtime
        .block_on(pc.daemon.open_pairing())
        .unwrap()
        .to_uri();
    let phone_dir = tempfile::tempdir().unwrap();
    let phone = PhoneClient::new(path_of(&phone_dir), "POCO M8 5G".into()).unwrap();

    // Scan the QR code.
    let computer = phone.pair(qr_code).unwrap();
    assert_eq!(computer.device_id, pc.daemon.device_id().to_string());

    let listener = Arc::new(Recorder::default());
    phone.start(listener.clone());
    wait_until("the connection", || phone.connected_count() == 1);
    assert_eq!(*listener.connections.lock().unwrap(), [true]);

    // Share text: it lands on the PC clipboard.
    assert_eq!(phone.send_text("Bonjour du téléphone 👋".into()), 1);
    wait_until("the text on the PC clipboard", || {
        pc.clipboard.get() == Some(Raw::Text("Bonjour du téléphone 👋".into()))
    });

    // Share a screenshot: the PC clipboard holds the image.
    assert_eq!(phone.send_image(PNG_1X1.to_vec()).unwrap(), 1);
    wait_until("the image on the PC clipboard", || {
        matches!(
            pc.clipboard.get(),
            Some(Raw::Image(RgbaImage {
                width: 1,
                height: 1,
                ..
            }))
        )
    });

    // A JPEG screenshot is sent as it is, and decoded by the PC.
    assert_eq!(phone.send_image(jpeg(3, 2)).unwrap(), 1);
    wait_until("the JPEG on the PC clipboard", || {
        matches!(
            pc.clipboard.get(),
            Some(Raw::Image(RgbaImage {
                width: 3,
                height: 2,
                ..
            }))
        )
    });

    // Something sent from the PC reaches the phone.
    pc.daemon.send(Content {
        mime_type: mime::TEXT.into(),
        data: b"from the PC".to_vec(),
        sensitive: false,
    });
    wait_until("the clip on the phone", || {
        !listener.clips.lock().unwrap().is_empty()
    });
    assert_eq!(
        listener.clips.lock().unwrap()[0],
        (mime::TEXT.to_owned(), b"from the PC".to_vec())
    );

    // Stopping closes the connection on the PC side too.
    let phone_id = phone.device_id();
    phone.stop();
    wait_until("the PC to see the phone leave", || {
        let status = pc.runtime.block_on(pc.daemon.status());
        status.devices.iter().all(|d| !d.connected)
    });

    // The app restarts: same identity, same pairing.
    drop(phone);
    let again = PhoneClient::new(path_of(&phone_dir), "POCO M8 5G".into()).unwrap();
    assert_eq!(again.device_id(), phone_id);
    assert_eq!(again.paired_computers(), vec![computer]);
}

#[test]
fn forgetting_a_computer_disconnects_it() {
    let pc = start_pc();
    let qr_code = pc
        .runtime
        .block_on(pc.daemon.open_pairing())
        .unwrap()
        .to_uri();
    let phone_dir = tempfile::tempdir().unwrap();
    let phone = PhoneClient::new(path_of(&phone_dir), "POCO".into()).unwrap();
    let computer = phone.pair(qr_code).unwrap();
    phone.start(Arc::new(Recorder::default()));
    wait_until("the connection", || phone.connected_count() == 1);

    phone.forget(computer.device_id.clone()).unwrap();

    wait_until("the disconnection", || phone.connected_count() == 0);
    assert!(phone.paired_computers().is_empty());
    assert!(matches!(
        phone.forget(computer.device_id),
        Err(PastegetError::UnknownDevice)
    ));
}

#[test]
fn a_computer_found_at_a_new_address_is_reconnected() {
    let pc = start_pc();
    let qr_code = pc
        .runtime
        .block_on(pc.daemon.open_pairing())
        .unwrap()
        .to_uri();
    let phone_dir = tempfile::tempdir().unwrap();
    let phone = PhoneClient::new(path_of(&phone_dir), "POCO".into()).unwrap();
    let computer = phone.pair(qr_code).unwrap();
    phone.start(Arc::new(Recorder::default()));
    wait_until("the connection", || phone.connected_count() == 1);
    let old_address = pc.daemon.local_addr();

    // The PC comes back somewhere else, as after its IP address changed.
    let pc = restart(pc);
    let new_address = pc.daemon.local_addr();
    assert_ne!(new_address, old_address);
    wait_until("the phone to notice", || phone.connected_count() == 0);

    // Discovery finds it there: the phone dials it right away.
    assert!(phone.computer_found(
        computer.device_id.clone(),
        vec!["127.0.0.1".into(), "not an IP".into()],
        new_address.port(),
    ));
    wait_until("the reconnection", || phone.connected_count() == 1);

    // The address that worked is remembered, first, for the next start.
    assert_eq!(
        phone.paired_computers()[0].addresses,
        [new_address.to_string(), old_address.to_string()]
    );
    let stranger = Identity::generate().unwrap().device_id().to_string();
    assert!(!phone.computer_found(stranger, vec!["127.0.0.1".into()], 1));
}

/// Roadmap M4: a screenshot reaches the PC in under a second on a LAN. Loopback is faster than
/// Wi-Fi, so this measures the software's own share: TLS, hashing, chunking and the session.
#[test]
fn a_screenshot_sized_image_is_delivered_in_under_a_second() {
    let pc = start_pc();
    let qr_code = pc
        .runtime
        .block_on(pc.daemon.open_pairing())
        .unwrap()
        .to_uri();
    let phone_dir = tempfile::tempdir().unwrap();
    let phone = PhoneClient::new(path_of(&phone_dir), "POCO".into()).unwrap();
    let computer = phone.pair(qr_code).unwrap();
    let listener = Arc::new(Recorder::default());
    phone.start(listener.clone());
    wait_until("the connection", || phone.connected_count() == 1);

    // A HyperOS screenshot is a JPEG of about 1 MB; take 2 MB. The PC acknowledges it before
    // decoding it, so its content doesn't matter here.
    let mut screenshot = vec![0xff, 0xd8, 0xff, 0xe0];
    screenshot.extend((0..2_000_000u32).map(|i| (i.wrapping_mul(2_654_435_761) >> 24) as u8));
    let size = screenshot.len() as u64;
    assert_eq!(phone.send_image(screenshot).unwrap(), 1);

    wait_until("the delivery", || {
        !listener.deliveries.lock().unwrap().is_empty()
    });
    let delivery = listener.deliveries.lock().unwrap()[0].clone();
    println!("{size} bytes delivered in {} ms", delivery.millis);
    assert_eq!(delivery.device_id, computer.device_id);
    assert_eq!(delivery.mime_type, mime::JPEG);
    assert_eq!(delivery.size, size);
    assert!(delivery.accepted);
    assert!(delivery.millis < 1000, "took {} ms", delivery.millis);
}

/// Roadmap M6: a screenshot copied on the PC reaches the phone, as a PNG the app puts on its
/// clipboard.
#[test]
fn an_image_copied_on_the_pc_reaches_the_phone() {
    let pc = start_pc();
    let qr_code = pc
        .runtime
        .block_on(pc.daemon.open_pairing())
        .unwrap()
        .to_uri();
    let phone_dir = tempfile::tempdir().unwrap();
    let phone = PhoneClient::new(path_of(&phone_dir), "POCO".into()).unwrap();
    phone.pair(qr_code).unwrap();
    let listener = Arc::new(Recorder::default());
    phone.start(listener.clone());
    wait_until("the connection", || phone.connected_count() == 1);

    // Win+Shift+S, Spectacle or grim puts the screenshot on the PC clipboard.
    let screenshot = RgbaImage {
        width: 4,
        height: 3,
        pixels: (0..4 * 3 * 4).map(|i| (i * 21) as u8).collect(),
    };
    pc.clipboard.set(Raw::Image(screenshot.clone()));

    wait_until("the image on the phone", || {
        !listener.clips.lock().unwrap().is_empty()
    });
    let (mime_type, png) = listener.clips.lock().unwrap()[0].clone();
    assert_eq!(mime_type, mime::PNG);
    let received = image::load_from_memory_with_format(&png, image::ImageFormat::Png)
        .unwrap()
        .into_rgba8();
    assert_eq!((received.width(), received.height()), (4, 3));
    assert_eq!(received.into_raw(), screenshot.pixels);
}

#[test]
fn bad_input_is_refused() {
    let phone_dir = tempfile::tempdir().unwrap();
    let phone = PhoneClient::new(path_of(&phone_dir), "POCO".into()).unwrap();

    assert!(matches!(
        phone.pair("https://example.com".into()),
        Err(PastegetError::InvalidCode)
    ));
    assert!(matches!(
        phone.send_image(b"GIF89a".to_vec()),
        Err(PastegetError::UnsupportedImage)
    ));
    assert_eq!(phone.send_text("nobody listening".into()), 0);
}
