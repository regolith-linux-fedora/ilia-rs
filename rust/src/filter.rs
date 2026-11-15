//! The filter query shared by the pages: text is compared folded, every word
//! must match, and the words may also match as subsequences.

use glib::{NormalizeMode, Unichar, UnicodeType};

/// The form in which the filter compares text: compatibility decomposition
/// (NFKD), marks dropped, then full Unicode case folding. "Café" and "cafe",
/// "Straße" and "strasse", "ﬁle" and "file" have the same folded form.
pub fn fold_text(text: &str) -> String {
    // ASCII has nothing to decompose and no marks; folding it is lowering.
    if text.is_ascii() {
        return text.to_ascii_lowercase();
    }
    let unmarked: String = glib::normalize(text, NormalizeMode::All)
        .chars()
        .filter(|character| {
            !matches!(
                character.unicode_type(),
                UnicodeType::NonSpacingMark | UnicodeType::SpacingMark | UnicodeType::EnclosingMark
            )
        })
        .collect();
    glib::casefold(unmarked).into()
}

/// The words of a filter query, folded. An empty or blank query has no
/// words and matches every row.
pub fn query_words(query: &str) -> Vec<String> {
    fold_text(query)
        .split_whitespace()
        .map(str::to_owned)
        .collect()
}

/// Whether every word occurs in the already folded `text`.
pub fn contains_all_words(text: &str, words: &[String]) -> bool {
    words.iter().all(|word| text.contains(word.as_str()))
}

/// Whether every word occurs in at least one of the already folded fields.
pub fn words_in_fields(words: &[String], fields: &[&str]) -> bool {
    words
        .iter()
        .all(|word| fields.iter().any(|field| field.contains(word.as_str())))
}

/// Whether a row matches the query: every whitespace-separated word occurs,
/// folded and in any order, in at least one of the row's fields.
pub fn matches_all_words(query: &str, fields: &[&str]) -> bool {
    let words = query_words(query);
    if words.is_empty() {
        return true;
    }
    let folded: Vec<String> = fields.iter().map(|field| fold_text(field)).collect();
    let folded: Vec<&str> = folded.iter().map(String::as_str).collect();
    words_in_fields(&words, &folded)
}

/// Whether the characters of `word` appear in `text` in order, with any
/// other characters between them. Each character is looked for with a
/// substring search from just after the previous one: in UTF-8 a whole
/// encoded character only matches at a character boundary, so this is
/// linear in the length of `text`.
pub fn is_subsequence(word: &str, text: &str) -> bool {
    let mut rest = text;
    for wanted in word.chars() {
        match rest.find(wanted) {
            Some(offset) => rest = &rest[offset + wanted.len_utf8()..],
            None => return false,
        }
    }
    true
}

/// The length, in characters, of the shortest stretch of `text` that holds
/// the characters of `word` in order, or `None` when there is none.
///
/// One pass over `text`: for each prefix of `word`, the latest start of a
/// stretch ending here that holds that prefix in order. The work is the
/// length of `text` times the length of `word`.
pub fn subsequence_span(word: &str, text: &str) -> Option<usize> {
    let word: Vec<char> = word.chars().collect();
    let Some(&last) = word.last() else {
        return Some(0);
    };
    let mut starts: Vec<Option<usize>> = vec![None; word.len()];
    let mut shortest: Option<usize> = None;
    for (position, character) in text.chars().enumerate() {
        // From the end of the word, so that this character extends a
        // stretch that ended before it, never one it has just extended.
        for index in (0..word.len()).rev() {
            if word[index] != character {
                continue;
            }
            if index == 0 {
                starts[0] = Some(position);
            } else if let Some(start) = starts[index - 1] {
                starts[index] = Some(start);
            }
        }
        if character == last {
            if let Some(start) = starts[word.len() - 1] {
                let span = position - start + 1;
                shortest = Some(shortest.map_or(span, |shortest| shortest.min(span)));
            }
        }
    }
    shortest
}

