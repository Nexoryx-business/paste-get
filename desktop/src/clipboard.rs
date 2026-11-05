// SPDX-License-Identifier: GPL-3.0-or-later

//! The local clipboard: a [`ClipboardBackend`] owned by a dedicated thread that polls for changes
//! and applies clips received from peers.
//!
//! Change detection is polling for now. Windows has a cheap sequence number to skip unchanged
//! polls; native change events for Wayland, X11 and macOS are on the roadmap.

use std::borrow::Cow;
use std::fmt;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::io::Cursor;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use image::codecs::png::{CompressionType, FilterType, PngEncoder};
use image::metadata::Orientation;
use image::{
    DynamicImage, ExtendedColorType, ImageDecoder, ImageEncoder, ImageFormat, ImageReader, Limits,
};
use pasteget_core::clip::mime;
use tokio::sync::mpsc as async_mpsc;

/// Widest or tallest image accepted from a peer, in pixels.
pub const MAX_IMAGE_DIMENSION: u32 = 16_384;
/// Most memory an image may need once decoded.
pub const MAX_IMAGE_ALLOC: u64 = 512 * 1024 * 1024;

/// How often the clipboard is checked for changes.
pub const DEFAULT_POLL_INTERVAL: Duration = Duration::from_millis(500);

/// Clipboard content as the OS exposes it.
#[derive(Clone, PartialEq, Eq)]
pub enum Raw {
    Text(String),
    Image(RgbaImage),
}

/// 8-bit RGBA pixels, row by row.
#[derive(Clone, PartialEq, Eq)]
pub struct RgbaImage {
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u8>,
}

impl Raw {
    /// Identifies content cheaply, to notice changes. Not a security measure.
    fn fingerprint(&self) -> u64 {
        let mut hasher = DefaultHasher::new();
        match self {
            Self::Text(text) => (0u8, text).hash(&mut hasher),
            Self::Image(image) => (1u8, image.width, image.height, &image.pixels).hash(&mut hasher),
        }
        hasher.finish()
    }
}

/// Clipboard contents: only sizes are shown.
impl fmt::Debug for Raw {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Text(text) => write!(f, "Text({} bytes)", text.len()),
            Self::Image(image) => write!(f, "Image({}x{})", image.width, image.height),
        }
    }
}

/// Clipboard content in wire form: UTF-8 text, PNG or JPEG. Local copies are sent as PNG.
#[derive(Clone, PartialEq, Eq)]
pub struct Content {
    pub mime_type: String,
    pub data: Vec<u8>,
    /// Keep out of clipboard history and managers.
    pub sensitive: bool,
}

impl Content {
    pub fn from_raw(raw: Raw) -> Result<Self> {
        let (mime_type, data) = match raw {
            Raw::Text(text) => (mime::TEXT, text.into_bytes()),
            Raw::Image(image) => (mime::PNG, encode_png(&image)?),
        };
        Ok(Self {
            mime_type: mime_type.to_owned(),
            data,
            sensitive: false,
        })
    }

    pub fn to_raw(&self) -> Result<Raw> {
        match self.mime_type.as_str() {
            mime::TEXT => Ok(Raw::Text(
                String::from_utf8(self.data.clone()).context("text is not UTF-8")?,
            )),
            mime::PNG => Ok(Raw::Image(decode_image(
                &self.data,
                ImageFormat::Png,
                image_limits(),
            )?)),
            mime::JPEG => Ok(Raw::Image(decode_image(
                &self.data,
                ImageFormat::Jpeg,
                image_limits(),
            )?)),
            other => anyhow::bail!("unsupported type {other}"),
        }
    }
}

impl fmt::Debug for Content {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Content")
            .field("mime_type", &self.mime_type)
            .field("len", &self.data.len())
            .field("sensitive", &self.sensitive)
            .finish()
    }
}

fn encode_png(image: &RgbaImage) -> Result<Vec<u8>> {
    let mut png = Vec::new();
    // Fast compression: screenshots should reach the PC quickly; the LAN has bandwidth to spare.
    PngEncoder::new_with_quality(&mut png, CompressionType::Fast, FilterType::Adaptive)
        .write_image(
            &image.pixels,
            image.width,
            image.height,
            ExtendedColorType::Rgba8,
        )
        .context("could not encode the image as PNG")?;
    Ok(png)
}

