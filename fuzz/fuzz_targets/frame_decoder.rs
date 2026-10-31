// SPDX-License-Identifier: GPL-3.0-or-later

//! Feeds arbitrary bytes to the frame decoder, split at arbitrary points.
//! Run with `cargo +nightly fuzz run frame_decoder`.

#![no_main]

use libfuzzer_sys::fuzz_target;
use pasteget_core::framing::FrameDecoder;

fuzz_target!(|data: &[u8]| {
    // The first byte picks how the stream is split into reads.
    let Some((&split, stream)) = data.split_first() else {
        return;
    };
    let piece_len = usize::from(split).max(1);

    let mut decoder = FrameDecoder::new();
    for piece in stream.chunks(piece_len) {
        decoder.push(piece);
        loop {
            match decoder.next_envelope() {
                Ok(Some(_)) => {}
                Ok(None) => break,
                Err(_) => return,
            }
        }
    }
});