/// The sum, over the words, of each word's shortest span in any of the
/// already folded fields, or `None` when a word is a subsequence of none.
pub fn summed_span(words: &[String], fields: &[&str]) -> Option<usize> {
    words
        .iter()
        .map(|word| {
            fields
                .iter()
                .filter_map(|field| subsequence_span(word, field))
                .min()
        })
        .sum()
}

/// How a row matches the filter words, in the order the rows are listed:
/// word-rule matches first, then subsequence-only matches, shorter summed
/// span first.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Match {
    /// Every word occurs in one of the row's fields.
    Words,
    /// Every word is a subsequence of one of the row's fields; the summed
    /// span.
    Subsequence(usize),
}

/// How a row with the already folded `fields` matches the words, or `None`
/// when it does not. Subsequences count only when `subsequence` is set.
pub fn match_fields(words: &[String], fields: &[&str], subsequence: bool) -> Option<Match> {
    if words_in_fields(words, fields) {
        Some(Match::Words)
    } else if subsequence {
        summed_span(words, fields).map(Match::Subsequence)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::{
        contains_all_words, fold_text, is_subsequence, match_fields, matches_all_words,
        query_words, subsequence_span, summed_span, Match,
    };

    fn words(words: &[&str]) -> Vec<String> {
        words.iter().map(|word| word.to_string()).collect()
    }

    #[test]
    fn folding_decomposes_drops_marks_and_folds_case() {
        assert_eq!(fold_text("Plain ASCII"), "plain ascii");
        assert_eq!(fold_text("Café"), "cafe");
        assert_eq!(fold_text("CAFÉ"), "cafe");
        assert_eq!(fold_text("Cafe\u{301}"), "cafe");
        assert_eq!(fold_text("Straße"), "strasse");
        assert_eq!(fold_text("ﬁle"), "file");
        assert_eq!(fold_text("Ｆｕｌｌ"), "full");
        assert_eq!(fold_text("ΟΣ"), "οσ");
        // Spacing (Mc) and enclosing (Me) marks go as well.
        assert_eq!(fold_text("X\u{903}ray"), "xray");
        assert_eq!(fold_text("A\u{20dd}corn"), "acorn");
        // U+0345 is a mark: it is dropped before case folding would turn it
        // into a letter.
        assert_eq!(fold_text("Alpha\u{345}"), "alpha");
    }

    #[test]
    fn query_splits_at_any_whitespace_and_folds() {
        assert_eq!(query_words("  Café\tÉLÉ  x "), ["cafe", "ele", "x"]);
        assert!(query_words(" \t\n").is_empty());
    }

    #[test]
    fn every_word_must_match_in_any_order_and_case() {
        assert!(matches_all_words("", &["anything"]));
        assert!(matches_all_words("  ", &["anything"]));
        assert!(matches_all_words(
            "backup needle",
            &["needle backup second"]
        ));
        assert!(matches_all_words("NEEDLE", &["needle-hay"]));
        assert!(matches_all_words("CAFÉ ÉLÉ", &["Éléphant Café à Paris"]));
        assert!(!matches_all_words("backup needle", &["backup only"]));
        assert!(!matches_all_words("x", &[""]));
    }

    #[test]
    fn query_and_fields_are_both_folded() {
        assert!(matches_all_words("cafe", &["Café Racer"]));
        assert!(matches_all_words("CAFÉ", &["cafe"]));
        assert!(matches_all_words("strasse", &["Straße"]));
        assert!(matches_all_words("STRASSE", &["straße"]));
        assert!(matches_all_words("file", &["ﬁle Manager"]));
        assert!(!matches_all_words("cafes", &["Café"]));
    }

    #[test]
    fn words_may_match_different_fields() {
        assert!(matches_all_words(
            "charlie msg",
            &["msg-Charlie", "Charlie"]
        ));
        assert!(matches_all_words(
            "second fixture",
            &["Fixture - Second binding", "Super b"]
        ));
        assert!(!matches_all_words(
            "second missing",
            &["Fixture - Second binding", "Super b"]
        ));
    }

    #[test]
    fn prefolded_text_matches_like_the_fields() {
        let words = query_words("Needle  Backup");
        assert!(contains_all_words("/home/needle/backup_file.txt", &words));
        assert!(!contains_all_words("/home/needle/file.txt", &words));
        assert!(contains_all_words("anything", &[]));
    }

    #[test]
    fn subsequences_keep_the_order_of_the_characters() {
        assert!(is_subsequence("ffx", "firefox"));
        assert!(is_subsequence("", "firefox"));
        assert!(is_subsequence("éx", "aébx"));
        assert!(!is_subsequence("xff", "firefox"));
        assert!(!is_subsequence("ccc", "cafe cafe"));
        assert!(!is_subsequence("x", ""));
        // A character never matches inside another one's encoding: "é" is
        // C3 A9, and "Ã©" is C3 83 C2 A9.
        assert!(!is_subsequence("é", "Ã©"));
        assert!(is_subsequence("ss", "s.s"));
        assert!(!is_subsequence("ss", "s"));
    }

    #[test]
    fn span_is_the_shortest_stretch_holding_the_word() {
        assert_eq!(subsequence_span("ffx", "firefox"), Some(7));
        assert_eq!(subsequence_span("ffx", "ffax tool"), Some(4));
        assert_eq!(subsequence_span("q", "aqa"), Some(1));
        // The shortest stretch may start at a later occurrence.
        assert_eq!(subsequence_span("qx", "q....qzx"), Some(3));
        assert_eq!(subsequence_span("ab", "a.b.ab"), Some(2));
        // Spans count characters, not bytes.
        assert_eq!(subsequence_span("éé", "éàé"), Some(3));
        assert_eq!(subsequence_span("qx", "xq"), None);
        assert_eq!(subsequence_span("x", ""), None);
        // Each character of the word takes its own character of the text.
        assert_eq!(subsequence_span("ss", "s"), None);
        assert_eq!(subsequence_span("ss", "sas"), Some(3));
        assert_eq!(subsequence_span("aab", "abaab"), Some(3));
        assert_eq!(subsequence_span("aba", "aabba"), Some(4));
    }

    #[test]
    fn span_stays_linear_for_long_repetitive_text() {
        let text = "a".repeat(200_000) + "b";
        assert_eq!(subsequence_span("ab", &text), Some(2));
        assert_eq!(subsequence_span("aab", &text), Some(3));
        assert!(is_subsequence("aaab", &text));
        assert!(!is_subsequence("ba", &text));
    }

    #[test]
    fn summed_span_takes_each_word_from_its_best_field() {
        assert_eq!(
            summed_span(
                &words(&["brsr", "stss"]),
                &["probe - browser", "mod4+strasse"]
            ),
            Some(7 + 6)
        );
        assert_eq!(
            summed_span(&words(&["ffx"]), &["firefox focus", "mod4+ffax"]),
            Some(4)
        );
        // Words may use the same characters.
        assert_eq!(summed_span(&words(&["ab", "ab"]), &["ab"]), Some(4));
        // A word lies within one field.
        assert_eq!(summed_span(&words(&["ab"]), &["a", "b"]), None);
        assert_eq!(summed_span(&words(&["ffx", "zz"]), &["firefox"]), None);
        assert_eq!(summed_span(&[], &["anything"]), Some(0));
    }

    #[test]
    fn word_matches_come_before_subsequence_matches() {
        let ffx = words(&["ffx"]);
        assert_eq!(
            match_fields(&ffx, &["ffx window"], true),
            Some(Match::Words)
        );
        assert_eq!(
            match_fields(&ffx, &["firefox"], true),
            Some(Match::Subsequence(7))
        );
        assert_eq!(match_fields(&ffx, &["firefox"], false), None);
        assert_eq!(match_fields(&ffx, &["fox"], true), None);
        assert_eq!(match_fields(&[], &["anything"], false), Some(Match::Words));
        assert!(Match::Words < Match::Subsequence(1));
        assert!(Match::Subsequence(3) < Match::Subsequence(4));
    }
}
