//! Two-pass fuzzy subsequence scorer for the composer popups.
//!
//! Replaces the prefix-only filter the slash menu and `@`-mention popup
//! previously used so a query like `cmp` resolves `/compact` and
//! `gh/lib` resolves `crates/squeezy-graph/src/lib.rs`. Both popups call
//! [`score`] directly, filtering on `is_some` and sorting by the
//! returned `i32` descending.
//!
//! Scoring rules (higher is better, `None` means the query is not a
//! case-insensitive subsequence of the candidate):
//! - `+16` per matched char at a word boundary — index 0 or immediately
//!   after a non-alphanumeric char.
//! - `+8`  per matched char that is adjacent to the previous match
//!   position (a consecutive run).
//! - `-3` per position skipped between two matched chars (gap penalty).
//!
//! Implementation: dynamic programming over `(query_index, candidate_index)`
//! with row swapping so memory stays at `O(candidate_len)`. The two passes
//! are the `q[0]` seed pass and the recurrence pass that backtracks over
//! every prior match position to find the best score.
//!
//! `pub` so internal modules (mention/slash) can call it directly; not
//! re-exported from the crate.

const WORD_BOUNDARY_BONUS: i32 = 16;
const CONSECUTIVE_BONUS: i32 = 8;
const GAP_PENALTY: i32 = -3;

/// Sentinel for unreachable DP cells. Picked so that adding a realistic
/// gap penalty (at most `GAP_PENALTY * len`) cannot bring a real score
/// down into the sentinel band — `i32::MIN / 4` leaves ~536M of headroom.
const NEG_INF: i32 = i32::MIN / 4;

/// Score `candidate` against `query`. Returns `None` if the lowercased
/// chars of `query` are not, in order, a subsequence of the lowercased
/// chars of `candidate`. Higher is better. An empty query returns
/// `Some(0)`.
pub fn score(candidate: &str, query: &str) -> Option<i32> {
    if query.is_empty() {
        return Some(0);
    }
    let cand: Vec<char> = candidate.chars().flat_map(char::to_lowercase).collect();
    let q: Vec<char> = query.chars().flat_map(char::to_lowercase).collect();
    if q.is_empty() {
        return Some(0);
    }
    if q.len() > cand.len() {
        return None;
    }

    let n = cand.len();
    let is_word_boundary = |j: usize| j == 0 || !cand[j - 1].is_alphanumeric();

    // Pass 1: seed `prev[j]` with the score of matching `q[0]` at `j`.
    let q0 = q[0];
    let mut prev = vec![NEG_INF; n];
    for (j, &ch) in cand.iter().enumerate() {
        if ch == q0 {
            prev[j] = if is_word_boundary(j) {
                WORD_BOUNDARY_BONUS
            } else {
                0
            };
        }
    }

    // Pass 2: for each subsequent query char, pick the best prior match
    // position `k < j` and apply the bonus/penalty deltas for the
    // transition `k -> j`.
    let mut curr = vec![NEG_INF; n];
    for (i, &qi) in q.iter().enumerate().skip(1) {
        curr.fill(NEG_INF);
        for (j, &ch) in cand.iter().enumerate().skip(i) {
            if ch != qi {
                continue;
            }
            let mut best = NEG_INF;
            for (k, &prev_score) in prev.iter().enumerate().take(j).skip(i - 1) {
                if prev_score <= NEG_INF / 2 {
                    continue;
                }
                let gap = (j - k - 1) as i32;
                let mut delta = GAP_PENALTY * gap;
                if j == k + 1 {
                    delta += CONSECUTIVE_BONUS;
                }
                if is_word_boundary(j) {
                    delta += WORD_BOUNDARY_BONUS;
                }
                let candidate_score = prev_score + delta;
                if candidate_score > best {
                    best = candidate_score;
                }
            }
            curr[j] = best;
        }
        std::mem::swap(&mut prev, &mut curr);
    }

    let best = prev.iter().copied().max().unwrap_or(NEG_INF);
    if best <= NEG_INF / 2 {
        None
    } else {
        Some(best)
    }
}

#[cfg(test)]
#[path = "fuzzy_tests.rs"]
mod tests;
