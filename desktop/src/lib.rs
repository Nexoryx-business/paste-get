// SPDX-License-Identifier: GPL-3.0-or-later

//! paste-get desktop daemon. `main.rs` is a thin command-line front end over this library,
//! which also lets integration tests run whole daemons in one process.

pub mod clipboard;
pub mod config;
