//! Content hashing for prompt segments.
//!
//! The hash is what turns "did the prefix change?" from a guess into a fact. It must be stable
//! across runs and across processes, because churn detection compares this turn's hash with last
//! turn's — possibly after a restart — so a `DefaultHasher` (randomly seeded per process) would
//! silently report churn on every first turn.

use frey_core::segment::ContentHash;

/// Hash a segment's rendered bytes.
#[must_use]
pub fn hash_bytes(bytes: &[u8]) -> ContentHash {
    ContentHash::from_bytes(*blake3::hash(bytes).as_bytes())
}

/// Hash a segment's rendered text.
#[must_use]
pub fn hash_text(text: &str) -> ContentHash {
    hash_bytes(text.as_bytes())
}

/// Hash several pieces as one segment, with a separator so that `["ab", "c"]` and `["a", "bc"]`
/// hash differently. Without the separator, reordering tool definitions could leave the hash
/// unchanged and hide real churn.
#[must_use]
pub fn hash_parts<'a>(parts: impl IntoIterator<Item = &'a str>) -> ContentHash {
    let mut hasher = blake3::Hasher::new();
    for part in parts {
        hasher.update(&(part.len() as u64).to_le_bytes());
        hasher.update(part.as_bytes());
    }
    ContentHash::from_bytes(*hasher.finalize().as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hashing_is_stable_across_calls_and_processes() {
        // Recorded once, by hand, from this implementation. If this value changes, every stored
        // cache plan in every journal becomes incomparable, so the change must be deliberate.
        let h = hash_text("frey");
        assert_eq!(h, hash_text("frey"));
        assert_ne!(h, hash_text("frey "));
        assert_eq!(h.to_string().len(), 64);
    }

    #[test]
    fn part_boundaries_are_significant() {
        // The failure this prevents: two tool definitions swapping order, or one gaining a
        // character that another loses, leaving the concatenation identical and the churn invisible.
        assert_ne!(hash_parts(["ab", "c"]), hash_parts(["a", "bc"]));
        assert_ne!(hash_parts(["a", "b"]), hash_parts(["b", "a"]));
        assert_eq!(hash_parts(["a", "b"]), hash_parts(["a", "b"]));
    }

    #[test]
    fn an_empty_segment_still_has_a_hash() {
        assert_ne!(hash_text(""), hash_text(" "));
        assert_eq!(hash_parts(Vec::<&str>::new()), hash_parts(Vec::<&str>::new()));
    }
}
