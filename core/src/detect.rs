// SPDX-License-Identifier: GPL-3.0-or-later

//! What a piece of text holds that an app can act on, such as a web link to offer to open.
//!
//! The text is untrusted (a clip from another device, a notification from any app): these
//! functions are bounded, never panic, and are fuzzed (`fuzz/fuzz_targets/detect.rs`).

/// Longest link offered to open. Browsers take more, but a copied link is rarely longer.
pub const MAX_LINK_LEN: usize = 2048;

/// A text that is nothing but a web link.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Link {
    /// The link as copied, without the spaces around it.
    pub url: String,
    /// The site it leads to, lowercase, to show the user before they open it: "example.com".
    pub host: String,
}

/// The text as a link to open, if it is a single `http` or `https` link and nothing else.
///
/// Strict on purpose: the user is shown [`Link::host`] and decides from it, so anything that
/// could make the host misleading is refused rather than shown.
pub fn link(text: &str) -> Option<Link> {
    let url = text.trim();
    if url.is_empty() || url.len() > MAX_LINK_LEN {
        return None;
    }
    // Plain ASCII without spaces, quotes or brackets, which is how browsers copy links (they
    // percent-encode the rest). Rules out text around the link, look-alike letters in the host,
    // and characters a command line could read differently.
    if !url
        .bytes()
        .all(|b| b.is_ascii_graphic() && !b"\"<>\\^`{|}".contains(&b))
    {
        return None;
    }
    let rest = strip_prefix_ignore_case(url, "https://")
        .or_else(|| strip_prefix_ignore_case(url, "http://"))?;
    let authority = &rest[..rest.find(['/', '?', '#']).unwrap_or(rest.len())];
    // "https://bank.example@evil.example" leads to evil.example: refused.
    if authority.contains('@') {
        return None;
    }
    let host = host_of(authority)?;
    Some(Link {
        url: url.to_owned(),
        host,
    })
}

fn strip_prefix_ignore_case<'a>(text: &'a str, prefix: &str) -> Option<&'a str> {
    let head = text.get(..prefix.len())?;
    head.eq_ignore_ascii_case(prefix)
        .then(|| &text[prefix.len()..])
}

/// The host of `host[:port]`, lowercase, if both are well formed.
fn host_of(authority: &str) -> Option<String> {
    let (host, port) = match authority.strip_prefix('[') {
        // An IPv6 address: "[2001:db8::1]:8080".
        Some(rest) => {
            let (inside, _) = rest.split_once(']')?;
            let valid = !inside.is_empty()
                && inside
                    .bytes()
                    .all(|b| b.is_ascii_hexdigit() || b == b':' || b == b'.');
            if !valid {
                return None;
            }
            authority.split_at(inside.len() + 2)
        }
        None => {
            let (name, port) = authority.split_at(authority.find(':').unwrap_or(authority.len()));
            if !is_host_name(name) {
                return None;
            }
            (name, port)
        }
    };
    if !port.is_empty() {
        let digits = port.strip_prefix(':')?;
        let valid = (1..=5).contains(&digits.len())
            && digits.bytes().all(|b| b.is_ascii_digit())
            && digits.parse::<u32>().is_ok_and(|n| n <= 65_535);
        if !valid {
            return None;
        }
    }
    // "example.com." is shown as "example.com".
    Some(host.strip_suffix('.').unwrap_or(host).to_ascii_lowercase())
}

/// "example.com", "localhost", "192.168.1.20": labels of letters, digits and inner hyphens.
fn is_host_name(name: &str) -> bool {
    // A final dot is allowed: "example.com." is the same site.
    let name = name.strip_suffix('.').unwrap_or(name);
    !name.is_empty()
        && name.len() <= 253
        && name.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host(text: &str) -> Option<String> {
        link(text).map(|link| link.host)
    }

    #[test]
    fn a_copied_link_is_found_with_its_host() {
        let found = link("  https://www.Example.com/path?q=1#top\n").unwrap();
        assert_eq!(found.url, "https://www.Example.com/path?q=1#top");
        assert_eq!(found.host, "www.example.com");

        assert_eq!(host("HTTP://localhost:8080"), Some("localhost".into()));
        assert_eq!(
            host("http://192.168.1.20/admin"),
            Some("192.168.1.20".into())
        );
        assert_eq!(
            host("https://[2001:DB8::1]:443/"),
            Some("[2001:db8::1]".into())
        );
        assert_eq!(host("https://example.com."), Some("example.com".into()));
        assert_eq!(
            host("https://fr.wikipedia.org/wiki/Presse-papiers_(informatique)"),
            Some("fr.wikipedia.org".into())
        );
    }

    #[test]
    fn text_that_is_more_than_a_link_is_not_one() {
        for text in [
            "",
            "example.com",
            "look: https://example.com",
            "https://example.com and more",
            "https://example.com\nhttps://example.org",
            "www.example.com/page",
            "ftp://example.com/file",
            "javascript:alert(1)",
            "file:///etc/passwd",
            "https://",
            "https:///path",
        ] {
            assert_eq!(link(text), None, "{text:?}");
        }
    }

    #[test]
    fn misleading_or_malformed_hosts_are_refused() {
        for text in [
            // Leads to evil.example, not to the bank.
            "https://bank.example@evil.example/login",
            "https://user:pass@example.com",
            // Look-alike letters.
            "https://exаmple.com",
            "https://-example.com",
            "https://example-.com",
            "https://exa_mple.com",
            "https://example..com",
            "https://example.com:",
            "https://example.com:99999",
            "https://example.com:80a",
            "https://[]/",
            "https://[zz::1]/",
            "https://example.com/\"quoted\"",
            "https://example.com/a|b",
        ] {
            assert_eq!(link(text), None, "{text:?}");
        }
    }

    #[test]
    fn length_is_bounded() {
        let long = format!("https://example.com/{}", "a".repeat(MAX_LINK_LEN));
        assert_eq!(link(&long), None);
        let label = format!("https://{}.com", "a".repeat(64));
        assert_eq!(link(&label), None);
    }
}
