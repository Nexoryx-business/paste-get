// SPDX-License-Identifier: GPL-3.0-or-later

//! Parses arbitrary bytes as what wireless debugging sends: ADB message headers, pairing
//! packet headers and peer info. Run with `cargo +nightly fuzz run adb_wire`.

#![no_main]

use libfuzzer_sys::fuzz_target;
use pasteget_ffi::wireless_adb::wire;

fuzz_target!(|data: &[u8]| {
    if let Ok(header) = <&[u8; wire::HEADER_LEN]>::try_from(data.get(..wire::HEADER_LEN).unwrap_or(&[])) {
        if let Ok(parsed) = wire::parse_header(header) {
            assert!(parsed.data_length <= wire::MAX_PAYLOAD);
        }
    }
    if let Ok(header) = <&[u8; wire::PAIRING_HEADER_LEN]>::try_from(data.get(..wire::PAIRING_HEADER_LEN).unwrap_or(&[])) {
        if let Ok((_, len)) = wire::parse_pairing_header(header) {
            assert!(len > 0 && len <= wire::MAX_PAIRING_PAYLOAD);
        }
    }
    if let Ok((_, info)) = wire::parse_peer_info(data) {
        // What parses must survive a round trip.
        assert!(info.len() < wire::PEER_INFO_LEN);
        assert!(!info.contains(&0));
        let rebuilt = wire::peer_info(data[0], &info).expect("fits");
        assert_eq!(wire::parse_peer_info(&rebuilt).expect("round trip").1, info);
    }
});
