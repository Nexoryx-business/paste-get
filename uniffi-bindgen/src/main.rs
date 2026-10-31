// SPDX-License-Identifier: GPL-3.0-or-later

//! Generates the Kotlin bindings for `pasteget-ffi`:
//! `cargo run -p uniffi-bindgen -- generate --library <libpasteget_ffi.so> --language kotlin --out-dir <dir>`

fn main() {
    uniffi::uniffi_bindgen_main()
}
