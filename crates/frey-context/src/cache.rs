//! The cache planner.
//!
//! Given the segments of a rendered prompt, last turn's hashes, and what the provider can do, decide
//! where the cache breakpoints go — and say so out loud when the answer is "nowhere useful".
//!
//! This is a **pure function**. No I/O, no clock, no randomness. Every provider rule it enforces is
//! therefore a unit test rather than a production surprise, and the rules are worth listing because
//! each one is a way to lose money silently:
//!
//! * a breakpoint may sit only at the end of a segment that did **not** change since last turn;
//! * providers cap how many breakpoints a request may carry, and automatic caching consumes one;
//! * where mixed lifetimes are supported, longer-lived entries must appear before shorter-lived ones;
//! * a prefix shorter than the model's minimum is **silently** not cached, with no error from the
//!   provider — the single most expensive quiet failure in the whole system;
//! * the prefix hierarchy is `tools → system → … → history`, and a change at one level invalidates
//!   that level and everything after it, so a breakpoint after a churning segment is worthless.

use std::collections::BTreeMap;

use frey_core::event::Warning;
use frey_core::ids::SegmentId;
use frey_core::provider_caps::{CacheSupport, ProviderCapabilities};
use frey_core::segment::{
    CacheMark, CacheTtl, ContentHash, Segment, SegmentKind, Stability, ttls_are_correctly_ordered,
};

/// What the previous turn's prompt looked like, for churn detection.
///
/// Kept as a map rather than the whole plan so it can be persisted cheaply in the run journal and
/// survive a restart, which is when churn is least obvious and most expensive.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PreviousPrompt {
    hashes: BTreeMap<SegmentId, ContentHash>,
}

impl PreviousPrompt {
    /// Nothing seen yet: the first turn of a run.
    #[must_use]
    pub fn none() -> Self {
        Self::default()
    }

    /// Record what a set of segments hashed to.
    #[must_use]
    pub fn from_segments(segments: &[Segment]) -> Self {
        Self { hashes: segments.iter().map(|s| (s.id, s.hash)).collect() }
    }

    /// Whether `segment` differs from what was seen last turn.
    ///
    /// A segment that was not present last turn is **not** churn: it is new, and new content behind
    /// a breakpoint is the normal way a cache grows.
    #[must_use]
    pub fn changed(&self, segment: &Segment) -> bool {
        self.hashes.get(&segment.id).is_some_and(|prev| *prev != segment.hash)
    }

    /// What a segment hashed to last turn, if it was present.
    #[must_use]
    pub fn previous_hash(&self, id: SegmentId) -> Option<ContentHash> {
        self.hashes.get(&id).copied()
    }

    /// Whether nothing has been recorded.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.hashes.is_empty()
    }
}

/// Where the breakpoints go, and what the developer should know.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CachePlan {
    /// The marks to attach to the request, in prompt order.
    pub marks: Vec<CacheMark>,
    /// Diagnostics. Empty is the goal; each entry names a cost and a fix.
    pub warnings: Vec<Warning>,
    /// How many tokens the cached prefix covers, when Frey placed the breakpoint.
    pub cached_prefix_tokens: u32,
    /// The provider caches on its own, without marks.
    ///
    /// Without this, an empty `marks` list is ambiguous between "the provider handles it" and
    /// "nothing here was cacheable", and a developer asking whether their prompt is cached
    /// deserves a different answer in each case.
    pub provider_caches_automatically: bool,
}

impl CachePlan {
    /// A plan that places no marks.
    #[must_use]
    pub fn empty() -> Self {
        Self {
            marks: Vec::new(),
            warnings: Vec::new(),
            cached_prefix_tokens: 0,
            provider_caches_automatically: false,
        }
    }

    /// Whether Frey placed a breakpoint.
    #[must_use]
    pub fn caches_anything(&self) -> bool {
        !self.marks.is_empty()
    }

    /// Whether the prompt is cached at all, by Frey or by the provider itself.
    #[must_use]
    pub fn is_cached(&self) -> bool {
        self.caches_anything() || self.provider_caches_automatically
    }
}

