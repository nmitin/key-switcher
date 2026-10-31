//! Word segmentation for the correction pipeline.
//!
//! A word is a sequence of alphabetic characters, hyphens and apostrophes
//! (TZ §9.1). Everything else is a separator. Enter and Tab are deliberately
//! not special here: they are not correction triggers in v1 (ADR-001).

/// Whether a character can be part of a word.
#[must_use]
pub fn is_word_char(ch: char) -> bool {
    ch.is_alphabetic() || matches!(ch, '-' | '\'' | '\u{2019}')
}

/// A token kind produced by [`tokenize`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenKind {
    /// Alphabetic characters, hyphens and apostrophes.
    Word,
    /// Everything else.
    Separator,
}

/// A byte range in the source string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Token {
    /// Token kind.
    pub kind: TokenKind,
    /// Start byte offset (inclusive).
    pub start: usize,
    /// End byte offset (exclusive).
    pub end: usize,
}

/// A borrowed word with its byte range.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WordSlice<'a> {
    /// The word itself.
    pub text: &'a str,
    /// Start byte offset (inclusive).
    pub start: usize,
    /// End byte offset (exclusive).
    pub end: usize,
}

/// Split text into word and separator tokens.
#[must_use]
pub fn tokenize(text: &str) -> Vec<Token> {
    let mut tokens = Vec::new();
    let mut iter = text.char_indices().peekable();
    while let Some((start, first)) = iter.next() {
        let kind = if is_word_char(first) {
            TokenKind::Word
        } else {
            TokenKind::Separator
        };
        let mut end = start + first.len_utf8();
        while let Some(&(index, ch)) = iter.peek() {
            if is_word_char(ch) != (kind == TokenKind::Word) {
                break;
            }
            iter.next();
            end = index + ch.len_utf8();
        }
        tokens.push(Token { kind, start, end });
    }
    tokens
}

/// All words in `text`, in order.
#[must_use]
pub fn words(text: &str) -> Vec<WordSlice<'_>> {
    tokenize(text)
        .into_iter()
        .filter(|token| token.kind == TokenKind::Word)
        .filter_map(|token| {
            let word = &text[token.start..token.end];
            if word.chars().any(char::is_alphabetic) {
                Some(WordSlice {
                    text: word,
                    start: token.start,
                    end: token.end,
                })
            } else {
                None
            }
        })
        .collect()
}

/// The last word in `text`, ignoring trailing separators.
///
/// Returns `None` if there is no alphabetic word. This is the word the
/// correction engine looks at when a separator is pressed.
#[must_use]
pub fn last_word(text: &str) -> Option<WordSlice<'_>> {
    tokenize(text)
        .into_iter()
        .rev()
        .filter(|token| token.kind == TokenKind::Word)
        .find_map(|token| {
            let word = &text[token.start..token.end];
            if word.chars().any(char::is_alphabetic) {
                Some(WordSlice {
                    text: word,
                    start: token.start,
                    end: token.end,
                })
            } else {
                None
            }
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokenizes_words_and_separators() {
        let tokens = tokenize("ghbdtn, мир!");
        assert_eq!(
            tokens,
            vec![
                Token {
                    kind: TokenKind::Word,
                    start: 0,
                    end: 6
                },
                Token {
                    kind: TokenKind::Separator,
                    start: 6,
                    end: 8
                },
                Token {
                    kind: TokenKind::Word,
                    start: 8,
                    end: 14
                },
                Token {
                    kind: TokenKind::Separator,
                    start: 14,
                    end: 15
                },
            ]
        );
    }

    #[test]
    fn hyphens_and_apostrophes_belong_to_words() {
        let tokens = tokenize("по-русски don't");
        assert_eq!(tokens[0].kind, TokenKind::Word);
        assert_eq!(
            &"по-русски don't"[tokens[0].start..tokens[0].end],
            "по-русски"
        );
        assert_eq!(tokens[2].kind, TokenKind::Word);
        assert_eq!(&"по-русски don't"[tokens[2].start..tokens[2].end], "don't");
    }

    #[test]
    fn last_word_ignores_trailing_separators() {
        let word = last_word("привет, мир! ").unwrap();
        assert_eq!(word.text, "мир");
        assert_eq!(word.start, 14);
        assert_eq!(word.end, 20);
    }

    #[test]
    fn last_word_is_none_without_letters() {
        assert_eq!(last_word("123 --- !!!").map(|w| w.text), None);
        assert_eq!(last_word("").map(|w| w.text), None);
    }

    #[test]
    fn utf8_offsets_are_byte_offsets() {
        let text = "ёж, ёж";
        let word = last_word(text).unwrap();
        assert_eq!(word.text, "ёж");
        assert_eq!(&text[word.start..word.end], "ёж");
    }

    #[test]
    fn words_returns_all_words() {
        let found: Vec<_> = words("ghbdtn, мир!").into_iter().map(|w| w.text).collect();
        assert_eq!(found, vec!["ghbdtn", "мир"]);
    }
}
