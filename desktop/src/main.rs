// SPDX-License-Identifier: GPL-3.0-or-later

//! `pasteget`: desktop daemon and command-line tool.

fn main() {
    println!(
        "pasteget {} (core {})",
        env!("CARGO_PKG_VERSION"),
        pasteget_core::VERSION
    );
}