/// Plans cache breakpoints for one request.
#[derive(Debug, Clone, Copy, Default)]
pub struct CachePlanner;

impl CachePlanner {
    /// Decide where the breakpoints go.
    ///
    /// `segments` must be in prompt order. `previous` is last turn's hashes, or
    /// [`PreviousPrompt::none`] on the first turn.
    #[must_use]
    pub fn plan(
        segments: &[Segment],
        previous: &PreviousPrompt,
        caps: &ProviderCapabilities,
    ) -> CachePlan {
        let mut warnings = Vec::new();

        let automatic = matches!(caps.cache, CacheSupport::Automatic { .. });
        let budget = caps.cache.breakpoint_budget();
        if budget == 0 {
            if !segments.is_empty() && matches!(caps.cache, CacheSupport::None) {
                warnings.push(Warning::Degraded {
                    capability: "prompt-cache".into(),
                    fallback: "this provider does not cache prompts; every turn pays full price"
                        .into(),
                });
            }
            return CachePlan {
                warnings,
                provider_caches_automatically: automatic,
                ..CachePlan::empty()
            };
        }

        // 1. Churn. A segment that claims to be stable but changed is the expensive case: a
        //    breakpoint after it would rewrite the whole prefix every turn. Warn, and treat it as
        //    volatile for the rest of this plan.
        let mut effective: Vec<(&Segment, Stability)> = Vec::with_capacity(segments.len());
        for segment in segments {
            let churned = segment.stability.is_cacheable() && previous.changed(segment);
            if churned {
                warnings.push(Warning::CacheChurn {
                    segment: segment.label.clone(),
                    tokens: segment.est_tokens,
                    advice: churn_advice(segment.kind),
                });
            }
            let stability = if churned { Stability::Volatile } else { segment.stability };
            effective.push((segment, stability));
        }

        // 2. Candidates: the last stable segment of each stable run, in prompt order. A breakpoint
        //    anywhere inside a run is strictly worse than one at its end, since the end covers more.
        let candidates = stable_run_ends(&effective);
        if candidates.is_empty() {
            if !segments.is_empty() {
                warnings.push(Warning::Degraded {
                    capability: "prompt-cache".into(),
                    fallback: "no segment was stable enough to cache behind".into(),
                });
            }
            return CachePlan {
                warnings,
                provider_caches_automatically: automatic,
                ..CachePlan::empty()
            };
        }

        // 3. Prefix length. Cumulative tokens up to and including each candidate.
        let cumulative = cumulative_tokens(segments);

        // 4. Drop candidates whose prefix is below the model's minimum. The provider will accept
        //    them and silently not cache, which is why this is a warning rather than a shrug.
        let min_prefix = caps.cache.min_prefix_tokens().unwrap_or(0);
        let mut usable: Vec<SegmentId> = Vec::new();
        for id in &candidates {
            let covered = cumulative.get(&id.index()).copied().unwrap_or(0);
            if covered < min_prefix {
                continue;
            }
            usable.push(*id);
        }
        if usable.is_empty() {
            let best =
                candidates.last().and_then(|id| cumulative.get(&id.index()).copied()).unwrap_or(0);
            warnings.push(Warning::BelowMinPrefix { have: best, need: min_prefix });
            return CachePlan {
                warnings,
                provider_caches_automatically: automatic,
                ..CachePlan::empty()
            };
        }

        // 5. Budget. Keep the candidates that cover the most tokens, then restore prompt order:
        //    an out-of-order mark is meaningless to every provider.
        if usable.len() > budget as usize {
            warnings.push(Warning::Degraded {
                capability: "cache-breakpoints".into(),
                fallback: format!(
                    "wanted {} breakpoints, this model allows {budget}; kept the {budget} \
                     covering the most tokens",
                    usable.len()
                )
                .into(),
            });
            usable.sort_by_key(|id| std::cmp::Reverse(cumulative.get(&id.index()).copied()));
            usable.truncate(budget as usize);
            usable.sort_unstable();
        }

        // 6. Lifetimes. Long-lived entries for the parts that outlive a session; short-lived for
        //    conversation. Ordering is then correct by construction, because the segment kinds that
        //    earn a long lifetime sort before the ones that do not — asserted below regardless.
        let supports_long = caps.cache.supports_ttl(CacheTtl::Long);
        let kind_of: BTreeMap<SegmentId, SegmentKind> =
            segments.iter().map(|s| (s.id, s.kind)).collect();

        // Long lifetimes go to the *leading run* of eligible marks, not to whichever marks happen
        // to sit on a long-lived kind. Nothing forces a caller to order segments by kind, and the
        // moment a short-lived mark precedes a long-lived one the request is invalid. Deciding it
        // positionally makes the ordering correct by construction rather than by convention, and it
        // is the better answer anyway: the earliest prefix is the one reused across most requests.
        let mut long_still_allowed = supports_long;
        let marks: Vec<CacheMark> = usable
            .iter()
            .map(|id| {
                let eligible = long_still_allowed
                    && kind_of
                        .get(id)
                        .is_some_and(|k| matches!(k, SegmentKind::Tools | SegmentKind::System));
                if !eligible {
                    long_still_allowed = false;
                }
                CacheMark { at: *id, ttl: if eligible { CacheTtl::Long } else { CacheTtl::Short } }
            })
            .collect();

        debug_assert!(
            ttls_are_correctly_ordered(&marks),
            "long-lived cache entries must precede short-lived ones"
        );

        let cached_prefix_tokens =
            marks.last().and_then(|m| cumulative.get(&m.at.index()).copied()).unwrap_or(0);

        CachePlan {
            marks,
            warnings,
            cached_prefix_tokens,
            provider_caches_automatically: automatic,
        }
    }
}

