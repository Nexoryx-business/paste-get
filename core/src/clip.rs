// SPDX-License-Identifier: GPL-3.0-or-later

//! Clips and their transfer: one [`ClipOffer`] followed by ordered [`ClipChunk`]s, answered by an
//! [`Ack`]. See `docs/PROTOCOL.md`, section 4.2.
//!
//! Both halves are sans-IO: [`outgoing`] yields the envelopes to send, [`ClipReceiver`] consumes
//! received payloads and tells the caller what to answer.

use std::fmt;

use ring::digest::{SHA256, digest};
use ring::rand::{SecureRandom, SystemRandom};

use crate::identity::DeviceId;
use crate::proto::{Ack, AckStatus, ClipChunk, ClipOffer, Envelope, envelope::Payload};

/// Largest `data` in one chunk.
pub const MAX_CHUNK_LEN: usize = 256 * 1024;

/// Default size limit for received clips.
pub const DEFAULT_MAX_CLIP_LEN: u64 = 64 * 1024 * 1024;

/// MIME types of protocol version 0.
pub mod mime {
    pub const TEXT: &str = "text/plain;charset=utf-8";
    pub const PNG: &str = "image/png";
    /// Sent as it is when the image already is a JPEG: phone screenshots and photos often are,
    /// and re-encoding them as PNG on the phone would cost time and bandwidth.
    pub const JPEG: &str = "image/jpeg";

    const PNG_SIGNATURE: &[u8] = b"\x89PNG\r\n\x1a\n";
    const JPEG_SIGNATURE: &[u8] = b"\xff\xd8\xff";

    pub fn is_supported(mime_type: &str) -> bool {
        [TEXT, PNG, JPEG].contains(&mime_type)
    }

    /// [`PNG`] or [`JPEG`], from the image's signature. `None` for any other data.
    pub fn of_image(data: &[u8]) -> Option<&'static str> {
        if data.starts_with(PNG_SIGNATURE) {
            Some(PNG)
        } else if data.starts_with(JPEG_SIGNATURE) {
            Some(JPEG)
        } else {
            None
        }
    }
}

const CLIP_ID_LEN: usize = 16;
const SHA256_LEN: usize = 32;

#[derive(Debug, thiserror::Error)]
#[error("could not generate a random clip ID")]
pub struct RandomError;

/// Clipboard content in one representation.
#[derive(Clone, PartialEq, Eq)]
pub struct Clip {
    pub mime_type: String,
    pub data: Vec<u8>,
    /// The source marked it as secret: keep it out of history and clipboard managers.
    pub sensitive: bool,
    /// Device where the clip was first copied.
    pub origin: DeviceId,
}

impl Clip {
    pub fn text(text: impl Into<String>, origin: DeviceId) -> Self {
        Self {
            mime_type: mime::TEXT.to_owned(),
            data: text.into().into_bytes(),
            sensitive: false,
            origin,
        }
    }

    pub fn png(data: Vec<u8>, origin: DeviceId) -> Self {
        Self {
            mime_type: mime::PNG.to_owned(),
            data,
            sensitive: false,
            origin,
        }
    }

    /// The text, if this is a text clip.
    pub fn as_text(&self) -> Option<&str> {
        (self.mime_type == mime::TEXT)
            .then(|| std::str::from_utf8(&self.data).ok())
            .flatten()
    }

    pub fn sha256(&self) -> [u8; SHA256_LEN] {
        sha256(&self.data)
    }
}

/// Clip bytes are clipboard contents: only metadata is shown.
impl fmt::Debug for Clip {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Clip")
            .field("mime_type", &self.mime_type)
            .field("len", &self.data.len())
            .field("sensitive", &self.sensitive)
            .field("origin", &self.origin)
            .finish()
    }
}