fn image_limits() -> Limits {
    let mut limits = Limits::default();
    limits.max_image_width = Some(MAX_IMAGE_DIMENSION);
    limits.max_image_height = Some(MAX_IMAGE_DIMENSION);
    limits.max_alloc = Some(MAX_IMAGE_ALLOC);
    limits
}

/// Decodes an image from a peer, turned the right way up. `limits` stop small files that claim
/// huge dimensions.
fn decode_image(data: &[u8], format: ImageFormat, limits: Limits) -> Result<RgbaImage> {
    let mut reader = ImageReader::with_format(Cursor::new(data), format);
    reader.limits(limits);
    let mut decoder = reader.into_decoder().context("invalid image")?;
    // Phone photos are often stored sideways, with an EXIF tag saying how to turn them.
    let orientation = decoder.orientation().unwrap_or(Orientation::NoTransforms);
    let mut image = DynamicImage::from_decoder(decoder).context("invalid image")?;
    image.apply_orientation(orientation);
    let image = image.into_rgba8();
    Ok(RgbaImage {
        width: image.width(),
        height: image.height(),
        pixels: image.into_raw(),
    })
}

/// What the daemon needs from a clipboard.
pub trait ClipboardBackend: Send + 'static {
    /// Current content, or `None` if empty or in a format paste-get doesn't handle.
    fn read(&mut self) -> Result<Option<Raw>>;

    /// Puts a clip from another device on the clipboard. It never goes to the Windows cloud
    /// clipboard; a sensitive one stays out of clipboard history too.
    fn write(&mut self, raw: &Raw, sensitive: bool) -> Result<()>;

    /// Cheap pre-check before [`ClipboardBackend::read`]. `false` means the content surely
    /// hasn't changed since the last read or write.
    fn may_have_changed(&mut self) -> bool {
        true
    }
}

/// The OS clipboard.
pub struct SystemClipboard {
    inner: arboard::Clipboard,
    #[cfg(windows)]
    last_sequence: u32,
}

impl SystemClipboard {
    pub fn new() -> Result<Self> {
        Ok(Self {
            inner: arboard::Clipboard::new().context("could not open the clipboard")?,
            #[cfg(windows)]
            last_sequence: 0,
        })
    }
}

impl ClipboardBackend for SystemClipboard {
    fn read(&mut self) -> Result<Option<Raw>> {
        // Text first: it's cheap, and apps that copy images rarely offer text alongside.
        match self.inner.get_text() {
            Ok(text) => return Ok(Some(Raw::Text(text))),
            Err(arboard::Error::ContentNotAvailable) => {}
            Err(e) => return Err(e).context("could not read text from the clipboard"),
        }
        match self.inner.get_image() {
            Ok(image) => Ok(Some(Raw::Image(RgbaImage {
                width: u32::try_from(image.width)?,
                height: u32::try_from(image.height)?,
                pixels: image.bytes.into_owned(),
            }))),
            Err(arboard::Error::ContentNotAvailable) => Ok(None),
            Err(e) => Err(e).context("could not read an image from the clipboard"),
        }
    }

    fn write(&mut self, raw: &Raw, sensitive: bool) -> Result<()> {
        let mut set = self.inner.set();
        if sensitive {
            set = exclude_from_history(set);
        } else {
            set = exclude_from_cloud(set);
        }
        match raw {
            Raw::Text(text) => set.text(text.as_str()),
            Raw::Image(image) => set.image(arboard::ImageData {
                width: image.width as usize,
                height: image.height as usize,
                bytes: Cow::Borrowed(&image.pixels),
            }),
        }
        .context("could not write to the clipboard")?;
        #[cfg(windows)]
        {
            self.last_sequence = windows_clipboard_sequence();
        }
        Ok(())
    }

    #[cfg(windows)]
    fn may_have_changed(&mut self) -> bool {
        let sequence = windows_clipboard_sequence();
        let changed = sequence != self.last_sequence;
        self.last_sequence = sequence;
        changed
    }
}

