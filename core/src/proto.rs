// SPDX-License-Identifier: GPL-3.0-or-later

//! Wire messages generated from `proto/pasteget.proto`. See `docs/PROTOCOL.md`.

use std::fmt;

include!(concat!(env!("OUT_DIR"), "/pasteget.v0.rs"));

/// Version of the wire protocol implemented by this crate, sent in [`Hello`].
pub const PROTOCOL_VERSION: u32 = 0;

/// Clip bytes are clipboard contents: only their length is shown.
impl fmt::Debug for ClipChunk {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ClipChunk")
            .field("clip_id", &self.clip_id)
            .field("offset", &self.offset)
            .field("data_len", &self.data.len())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use prost::Message;

    #[test]
    fn envelope_round_trips() {
        let original = Envelope {
            payload: Some(envelope::Payload::ClipOffer(ClipOffer {
                clip_id: vec![7; 16],
                origin_device: vec![1; 32],
                sha256: vec![2; 32],
                mime_type: "text/plain;charset=utf-8".into(),
                size: 5,
                sensitive: true,
            })),
        };

        let decoded = Envelope::decode(original.encode_to_vec().as_slice()).unwrap();

        assert_eq!(decoded, original);
    }

    #[test]
    fn chunk_debug_hides_clip_bytes() {
        let chunk = ClipChunk {
            clip_id: vec![1],
            offset: 0,
            data: b"hunter2".to_vec(),
        };

        let shown = format!("{chunk:?}");

        assert!(shown.contains("data_len: 7"));
        assert!(!shown.contains("104")); // b'h'
    }
}
