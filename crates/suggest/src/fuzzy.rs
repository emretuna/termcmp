use frizbee::{CaseMatching, Config, Matcher, Matching};

use crate::types::Suggestion;

/// How the typed query filters candidates. Re-exported from `config` so
/// callers in this crate (and the PTY handler) can pass it to [`rank_with_mode`]
/// without depending on `config` directly.
pub use config::MatchMode;

pub const DEFAULT_MAX_RESULTS: usize = 50;

/// Map a [`MatchMode`] to the corresponding frizbee matching algorithm.
fn matching_mode(mode: MatchMode) -> Matching {
    match mode {
        MatchMode::Fuzzy => Matching::Fuzzy,
        MatchMode::Substring => Matching::Substring,
    }
}

/// Rank `suggestions` against `query` using the default fuzzy (subsequence)
/// match mode. Thin wrapper over [`rank_with_mode`] preserved for callers and
/// tests that always want fuzzy matching.
pub fn rank(query: &str, suggestions: Vec<Suggestion>, max_results: usize) -> Vec<Suggestion> {
    rank_with_mode(query, suggestions, max_results, MatchMode::Fuzzy)
}

/// Rank `suggestions` against `query` under the given [`MatchMode`].
///
/// In [`MatchMode::Substring`] only candidates that contain the typed
/// characters as a contiguous run survive; in [`MatchMode::Fuzzy`] the
/// characters may be spread out as a subsequence. Surviving candidates are
/// returned in frizbee's score order (score descending, input index
/// ascending as tiebreak) and truncated to `max_results`. Callers
/// pre-arrange the input to encode ordering preferences — frizbee's
/// `ScoreThenIndexAsc` default preserves that arrangement for equal scores.
pub fn rank_with_mode(
    query: &str,
    mut suggestions: Vec<Suggestion>,
    max_results: usize,
    mode: MatchMode,
) -> Vec<Suggestion> {
    if query.is_empty() {
        // Empty query: callers pre-arrange input; preserve it as-is.
        suggestions.truncate(max_results);
        return suggestions;
    }

    let config = Config::default()
        .matching(matching_mode(mode))
        .casing(CaseMatching::Smart);
    let mut matcher = Matcher::new(query, &config);

    let haystacks: Vec<&str> = suggestions.iter().map(|s| s.text.as_str()).collect();
    let matches = matcher.match_list_indices(&haystacks);

    // Extract matched candidates in frizbee's returned order (score desc,
    // index asc), updating score and match_indices.
    // `std::mem::take` avoids cloning (Suggestion implements Default).
    let mut matched: Vec<Suggestion> = Vec::with_capacity(matches.len());
    for m in matches {
        let mut s = std::mem::take(&mut suggestions[m.index as usize]);
        s.score = m.score as u32;
        let mut indices = m.indices;
        indices.sort_unstable();
        indices.dedup();
        s.match_indices = indices;
        matched.push(s);
    }

    matched.truncate(max_results);
    matched
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::SuggestionKind;

    fn make(text: &str) -> Suggestion {
        Suggestion {
            text: text.to_string(),
            kind: SuggestionKind::Command,
            ..Default::default()
        }
    }

    #[test]
    fn test_empty_query_returns_all() {
        let items: Vec<Suggestion> = (0..10).map(|i| make(&format!("item{i}"))).collect();
        let result = rank("", items, DEFAULT_MAX_RESULTS);
        assert_eq!(result.len(), 10);
    }

    #[test]
    fn test_fuzzy_match_filters() {
        let items = vec![make("checkout"), make("cherry-pick"), make("zzzzz")];
        let result = rank("che", items, DEFAULT_MAX_RESULTS);
        assert!(result.iter().any(|s| s.text == "checkout"));
        assert!(result.iter().any(|s| s.text == "cherry-pick"));
        assert!(!result.iter().any(|s| s.text == "zzzzz"));
    }

    #[test]
    fn test_exact_prefix_scores_higher() {
        let items = vec![make("achievement"), make("checkout")];
        let result = rank("check", items, DEFAULT_MAX_RESULTS);
        assert!(!result.is_empty());
        assert_eq!(result[0].text, "checkout");
    }

    #[test]
    fn test_no_matches_returns_empty() {
        let items = vec![make("alpha"), make("beta"), make("gamma")];
        let result = rank("zzzzxxx", items, DEFAULT_MAX_RESULTS);
        assert!(result.is_empty());
    }

    #[test]
    fn test_max_results_cap() {
        let items: Vec<Suggestion> = (0..100).map(|i| make(&format!("item{i}"))).collect();
        let result = rank("item", items, DEFAULT_MAX_RESULTS);
        assert!(result.len() <= DEFAULT_MAX_RESULTS);
    }

    #[test]
    fn test_custom_max_results() {
        let items: Vec<Suggestion> = (0..100).map(|i| make(&format!("item{i}"))).collect();
        let result = rank("item", items, 5);
        assert!(result.len() <= 5);
    }

    #[test]
    fn test_equal_score_preserves_input_order() {
        use crate::types::{SuggestionKind, SuggestionSource};
        let items = vec![
            Suggestion {
                text: "checkout".to_string(),
                kind: SuggestionKind::Subcommand,
                source: SuggestionSource::Commands,
                ..Default::default()
            },
            Suggestion {
                text: "checkout".to_string(),
                kind: SuggestionKind::History,
                source: SuggestionSource::History,
                ..Default::default()
            },
        ];
        let result = rank("checkout", items, DEFAULT_MAX_RESULTS);
        // Same text → same fuzzy score → frizbee's index-asc tiebreak
        // preserves input order: Commands (index 0) before History (index 1).
        assert_eq!(result[0].source, SuggestionSource::Commands);
        assert_eq!(result[1].source, SuggestionSource::History);
    }

    #[test]
    fn test_scores_are_set() {
        let items = vec![make("checkout"), make("cherry-pick")];
        let result = rank("ch", items, DEFAULT_MAX_RESULTS);
        for s in &result {
            assert!(s.score > 0, "score should be > 0 after ranking");
        }
    }

    #[test]
    fn test_match_indices_populated() {
        let items = vec![make("checkout"), make("cherry-pick")];
        let result = rank("che", items, DEFAULT_MAX_RESULTS);
        for s in &result {
            assert!(
                !s.match_indices.is_empty(),
                "match_indices should be populated for '{}'",
                s.text
            );
        }
        let checkout = result.iter().find(|s| s.text == "checkout").unwrap();
        assert_eq!(checkout.match_indices, vec![0, 1, 2]);
    }

    #[test]
    fn test_match_indices_sorted_and_deduped() {
        let items = vec![make("abcabc")];
        let result = rank("abc", items, DEFAULT_MAX_RESULTS);
        let s = &result[0];
        for window in s.match_indices.windows(2) {
            assert!(window[0] < window[1], "indices must be sorted and unique");
        }
    }

    #[test]
    fn test_provider_value_gets_frizbee_indices() {
        // Regression: ProviderValue suggestions (shell completions, LLM) must
        // receive frizbee's scattered match indices, not the provider's
        // prefix-only seed. "suback" is a subsequence of "supabase backups"
        // at positions 0,1,3,5,9,10 — not just [0].
        let items = vec![Suggestion {
            text: "supabase backups".to_string(),
            kind: SuggestionKind::ProviderValue,
            match_indices: vec![0], // provider seed: prefix-only
            ..Default::default()
        }];
        let result = rank("suback", items, DEFAULT_MAX_RESULTS);
        assert_eq!(result.len(), 1);
        let indices = &result[0].match_indices;
        // Must contain more than just the prefix seed
        assert!(
            indices.len() > 1,
            "frizbee must overwrite prefix-only indices: got {indices:?}"
        );
        // Every index must point to a character in the query
        let text = "supabase backups";
        let query = "suback";
        for (qi, &idx) in indices.iter().enumerate() {
            let ch = text.chars().nth(idx as usize).unwrap();
            assert_eq!(
                ch,
                query.chars().nth(qi).unwrap(),
                "index {idx} should match query char {qi}"
            );
        }
    }

    #[test]
    fn test_provider_value_substring_gets_contiguous_indices() {
        // Substring mode: "base" is contiguous in "supabase backups" at
        // indices 4,5,6,7. ProviderValue must still get frizbee's indices.
        let items = vec![Suggestion {
            text: "supabase backups".to_string(),
            kind: SuggestionKind::ProviderValue,
            match_indices: vec![0], // provider seed
            ..Default::default()
        }];
        let result = rank_with_mode("base", items, DEFAULT_MAX_RESULTS, MatchMode::Substring);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].match_indices, vec![4, 5, 6, 7]);
    }

    #[test]
    fn test_empty_query_no_match_indices() {
        let items = vec![make("checkout")];
        let result = rank("", items, DEFAULT_MAX_RESULTS);
        assert!(result[0].match_indices.is_empty());
    }

    #[test]
    fn test_substring_excludes_subsequence_only_matches() {
        // The issue #149 case: typing "cl" should keep only candidates that
        // contain "cl" contiguously, not every word that has a 'c' and an 'l'
        // somewhere.
        let items = vec![make("clone"), make("include"), make("calendar")];
        let result = rank_with_mode("cl", items, DEFAULT_MAX_RESULTS, MatchMode::Substring);
        let texts: Vec<&str> = result.iter().map(|s| s.text.as_str()).collect();
        assert!(texts.contains(&"clone"), "clone contains 'cl'");
        assert!(texts.contains(&"include"), "include contains 'cl'");
        assert!(
            !texts.contains(&"calendar"),
            "calendar has c..l as a subsequence but not 'cl' contiguously"
        );
    }

    #[test]
    fn test_fuzzy_keeps_subsequence_matches_substring_drops() {
        // Same candidate set, contrasting the two modes: fuzzy keeps the
        // subsequence-only "calendar", substring drops it.
        let fuzzy = rank_with_mode(
            "cl",
            vec![make("calendar")],
            DEFAULT_MAX_RESULTS,
            MatchMode::Fuzzy,
        );
        assert_eq!(fuzzy.len(), 1, "fuzzy keeps c..l subsequence");

        let substring = rank_with_mode(
            "cl",
            vec![make("calendar")],
            DEFAULT_MAX_RESULTS,
            MatchMode::Substring,
        );
        assert!(
            substring.is_empty(),
            "substring rejects non-contiguous c..l"
        );
    }

    #[test]
    fn test_substring_multi_word_requires_every_word_as_substring() {
        // Pins the documented contract: in substring mode, space-separated
        // words are matched as independent substrings and EVERY word must be
        // present. "git ch" keeps only the candidate containing both "git"
        // and "ch" contiguously. This behavior rides on frizbee's multi-pattern
        // AND semantics — a regression there would otherwise pass silently.
        let items = vec![
            make("git checkout"), // has "git" and "ch"
            make("git push"),     // has "git", lacks "ch"
            make("touch change"), // has "ch" (twice), lacks "git"
        ];
        let result = rank_with_mode("git ch", items, DEFAULT_MAX_RESULTS, MatchMode::Substring);
        let texts: Vec<&str> = result.iter().map(|s| s.text.as_str()).collect();
        assert_eq!(
            texts,
            vec!["git checkout"],
            "only the candidate containing every space-separated substring survives: {texts:?}"
        );
    }

    #[test]
    fn test_substring_smart_case_is_case_insensitive_for_lowercase_query() {
        // Smart-case (inherited from the fuzzy path): an all-lowercase query
        // matches case-insensitively, so "cl" still finds "CLONE".
        let result = rank_with_mode(
            "cl",
            vec![make("CLONE")],
            DEFAULT_MAX_RESULTS,
            MatchMode::Substring,
        );
        assert_eq!(
            result.len(),
            1,
            "lowercase query matches uppercase haystack"
        );
    }

    #[test]
    fn test_substring_match_indices_are_contiguous() {
        let items = vec![make("include")];
        let result = rank_with_mode("cl", items, DEFAULT_MAX_RESULTS, MatchMode::Substring);
        assert_eq!(result.len(), 1);
        // "in*cl*ude" — the 'c' and 'l' are at indices 2 and 3.
        assert_eq!(result[0].match_indices, vec![2, 3]);
    }

    #[test]
    fn test_rank_delegates_to_fuzzy_mode() {
        // `rank` must remain a fuzzy alias: a subsequence-only candidate that
        // substring mode would drop still survives through `rank`.
        let result = rank("cl", vec![make("calendar")], DEFAULT_MAX_RESULTS);
        assert_eq!(result.len(), 1, "rank() keeps fuzzy subsequence match");
    }

    #[test]
    fn test_substring_empty_query_returns_all() {
        let items: Vec<Suggestion> = (0..5).map(|i| make(&format!("item{i}"))).collect();
        let result = rank_with_mode("", items, DEFAULT_MAX_RESULTS, MatchMode::Substring);
        assert_eq!(result.len(), 5, "empty query is mode-agnostic");
    }

    #[test]
    fn test_empty_query_preserves_input_order() {
        // Empty query returns candidates in input order — no sort is applied.
        // Callers (engine::rank_with_history) pre-arrange input by priority.
        use crate::priority::Priority;
        use crate::types::SuggestionKind;
        let items = vec![
            Suggestion {
                text: "A".to_string(),
                kind: SuggestionKind::Flag,
                priority: Some(Priority::new(95)),
                ..Default::default()
            },
            Suggestion {
                text: "B".to_string(),
                kind: SuggestionKind::Subcommand,
                priority: None,
                ..Default::default()
            },
        ];
        let result = rank("", items, DEFAULT_MAX_RESULTS);
        assert_eq!(
            result[0].text, "A",
            "empty query preserves input order (callers pre-sort by priority)"
        );
    }

    #[test]
    fn test_empty_query_preserves_input_order_regardless_of_source() {
        // Empty query preserves input order — source and priority do not
        // trigger a re-sort. Callers pre-arrange input.
        use crate::types::{SuggestionKind, SuggestionSource};
        let items = vec![
            Suggestion {
                text: "z-flag".to_string(),
                kind: SuggestionKind::Flag,
                ..Default::default()
            },
            Suggestion {
                text: "a-history".to_string(),
                kind: SuggestionKind::History,
                source: SuggestionSource::History,
                ..Default::default()
            },
        ];
        let result = rank("", items, DEFAULT_MAX_RESULTS);
        assert_eq!(
            result[0].text, "z-flag",
            "input order preserved: z-flag was first in input"
        );
        assert_eq!(result[1].text, "a-history");
    }

    #[test]
    fn test_exact_buffer_match_ranks_first() {
        // Typing "git diff" must rank "git diff" above "git difftool" and
        // "git diff --cached" — the exact match gets the highest score.
        let items = vec![
            make("git difftool"),
            make("git diff --cached"),
            make("git diff"),
        ];
        let result = rank_with_mode("git diff", items, DEFAULT_MAX_RESULTS, MatchMode::Fuzzy);
        assert_eq!(
            result[0].text, "git diff",
            "exact match must rank first: {result:?}"
        );
    }
}
