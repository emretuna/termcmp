/// Count of leading characters that are identical between `a` and `b`.
///
/// Used by async completion providers to seed `match_indices` with the
/// typed-prefix positions before frizbee re-ranks. After ranking,
/// `fuzzy::rank_with_mode` overwrites `match_indices` with frizbee's
/// full set of matched character positions (used for popup highlighting).
pub fn common_prefix_char_count(a: &str, b: &str) -> usize {
    a.chars()
        .zip(b.chars())
        .take_while(|(ca, cb)| ca == cb)
        .count()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_prefix() {
        assert_eq!(common_prefix_char_count("supabase", "supabase backups"), 8);
    }

    #[test]
    fn partial_prefix() {
        assert_eq!(common_prefix_char_count("sup", "super-user"), 3);
    }

    #[test]
    fn no_prefix() {
        assert_eq!(common_prefix_char_count("abc", "xyz"), 0);
    }

    #[test]
    fn empty_inputs() {
        assert_eq!(common_prefix_char_count("", "hello"), 0);
        assert_eq!(common_prefix_char_count("hello", ""), 0);
        assert_eq!(common_prefix_char_count("", ""), 0);
    }

    #[test]
    fn identical() {
        assert_eq!(common_prefix_char_count("same", "same"), 4);
    }
}
