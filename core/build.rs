// SPDX-License-Identifier: GPL-3.0-or-later

//! Generates Rust types from `proto/pasteget.proto`.
//!
//! `protox` parses the schema in pure Rust, so contributors don't need `protoc` installed.

use std::error::Error;

fn main() -> Result<(), Box<dyn Error>> {
    println!("cargo:rerun-if-changed=../proto");

    let descriptors = protox::compile(["pasteget.proto"], ["../proto"])?;
    prost_build::Config::new()
        // Implemented by hand in `proto.rs` so clip bytes never reach logs.
        .skip_debug([".pasteget.v0.ClipChunk"])
        .compile_fds(descriptors)?;
    Ok(())
}
