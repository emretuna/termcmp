use std::cmp::Reverse;

use frizbee::{CaseMatching, Config, Matcher, Matching};

use crate::frecency::FrecencyStore;
use crate::types::{SourceOrder, Suggestion, SuggestionSource};

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
pub fn rank(
    query: &str,
    suggestions: Vec<Suggestion>,
    max_results: usize,
    frecency: &FrecencyStore,
) -> Vec<Suggestion> {
    rank_with_mode(query, suggestions, max_results, MatchMode::Fuzzy, frecency)
}

/// Rank `suggestions` against `query` under the given [`MatchMode`].
///
/// In [`MatchMode::Substring`] only candidates that contain the typed
/// characters as a contiguous run survive; in [`MatchMode::Fuzzy`] the
/// characters may be spread out as a subsequence. Surviving candidates are
/// returned in score order (boosted score descending, input index ascending
/// as tiebreak) and truncated to `max_results`. Frecency boosts are applied
/// to each candidate's frizbee score based on prior acceptances recorded in
/// the frecency store. Callers pre-arrange the input to encode ordering
/// preferences — the index-asc tiebreak preserves that arrangement for equal
/// boosted scores.
pub fn rank_with_mode(
    query: &str,
    mut suggestions: Vec<Suggestion>,
    max_results: usize,
    mode: MatchMode,
    frecency: &FrecencyStore,
) -> Vec<Suggestion> {
    if query.is_empty() {
        // No fuzzy signal: rank by frecency boost, stable — callers
        // pre-arrange input and that arrangement survives among equal boosts.
        suggestions.sort_by_key(|s| Reverse(frecency.boost(&s.text)));
        suggestions.truncate(max_results);
        return suggestions;
    }

    let config = Config::default()
        .matching(matching_mode(mode))
        .casing(CaseMatching::Smart);
    let mut matcher = Matcher::new(query, &config);

    let haystacks: Vec<&str> = suggestions.iter().map(|s| s.text.as_str()).collect();
    // Use the unsorted iterator so we can apply frecency boosts before sorting.
    let matches: Vec<_> = matcher.match_iter_indices(haystacks.iter()).collect();

    // Extract matched candidates, applying frecency boosts to scores, then
    // sort by (boosted score desc, original index asc) to reproduce frizbee's
    // ScoreThenIndexAsc ordering on the boosted scores.
    let mut matched: Vec<(Suggestion, u32)> = Vec::with_capacity(matches.len());
    for m in matches {
        let mut s = std::mem::take(&mut suggestions[m.index as usize]);
        let boosted = (m.score as u32 + frecency.boost(&s.text)).min(u16::MAX as u32) as u16;
        s.score = boosted as u32;
        let mut indices = m.indices;
        indices.sort_unstable();
        indices.dedup();
        s.match_indices = indices;
        matched.push((s, m.index));
    }

    // Sort by boosted score descending, then original index ascending.
    matched.sort_by(|a, b| b.0.score.cmp(&a.0.score).then_with(|| a.1.cmp(&b.1)));

    let mut result: Vec<Suggestion> = matched.into_iter().map(|(s, _)| s).collect();
    result.truncate(max_results);
    result
}

/// Rank candidates against the full buffer (for full-line candidates) or
/// current word (for token-local candidates).
///
/// Full-line candidates (`Commands`, `History`, `Provider`, `Llm`)
/// are matched against the complete `buffer` because their `text` field
/// contains the full replacement line. Token-local candidates (`Filesystem`,
/// `Env`, `SshConfig`) are matched against `current_word` because their
/// `text` is intentionally only the token that the accept path replaces.
///
/// This separation fixes the bug where `supabase back` with cached candidate
/// `supabase backups` would fail to match if only `current_word` ("back") was
/// used — the full buffer "supabase back" matches the full candidate text.
pub fn rank_candidates(
    buffer: &str,
    current_word: &str,
    suggestions: Vec<Suggestion>,
    max_results: usize,
    mode: MatchMode,
    frecency: &FrecencyStore,
) -> Vec<Suggestion> {
    rank_candidates_with_order(
        buffer,
        current_word,
        suggestions,
        max_results,
        mode,
        &SourceOrder::default_order(),
        frecency,
    )
}

