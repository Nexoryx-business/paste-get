// SPDX-License-Identifier: GPL-3.0-or-later

//! Decodes arbitrary bytes as a stream of frames and feeds clip payloads to a receiver.
//! Run with `cargo +nightly fuzz run clip_receiver`.

#![no_main]

use libfuzzer_sys::fuzz_target;
use pasteget_core::clip::{ClipReceiver, Received};
use pasteget_core::framing::FrameDecoder;
use pasteget_core::proto::envelope::Payload;

fuzz_target!(|data: &[u8]| {
    let mut decoder = FrameDecoder::new();
    decoder.push(data);
    let mut receiver = ClipReceiver::new(64 * 1024);

    while let Ok(Some(envelope)) = decoder.next_envelope() {
        let received = match envelope.payload {
            Some(Payload::ClipOffer(offer)) => receiver.on_offer(offer),
            Some(Payload::ClipChunk(chunk)) => receiver.on_chunk(chunk),
            _ => continue,
        };
        if let Received::Clip { clip, .. } = received {
            // A received clip never exceeds the receiver's limit.
            assert!(clip.data.len() <= 64 * 1024);
        }
    }
});
