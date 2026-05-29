use super::score;

#[test]
fn empty_query_scores_zero_against_anything() {
    assert_eq!(score("crates/squeezy-graph/src/lib.rs", ""), Some(0));
    assert_eq!(score("", ""), Some(0));
}

#[test]
fn returns_none_when_chars_missing_or_out_of_order() {
    assert!(score("foo", "fbz").is_none());
    // Out of order: 'b' before 'a' in the query has no 'a' after the 'b'.
    assert!(score("abc", "ba").is_none());
}

#[test]
fn rewards_word_boundary_and_consecutive_runs() {
    // `/compact` vs `/co`: word boundary on `/`, then word boundary on
    // `c` (after the non-alphanumeric `/`) plus consecutive, then
    // consecutive on `o`. 16 + (16 + 8) + 8 = 48.
    assert_eq!(score("/compact", "/co"), Some(48));
    // `lib.rs` vs `lib`: 16 + 8 + 8 = 32.
    assert_eq!(score("lib.rs", "lib"), Some(32));
}

#[test]
fn backtracks_to_prefer_later_word_boundary_match() {
    // Greedy would match `a` at 0 and `b` at 3 (16 - 6 = 10). The
    // backtracking pass must instead match `a` at 2 (after `-`,
    // +16) and `b` at 3 (consecutive, +8) for 24.
    assert_eq!(score("a-ab", "ab"), Some(24));
}

#[test]
fn penalises_gaps_and_is_case_insensitive() {
    // `aXXb` vs `ab`: a at 0 (+16), b at 3 with gap 2 (-6) → 10.
    // `aXXXb` vs `ab`: same start, larger gap (-9) → 7.
    assert_eq!(score("aXXb", "ab"), Some(10));
    assert_eq!(score("aXXXb", "ab"), Some(7));
    // Lowercasing happens before the walk so an uppercase query
    // produces the same score as its lowercase form.
    assert_eq!(score("/attach", "/ATC"), score("/attach", "/atc"));
}
