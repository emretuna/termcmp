//! Context classifier for the suggest pipeline.
//!
//! Inspects `(current_word, in_redirect, word_index)` and emits exactly
//! ONE `Context` value. The engine dispatches off this to decide which
//! providers run and what suppression rules apply.
//!
//! Precedence (top wins):
//!   1. CommandPosition  — typing the binary name
//!   2. Redirect         — typing after a shell redirect operator
//!   3. PathPrefix       — current_word starts with `./`, `/`, `~/`, `../`
//!   4. FlagPrefix       — current_word starts with `-` or `--`
//!   5. UnspeccedArg     — any other argument position

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Context {
    CommandPosition,
    Redirect,
    PathPrefix,
    FlagPrefix,
    UnspeccedArg,
}

/// Inputs passed in by the engine. Kept as a struct (not borrows of
/// `CommandContext`) so the classifier can be tested without dragging
/// the full engine context types into scope.
pub struct ClassifyInput<'a> {
    pub current_word: &'a str,
    pub in_redirect: bool,
    pub word_index: usize,
}

pub fn classify(i: ClassifyInput<'_>) -> Context {
    if i.word_index == 0 {
        return Context::CommandPosition;
    }
    if i.in_redirect {
        return Context::Redirect;
    }
    if has_path_prefix(i.current_word) {
        return Context::PathPrefix;
    }
    if has_flag_prefix(i.current_word) {
        return Context::FlagPrefix;
    }
    Context::UnspeccedArg
}

fn has_path_prefix(s: &str) -> bool {
    s.starts_with("./") || s.starts_with("../") || s.starts_with('/') || s.starts_with("~/")
}

fn has_flag_prefix(s: &str) -> bool {
    s.starts_with('-')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input<'a>(word: &'a str, word_index: usize, in_redirect: bool) -> ClassifyInput<'a> {
        ClassifyInput {
            current_word: word,
            in_redirect,
            word_index,
        }
    }

    #[test]
    fn command_position_takes_precedence() {
        // word_index 0 wins even if word looks like a path.
        assert_eq!(classify(input("./foo", 0, false)), Context::CommandPosition);
    }

    #[test]
    fn redirect_beats_path_prefix() {
        assert_eq!(classify(input("./out.txt", 1, true)), Context::Redirect);
    }

    #[test]
    fn redirect_fires_with_plain_word() {
        // Redirect classification must not depend on the word looking
        // like a path — `echo foo > output.txt` is the canonical case.
        assert_eq!(classify(input("output.txt", 1, true)), Context::Redirect);
    }

    #[test]
    fn path_prefix_beats_flag_prefix() {
        // "./--foo" is a weird edge — path prefix still wins.
        assert_eq!(classify(input("./--foo", 1, false)), Context::PathPrefix);
    }

    #[test]
    fn path_prefix_variants() {
        for prefix in ["./foo", "../bar", "/abs", "~/home"] {
            assert_eq!(
                classify(input(prefix, 1, false)),
                Context::PathPrefix,
                "prefix {prefix:?} should classify as PathPrefix"
            );
        }
    }

    #[test]
    fn flag_prefix_short_and_long() {
        for prefix in ["-v", "--verbose", "--"] {
            assert_eq!(
                classify(input(prefix, 1, false)),
                Context::FlagPrefix,
                "prefix {prefix:?} should classify as FlagPrefix"
            );
        }
    }

    #[test]
    fn bare_single_dash_is_flag_prefix() {
        assert_eq!(classify(input("-", 1, false)), Context::FlagPrefix);
    }

    #[test]
    fn unspecced_arg_for_plain_word() {
        assert_eq!(classify(input("anything", 1, false)), Context::UnspeccedArg);
    }

    #[test]
    fn empty_current_word_is_unspecced_arg() {
        assert_eq!(classify(input("", 1, false)), Context::UnspeccedArg);
    }
}