/// The envelopes that send `clip`: its offer, then its chunks.
pub fn outgoing(clip: &Clip) -> Result<impl Iterator<Item = Envelope> + '_, RandomError> {
    let mut clip_id = [0; CLIP_ID_LEN];
    SystemRandom::new()
        .fill(&mut clip_id)
        .map_err(|_| RandomError)?;

    let offer = Payload::ClipOffer(ClipOffer {
        clip_id: clip_id.to_vec(),
        origin_device: clip.origin.as_bytes().to_vec(),
        sha256: clip.sha256().to_vec(),
        mime_type: clip.mime_type.clone(),
        size: clip.data.len() as u64,
        sensitive: clip.sensitive,
    });
    let chunks = clip
        .data
        .chunks(MAX_CHUNK_LEN)
        .enumerate()
        .map(move |(i, data)| {
            Payload::ClipChunk(ClipChunk {
                clip_id: clip_id.to_vec(),
                offset: (i * MAX_CHUNK_LEN) as u64,
                data: data.to_vec(),
            })
        });

    Ok(std::iter::once(offer)
        .chain(chunks)
        .map(|payload| Envelope {
            payload: Some(payload),
        }))
}

/// What the caller must do after feeding a payload to [`ClipReceiver`].
#[derive(Debug)]
pub enum Received {
    /// More chunks are expected.
    Pending,
    /// The clip is complete and verified. Send `ack` back.
    Clip { clip: Clip, ack: Ack },
    /// The clip was refused or broken and is discarded. Send `ack` back.
    Rejected { ack: Ack },
}

/// Reassembles and verifies incoming clips, one at a time.
#[derive(Debug)]
pub struct ClipReceiver {
    max_len: u64,
    current: Option<Incoming>,
}

struct Incoming {
    clip_id: Vec<u8>,
    origin: DeviceId,
    sha256: Vec<u8>,
    mime_type: String,
    size: u64,
    sensitive: bool,
    data: Vec<u8>,
}

impl fmt::Debug for Incoming {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Incoming")
            .field("mime_type", &self.mime_type)
            .field("size", &self.size)
            .field("received", &self.data.len())
            .finish_non_exhaustive()
    }
}

impl Default for ClipReceiver {
    fn default() -> Self {
        Self::new(DEFAULT_MAX_CLIP_LEN)
    }
}

impl ClipReceiver {
    /// Refuses clips larger than `max_len` bytes.
    pub fn new(max_len: u64) -> Self {
        Self {
            max_len,
            current: None,
        }
    }

    /// A new offer. Cancels any clip still in flight.
    pub fn on_offer(&mut self, offer: ClipOffer) -> Received {
        self.current = None;
        let reject = |status| Received::Rejected {
            ack: ack(&offer.clip_id, status),
        };

        let Ok(origin) = DeviceId::try_from(offer.origin_device.as_slice()) else {
            return reject(AckStatus::ProtocolError);
        };
        if offer.clip_id.len() != CLIP_ID_LEN || offer.sha256.len() != SHA256_LEN {
            return reject(AckStatus::ProtocolError);
        }
        if !mime::is_supported(&offer.mime_type) {
            return reject(AckStatus::UnsupportedType);
        }
        if offer.size > self.max_len {
            return reject(AckStatus::TooLarge);
        }

        let incoming = Incoming {
            // Grow as chunks arrive rather than trusting `size` for one big allocation.
            data: Vec::with_capacity(offer.size.min(MAX_CHUNK_LEN as u64) as usize),
            clip_id: offer.clip_id,
            origin,
            sha256: offer.sha256,
            mime_type: offer.mime_type,
            size: offer.size,
            sensitive: offer.sensitive,
        };
        if incoming.size == 0 {
            return finish(incoming);
        }
        self.current = Some(incoming);
        Received::Pending
    }

    /// The next chunk of the clip in flight.
    pub fn on_chunk(&mut self, chunk: ClipChunk) -> Received {
        let Some(incoming) = self.current.as_mut() else {
            return Received::Rejected {
                ack: ack(&chunk.clip_id, AckStatus::ProtocolError),
            };
        };
        let in_sequence = chunk.clip_id == incoming.clip_id
            && chunk.offset == incoming.data.len() as u64
            && !chunk.data.is_empty()
            && chunk.data.len() <= MAX_CHUNK_LEN
            && chunk.offset + chunk.data.len() as u64 <= incoming.size;
        if !in_sequence {
            self.current = None;
            return Received::Rejected {
                ack: ack(&chunk.clip_id, AckStatus::ProtocolError),
            };
        }

        incoming.data.extend_from_slice(&chunk.data);
        if (incoming.data.len() as u64) < incoming.size {
            return Received::Pending;
        }
        match self.current.take() {
            Some(complete) => finish(complete),
            None => Received::Pending, // Unreachable: checked above.
        }
    }
}

