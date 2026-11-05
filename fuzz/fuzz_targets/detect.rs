// SPDX-License-Identifier: GPL-3.0-or-later

//! Looks for a link in arbitrary text, as the PC does with every text clip it receives, and for
//! a one-time code, as the phone does with every notification.
//! Run with `cargo +nightly fuzz run detect`.

#![no_main]

use libfuzzer_sys::fuzz_target;
use pasteget_core::detect;

fuzz_target!(|data: &[u8]| {
    let text = String::from_utf8_lossy(data);
    if let Some(link) = detect::link(&text) {
        assert!(link.url.len() <= detect::MAX_LINK_LEN);
        assert!(!link.host.is_empty());
        assert!(link.url.is_ascii() && !link.url.contains(char::is_whitespace));
        // What was found is found again, the same.
        assert_eq!(detect::link(&link.url).as_ref(), Some(&link));
    }
    if let Some(code) = detect::one_time_code(&text) {
        assert!(detect::CODE_DIGITS.contains(&code.len()));
        assert!(code.bytes().all(|b| b.is_ascii_digit()));
    }
});
