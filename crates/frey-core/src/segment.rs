//! Prompt segments and cache marks.
//!
//! A rendered prompt is an ordered list of [`Segment`]s. Each has a [`Stability`] class and a
//! content hash, and the cache planner uses exactly those two facts to decide where breakpoints go.
//!
//! Two provider rules make ordering load-bearing rather than cosmetic:
//!
//! * the cache prefix hierarchy is strictly `tools → system → messages`, and a change at one level
//!   invalidates that level **and every level after it**;
//! * where mixed TTLs are supported, longer-lived entries must appear before shorter-lived ones.
//!
//! Hashing itself lives in `frey-context` (which owns the `blake3` dependency); this module only
//! defines the value type, so `frey-core` stays free of transitive dependencies.

use std::fmt;

use crate::ids::SegmentId;

/// A 32-byte content hash. Serialised as lowercase hex so journals stay diffable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ContentHash([u8; 32]);

impl ContentHash {
    /// Wrap raw digest bytes.
    #[must_use]
    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// The raw digest bytes.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// A short prefix, for log lines and warning messages.
    #[must_use]
    pub fn short(&self) -> String {
        self.0[..4].iter().fold(String::new(), |mut acc, b| {
            use fmt::Write as _;
            let _ = write!(acc, "{b:02x}");
            acc
        })
    }
}

impl fmt::Display for ContentHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in &self.0 {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

impl serde::Serialize for ContentHash {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

impl<'de> serde::Deserialize<'de> for ContentHash {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        use serde::de::Error as _;
        let hex = <&str as serde::Deserialize>::deserialize(d)?;
        if hex.len() != 64 {
            return Err(D::Error::custom(format!(
                "a content hash is 64 hex characters, got {}",
                hex.len()
            )));
        }
        let mut out = [0u8; 32];
        for (i, slot) in out.iter_mut().enumerate() {
            *slot = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16)
                .map_err(|e| D::Error::custom(format!("invalid hex in content hash: {e}")))?;
        }
        Ok(Self(out))
    }
}

/// Which part of the prompt a segment belongs to.
///
/// The discriminant order **is** the prefix hierarchy: a change in a lower-numbered kind
/// invalidates every higher-numbered one.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum SegmentKind {
    /// Tool definitions. First, and therefore the most expensive thing to churn.
    Tools,
    /// Operator-authored instructions.
    System,
    /// The skill index: names and descriptions only.
    SkillIndex,
    /// Conversation history.
    History,
    /// Definitions injected mid-conversation by discovery. Deliberately last, so that discovering a
    /// tool never invalidates the stable prefix.
    Discovered,
}

/// How likely a segment is to change between turns.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum Stability {
    /// Identical every turn. Safe to cache behind a breakpoint.
    Static,
    /// Changes occasionally, e.g. when a toolset is filtered differently.
    Slow,
    /// Changes every turn. A breakpoint here is money set on fire.
    Volatile,
}

impl Stability {
    /// Whether a cache breakpoint may be placed at the end of a segment with this class.
    #[must_use]
    pub fn is_cacheable(self) -> bool {
        matches!(self, Self::Static | Self::Slow)
    }
}

/// How long a cache entry should live.
///
/// Deliberately coarse: providers spell these differently (Anthropic 5m/1h, OpenAI a 30m TTL, and
/// per-provider variation through OpenRouter), so the planner reasons in intent and each adapter
/// maps intent to whatever its provider actually accepts.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Default,
    serde::Serialize,
    serde::Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum CacheTtl {
    /// Minutes. Cheaper to write.
    #[default]
    Short,
    /// An hour or more. More expensive to write, worth it for a prefix reused across sessions.
    Long,
}

/// One contiguous piece of a rendered prompt.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Segment {
    /// Position in the rendered prompt.
    pub id: SegmentId,
    /// Which part of the prompt this is.
    pub kind: SegmentKind,
    /// How likely it is to change.
    pub stability: Stability,
    /// Hash of the rendered content.
    pub hash: ContentHash,
    /// Estimated token count. Approximate by nature; used for budgeting and for deciding whether a
    /// prefix clears a provider's minimum cacheable length.
    pub est_tokens: u32,
    /// A short human label for warnings, e.g. `system:prompts/system.md`.
    pub label: smol_str::SmolStr,
}

/// A request that the cache be broken after a particular segment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CacheMark {
    /// The last segment covered by this cache entry.
    pub at: SegmentId,
    /// How long the entry should live.
    pub ttl: CacheTtl,
}

/// Whether a list of cache marks satisfies the ordering rule that mixed TTLs impose.
///
/// Anthropic require longer-lived entries to appear before shorter-lived ones. Checking it here,
/// on plain data, means the rule is unit-testable without a provider.
#[must_use]
pub fn ttls_are_correctly_ordered(marks: &[CacheMark]) -> bool {
    let mut seen_short = false;
    for mark in marks {
        match mark.ttl {
            CacheTtl::Short => seen_short = true,
            CacheTtl::Long if seen_short => return false,
            CacheTtl::Long => {}
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hash(seed: u8) -> ContentHash {
        ContentHash::from_bytes([seed; 32])
    }

    #[test]
    fn content_hashes_round_trip_as_hex() {
        let h = hash(0xab);
        assert_eq!(h.to_string().len(), 64);
        assert!(h.to_string().starts_with("abab"));
        assert_eq!(h.short(), "abababab");

        let json = serde_json::to_string(&h).unwrap();
        assert_eq!(serde_json::from_str::<ContentHash>(&json).unwrap(), h);
    }

    #[test]
    fn malformed_hashes_are_rejected_rather_than_padded() {
        assert!(serde_json::from_str::<ContentHash>("\"abcd\"").is_err());
        assert!(serde_json::from_str::<ContentHash>(&format!("\"{}\"", "z".repeat(64))).is_err());
    }

    #[test]
    fn segment_kind_order_is_the_prefix_hierarchy() {
        // A change in tools invalidates system and everything after; discovery invalidates nothing.
        assert!(SegmentKind::Tools < SegmentKind::System);
        assert!(SegmentKind::System < SegmentKind::History);
        assert!(SegmentKind::History < SegmentKind::Discovered);
        assert_eq!(
            SegmentKind::Discovered.max(SegmentKind::Tools),
            SegmentKind::Discovered,
            "discovered definitions must sort last so they never disturb the stable prefix"
        );
    }

    #[test]
    fn breakpoints_are_only_permitted_on_stable_segments() {
        assert!(Stability::Static.is_cacheable());
        assert!(Stability::Slow.is_cacheable());
        assert!(
            !Stability::Volatile.is_cacheable(),
            "a breakpoint here re-writes the cache each turn"
        );
    }

    #[test]
    fn long_lived_cache_entries_must_precede_short_lived_ones() {
        let long = CacheMark { at: SegmentId(0), ttl: CacheTtl::Long };
        let short = CacheMark { at: SegmentId(1), ttl: CacheTtl::Short };

        assert!(ttls_are_correctly_ordered(&[long, short]));
        assert!(ttls_are_correctly_ordered(&[long, long, short, short]));
        assert!(ttls_are_correctly_ordered(&[]));
        assert!(!ttls_are_correctly_ordered(&[short, long]), "Anthropic reject this ordering");
    }
}
