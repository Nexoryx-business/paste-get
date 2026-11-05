// SPDX-License-Identifier: GPL-3.0-or-later

//! What a piece of text holds that an app can act on: a web link to offer to open, a one-time
//! code (2FA) to put on a computer's clipboard.
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

/// Only the start of a notification is searched for a code.
pub const MAX_CODE_TEXT_CHARS: usize = 4096;

/// How much farther a word counts when a sentence ends between it and a number.
const SENTENCE_GAP: usize = 100;

/// Shortest and longest one-time codes, in digits.
pub const CODE_DIGITS: std::ops::RangeInclusive<usize> = 4..=8;

/// Words that come with a one-time code, lowercase. English, French, Spanish, Portuguese,
/// Italian, German. Without one of them nearby, a number is just a number (an order, an amount).
const CODE_WORDS: &[&str] = &[
    "code",
    "otp",
    "pin",
    "2fa",
    "passcode",
    "password",
    "verification",
    "verify",
    "authentication",
    "one-time",
    "one time",
    "security",
    "login",
    "log in",
    "sign in",
    "sign-in",
    "mot de passe",
    "vérification",
    "authentification",
    "usage unique",
    "connexion",
    "confirmation",
    "código",
    "codigo",
    "contraseña",
    "verificación",
    "senha",
    "codice",
    "verifica",
    "bestätigung",
    "passwort",
    "kennwort",
    "anmeld",
];

/// The one-time code in a notification's text, digits only: "G-482913 is your Google
/// verification code" gives "482913".
///
/// A code is 4 to 8 digits, possibly in two groups ("482 913", "4829-1374"), standing on its own
/// (not part of a word, an amount, a date, a time or a phone number), in a text that has a word
/// such as "code" or "mot de passe". If several numbers qualify, the one closest to such a word
/// wins; a tie gives nothing rather than a guess.
pub fn one_time_code(text: &str) -> Option<String> {
    let chars: Vec<char> = text.chars().take(MAX_CODE_TEXT_CHARS).collect();
    // Lowercase char by char, so positions stay the same.
    let lower: Vec<char> = chars
        .iter()
        .map(|c| c.to_lowercase().next().unwrap_or(*c))
        .collect();
    let words = code_words(&lower);
    if words.is_empty() {
        return None;
    }

    // How many sentence ends come before each position, to tell in O(1) whether a word is in
    // another sentence than a number.
    let mut sentence_ends = Vec::with_capacity(chars.len() + 1);
    sentence_ends.push(0);
    let mut ends = 0;
    for c in &chars {
        ends += usize::from(matches!(c, '.' | '!' | '?' | '\n'));
        sentence_ends.push(ends);
    }

    let mut best: Option<(usize, String)> = None;
    let mut tie = false;
    for (start, end, code) in candidates(&chars) {
        let distance = words
            .iter()
            .map(|&(word_start, word_end)| {
                let gap = if word_end <= start {
                    word_end..start
                } else if end <= word_start {
                    end..word_start
                } else {
                    return 0;
                };
                // A word in another sentence counts for less: "card 1234. Code: 584921".
                let other_sentence = sentence_ends[gap.end] > sentence_ends[gap.start];
                gap.len() + if other_sentence { SENTENCE_GAP } else { 0 }
            })
            .min()
            .unwrap_or(usize::MAX);
        match &best {
            Some((closest, found)) if distance == *closest && *found != code => tie = true,
            Some((closest, _)) if distance >= *closest => {}
            _ => {
                best = Some((distance, code));
                tie = false;
            }
        }
    }
    if tie {
        None
    } else {
        best.map(|(_, code)| code)
    }
}

/// Where the words of [`CODE_WORDS`] are, as `(start, end)` char positions. A word must start
/// a word of the text ("pin", not "shipping"); short ones must end one too, or be plural.
fn code_words(lower: &[char]) -> Vec<(usize, usize)> {
    let words: Vec<Vec<char>> = CODE_WORDS
        .iter()
        .map(|word| word.chars().collect())
        .collect();
    let mut found = Vec::new();
    for start in 0..lower.len() {
        if start > 0 && lower[start - 1].is_alphanumeric() {
            continue;
        }
        for word in &words {
            let end = start + word.len();
            if lower.get(start..end) != Some(&word[..]) {
                continue;
            }
            let ends_well = word.len() > 4
                || match lower.get(end) {
                    None => true,
                    Some('s') => lower.get(end + 1).is_none_or(|c| !c.is_alphanumeric()),
                    Some(c) => !c.is_alphanumeric(),
                };
            if ends_well {
                found.push((start, end));
            }
        }
    }
    found
}

