//! Contexts where automatic correction must not touch a word (FR-08).
//!
//! The engine calls these checks before scoring. The rules are intentionally
//! conservative: a false negative (not correcting a URL) is cheap, a false
//! positive (corrupting a path or an email) is not.

/// Whether the current whitespace-delimited token looks like a URL, email,
/// path, version, identifier or shell flag.
#[must_use]
pub fn is_protected_token(token: &str) -> bool {
    if token.is_empty() {
        return false;
    }

    let lower = token.to_ascii_lowercase();

    token.contains("://")
        || token.contains('@')
        || token.contains('/')
        || token.contains('\\')
        || token.contains('_')
        || looks_like_domain(token)
        || lower.starts_with("www.")
        || token.starts_with('$')
        || token.starts_with('#')
        || (token.starts_with('-') && token.chars().count() > 1)
}

fn looks_like_domain(token: &str) -> bool {
    let trimmed = token.trim_end_matches(|ch: char| {
        matches!(
            ch,
            '.' | ',' | ';' | ':' | ')' | ']' | '}' | '"' | '\'' | '!' | '?'
        )
    });
    if trimmed.matches('.').count() >= 2 {
        return true;
    }
    trimmed
        .rsplit_once('.')
        .is_some_and(|(_, tld)| tld.chars().filter(|ch| ch.is_alphabetic()).count() >= 2)
}

/// Whether the token at the end of `text` is protected.
///
/// The engine calls this before pushing the separator, so `text` normally ends
/// with the word being considered. Punctuation of the sentence is not yet in
/// the buffer and therefore does not make an ordinary word look like a domain.
#[must_use]
pub fn is_protected_context(text: &str) -> bool {
    let token = text
        .rsplit(|ch: char| ch.is_whitespace())
        .next()
        .unwrap_or("");
    is_protected_token(token)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_and_emails_are_protected() {
        assert!(is_protected_context("https://example.com"));
        assert!(is_protected_context("http://example.com/path"));
        assert!(is_protected_context("user@example.com"));
        assert!(is_protected_context("www.example.com"));
    }

    #[test]
    fn paths_and_identifiers_are_protected() {
        assert!(is_protected_context("/usr/local/bin"));
        assert!(is_protected_context("src\\main.rs"));
        assert!(is_protected_context("some_identifier"));
        assert!(is_protected_context("v1.2.3"));
        assert!(is_protected_context("$HOME"));
        assert!(is_protected_context("--verbose"));
    }

    #[test]
    fn ordinary_words_are_not_protected() {
        assert!(!is_protected_context("ghbdtn"));
        assert!(!is_protected_context("привет"));
        assert!(!is_protected_context("hello"));
        assert!(!is_protected_context("don't"));
        assert!(!is_protected_context("по-русски"));
        assert!(!is_protected_context("ghbdtn."));
        assert!(!is_protected_context("cfk.n"));
        assert!(!is_protected_context(""));
    }

    #[test]
    fn only_the_last_token_is_checked() {
        assert!(!is_protected_context("https://example.com привет"));
        assert!(is_protected_context("привет https://example.com"));
    }

    #[test]
    fn digits_alone_are_not_protected_tokens() {
        assert!(!is_protected_token("123"));
    }
}
