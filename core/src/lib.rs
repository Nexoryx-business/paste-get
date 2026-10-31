// SPDX-License-Identifier: GPL-3.0-or-later

//! Platform-independent core of paste-get: protocol, crypto, pairing and history.
//!
//! This crate must not contain OS-specific code; desktop and Android apps wrap it.

pub mod clip;
pub mod framing;
pub mod identity;
pub mod pairing;
pub mod proto;
pub mod transport;

/// Version of this crate, reported by the apps that embed it.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_matches_manifest() {
        assert_eq!(VERSION, env!("CARGO_PKG_VERSION"));
    }
}