/// Keeps a sensitive clip out of the OS clipboard history (and Windows cloud clipboard) and out
/// of clipboard managers that follow the common conventions.
fn exclude_from_history(set: arboard::Set<'_>) -> arboard::Set<'_> {
    #[cfg(windows)]
    {
        use arboard::SetExtWindows;
        set.exclude_from_monitoring()
    }
    #[cfg(target_os = "macos")]
    {
        use arboard::SetExtApple;
        set.exclude_from_history()
    }
    #[cfg(all(
        unix,
        not(any(target_os = "macos", target_os = "android", target_os = "emscripten"))
    ))]
    {
        use arboard::SetExtLinux;
        set.exclude_from_history()
    }
}

/// Keeps a received clip out of the Windows cloud clipboard, which would upload it to the user's
/// Microsoft account and other PCs: paste-get promises clips only go to paired devices. It still
/// shows in the local history (Win+V). Nothing to do elsewhere.
fn exclude_from_cloud(set: arboard::Set<'_>) -> arboard::Set<'_> {
    #[cfg(windows)]
    {
        use arboard::SetExtWindows;
        set.exclude_from_cloud()
    }
    #[cfg(not(windows))]
    {
        set
    }
}

/// Increases every time any app changes the clipboard.
#[cfg(windows)]
#[allow(unsafe_code)]
fn windows_clipboard_sequence() -> u32 {
    // SAFETY: GetClipboardSequenceNumber takes no arguments, has no preconditions and only
    // returns a counter.
    unsafe { windows_sys::Win32::System::DataExchange::GetClipboardSequenceNumber() }
}

/// An in-memory clipboard, shared between clones. For tests and for running two daemons on one
/// machine without them fighting over the real clipboard.
#[derive(Clone, Default)]
pub struct MemoryClipboard(Arc<Mutex<Option<Raw>>>);

impl MemoryClipboard {
    pub fn get(&self) -> Option<Raw> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    pub fn set(&self, raw: Raw) {
        *self.0.lock().unwrap_or_else(PoisonError::into_inner) = Some(raw);
    }
}

impl ClipboardBackend for MemoryClipboard {
    fn read(&mut self) -> Result<Option<Raw>> {
        Ok(self.get())
    }

    fn write(&mut self, raw: &Raw, _sensitive: bool) -> Result<()> {
        self.set(raw.clone());
        Ok(())
    }
}

/// Owns the clipboard on its own thread: reports local changes, applies remote clips.
///
/// Content it wrote itself is never reported as a change. What was on the clipboard when the
/// daemon started isn't reported either: only new copies are sent.
#[derive(Debug)]
pub struct ClipboardWatcher {
    requests: mpsc::Sender<Request>,
}

enum Request {
    Write(WriteRequest),
    /// What's on the clipboard now, for "send the clipboard" commands.
    Read(tokio::sync::oneshot::Sender<Result<Option<Content>>>),
}

/// Content from a peer to put on the clipboard.
struct WriteRequest {
    content: Content,
    /// Called once it's on the clipboard, with the time decoding and writing took.
    applied: Box<dyn FnOnce(Duration) + Send>,
}

impl ClipboardWatcher {
    /// Starts the thread. Local changes are sent on `changes`; the thread stops when the
    /// watcher is dropped or `changes` is closed.
    pub fn spawn(
        backend: impl ClipboardBackend,
        poll_interval: Duration,
        changes: async_mpsc::Sender<Content>,
    ) -> Result<Self> {
        let (requests, received) = mpsc::channel();
        thread::Builder::new()
            .name("clipboard".into())
            .spawn(move || watch(backend, poll_interval, &received, &changes))
            .context("could not start the clipboard thread")?;
        Ok(Self { requests })
    }

    /// Reads the clipboard now. `None` when it holds nothing that can be sent.
    pub async fn read(&self) -> Result<Option<Content>> {
        let (reply, answer) = tokio::sync::oneshot::channel();
        self.requests
            .send(Request::Read(reply))
            .map_err(|_| anyhow::anyhow!("the clipboard thread has stopped"))?;
        answer
            .await
            .map_err(|_| anyhow::anyhow!("the clipboard thread has stopped"))?
    }

    /// Puts content received from a peer on the clipboard, then calls `applied` (from the
    /// clipboard thread: keep it quick). Not called if the content can't be applied.
    pub fn write(
        &self,
        content: Content,
        applied: impl FnOnce(Duration) + Send + 'static,
    ) -> Result<()> {
        self.requests
            .send(Request::Write(WriteRequest {
                content,
                applied: Box::new(applied),
            }))
            .map_err(|_| anyhow::anyhow!("the clipboard thread has stopped"))
    }
}

fn watch(
    mut backend: impl ClipboardBackend,
    poll_interval: Duration,
    requests: &mpsc::Receiver<Request>,
    changes: &async_mpsc::Sender<Content>,
) {
    let mut last_seen = backend.read().ok().flatten().map(|raw| raw.fingerprint());

    loop {
        match requests.recv_timeout(poll_interval) {
            Ok(Request::Read(reply)) => {
                let content = match backend.read() {
                    Ok(Some(raw)) => {
                        // Sent by whoever asked: the next poll needn't report it too.
                        last_seen = Some(raw.fingerprint());
                        Content::from_raw(raw).map(Some)
                    }
                    Ok(None) => Ok(None),
                    Err(e) => Err(e),
                };
                let _ = reply.send(content);
            }
            Ok(Request::Write(WriteRequest { content, applied })) => {
                let started = Instant::now();
                let raw = match content.to_raw() {
                    Ok(raw) => raw,
                    Err(e) => {
                        tracing::warn!(error = %e, mime_type = %content.mime_type, "received clip not applied");
                        continue;
                    }
                };
                match backend.write(&raw, content.sensitive) {
                    Ok(()) => {
                        last_seen = Some(raw.fingerprint());
                        applied(started.elapsed());
                    }
                    Err(e) => tracing::warn!(error = %e, "could not write to the clipboard"),
                }
            }
            Err(RecvTimeoutError::Timeout) => {
                if !backend.may_have_changed() {
                    continue;
                }
                let raw = match backend.read() {
                    Ok(Some(raw)) => raw,
                    Ok(None) => continue,
                    Err(e) => {
                        tracing::debug!(error = %e, "could not read the clipboard");
                        continue;
                    }
                };
                let fingerprint = raw.fingerprint();
                if last_seen == Some(fingerprint) {
                    continue;
                }
                last_seen = Some(fingerprint);
                match Content::from_raw(raw) {
                    Ok(content) => {
                        if changes.blocking_send(content).is_err() {
                            return;
                        }
                    }
                    Err(e) => tracing::warn!(error = %e, "could not convert clipboard content"),
                }
            }
            Err(RecvTimeoutError::Disconnected) => return,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const POLL: Duration = Duration::from_millis(5);

    fn image(width: u32, height: u32) -> RgbaImage {
        RgbaImage {
            width,
            height,
            pixels: (0..width * height * 4).map(|i| i as u8).collect(),
        }
    }

    async fn next_change(changes: &mut async_mpsc::Receiver<Content>) -> Option<Content> {
        tokio::time::timeout(Duration::from_millis(500), changes.recv())
            .await
            .ok()
            .flatten()
    }

    #[test]
    fn text_round_trips() {
        let raw = Raw::Text("Ça marche ✓".into());

        let content = Content::from_raw(raw.clone()).unwrap();

        assert_eq!(content.mime_type, mime::TEXT);
        assert_eq!(content.to_raw().unwrap(), raw);
    }

    #[test]
    fn image_round_trips_through_png() {
        let raw = Raw::Image(image(7, 3));

        let content = Content::from_raw(raw.clone()).unwrap();

        assert_eq!(content.mime_type, mime::PNG);
        assert!(content.data.starts_with(b"\x89PNG"));
        assert_eq!(content.to_raw().unwrap(), raw);
    }

    #[test]
    fn oversized_images_are_refused() {
        let png = Content::from_raw(Raw::Image(image(4, 4))).unwrap().data;
        let mut limits = image_limits();
        limits.max_image_width = Some(3);

        assert!(decode_image(&png, ImageFormat::Png, limits).is_err());
    }

    fn jpeg(width: u32, height: u32, exif: Option<Vec<u8>>) -> Content {
        let pixels = vec![200; (width * height * 3) as usize];
        let mut data = Vec::new();
        let mut encoder = image::codecs::jpeg::JpegEncoder::new(&mut data);
        if let Some(exif) = exif {
            encoder.set_exif_metadata(exif).unwrap();
        }
        encoder
            .write_image(&pixels, width, height, ExtendedColorType::Rgb8)
            .unwrap();
        Content {
            mime_type: mime::JPEG.into(),
            data,
            sensitive: false,
        }
    }

    #[test]
    fn jpeg_from_a_phone_becomes_an_image() {
        let Raw::Image(image) = jpeg(4, 2, None).to_raw().unwrap() else {
            panic!("not an image");
        };
        assert_eq!((image.width, image.height), (4, 2));
    }

    #[test]
    fn sideways_photos_are_turned_upright() {
        // A little-endian TIFF block with one tag: orientation (0x0112) = 6, "rotate 90°".
        let exif = vec![
            0x49, 0x49, 0x2a, 0x00, 0x08, 0x00, 0x00, 0x00, 0x01, 0x00, 0x12, 0x01, 0x03, 0x00,
            0x01, 0x00, 0x00, 0x00, 0x06, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        ];

        let Raw::Image(image) = jpeg(4, 2, Some(exif)).to_raw().unwrap() else {
            panic!("not an image");
        };
        assert_eq!((image.width, image.height), (2, 4));
    }

    #[test]
    fn a_png_labelled_jpeg_is_not_decoded_as_one() {
        let mut content = Content::from_raw(Raw::Image(image(2, 2))).unwrap();
        content.mime_type = mime::JPEG.into();

        assert!(content.to_raw().is_err());
    }

    #[test]
    fn debug_hides_contents() {
        let shown = format!(
            "{:?} {:?}",
            Raw::Text("hunter2".into()),
            Content::from_raw(Raw::Text("hunter2".into())).unwrap()
        );

        assert!(!shown.contains("hunter2"));
    }

    #[tokio::test]
    async fn reports_new_copies_but_not_what_was_there_at_start() {
        let clipboard = MemoryClipboard::default();
        clipboard.set(Raw::Text("old".into()));
        let (tx, mut changes) = async_mpsc::channel(8);
        let _watcher = ClipboardWatcher::spawn(clipboard.clone(), POLL, tx).unwrap();

        assert!(next_change(&mut changes).await.is_none());

        clipboard.set(Raw::Text("new".into()));
        let change = next_change(&mut changes).await.unwrap();
        assert_eq!(change.data, b"new");
    }

    #[tokio::test]
    async fn content_it_writes_is_not_reported_back() {
        let clipboard = MemoryClipboard::default();
        let (tx, mut changes) = async_mpsc::channel(8);
        let watcher = ClipboardWatcher::spawn(clipboard.clone(), POLL, tx).unwrap();

        let received = Content::from_raw(Raw::Image(image(2, 2))).unwrap();
        let (applied_tx, applied) = mpsc::channel();
        watcher
            .write(received, move |_| applied_tx.send(()).unwrap())
            .unwrap();
        tokio::time::sleep(POLL * 10).await;

        assert_eq!(clipboard.get(), Some(Raw::Image(image(2, 2))));
        assert!(applied.try_recv().is_ok(), "the write was not reported");
        assert!(next_change(&mut changes).await.is_none());
    }

    #[tokio::test]
    async fn reads_on_demand_without_reporting_it_again() {
        let clipboard = MemoryClipboard::default();
        let (tx, mut changes) = async_mpsc::channel(8);
        let watcher = ClipboardWatcher::spawn(clipboard.clone(), POLL, tx).unwrap();
        assert!(watcher.read().await.unwrap().is_none());

        clipboard.set(Raw::Text("read me".into()));
        let content = watcher.read().await.unwrap().unwrap();

        assert_eq!(content.mime_type, mime::TEXT);
        assert_eq!(content.data, b"read me");
        assert!(next_change(&mut changes).await.is_none());
    }

    #[tokio::test]
    async fn undecodable_clips_are_skipped() {
        let clipboard = MemoryClipboard::default();
        let (tx, _changes) = async_mpsc::channel(8);
        let watcher = ClipboardWatcher::spawn(clipboard.clone(), POLL, tx).unwrap();

        let (applied_tx, applied) = mpsc::channel();
        let broken = Content {
            mime_type: mime::PNG.into(),
            data: b"\x89PNG but not really".to_vec(),
            sensitive: false,
        };
        watcher
            .write(broken, move |_| applied_tx.send(()).unwrap())
            .unwrap();
        tokio::time::sleep(POLL * 10).await;

        assert_eq!(clipboard.get(), None);
        assert!(applied.try_recv().is_err(), "a failed write was reported");
    }
}