fn finish(incoming: Incoming) -> Received {
    let status = if sha256(&incoming.data).as_slice() != incoming.sha256.as_slice() {
        AckStatus::HashMismatch
    } else if !content_matches_type(&incoming.mime_type, &incoming.data) {
        AckStatus::ProtocolError
    } else {
        AckStatus::Ok
    };
    let ack = ack(&incoming.clip_id, status);
    if status != AckStatus::Ok {
        return Received::Rejected { ack };
    }
    Received::Clip {
        clip: Clip {
            mime_type: incoming.mime_type,
            data: incoming.data,
            sensitive: incoming.sensitive,
            origin: incoming.origin,
        },
        ack,
    }
}

/// Cheap sanity checks. Full image decoding is left to the platform.
fn content_matches_type(mime_type: &str, data: &[u8]) -> bool {
    match mime_type {
        mime::TEXT => std::str::from_utf8(data).is_ok(),
        mime::PNG | mime::JPEG => mime::of_image(data) == Some(mime_type),
        _ => false,
    }
}

fn ack(clip_id: &[u8], status: AckStatus) -> Ack {
    Ack {
        clip_id: clip_id.to_vec(),
        status: status.into(),
    }
}

fn sha256(data: &[u8]) -> [u8; SHA256_LEN] {
    let mut hash = [0; SHA256_LEN];
    hash.copy_from_slice(digest(&SHA256, data).as_ref());
    hash
}

#[cfg(test)]
mod tests {
    use super::*;

    const ORIGIN: DeviceId = DeviceId::from_bytes([9; DeviceId::LEN]);

    fn png_of_len(len: usize) -> Vec<u8> {
        let mut data = b"\x89PNG\r\n\x1a\n".to_vec();
        data.resize(len, 0x42);
        data
    }

    /// Sends `clip` through a receiver and returns what it produced for the last payload.
    fn transfer(clip: &Clip, receiver: &mut ClipReceiver) -> Received {
        let mut last = Received::Pending;
        for envelope in outgoing(clip).unwrap() {
            last = match envelope.payload.unwrap() {
                Payload::ClipOffer(offer) => receiver.on_offer(offer),
                Payload::ClipChunk(chunk) => receiver.on_chunk(chunk),
                other => panic!("unexpected payload {other:?}"),
            };
        }
        last
    }

    fn offer_of(clip: &Clip) -> ClipOffer {
        match outgoing(clip).unwrap().next().unwrap().payload {
            Some(Payload::ClipOffer(offer)) => offer,
            other => panic!("first envelope must be the offer, got {other:?}"),
        }
    }

    fn status(received: &Received) -> AckStatus {
        match received {
            Received::Clip { ack, .. } | Received::Rejected { ack } => ack.status(),
            Received::Pending => panic!("clip still pending"),
        }
    }

    #[test]
    fn text_clip_arrives_intact() {
        let clip = Clip::text("Bonjour 👋", ORIGIN);

        let Received::Clip {
            clip: received,
            ack,
        } = transfer(&clip, &mut ClipReceiver::default())
        else {
            panic!("clip not received");
        };

        assert_eq!(received, clip);
        assert_eq!(received.as_text(), Some("Bonjour 👋"));
        assert_eq!(ack.status(), AckStatus::Ok);
    }

    #[test]
    fn large_png_is_chunked_and_reassembled() {
        let clip = Clip::png(png_of_len(MAX_CHUNK_LEN * 2 + 123), ORIGIN);

        let envelopes = outgoing(&clip).unwrap().count();
        let received = transfer(&clip, &mut ClipReceiver::default());

        assert_eq!(envelopes, 1 + 3);
        assert!(matches!(received, Received::Clip { clip: ref c, .. } if *c == clip));
    }

    #[test]
    fn empty_clip_completes_on_offer() {
        let clip = Clip::text("", ORIGIN);

        assert_eq!(
            status(&transfer(&clip, &mut ClipReceiver::default())),
            AckStatus::Ok
        );
    }

    #[test]
    fn refuses_clips_over_the_limit() {
        let clip = Clip::png(png_of_len(1000), ORIGIN);
        let mut receiver = ClipReceiver::new(999);

        assert_eq!(
            status(&receiver.on_offer(offer_of(&clip))),
            AckStatus::TooLarge
        );
    }