/// Numbers that could be a code: `(start, end, digits)`.
fn candidates(chars: &[char]) -> Vec<(usize, usize, String)> {
    let digits_at = |at: usize| {
        chars[at..]
            .iter()
            .take_while(|c| c.is_ascii_digit())
            .count()
    };
    let mut found = Vec::new();
    let mut at = 0;
    while at < chars.len() {
        if !chars[at].is_ascii_digit() {
            at += 1;
            continue;
        }
        let first = digits_at(at);
        let mut end = at + first;
        // "482 913" or "4829-1374": two groups of 3 or 4 digits, the same length.
        if (3..=4).contains(&first)
            && matches!(chars.get(end), Some(' ' | '-'))
            && end + 1 < chars.len()
            && digits_at(end + 1) == first
        {
            end += 1 + first;
        }
        let code: String = chars[at..end]
            .iter()
            .filter(|c| c.is_ascii_digit())
            .collect();
        if CODE_DIGITS.contains(&code.len()) && stands_alone(chars, at, end) {
            found.push((at, end, code));
        }
        at = end;
    }
    found
}

/// Not part of a word, an amount, a date, a time, a version or a phone number.
fn stands_alone(chars: &[char], start: usize, end: usize) -> bool {
    let before = start.checked_sub(1).map(|i| chars[i]);
    let after = chars.get(end).copied();
    let after_next = chars.get(end + 1).copied();
    let joined = |c: Option<char>| c.is_some_and(|c| c.is_alphanumeric() || c == '_');
    if joined(before) || joined(after) {
        return false;
    }
    if matches!(before, Some('+' | '#' | '/' | ':' | '€' | '$' | '£' | '¥')) {
        return false;
    }
    if matches!(after, Some('/' | ':' | '%' | '€' | '$' | '£' | '¥')) {
        return false;
    }
    // "1.234,56", "12.05.2026", "v1.2": a separator between digits.
    let digit = |c: Option<char>| c.is_some_and(|c| c.is_ascii_digit());
    let before_previous = start.checked_sub(2).map(|i| chars[i]);
    if matches!(before, Some('.' | ',')) && digit(before_previous) {
        return false;
    }
    if matches!(after, Some('.' | ',')) && digit(after_next) {
        return false;
    }
    true
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
    fn codes_are_found_in_real_messages() {
        for (text, code) in [
            ("G-482913 is your Google verification code.", "482913"),
            (
                "Votre code de vérification est 482 913. Ne le partagez pas.",
                "482913",
            ),
            (
                "Your WhatsApp code: 123-456\nDon't share this code",
                "123456",
            ),
            ("Your Uber code: 7391. Never share this code.", "7391"),
            (
                "Amazon: 731948 is your one-time password. Don't share it.",
                "731948",
            ),
            ("Tu código de Instagram es 58204917", "58204917"),
            ("Ihr Bestätigungscode lautet 4821", "4821"),
            ("123456 est votre mot de passe à usage unique", "123456"),
            ("Use 4829-1374 to sign in", "48291374"),
            // The number next to the word, not the card or the amount.
            (
                "Paiement de 49,99 € avec la carte 1234. Code : 584921",
                "584921",
            ),
            ("Meeting at 12:30, door code 4821", "4821"),
        ] {
            assert_eq!(one_time_code(text).as_deref(), Some(code), "{text:?}");
        }
    }

    #[test]
    fn numbers_that_are_not_codes_are_left_alone() {
        for text in [
            "Your order #123456 has shipped",
            "Your parcel 482913 is shipping today",
            "Payment of 1234,56 accepted",
            "Your code expires in 10 minutes",
            "Promo code SUMMER2025",
            "Call +33612345678 to verify",
            "Code valid until 12/05/2026",
            "Version 2.4.1801 installed, see the release code",
            "Your code costs 1500€",
            "",
            // Two codes, as close to a word each: no guess.
            "Door code 1234, wifi password 56789",
        ] {
            assert_eq!(one_time_code(text), None, "{text:?}");
        }
    }

    #[test]
    fn only_the_start_of_a_long_text_is_searched() {
        let text = format!("{} code 482913", "x ".repeat(MAX_CODE_TEXT_CHARS));
        assert_eq!(one_time_code(&text), None);
    }

    #[test]
    fn length_is_bounded() {
        let long = format!("https://example.com/{}", "a".repeat(MAX_LINK_LEN));
        assert_eq!(link(&long), None);
        let label = format!("https://{}.com", "a".repeat(64));
        assert_eq!(link(&label), None);
    }
}
