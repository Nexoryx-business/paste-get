// SPDX-License-Identifier: GPL-3.0-or-later

//! Parses arbitrary text as a scanned pairing QR code.
//! Run with `cargo +nightly fuzz run pairing_invite`.

#![no_main]

use libfuzzer_sys::fuzz_target;
use pasteget_core::pairing::PairingInvite;

fuzz_target!(|data: &[u8]| {
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };
    if let Ok(invite) = text.parse::<PairingInvite>() {
        // Whatever parses must survive a round trip.
        let reparsed: PairingInvite = invite.to_uri().parse().expect("round trip");
        assert_eq!(reparsed.device_id, invite.device_id);
        assert_eq!(reparsed.addresses, invite.addresses);
    }
});
