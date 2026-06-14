use std::collections::{BTreeSet, HashMap};

use squeezy_parse::BodyHit;

pub(crate) enum CandidateSet<'a, T> {
    All,
    None,
    Indexes(&'a [T]),
}

pub(crate) fn rarest_indexed_trigram<'a, T>(
    needle: &str,
    index: &'a HashMap<[u8; 3], Vec<T>>,
) -> CandidateSet<'a, T> {
    let trigrams = unique_trigrams(needle);
    if trigrams.is_empty() {
        return CandidateSet::All;
    }

    let mut best = None;
    for trigram in trigrams {
        let Some(candidates) = index.get(&trigram) else {
            return CandidateSet::None;
        };
        if best
            .as_ref()
            .map(|current: &&Vec<T>| candidates.len() < current.len())
            .unwrap_or(true)
        {
            best = Some(candidates);
        }
    }

    best.map(|candidates| CandidateSet::Indexes(candidates.as_slice()))
        .unwrap_or(CandidateSet::All)
}

pub(crate) fn unique_trigrams(text: &str) -> BTreeSet<[u8; 3]> {
    let bytes = text.as_bytes();
    if bytes.len() < 3 {
        return BTreeSet::new();
    }
    bytes
        .windows(3)
        .map(|window| [window[0], window[1], window[2]])
        .collect()
}

/// Fingerprint the ordered `body_hits` text so `rebuild_indexes` can detect a
/// no-op refresh and reuse the existing lowercase shadow + trigram index.
pub(crate) fn body_hits_fingerprint(body_hits: &[BodyHit]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    body_hits.len().hash(&mut hasher);
    for hit in body_hits {
        hit.text.hash(&mut hasher);
    }
    hasher.finish()
}