fn churn_advice(kind: SegmentKind) -> smol_str::SmolStr {
    match kind {
        SegmentKind::Tools => {
            "the tool block changed between turns. A toolset that reorders its listing, or a \
             description containing a timestamp or counter, will do this."
        }
        SegmentKind::System => {
            "the system prompt changed between turns. A clock, a session id, or a token count \
             interpolated into it will do this."
        }
        SegmentKind::SkillIndex => {
            "the skill index changed between turns. Skills discovered mid-run belong after the \
             breakpoint, not in the index."
        }
        SegmentKind::History | SegmentKind::Discovered => {
            "this segment was expected to be stable but changed."
        }
    }
    .into()
}

/// The last cacheable segment of each maximal cacheable run.
fn stable_run_ends(effective: &[(&Segment, Stability)]) -> Vec<SegmentId> {
    let mut ends = Vec::new();
    for (i, (segment, stability)) in effective.iter().enumerate() {
        if !stability.is_cacheable() {
            continue;
        }
        let next_is_cacheable = effective.get(i + 1).is_some_and(|(_, s)| s.is_cacheable());
        if !next_is_cacheable {
            ends.push(segment.id);
        }
    }
    ends
}

/// Tokens covered by the prompt up to and including each segment, keyed by segment index.
fn cumulative_tokens(segments: &[Segment]) -> BTreeMap<u32, u32> {
    let mut out = BTreeMap::new();
    let mut running = 0u32;
    for segment in segments {
        running = running.saturating_add(segment.est_tokens);
        out.insert(segment.id.index(), running);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hash::hash_text;
    use crate::profiles;

    fn seg(id: u32, kind: SegmentKind, stability: Stability, tokens: u32, body: &str) -> Segment {
        Segment {
            id: SegmentId(id),
            kind,
            stability,
            hash: hash_text(body),
            est_tokens: tokens,
            label: format!("{kind:?}:{id}").into(),
        }
    }

    /// A realistic prompt: a big stable tool block, a stable system prompt, and churning history.
    fn typical() -> Vec<Segment> {
        vec![
            seg(0, SegmentKind::Tools, Stability::Static, 12_000, "tool definitions"),
            seg(1, SegmentKind::System, Stability::Static, 800, "you are a careful assistant"),
            seg(2, SegmentKind::History, Stability::Volatile, 400, "turn 1"),
        ]
    }

    #[test]
    fn the_breakpoint_lands_at_the_end_of_the_stable_prefix() {
        let plan = CachePlanner::plan(&typical(), &PreviousPrompt::none(), &profiles::opus5());
        assert_eq!(plan.marks.len(), 1);
        assert_eq!(plan.marks[0].at, SegmentId(1), "after system, before history");
        assert_eq!(plan.cached_prefix_tokens, 12_800);
        assert!(plan.warnings.is_empty(), "{:?}", plan.warnings);
    }

    #[test]
    fn a_clock_in_the_system_prompt_is_named_and_priced() {
        // The single most common way to destroy a prompt cache, and the one a developer is least
        // likely to notice: the provider reports no error, the bill just grows.
        let turn1 = typical();
        let previous = PreviousPrompt::from_segments(&turn1);

        let mut turn2 = typical();
        turn2[1].hash = hash_text("you are a careful assistant. The time is 14:32:06.");

        let plan = CachePlanner::plan(&turn2, &previous, &profiles::opus5());

        let churn = plan
            .warnings
            .iter()
            .find(|w| matches!(w, Warning::CacheChurn { .. }))
            .expect("churn must be reported");
        let Warning::CacheChurn { segment, tokens, advice } = churn else { unreachable!() };
        assert_eq!(segment.as_str(), "System:1");
        assert_eq!(*tokens, 800);
        assert!(advice.contains("clock"), "the advice must be actionable: {advice}");

        // And the plan must not put a breakpoint after the churning segment, which would rewrite
        // the whole prefix every turn.
        assert!(plan.marks.iter().all(|m| m.at != SegmentId(1)));
        assert_eq!(plan.marks[0].at, SegmentId(0), "the tool block is still worth caching");
    }

    #[test]
    fn a_stable_prompt_produces_no_warnings_on_the_second_turn() {
        let turn1 = typical();
        let previous = PreviousPrompt::from_segments(&turn1);
        let plan = CachePlanner::plan(&typical(), &previous, &profiles::opus5());
        assert!(plan.warnings.is_empty(), "{:?}", plan.warnings);
    }

    #[test]
    fn a_new_segment_is_not_churn() {
        // Growing a prompt is normal. Reporting it as churn would train developers to ignore the
        // warning, which is worse than not having it.
        let previous = PreviousPrompt::from_segments(&typical());
        let mut grown = typical();
        grown.push(seg(3, SegmentKind::Discovered, Stability::Static, 300, "a discovered tool"));

        let plan = CachePlanner::plan(&grown, &previous, &profiles::opus5());
        assert!(
            !plan.warnings.iter().any(|w| matches!(w, Warning::CacheChurn { .. })),
            "{:?}",
            plan.warnings
        );
    }

    #[test]
    fn a_prefix_below_the_models_minimum_is_reported_rather_than_silently_wasted() {
        // Haiku 4.5 needs 4,096 tokens. A 500-token prefix is accepted by the API and simply not
        // cached, with no error anywhere.
        let small = vec![
            seg(0, SegmentKind::System, Stability::Static, 380, "short prompt"),
            seg(1, SegmentKind::History, Stability::Volatile, 100, "hi"),
        ];
        let plan = CachePlanner::plan(&small, &PreviousPrompt::none(), &profiles::haiku45());

        assert!(!plan.caches_anything());
        assert!(
            plan.warnings.contains(&Warning::BelowMinPrefix { have: 380, need: 4_096 }),
            "{:?}",
            plan.warnings
        );
    }

    #[test]
    fn the_same_prefix_is_cacheable_on_a_model_with_a_lower_minimum() {
        // The same prompt, a different model: 380 tokens clears Opus 5's 512-token floor? No — it
        // does not, and the planner must say so per model rather than per vendor.
        let small = vec![
            seg(0, SegmentKind::System, Stability::Static, 600, "a longer system prompt"),
            seg(1, SegmentKind::History, Stability::Volatile, 100, "hi"),
        ];
        assert!(
            CachePlanner::plan(&small, &PreviousPrompt::none(), &profiles::opus5())
                .caches_anything()
        );
        assert!(
            !CachePlanner::plan(&small, &PreviousPrompt::none(), &profiles::haiku45())
                .caches_anything()
        );
    }

    #[test]
    fn breakpoints_never_exceed_the_providers_budget() {
        // Four stable runs separated by volatile segments, on a provider allowing one breakpoint.
        let mut segments = Vec::new();
        for i in 0..4u32 {
            segments.push(seg(i * 2, SegmentKind::History, Stability::Static, 2_000, "stable"));
            segments.push(seg(i * 2 + 1, SegmentKind::History, Stability::Volatile, 50, "churn"));
        }
        let plan = CachePlanner::plan(&segments, &PreviousPrompt::none(), &profiles::openai());
        assert_eq!(plan.marks.len(), 1, "one breakpoint on automatic-caching providers");
        assert_eq!(plan.marks[0].at, SegmentId(6), "the one covering the most tokens");
        assert!(plan.warnings.iter().any(|w| matches!(w, Warning::Degraded { .. })));
    }

    #[test]
    fn marks_stay_in_prompt_order_after_being_pruned() {
        let mut segments = Vec::new();
        for i in 0..6u32 {
            segments.push(seg(i * 2, SegmentKind::History, Stability::Static, 1_000, "stable"));
            segments.push(seg(i * 2 + 1, SegmentKind::History, Stability::Volatile, 10, "churn"));
        }
        let plan = CachePlanner::plan(&segments, &PreviousPrompt::none(), &profiles::opus5());
        assert_eq!(plan.marks.len(), 4, "Anthropic allow four");

        let ids: Vec<u32> = plan.marks.iter().map(|m| m.at.index()).collect();
        let mut sorted = ids.clone();
        sorted.sort_unstable();
        assert_eq!(ids, sorted, "an out-of-order mark means nothing to any provider");
    }

    #[test]
    fn long_lived_entries_precede_short_lived_ones() {
        let segments = vec![
            seg(0, SegmentKind::Tools, Stability::Static, 8_000, "tools"),
            seg(1, SegmentKind::History, Stability::Volatile, 50, "churn"),
            seg(2, SegmentKind::History, Stability::Static, 4_000, "settled history"),
            seg(3, SegmentKind::History, Stability::Volatile, 50, "churn"),
        ];
        let plan = CachePlanner::plan(&segments, &PreviousPrompt::none(), &profiles::opus5());
        assert!(ttls_are_correctly_ordered(&plan.marks), "{:?}", plan.marks);
        assert_eq!(plan.marks[0].ttl, CacheTtl::Long, "the tool block outlives the session");
        assert_eq!(plan.marks[1].ttl, CacheTtl::Short, "history does not");
    }

    #[test]
    fn a_provider_without_caching_says_so_once_rather_than_pretending() {
        let plan = CachePlanner::plan(&typical(), &PreviousPrompt::none(), &profiles::no_cache());
        assert!(!plan.caches_anything());
        assert_eq!(plan.warnings.len(), 1);
        assert!(matches!(plan.warnings[0], Warning::Degraded { .. }));
    }

    #[test]
    fn an_entirely_volatile_prompt_caches_nothing_and_explains_why() {
        let segments = vec![
            seg(0, SegmentKind::System, Stability::Volatile, 5_000, "a"),
            seg(1, SegmentKind::History, Stability::Volatile, 5_000, "b"),
        ];
        let plan = CachePlanner::plan(&segments, &PreviousPrompt::none(), &profiles::opus5());
        assert!(!plan.caches_anything());
        assert!(matches!(plan.warnings[0], Warning::Degraded { .. }));
    }

    #[test]
    fn an_empty_prompt_is_not_an_error() {
        let plan = CachePlanner::plan(&[], &PreviousPrompt::none(), &profiles::opus5());
        assert!(!plan.caches_anything());
        assert!(plan.warnings.is_empty(), "nothing to warn about: {:?}", plan.warnings);
    }

    #[test]
    fn planning_is_pure() {
        let segments = typical();
        let previous = PreviousPrompt::from_segments(&segments);
        let caps = profiles::opus5();
        let a = CachePlanner::plan(&segments, &previous, &caps);
        let b = CachePlanner::plan(&segments, &previous, &caps);
        assert_eq!(a, b, "the same inputs must always produce the same plan");
    }
}
