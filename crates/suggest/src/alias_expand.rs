//! Alias-expanded command head for SSH-host injection.
//!
//! Walks `ctx.command` through the alias map (cycle-guarded, depth-capped)
//! so an alias like `dev=ssh host` still triggers SSH-host completion even
//! though the literal command token is `dev`.

use std::borrow::Cow;
use std::collections::HashSet;

use buffer::CommandContext;

use crate::alias::AliasStore;

/// Recursion cap for chained alias-of-alias expansion (cycle/depth guard).
pub(crate) const MAX_ALIAS_HOPS: usize = 16;

/// Resolve the effective command head for `ctx` by expanding aliases.
///
/// Returns `None` when there is nothing to resolve (word_index 0 means the
/// current word IS the alias name) or when expansion yields an empty token
/// list. Otherwise returns the resolved head command, borrowed when no alias
/// matched and owned when expansion rewrote it.
pub(crate) fn expand_alias_head<'a>(
    ctx: &'a CommandContext,
    alias_map: &AliasStore,
) -> Option<Cow<'a, str>> {
    if ctx.word_index == 0 {
        return None;
    }
    let command = ctx.command.as_deref()?;

    let Some(initial) = alias_map.get(command) else {
        return Some(Cow::Borrowed(command));
    };

    let mut tokens: Vec<String> = initial;
    let mut visited: HashSet<String> = HashSet::new();
    visited.insert(command.to_string());

    for _ in 0..MAX_ALIAS_HOPS {
        let head = match tokens.first() {
            Some(h) => h.clone(),
            None => break,
        };
        if visited.contains(&head) {
            break;
        }
        match alias_map.get(&head) {
            Some(next) => {
                visited.insert(head);
                let tail: Vec<String> = tokens.drain(1..).collect();
                tokens = next;
                tokens.extend(tail);
            }
            None => break,
        }
    }

    let resolved = tokens.into_iter().next()?;
    Some(Cow::Owned(resolved))
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use buffer::{CommandContext, QuoteState};

    use super::*;
    use crate::alias::AliasStore;

    fn ctx(
        command: Option<&str>,
        args: &[&str],
        current_word: &str,
        word_index: usize,
    ) -> CommandContext {
        CommandContext {
            command: command.map(String::from),
            args: args.iter().map(|s| (*s).to_string()).collect(),
            current_word: current_word.to_string(),
            word_index,
            is_flag: current_word.starts_with('-'),
            is_long_flag: current_word.starts_with("--"),
            preceding_flag: None,
            in_pipe: false,
            in_redirect: false,
            quote_state: QuoteState::None,
            is_first_segment: true,
        }
    }

    fn store(entries: &[(&str, &[&str])]) -> AliasStore {
        let store = AliasStore::empty();
        let map: HashMap<String, crate::alias::AliasEntry> = entries
            .iter()
            .map(|(name, toks)| {
                let v: Vec<String> = toks.iter().map(|s| (*s).to_string()).collect();
                ((*name).to_string(), crate::alias::AliasEntry::new(v))
            })
            .collect();
        store.populate(map);
        store
    }

    #[test]
    fn expand_returns_none_at_word_index_zero() {
        // word_index 0 means current_word is the alias name itself, not a positional.
        let aliases = store(&[("gco", &["git", "checkout"])]);
        let c = ctx(None, &[], "gc", 0);
        assert!(expand_alias_head(&c, &aliases).is_none());
    }

    #[test]
    fn expand_returns_borrowed_when_no_alias() {
        let aliases = AliasStore::empty();
        let c = ctx(Some("git"), &["checkout"], "main", 2);
        let exp = expand_alias_head(&c, &aliases).unwrap();
        assert!(matches!(exp, Cow::Borrowed("git")));
    }

    #[test]
    fn expand_single_word_alias() {
        let aliases = store(&[("g", &["git"])]);
        let c = ctx(Some("g"), &["push"], "", 2);
        let exp = expand_alias_head(&c, &aliases).unwrap();
        assert_eq!(exp.as_ref(), "git");
    }

    #[test]
    fn expand_multi_word_alias_resolves_head() {
        let aliases = store(&[("gco", &["git", "checkout"])]);
        let c = ctx(Some("gco"), &["main"], "", 2);
        let exp = expand_alias_head(&c, &aliases).unwrap();
        assert_eq!(exp.as_ref(), "git");
    }

    #[test]
    fn expand_chained_aliases_resolves_final_head() {
        let aliases = store(&[("gcb", &["gco", "-b"]), ("gco", &["git", "checkout"])]);
        let c = ctx(Some("gcb"), &["feature"], "", 2);
        let exp = expand_alias_head(&c, &aliases).unwrap();
        assert_eq!(exp.as_ref(), "git");
    }

    #[test]
    fn expand_cycle_guard() {
        // a -> b -> a must terminate, not stack-overflow.
        let aliases = store(&[("a", &["b"]), ("b", &["a"])]);
        let c = ctx(Some("a"), &[], "", 1);
        let exp = expand_alias_head(&c, &aliases).unwrap();
        assert!(["a", "b"].contains(&exp.as_ref()));
    }

    #[test]
    fn expand_depth_cap_stops_at_max_hops() {
        let chain_len = MAX_ALIAS_HOPS + 5;
        let names: Vec<String> = (0..chain_len).map(|i| format!("a{i}")).collect();
        let mut entries: Vec<(&str, Vec<String>)> = Vec::new();
        for i in 0..chain_len - 1 {
            entries.push((names[i].as_str(), vec![names[i + 1].clone()]));
        }
        let store = AliasStore::empty();
        let map: HashMap<String, crate::alias::AliasEntry> = entries
            .into_iter()
            .map(|(n, v)| (n.to_string(), crate::alias::AliasEntry::new(v)))
            .collect();
        store.populate(map);

        let c = ctx(Some("a0"), &[], "", 1);
        let exp = expand_alias_head(&c, &store).unwrap();
        let head = exp.as_ref();
        assert!(head.starts_with('a'));
        let idx: usize = head[1..].parse().unwrap();
        assert!(
            idx >= MAX_ALIAS_HOPS && idx < chain_len,
            "expansion must stop at the depth cap, not unwind the whole chain (head={head})"
        );
    }
}
