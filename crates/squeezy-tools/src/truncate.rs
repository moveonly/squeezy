//! Middle-truncation utilities that keep both head and tail.
//!
//! Prefix-only truncation (`&s[..cap]`) drops everything after the cap,
//! discarding tails that frequently carry the signal we actually want — the
//! error footer of a build log, the last paragraph of an article, the closing
//! lines of a stack trace. [`truncate_middle_bytes`] splits the cap in half,
//! keeps the first and last slices, and inserts a marker counting the
//! removed characters in between.

/// Truncate `value` so the serialized result is at most `cap` bytes long,
/// preserving the start and end. Returns `(truncated, was_truncated)`.
///
/// When the input fits, returns the original unchanged.
pub(crate) fn truncate_middle_bytes(value: &str, cap: usize) -> (String, bool) {
    if value.len() <= cap {
        return (value.to_string(), false);
    }
    let truncated = truncate_middle_chars(value, cap);
    (truncated, true)
}

/// Truncate `value` so the serialized result is at most `cap` bytes long,
/// preserving the start and end. The marker `…N chars truncated…` records
/// how many characters were dropped.
///
/// Always returns a string ≤ `cap` bytes. For tiny caps where even the
/// marker does not fit, returns a byte-bounded prefix instead.
pub(crate) fn truncate_middle_chars(value: &str, cap: usize) -> String {
    if value.len() <= cap {
        return value.to_string();
    }

    // Marker length grows with the digit count of the removed-char count, but
    // we don't know that count until we know the marker length. Iterate a
    // couple times — convergence is immediate after the first pass because
    // the digit count rarely changes once the split is in the right
    // neighborhood.
    let mut marker = format!("\n…{} chars truncated…\n", value.chars().count());
    for _ in 0..3 {
        if marker.len() >= cap {
            // Cap is too small to hold the marker; fall back to a byte-bounded
            // prefix that respects char boundaries.
            return prefix_to_char_boundary(value, cap);
        }
        let body_budget = cap - marker.len();
        let left = body_budget / 2;
        let right = body_budget - left;
        let head = prefix_to_char_boundary(value, left);
        let tail = suffix_to_char_boundary(value, right);
        let removed = value.chars().count() - head.chars().count() - tail.chars().count();
        let next_marker = format!("\n…{removed} chars truncated…\n");
        if next_marker.len() == marker.len() {
            let mut out = String::with_capacity(head.len() + marker.len() + tail.len());
            out.push_str(&head);
            out.push_str(&next_marker);
            out.push_str(&tail);
            // Defensive: trim if the final string somehow ran over budget.
            if out.len() > cap {
                return prefix_to_char_boundary(&out, cap);
            }
            return out;
        }
        marker = next_marker;
    }
    // If three iterations did not converge, fall back to a head-only prefix.
    prefix_to_char_boundary(value, cap)
}

fn prefix_to_char_boundary(value: &str, mut end: usize) -> String {
    if end >= value.len() {
        return value.to_string();
    }
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_string()
}

fn suffix_to_char_boundary(value: &str, max_bytes: usize) -> String {
    if max_bytes >= value.len() {
        return value.to_string();
    }
    let mut start = value.len() - max_bytes;
    while start < value.len() && !value.is_char_boundary(start) {
        start += 1;
    }
    value[start..].to_string()
}

#[cfg(test)]
#[path = "truncate_tests.rs"]
mod tests;
