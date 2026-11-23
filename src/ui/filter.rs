//! Incremental list filtering for TUI panes (the SPACES pane `/` filter).

use crate::models::SpaceSummary;

/// Score of an exact (case-insensitive) space key match.
const SCORE_EXACT_KEY: u32 = 500;
/// Score of a space key that starts with the query.
const SCORE_KEY_PREFIX: u32 = 400;
/// Score of a space name with a word that starts with the query.
const SCORE_NAME_WORD_PREFIX: u32 = 300;
/// Score of a query found anywhere in the key or name.
const SCORE_SUBSTRING: u32 = 200;
/// Score of a query whose characters appear in order in the key or name.
const SCORE_SUBSEQUENCE: u32 = 100;

/// True for Confluence personal space keys (`~jdoe`, `~5b10ac8d82e05b22cc7d4ef5`).
pub fn is_personal_space_key(key: &str) -> bool {
    key.trim_start().starts_with('~')
}

/// True for personal spaces: a `~` key or a `personal` space type.
pub fn is_personal_space(sp: &SpaceSummary) -> bool {
    is_personal_space_key(&sp.key) || sp.space_type.trim().eq_ignore_ascii_case("personal")
}

/// Ranks `sp` against a SPACES filter query; `None` when it does not match.
///
/// Case-insensitive, highest first: exact key > key prefix > name word prefix >
/// substring of key or name > in-order subsequence of key or name. A leading `~`
/// keeps personal spaces only and matches the rest of the query as usual.
pub fn space_match_score(query: &str, sp: &SpaceSummary) -> Option<u32> {
    let query = query.trim();
    let (personal_only, query) = match query.strip_prefix('~') {
        Some(rest) => (true, rest.trim_start()),
        None => (false, query),
    };
    if personal_only && !is_personal_space(sp) {
        return None;
    }
    let q = query.to_lowercase();
    if q.is_empty() {
        return Some(SCORE_SUBSEQUENCE);
    }
    let key = sp.key.trim().to_lowercase();
    let bare_key = key.trim_start_matches('~');
    let name = sp.name.to_lowercase();

    if key == q || bare_key == q {
        Some(SCORE_EXACT_KEY)
    } else if key.starts_with(&q) || bare_key.starts_with(&q) {
        Some(SCORE_KEY_PREFIX)
    } else if has_word_prefix(&name, &q) {
        Some(SCORE_NAME_WORD_PREFIX)
    } else if key.contains(&q) || name.contains(&q) {
        Some(SCORE_SUBSTRING)
    } else if is_subsequence(&q, &key) || is_subsequence(&q, &name) {
        Some(SCORE_SUBSEQUENCE)
    } else {
        None
    }
}

/// True when `haystack` has a word (start, or after a non-alphanumeric char) starting with `q`.
fn has_word_prefix(haystack: &str, q: &str) -> bool {
    let mut prev_is_word = false;
    for (i, ch) in haystack.char_indices() {
        if !prev_is_word && haystack[i..].starts_with(q) {
            return true;
        }
        prev_is_word = ch.is_alphanumeric();
    }
    false
}

/// True when every non-space char of `q` appears in `haystack` in order.
fn is_subsequence(q: &str, haystack: &str) -> bool {
    let mut rest = haystack.chars();
    q.chars()
        .filter(|c| !c.is_whitespace())
        .all(|qc| rest.by_ref().any(|hc| hc == qc))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn space(key: &str, name: &str) -> SpaceSummary {
        let space_type = if key.starts_with('~') {
            "personal"
        } else {
            "global"
        };
        SpaceSummary {
            key: key.into(),
            name: name.into(),
            space_type: space_type.into(),
            ..Default::default()
        }
    }

    #[test]
    fn space_match_score_ranks_key_prefix_over_substring() {
        let exact = space("ENG", "Engineering");
        let key_prefix = space("ENGOPS", "Operations");
        let name_word = space("PLAT", "Core Engine Platform");
        let substring = space("DOCS", "Reengineering notes");
        let subsequence = space("EXT", "External Nodes Guide");
        let none = space("HR", "People");

        let score = |sp: &SpaceSummary| space_match_score("eng", sp);
        assert_eq!(score(&exact), Some(SCORE_EXACT_KEY));
        assert_eq!(score(&key_prefix), Some(SCORE_KEY_PREFIX));
        assert_eq!(score(&name_word), Some(SCORE_NAME_WORD_PREFIX));
        assert_eq!(score(&substring), Some(SCORE_SUBSTRING));
        assert_eq!(score(&subsequence), Some(SCORE_SUBSEQUENCE));
        assert_eq!(score(&none), None);
        assert!(score(&exact) > score(&key_prefix));
        assert!(score(&key_prefix) > score(&name_word));
        assert!(score(&name_word) > score(&substring));
        assert!(score(&substring) > score(&subsequence));

        // Case-insensitive, and a key substring alone ranks below a key prefix.
        assert_eq!(space_match_score("Eng", &exact), Some(SCORE_EXACT_KEY));
        assert_eq!(
            space_match_score("ops", &key_prefix),
            Some(SCORE_SUBSTRING),
            "OPS is inside ENGOPS, not a prefix"
        );
        // Personal keys match with or without their `~` in the key position.
        assert_eq!(
            space_match_score("jdoe", &space("~jdoe", "John Doe")),
            Some(SCORE_EXACT_KEY)
        );
    }

    #[test]
    fn space_filter_tilde_limits_to_personal_spaces() {
        let spaces = [
            space("ENG", "Engineering"),
            space("~jdoe", "John Doe"),
            space("DOE", "Department of Energy"),
            space("~asmith", "Anna Smith"),
            SpaceSummary {
                key: "LEGACYJD".into(),
                name: "Jane Doe".into(),
                space_type: "personal".into(),
                ..Default::default()
            },
        ];
        let matching = |q: &str| -> Vec<&str> {
            spaces
                .iter()
                .filter(|sp| space_match_score(q, sp).is_some())
                .map(|sp| sp.key.as_str())
                .collect()
        };

        assert_eq!(matching("~"), vec!["~jdoe", "~asmith", "LEGACYJD"]);
        assert_eq!(matching("~doe"), vec!["~jdoe", "LEGACYJD"]);
        assert_eq!(matching("~ smith"), vec!["~asmith"]);
        assert_eq!(matching("doe"), vec!["~jdoe", "DOE", "LEGACYJD"]);
        assert!(is_personal_space_key("  ~jdoe"));
        assert!(!is_personal_space_key("ENG"));
    }
}