    #[test]
    fn refuses_unknown_types() {
        let mut clip = Clip::text("x", ORIGIN);
        clip.mime_type = "application/x-evil".into();

        assert_eq!(
            status(&ClipReceiver::default().on_offer(offer_of(&clip))),
            AckStatus::UnsupportedType
        );
    }

    #[test]
    fn detects_corrupted_data() {
        let clip = Clip::text("original", ORIGIN);
        let mut receiver = ClipReceiver::default();
        let mut envelopes = outgoing(&clip).unwrap();

        let Some(Payload::ClipOffer(offer)) = envelopes.next().unwrap().payload else {
            panic!("expected offer");
        };
        let Some(Payload::ClipChunk(mut chunk)) = envelopes.next().unwrap().payload else {
            panic!("expected chunk");
        };
        chunk.data = b"tampered".to_vec();

        assert!(matches!(receiver.on_offer(offer), Received::Pending));
        assert_eq!(status(&receiver.on_chunk(chunk)), AckStatus::HashMismatch);
    }

    #[test]
    fn rejects_invalid_utf8_text() {
        let clip = Clip {
            mime_type: mime::TEXT.into(),
            data: vec![0xff, 0xfe],
            sensitive: false,
            origin: ORIGIN,
        };

        assert_eq!(
            status(&transfer(&clip, &mut ClipReceiver::default())),
            AckStatus::ProtocolError
        );
    }

    #[test]
    fn rejects_a_png_that_is_not_a_png() {
        let clip = Clip::png(b"GIF89a".to_vec(), ORIGIN);

        assert_eq!(
            status(&transfer(&clip, &mut ClipReceiver::default())),
            AckStatus::ProtocolError
        );
    }

    #[test]
    fn jpeg_arrives_as_it_is_and_must_be_a_jpeg() {
        let jpeg = Clip {
            mime_type: mime::JPEG.to_owned(),
            data: b"\xff\xd8\xff\xe0 rest of the file".to_vec(),
            sensitive: false,
            origin: ORIGIN,
        };
        let Received::Clip { clip, .. } = transfer(&jpeg, &mut ClipReceiver::default()) else {
            panic!("the JPEG was refused");
        };
        assert_eq!(clip, jpeg);

        // A PNG labelled as a JPEG is refused.
        let mislabelled = Clip {
            mime_type: mime::JPEG.to_owned(),
            ..Clip::png(png_of_len(100), ORIGIN)
        };
        assert_eq!(
            status(&transfer(&mislabelled, &mut ClipReceiver::default())),
            AckStatus::ProtocolError
        );
    }

    #[test]
    fn image_types_come_from_signatures() {
        assert_eq!(mime::of_image(&png_of_len(20)), Some(mime::PNG));
        assert_eq!(mime::of_image(b"\xff\xd8\xff\xdb"), Some(mime::JPEG));
        assert_eq!(mime::of_image(b"GIF89a"), None);
        assert_eq!(mime::of_image(b"\xff\xd8"), None);
        assert_eq!(mime::of_image(b""), None);
    }

    #[test]
    fn rejects_out_of_order_and_stray_chunks() {
        let clip = Clip::png(png_of_len(MAX_CHUNK_LEN + 10), ORIGIN);
        let mut receiver = ClipReceiver::default();
        let envelopes: Vec<_> = outgoing(&clip).unwrap().collect();
        let payload = |i: usize| envelopes[i].payload.clone().unwrap();
        let (Payload::ClipOffer(offer), Payload::ClipChunk(second)) = (payload(0), payload(2))
        else {
            panic!("unexpected layout");
        };

        assert!(matches!(receiver.on_offer(offer), Received::Pending));
        assert_eq!(
            status(&receiver.on_chunk(second.clone())),
            AckStatus::ProtocolError
        );
        // The clip was dropped: further chunks are stray.
        assert_eq!(status(&receiver.on_chunk(second)), AckStatus::ProtocolError);
    }

    #[test]
    fn debug_hides_clip_contents() {
        let clip = Clip::text("hunter2", ORIGIN);

        let shown = format!("{clip:?}");

        assert!(shown.contains("len: 7"));
        assert!(!shown.contains("hunter2"));
    }
}
