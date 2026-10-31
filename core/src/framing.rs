// SPDX-License-Identifier: GPL-3.0-or-later

//! Frames on the wire: a 4-byte big-endian length followed by an encoded [`Envelope`].
//!
//! [`FrameDecoder`] is sans-IO: feed it bytes as they arrive and pull envelopes out.
//! It is the code path used by the transport and the target of `fuzz/fuzz_targets/frame_decoder.rs`.

use bytes::{Buf, BytesMut};
use prost::Message;

use crate::proto::Envelope;

/// Size of the length prefix.
pub const HEADER_LEN: usize = 4;

/// Largest encoded envelope accepted, in bytes. Clip data is split into chunks well below this.
pub const MAX_FRAME_LEN: usize = 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum FrameError {
    #[error("frame of {0} bytes exceeds the {MAX_FRAME_LEN}-byte limit")]
    TooLarge(usize),
    #[error("malformed envelope: {0}")]
    Decode(#[from] prost::DecodeError),
}

/// Encodes `envelope` as one frame.
pub fn encode(envelope: &Envelope) -> Result<Vec<u8>, FrameError> {
    let body = envelope.encode_to_vec();
    if body.len() > MAX_FRAME_LEN {
        return Err(FrameError::TooLarge(body.len()));
    }
    let mut frame = Vec::with_capacity(HEADER_LEN + body.len());
    // Fits: body.len() <= MAX_FRAME_LEN < u32::MAX.
    frame.extend_from_slice(&(body.len() as u32).to_be_bytes());
    frame.extend_from_slice(&body);
    Ok(frame)
}

/// Incremental frame decoder.
///
/// After it returns an error the stream is out of sync: close the connection.
#[derive(Debug, Default)]
pub struct FrameDecoder {
    buf: BytesMut,
}

impl FrameDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Appends bytes received from the peer.
    pub fn push(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    /// Bytes received but not yet returned as an envelope.
    pub fn buffered_len(&self) -> usize {
        self.buf.len()
    }

    /// Returns the next complete envelope, or `None` if more bytes are needed.
    pub fn next_envelope(&mut self) -> Result<Option<Envelope>, FrameError> {
        let Some(header) = self.buf.get(..HEADER_LEN) else {
            return Ok(None);
        };
        let mut len_bytes = [0; HEADER_LEN];
        len_bytes.copy_from_slice(header);
        let body_len = u32::from_be_bytes(len_bytes) as usize;
        if body_len > MAX_FRAME_LEN {
            return Err(FrameError::TooLarge(body_len));
        }
        if self.buf.len() < HEADER_LEN + body_len {
            self.buf.reserve(HEADER_LEN + body_len - self.buf.len());
            return Ok(None);
        }

        self.buf.advance(HEADER_LEN);
        let body = self.buf.split_to(body_len);
        Ok(Some(Envelope::decode(body.freeze())?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::{Ping, envelope::Payload};
    use proptest::prelude::*;

    fn ping(nonce: u64) -> Envelope {
        Envelope {
            payload: Some(Payload::Ping(Ping { nonce })),
        }
    }

    #[test]
    fn decodes_back_to_back_frames() {
        let mut decoder = FrameDecoder::new();
        decoder.push(&encode(&ping(1)).unwrap());
        decoder.push(&encode(&ping(2)).unwrap());

        assert_eq!(decoder.next_envelope().unwrap(), Some(ping(1)));
        assert_eq!(decoder.next_envelope().unwrap(), Some(ping(2)));
        assert_eq!(decoder.next_envelope().unwrap(), None);
    }

    #[test]
    fn waits_for_the_rest_of_a_frame() {
        let frame = encode(&ping(42)).unwrap();
        let mut decoder = FrameDecoder::new();

        decoder.push(&frame[..frame.len() - 1]);
        assert_eq!(decoder.next_envelope().unwrap(), None);

        decoder.push(&frame[frame.len() - 1..]);
        assert_eq!(decoder.next_envelope().unwrap(), Some(ping(42)));
    }

    #[test]
    fn empty_envelope_is_a_valid_frame() {
        // Unknown payloads from newer peers decode to an envelope without payload.
        let mut decoder = FrameDecoder::new();
        decoder.push(&[0, 0, 0, 0]);

        assert_eq!(
            decoder.next_envelope().unwrap(),
            Some(Envelope { payload: None })
        );
    }

    #[test]
    fn rejects_oversized_length_before_buffering_the_body() {
        let mut decoder = FrameDecoder::new();
        decoder.push(&(MAX_FRAME_LEN as u32 + 1).to_be_bytes());

        assert!(matches!(
            decoder.next_envelope(),
            Err(FrameError::TooLarge(len)) if len == MAX_FRAME_LEN + 1
        ));
    }

    #[test]
    fn encode_refuses_oversized_envelope() {
        let envelope = Envelope {
            payload: Some(Payload::ClipChunk(crate::proto::ClipChunk {
                clip_id: vec![0; 16],
                offset: 0,
                data: vec![0; MAX_FRAME_LEN],
            })),
        };

        assert!(matches!(encode(&envelope), Err(FrameError::TooLarge(_))));
    }

    #[test]
    fn rejects_garbage_body() {
        let mut decoder = FrameDecoder::new();
        decoder.push(&[0, 0, 0, 2, 0xff, 0xff]);

        assert!(matches!(
            decoder.next_envelope(),
            Err(FrameError::Decode(_))
        ));
    }

    proptest! {
        #[test]
        fn arbitrary_bytes_never_panic(bytes in proptest::collection::vec(any::<u8>(), 0..512)) {
            let mut decoder = FrameDecoder::new();
            decoder.push(&bytes);
            while let Ok(Some(_)) = decoder.next_envelope() {}
        }

        #[test]
        fn frames_survive_any_split(nonces in proptest::collection::vec(any::<u64>(), 1..8), split in 1usize..64) {
            let stream: Vec<u8> = nonces.iter().flat_map(|n| encode(&ping(*n)).unwrap()).collect();
            let mut decoder = FrameDecoder::new();
            let mut decoded = Vec::new();

            for piece in stream.chunks(split) {
                decoder.push(piece);
                while let Some(envelope) = decoder.next_envelope().unwrap() {
                    decoded.push(envelope);
                }
            }

            let expected: Vec<Envelope> = nonces.iter().map(|n| ping(*n)).collect();
            prop_assert_eq!(decoded, expected);
        }
    }
}
