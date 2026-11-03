//! The filter query shared by the pages: every word must match.

/// The words of a filter query, lowercased. An empty or blank query has no
/// words and matches every row.
pub fn query_words(query: &str) -> Vec<String> {
    query
        .to_lowercase()
        .split_whitespace()
        .map(str::to_owned)
        .collect()
}

/// Whether every word occurs in the already lowercased `text`.
pub fn contains_all_words(text: &str, words: &[String]) -> bool {
    words.iter().all(|word| text.contains(word.as_str()))
}

/// Whether a row matches the query: every whitespace-separated word occurs,
/// ignoring case and in any order, in at least one of the row's fields.
pub fn matches_all_words(query: &str, fields: &[&str]) -> bool {
    let words = query_words(query);
    if words.is_empty() {
        return true;
    }
    let fields: Vec<String> = fields.iter().map(|field| field.to_lowercase()).collect();
    words
        .iter()
        .all(|word| fields.iter().any(|field| field.contains(word.as_str())))
}

#[cfg(test)]
mod tests {
    use super::{contains_all_words, matches_all_words, query_words};

    #[test]
    fn query_splits_at_any_whitespace_and_lowercases() {
        assert_eq!(query_words("  Café\tÉLÉ  x "), ["café", "élé", "x"]);
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
    fn prelowered_text_matches_like_the_fields() {
        let words = query_words("Needle  Backup");
        assert!(contains_all_words("/home/needle/backup_file.txt", &words));
        assert!(!contains_all_words("/home/needle/file.txt", &words));
        assert!(contains_all_words("anything", &[]));
    }
}