/// Rank candidates using an explicit source order supplied by the caller.
pub fn rank_candidates_with_order(
    buffer: &str,
    current_word: &str,
    mut suggestions: Vec<Suggestion>,
    max_results: usize,
    mode: MatchMode,
    order: &SourceOrder,
    frecency: &FrecencyStore,
) -> Vec<Suggestion> {
    if buffer.is_empty() && current_word.is_empty() {
        suggestions.sort_by(|a, b| {
            order
                .rank(a.source)
                .cmp(&order.rank(b.source))
                .then_with(|| crate::priority::effective(b).cmp(&crate::priority::effective(a)))
                .then_with(|| frecency.boost(&b.text).cmp(&frecency.boost(&a.text)))
                .then_with(|| a.text.cmp(&b.text))
        });
        return suggestions;
    }

    let mut full_line = Vec::new();
    let mut token_local = Vec::new();
    for s in suggestions {
        match s.source {
            SuggestionSource::Filesystem
            | SuggestionSource::Zoxide
            | SuggestionSource::Env
            | SuggestionSource::SshConfig => token_local.push(s),
            _ => full_line.push(s),
        }
    }

    let full_line_query = if buffer.is_empty() {
        current_word
    } else {
        buffer
    };
    let token_local_query = current_word;
    let ranked_full = rank_with_mode(full_line_query, full_line, max_results, mode, frecency);
    let ranked_token = rank_with_mode(token_local_query, token_local, max_results, mode, frecency);

    let mut merged = Vec::with_capacity(ranked_full.len() + ranked_token.len());
    merged.extend(ranked_full);
    merged.extend(ranked_token);
    merged.sort_by(|a, b| {
        order
            .rank(a.source)
            .cmp(&order.rank(b.source))
            .then_with(|| crate::priority::effective(b).cmp(&crate::priority::effective(a)))
            .then_with(|| b.score.cmp(&a.score))
            .then_with(|| a.text.cmp(&b.text))
    });
    merged.truncate(max_results);
    merged
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

    fn empty_frecency() -> FrecencyStore {
        FrecencyStore::in_memory()
    }

    #[test]
    fn test_empty_query_returns_all() {
        let items: Vec<Suggestion> = (0..10).map(|i| make(&format!("item{i}"))).collect();
        let result = rank("", items, DEFAULT_MAX_RESULTS, &empty_frecency());
        assert_eq!(result.len(), 10);
    }

    #[test]
    fn test_fuzzy_match_filters() {
        let items = vec![make("checkout"), make("cherry-pick"), make("zzzzz")];
        let result = rank("che", items, DEFAULT_MAX_RESULTS, &empty_frecency());
        assert!(result.iter().any(|s| s.text == "checkout"));
        assert!(result.iter().any(|s| s.text == "cherry-pick"));
        assert!(!result.iter().any(|s| s.text == "zzzzz"));
    }

    #[test]
    fn test_exact_prefix_scores_higher() {
        let items = vec![make("achievement"), make("checkout")];
        let result = rank("check", items, DEFAULT_MAX_RESULTS, &empty_frecency());
        assert!(!result.is_empty());
        assert_eq!(result[0].text, "checkout");
    }

    #[test]
    fn test_no_matches_returns_empty() {
        let items = vec![make("alpha"), make("beta"), make("gamma")];
        let result = rank("zzzzxxx", items, DEFAULT_MAX_RESULTS, &empty_frecency());
        assert!(result.is_empty());
    }

    #[test]
    fn test_max_results_cap() {
        let items: Vec<Suggestion> = (0..100).map(|i| make(&format!("item{i}"))).collect();
        let result = rank("item", items, DEFAULT_MAX_RESULTS, &empty_frecency());
        assert!(result.len() <= DEFAULT_MAX_RESULTS);
    }

    #[test]
    fn test_custom_max_results() {
        let items: Vec<Suggestion> = (0..100).map(|i| make(&format!("item{i}"))).collect();
        let result = rank("item", items, 5, &empty_frecency());
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
        let result = rank("checkout", items, DEFAULT_MAX_RESULTS, &empty_frecency());
        // Same text → same fuzzy score → frizbee's index-asc tiebreak
        // preserves input order: Commands (index 0) before History (index 1).
        assert_eq!(result[0].source, SuggestionSource::Commands);
        assert_eq!(result[1].source, SuggestionSource::History);
    }

    #[test]
    fn test_scores_are_set() {
        let items = vec![make("checkout"), make("cherry-pick")];
        let result = rank("ch", items, DEFAULT_MAX_RESULTS, &empty_frecency());
        for s in &result {
            assert!(s.score > 0, "score should be > 0 after ranking");
        }
    }

    #[test]
    fn test_match_indices_populated() {
        let items = vec![make("checkout"), make("cherry-pick")];
        let result = rank("che", items, DEFAULT_MAX_RESULTS, &empty_frecency());
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
        let result = rank("abc", items, DEFAULT_MAX_RESULTS, &empty_frecency());
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
        let result = rank("suback", items, DEFAULT_MAX_RESULTS, &empty_frecency());
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
        let result = rank_with_mode(
            "base",
            items,
            DEFAULT_MAX_RESULTS,
            MatchMode::Substring,
            &empty_frecency(),
        );
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].match_indices, vec![4, 5, 6, 7]);
    }

    #[test]
    fn test_empty_query_no_match_indices() {
        let items = vec![make("checkout")];
        let result = rank("", items, DEFAULT_MAX_RESULTS, &empty_frecency());
        assert!(result[0].match_indices.is_empty());
    }

    #[test]
    fn test_substring_excludes_subsequence_only_matches() {
        // The issue #149 case: typing "cl" should keep only candidates that
        // contain "cl" contiguously, not every word that has a 'c' and an 'l'
        // somewhere.
        let items = vec![make("clone"), make("include"), make("calendar")];
        let result = rank_with_mode(
            "cl",
            items,
            DEFAULT_MAX_RESULTS,
            MatchMode::Substring,
            &empty_frecency(),
        );
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
            &empty_frecency(),
        );
        assert_eq!(fuzzy.len(), 1, "fuzzy keeps c..l subsequence");

        let substring = rank_with_mode(
            "cl",
            vec![make("calendar")],
            DEFAULT_MAX_RESULTS,
            MatchMode::Substring,
            &empty_frecency(),
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
        let result = rank_with_mode(
            "git ch",
            items,
            DEFAULT_MAX_RESULTS,
            MatchMode::Substring,
            &empty_frecency(),
        );
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
            &empty_frecency(),
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
        let result = rank_with_mode(
            "cl",
            items,
            DEFAULT_MAX_RESULTS,
            MatchMode::Substring,
            &empty_frecency(),
        );
        assert_eq!(result.len(), 1);
        // "in*cl*ude" — the 'c' and 'l' are at indices 2 and 3.
        assert_eq!(result[0].match_indices, vec![2, 3]);
    }

    #[test]
    fn test_rank_delegates_to_fuzzy_mode() {
        // `rank` must remain a fuzzy alias: a subsequence-only candidate that
        // substring mode would drop still survives through `rank`.
        let result = rank(
            "cl",
            vec![make("calendar")],
            DEFAULT_MAX_RESULTS,
            &empty_frecency(),
        );
        assert_eq!(result.len(), 1, "rank() keeps fuzzy subsequence match");
    }

    #[test]
    fn test_substring_empty_query_returns_all() {
        let items: Vec<Suggestion> = (0..5).map(|i| make(&format!("item{i}"))).collect();
        let result = rank_with_mode(
            "",
            items,
            DEFAULT_MAX_RESULTS,
            MatchMode::Substring,
            &empty_frecency(),
        );
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
        let result = rank("", items, DEFAULT_MAX_RESULTS, &empty_frecency());
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
        let result = rank("", items, DEFAULT_MAX_RESULTS, &empty_frecency());
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
        let result = rank_with_mode(
            "git diff",
            items,
            DEFAULT_MAX_RESULTS,
            MatchMode::Fuzzy,
            &empty_frecency(),
        );
        assert_eq!(
            result[0].text, "git diff",
            "exact match must rank first: {result:?}"
        );
    }

    #[test]
    fn rank_candidates_full_line_matches_buffer() {
        // Bug repro: buffer "supabase back" with cached candidate "supabase backups"
        // must match when ranked against the full buffer, not just current_word "back".
        use crate::types::{SuggestionKind, SuggestionSource};
        let candidates = vec![Suggestion {
            text: "supabase backups".to_string(),
            kind: SuggestionKind::Subcommand,
            source: SuggestionSource::Commands,
            ..Default::default()
        }];
        let ranked = rank_candidates(
            "supabase back",
            "back",
            candidates,
            10,
            MatchMode::Fuzzy,
            &empty_frecency(),
        );
        assert_eq!(
            ranked.len(),
            1,
            "full-line candidate must match full buffer: {ranked:?}"
        );
        assert_eq!(ranked[0].text, "supabase backups");
    }

    #[test]
    fn rank_candidates_token_local_matches_current_word() {
        // Filesystem/env/SSH candidates match against current_word, not full buffer.
        use crate::types::{SuggestionKind, SuggestionSource};
        let candidates = vec![Suggestion {
            text: "Documents".to_string(),
            kind: SuggestionKind::Directory,
            source: SuggestionSource::Filesystem,
            ..Default::default()
        }];
        let ranked = rank_candidates(
            "cd Doc",
            "Doc",
            candidates,
            10,
            MatchMode::Fuzzy,
            &empty_frecency(),
        );
        assert_eq!(
            ranked.len(),
            1,
            "token-local candidate must match current_word: {ranked:?}"
        );
        assert_eq!(ranked[0].text, "Documents");
    }

    #[test]
    fn rank_candidates_deep_cache_matches_full_buffer() {
        // Bug repro: buffer "supabase backup l" with cached candidate
        // "supabase backup list" must match when ranked against full buffer.
        use crate::types::{SuggestionKind, SuggestionSource};
        let candidates = vec![Suggestion {
            text: "supabase backup list".to_string(),
            kind: SuggestionKind::Subcommand,
            source: SuggestionSource::Commands,
            ..Default::default()
        }];
        let ranked = rank_candidates(
            "supabase backup l",
            "l",
            candidates,
            10,
            MatchMode::Fuzzy,
            &empty_frecency(),
        );
        assert_eq!(
            ranked.len(),
            1,
            "deep cache candidate must match full buffer: {ranked:?}"
        );
        assert_eq!(ranked[0].text, "supabase backup list");
    }

    #[test]
    fn rank_candidates_empty_buffer_sorts_by_priority() {
        // Empty buffer sorts by source order, priority, then text — but does
        // NOT truncate. Guards the empty-query branch.
        use crate::types::{SuggestionKind, SuggestionSource};
        let candidates = vec![
            Suggestion {
                text: "zebra".to_string(),
                kind: SuggestionKind::Command,
                source: SuggestionSource::Commands,
                ..Default::default()
            },
            Suggestion {
                text: "alpha".to_string(),
                kind: SuggestionKind::Command,
                source: SuggestionSource::Commands,
                ..Default::default()
            },
        ];
        let ranked = rank_candidates("", "", candidates, 10, MatchMode::Fuzzy, &empty_frecency());
        assert_eq!(ranked.len(), 2, "empty query must not truncate: {ranked:?}");
        assert_eq!(ranked[0].text, "alpha", "alphabetical tiebreak: {ranked:?}");
    }

    #[test]
    fn frecency_boost_reorders_candidates() {
        // With a frecency store where "git difftool" has been accepted many
        // times but "git diff" has not, ranking "git diff" should put
        // "git difftool" first due to the frecency boost overcoming frizbee's
        // exact-match preference.
        use crate::types::SuggestionKind;
        let store = FrecencyStore::in_memory();
        // Record many times to build up a significant frecency score
        for _ in 0..20 {
            store.record("git difftool");
        }
        let items = vec![
            Suggestion {
                text: "git diff".to_string(),
                kind: SuggestionKind::Command,
                ..Default::default()
            },
            Suggestion {
                text: "git difftool".to_string(),
                kind: SuggestionKind::Command,
                ..Default::default()
            },
        ];
        let result = rank("git diff", items, DEFAULT_MAX_RESULTS, &store);
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].text, "git difftool",);
        assert_eq!(result[1].text, "git diff");
    }

    #[test]
    fn test_empty_query_ranks_by_frecency_stably() {
        // Non-alphabetical input proves the empty-query path preserves
        // caller arrangement (stability) rather than sorting by text.
        let items = vec![make("ccc"), make("aaa"), make("bbb")];
        let baseline = rank("", items.clone(), DEFAULT_MAX_RESULTS, &empty_frecency());
        assert_eq!(baseline[0].text, "ccc");
        assert_eq!(baseline[1].text, "aaa");
        assert_eq!(baseline[2].text, "bbb");

        let frecency = FrecencyStore::in_memory();
        for _ in 0..3 {
            frecency.record("aaa");
        }
        let result = rank("", items, DEFAULT_MAX_RESULTS, &frecency);
        // Familiar item first; the rest keep their pre-arranged order.
        assert_eq!(result[0].text, "aaa");
        assert_eq!(result[1].text, "ccc");
        assert_eq!(result[2].text, "bbb");
    }

    #[test]
    fn test_repeated_accepts_flip_ranking() {
        // Calibration contract for BOOST_MULTIPLIER / normalize against real
        // frizbee score gaps: sustained accepts of the runner-up must flip it
        // above the frizbee-preferred candidate. If frizbee ever rescales
        // scores beyond max realistic boost (~+120), this fails loudly.
        let frecency = FrecencyStore::in_memory();
        let items = vec![make("checkout"), make("cherry-pick")];
        let baseline = rank("che", items.clone(), DEFAULT_MAX_RESULTS, &frecency);
        assert!(baseline.len() >= 2);
        let runner_up = baseline[1].text.clone();
        let gap = baseline[0].score.saturating_sub(baseline[1].score);

        for _ in 0..32 {
            frecency.record(&runner_up);
        }
        let result = rank("che", items, DEFAULT_MAX_RESULTS, &frecency);
        assert_eq!(
            result[0].text,
            runner_up,
            "32 fresh accepts (boost +{}) must overcome frizbee gap of {gap}",
            frecency.boost(&runner_up),
        );
    }

    #[test]
    fn test_empty_query_keeps_source_grouping_over_frecency() {
        use crate::types::SuggestionSource;
        // Boost reorders within a source group but must not override the
        // SourceOrder grouping the non-empty merge path preserves.
        let items = vec![
            Suggestion {
                text: "zzz".to_string(),
                kind: SuggestionKind::Command,
                source: SuggestionSource::Commands,
                ..Default::default()
            },
            Suggestion {
                text: "aaa".to_string(),
                kind: SuggestionKind::History,
                source: SuggestionSource::History,
                ..Default::default()
            },
        ];
        let frecency = FrecencyStore::in_memory();
        for _ in 0..32 {
            frecency.record("zzz");
        }
        let result = rank_candidates_with_order(
            "",
            "",
            items,
            DEFAULT_MAX_RESULTS,
            MatchMode::Fuzzy,
            &SourceOrder::default_order(),
            &frecency,
        );
        // History sorts before Commands in the default order despite zzz's
        // large frecency boost.
        assert_eq!(result[0].source, SuggestionSource::History);
        assert_eq!(result[1].source, SuggestionSource::Commands);
    }

    #[test]
    fn test_frecency_boost_moves_order_not_kind() {
        use crate::types::SuggestionSource;
        // Boost may reorder candidates but must never mutate kind/source —
        // icon selection is owned by the provider that produced the row.
        let frecency = FrecencyStore::in_memory();
        for _ in 0..32 {
            frecency.record("cherry-pick");
        }
        let items = vec![
            Suggestion {
                text: "checkout".to_string(),
                kind: SuggestionKind::Command,
                source: SuggestionSource::Commands,
                ..Default::default()
            },
            Suggestion {
                text: "cherry-pick".to_string(),
                kind: SuggestionKind::History,
                source: SuggestionSource::History,
                ..Default::default()
            },
        ];
        let result = rank("che", items, DEFAULT_MAX_RESULTS, &frecency);
        assert_eq!(result[0].text, "cherry-pick", "boost must flip the order");
        for s in &result {
            let expected = if s.text == "cherry-pick" {
                (SuggestionKind::History, SuggestionSource::History)
            } else {
                (SuggestionKind::Command, SuggestionSource::Commands)
            };
            assert_eq!(
                (s.kind, s.source),
                expected,
                "boost reordered {s:?} but must not retype it"
            );
        }
    }
}
