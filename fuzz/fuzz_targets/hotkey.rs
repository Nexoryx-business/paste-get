// SPDX-License-Identifier: GPL-3.0-or-later

//! Parses arbitrary text as a hotkey from `settings.toml`.
//! Run with `cargo +nightly fuzz run hotkey`.

#![no_main]

use libfuzzer_sys::fuzz_target;
use pasteget_desktop::hotkeys::Hotkey;

fuzz_target!(|data: &[u8]| {
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };
    if let Ok(hotkey) = text.parse::<Hotkey>() {
        // Whatever parses must survive a round trip, and convert without panicking.
        let reparsed: Hotkey = hotkey.to_string().parse().expect("round trip");
        assert_eq!(reparsed, hotkey);
        let _ = (hotkey.portal_trigger(), hotkey.hyprland_mods(), hotkey.key_name());
    }
});
