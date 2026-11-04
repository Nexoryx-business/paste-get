// SPDX-License-Identifier: GPL-3.0-or-later

//! The last few clips that went through the daemon, in either direction, for the tray's
//! "Recent" menu. Kept in memory only: the encrypted history is a v0.2 feature.
//!
//! Sensitive clips are never kept.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::SystemTime;

/// How many clips are kept.
pub const MAX_ITEMS: usize = 10;
/// Most memory the kept clips may use together. Older clips go first.
pub const MAX_BYTES: usize = 64 * 1024 * 1024;
/// Longest preview, in characters.
pub const PREVIEW_CHARS: usize = 60;

/// A clip that was copied here or received from a device.
#[derive(Clone)]
pub struct RecentClip {
    /// Unique while the daemon runs. Never reused.
    pub id: u64,
    pub mime_type: String,
    pub data: Arc<[u8]>,
    /// The device it came from, or `None` when it was copied on this computer.
    pub from: Option<String>,
    pub at: SystemTime,
}

impl std::fmt::Debug for RecentClip {
    // Never print the contents.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RecentClip")
            .field("id", &self.id)
            .field("mime_type", &self.mime_type)
            .field("len", &self.data.len())
            .field("from", &self.from)
            .finish_non_exhaustive()
    }
}

/// Newest first, bounded by [`MAX_ITEMS`] and [`MAX_BYTES`].
#[derive(Debug)]
pub struct RecentClips {
    items: VecDeque<RecentClip>,
    next_id: u64,
    max_items: usize,
    max_bytes: usize,
}

impl Default for RecentClips {
    fn default() -> Self {
        Self::with_limits(MAX_ITEMS, MAX_BYTES)
    }
}

impl RecentClips {
    pub fn with_limits(max_items: usize, max_bytes: usize) -> Self {
        Self {
            items: VecDeque::new(),
            next_id: 1,
            max_items,
            max_bytes,
        }
    }

    /// Records a clip. The same content again moves to the top instead of showing twice.
    pub fn push(&mut self, mime_type: &str, data: &[u8], from: Option<String>, at: SystemTime) {
        if data.len() > self.max_bytes {
            return;
        }
        self.items
            .retain(|item| item.mime_type != mime_type || *item.data != *data);
        self.items.push_front(RecentClip {
            id: self.next_id,
            mime_type: mime_type.to_owned(),
            data: data.into(),
            from,
            at,
        });
        self.next_id += 1;

        self.items.truncate(self.max_items);
        let mut total = 0;
        let keep = self
            .items
            .iter()
            .take_while(|item| {
                total += item.data.len();
                total <= self.max_bytes
            })
            .count();
        self.items.truncate(keep);
    }

    /// Newest first.
    pub fn list(&self) -> Vec<RecentClip> {
        self.items.iter().cloned().collect()
    }

    pub fn get(&self, id: u64) -> Option<RecentClip> {
        self.items.iter().find(|item| item.id == id).cloned()
    }
}

/// A one-line preview of a text clip: whitespace collapsed, at most [`PREVIEW_CHARS`]
/// characters, with "…" when cut.
pub fn preview(text: &str) -> String {
    // Lazily, so a 64 MiB text costs no more than a short one.
    let collapsed = text
        .split_whitespace()
        .enumerate()
        .flat_map(|(i, word)| (i > 0).then_some(' ').into_iter().chain(word.chars()));
    let mut out: Vec<char> = collapsed.take(PREVIEW_CHARS + 1).collect();
    if out.len() > PREVIEW_CHARS {
        out.truncate(PREVIEW_CHARS - 1);
        while out.last() == Some(&' ') {
            out.pop();
        }
        out.push('…');
    }
    out.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at() -> SystemTime {
        SystemTime::UNIX_EPOCH
    }

    #[test]
    fn newest_first_with_unique_ids() {
        let mut recent = RecentClips::default();
        recent.push("text/plain", b"one", None, at());
        recent.push("text/plain", b"two", Some("phone".into()), at());

        let list = recent.list();
        assert_eq!(&*list[0].data, b"two");
        assert_eq!(list[0].from.as_deref(), Some("phone"));
        assert_eq!(&*list[1].data, b"one");
        assert_ne!(list[0].id, list[1].id);
        assert_eq!(
            recent.get(list[1].id).map(|c| c.data),
            Some(list[1].data.clone())
        );
    }

    #[test]
    fn the_same_content_moves_to_the_top() {
        let mut recent = RecentClips::default();
        recent.push("text/plain", b"one", None, at());
        recent.push("text/plain", b"two", None, at());
        recent.push("text/plain", b"one", Some("phone".into()), at());

        let list = recent.list();
        assert_eq!(list.len(), 2);
        assert_eq!(&*list[0].data, b"one");
        assert_eq!(list[0].from.as_deref(), Some("phone"));
    }

    #[test]
    fn keeps_at_most_max_items() {
        let mut recent = RecentClips::with_limits(3, 1000);
        for i in 0..5u8 {
            recent.push("text/plain", &[b'a' + i], None, at());
        }

        let data: Vec<u8> = recent.list().iter().map(|c| c.data[0]).collect();
        assert_eq!(data, b"edc");
        // Dropped ids are gone for good.
        assert!(recent.get(1).is_none());
    }

    #[test]
    fn drops_older_clips_beyond_max_bytes() {
        let mut recent = RecentClips::with_limits(10, 10);
        recent.push("image/png", &[0; 6], None, at());
        recent.push("image/png", &[1; 6], None, at());
        recent.push("image/png", &[2; 11], None, at());

        let list = recent.list();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].data[0], 1);
    }

    #[test]
    fn debug_hides_contents() {
        let mut recent = RecentClips::default();
        recent.push("text/plain", b"secret words", None, at());

        assert!(!format!("{recent:?}").contains("secret"));
    }

    #[test]
    fn previews_are_one_short_line() {
        assert_eq!(preview("  hello \n\t world  "), "hello world");
        assert_eq!(preview(""), "");
        let long = "word ".repeat(40);
        let short = preview(&long);
        assert_eq!(short.chars().count(), PREVIEW_CHARS);
        assert!(short.ends_with("word…"));
        let wide = "é".repeat(100);
        assert_eq!(preview(&wide).chars().count(), PREVIEW_CHARS);
        assert_eq!(
            preview(&"x".repeat(PREVIEW_CHARS)),
            "x".repeat(PREVIEW_CHARS)
        );
    }
}
