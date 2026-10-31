// SPDX-License-Identifier: GPL-3.0-or-later

//! Breaks echo loops: a clip received from a peer and put on the local clipboard must not be
//! picked up by the local clipboard watcher and sent back. See `docs/PROTOCOL.md`, section 6.

use ring::digest::{Context, SHA256};

use crate::clip::Clip;
use crate::identity::DeviceId;

/// Decides which local clipboard changes are worth sending.
///
/// Rule: send only when the clipboard holds something new. Content put there from a peer counts
/// as already seen, so it is never sent back, even if the OS reports the change several times.
#[derive(Debug)]
pub struct EchoGuard {
    me: DeviceId,
    last_seen: Option<[u8; 32]>,
}

impl EchoGuard {
    pub fn new(me: DeviceId) -> Self {
        Self {
            me,
            last_seen: None,
        }
    }

    /// Call right after putting a received clip on the local clipboard.
    pub fn applied_remote(&mut self, clip: &Clip) {
        self.last_seen = Some(fingerprint(&clip.mime_type, &clip.data));
    }

    /// Call when the local clipboard changes. `false` means it's not new: don't send it.
    pub fn should_send(&mut self, mime_type: &str, data: &[u8]) -> bool {
        let current = fingerprint(mime_type, data);
        if self.last_seen == Some(current) {
            return false;
        }
        self.last_seen = Some(current);
        true
    }

    /// Whether an incoming offer is worth accepting. A clip that started on this device and
    /// came back through another one is an echo.
    pub fn accepts_origin(&self, origin: &DeviceId) -> bool {
        *origin != self.me
    }
}

fn fingerprint(mime_type: &str, data: &[u8]) -> [u8; 32] {
    let mut context = Context::new(&SHA256);
    context.update(mime_type.as_bytes());
    context.update(&[0]);
    context.update(data);
    let mut hash = [0; 32];
    hash.copy_from_slice(context.finish().as_ref());
    hash
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clip::mime;

    const ME: DeviceId = DeviceId::from_bytes([1; DeviceId::LEN]);
    const PHONE: DeviceId = DeviceId::from_bytes([2; DeviceId::LEN]);

    #[test]
    fn local_copies_are_sent() {
        let mut guard = EchoGuard::new(ME);

        assert!(guard.should_send(mime::TEXT, b"one"));
        assert!(guard.should_send(mime::TEXT, b"two"));
    }

    #[test]
    fn received_clip_is_not_sent_back_even_if_reported_twice() {
        let mut guard = EchoGuard::new(ME);
        let clip = Clip::text("from the phone", PHONE);

        guard.applied_remote(&clip);

        assert!(!guard.should_send(mime::TEXT, b"from the phone"));
        assert!(!guard.should_send(mime::TEXT, b"from the phone"));
    }

    #[test]
    fn new_copy_after_a_received_clip_is_sent() {
        let mut guard = EchoGuard::new(ME);
        guard.applied_remote(&Clip::text("from the phone", PHONE));

        assert!(guard.should_send(mime::TEXT, b"typed on the PC"));
    }

    #[test]
    fn same_bytes_with_another_type_are_new() {
        let mut guard = EchoGuard::new(ME);
        guard.applied_remote(&Clip::text("abc", PHONE));

        assert!(guard.should_send(mime::PNG, b"abc"));
    }

    #[test]
    fn duplicate_os_notifications_are_sent_once() {
        let mut guard = EchoGuard::new(ME);

        assert!(guard.should_send(mime::TEXT, b"copied"));
        assert!(!guard.should_send(mime::TEXT, b"copied"));
    }

    #[test]
    fn own_clips_coming_back_are_refused() {
        let guard = EchoGuard::new(ME);

        assert!(!guard.accepts_origin(&ME));
        assert!(guard.accepts_origin(&PHONE));
    }
}
