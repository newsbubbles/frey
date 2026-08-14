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

        // 0. A provider that caches nothing. Churn costs nothing here — every turn pays full price
        //    whatever the prompt does — so this is the one shape where the diagnostics below are
        //    noise, and it returns before them.
        if matches!(caps.cache, CacheSupport::None) {
            if !segments.is_empty() {
                warnings.push(Warning::Degraded {
                    capability: "prompt-cache".into(),
                    fallback: "this provider does not cache prompts; every turn pays full price"
                        .into(),
                });
            }
            return CachePlan { warnings, ..CachePlan::empty() };
        }

        // 1. Churn. A segment that claims to be stable but changed is the expensive case: a
        //    breakpoint after it would rewrite the whole prefix every turn. Warn, and treat it as
        //    volatile for the rest of this plan.
        //
        //    This runs before the breakpoint budget is consulted, deliberately. Churn is a property
        //    of the *prompt*, not of who places the breakpoints: on a provider that caches
        //    automatically the rewritten prefix is just as expensive, and there is not even a
        //    breakpoint to move out of the way. An earlier version returned above this loop
        //    whenever the budget was zero, which made this warning — the headline one, the one the
        //    README opens with — structurally unreachable on every automatic-caching provider.
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

        // 1b. The provider caches on its own and accepts no marks. There is no breakpoint to place,
        //     but the prefix can still be too short to be cached at all — silently, as ever. What
        //     matters here is the *leading* stable run, because automatic caching matches the
        //     longest common prefix: a stable segment sitting after a volatile one is not part of
        //     any prefix the provider can reuse.
        if budget == 0 {
            let leading = leading_stable_tokens(&effective);
            let min_prefix = caps.cache.min_prefix_tokens().unwrap_or(0);
            if !segments.is_empty() {
                if leading == 0 {
                    warnings.push(Warning::Degraded {
                        capability: "prompt-cache".into(),
                        fallback: "no segment was stable enough to cache behind".into(),
                    });
                } else if leading < min_prefix {
                    warnings.push(Warning::BelowMinPrefix { have: leading, need: min_prefix });
                }
            }
            return CachePlan {
                warnings,
                provider_caches_automatically: automatic,
                ..CachePlan::empty()
            };
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

/// How far back a provider searches from a breakpoint for a previously written cache entry.
///
/// Anthropic's documented figure. It matters because exceeding it fails *silently*: a turn that
/// adds more blocks than this makes the next request miss the cache with no error anywhere.
pub const LOOKBACK_BLOCKS: u32 = 20;

/// Check whether a turn added more blocks than the provider will look back through.
///
/// This is a different failure from churn and needs its own check. Churn is a segment changing;
/// this is a segment staying identical while the *distance* to it grows past what the provider
/// searches. A long agentic turn — several tool calls and their results — reaches it easily, and the
/// symptom is only ever the bill.
///
/// The fix is an intermediate breakpoint every fifteen blocks or so, which is why the warning says
/// that rather than merely reporting the number.
#[must_use]
pub fn check_lookback(blocks_added_this_turn: u32) -> Option<Warning> {
    if blocks_added_this_turn <= LOOKBACK_BLOCKS {
        return None;
    }
    Some(Warning::LookbackExceeded { blocks: blocks_added_this_turn, limit: LOOKBACK_BLOCKS })
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

/// Tokens in the leading cacheable run — the longest prefix an automatic cache could reuse.
///
/// Stops at the first segment that is not cacheable, which is the whole point: a provider matching
/// the longest common prefix cannot skip over a churning segment to reach a stable one behind it.
fn leading_stable_tokens(effective: &[(&Segment, Stability)]) -> u32 {
    let mut total = 0u32;
    for (segment, stability) in effective {
        if !stability.is_cacheable() {
            break;
        }
        total = total.saturating_add(segment.est_tokens);
    }
    total
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
        // A one-breakpoint provider, built here rather than borrowed from `profiles`: no shipped
        // profile has this budget any more, and a test that silently starts exercising a budget of
        // four is a test that stopped testing pruning.
        let one_breakpoint = ProviderCapabilities {
            cache: CacheSupport::Explicit {
                max_breakpoints: 1,
                ttls: vec![CacheTtl::Short],
                min_prefix_tokens: 1_024,
            },
            ..profiles::opus5()
        };
        let plan = CachePlanner::plan(&segments, &PreviousPrompt::none(), &one_breakpoint);
        assert_eq!(plan.marks.len(), 1, "the budget is one and the plan wanted four");
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
    fn a_turn_longer_than_the_providers_lookback_is_reported() {
        // A different failure from churn, and one the adversarial re-check of the wedge turned up
        // after the planner was written. The segment does not change; the distance to it grows past
        // what the provider searches, and the next request misses cache with no error anywhere.
        assert_eq!(check_lookback(5), None);
        assert_eq!(check_lookback(LOOKBACK_BLOCKS), None, "exactly at the limit still hits");

        let warning = check_lookback(31).expect("past the limit must be reported");
        let Warning::LookbackExceeded { blocks, limit } = warning else { panic!("wrong warning") };
        assert_eq!(blocks, 31);
        assert_eq!(limit, 20);
    }

    #[test]
    fn churn_is_reported_on_a_provider_that_caches_automatically() {
        // The regression this test exists for: OpenRouter declares
        // `Automatic { explicit_available: false }`, whose breakpoint budget is zero, and the
        // planner used to return before churn detection whenever the budget was zero. Every
        // OpenRouter session ever run therefore had `CacheChurn` structurally unreachable — on the
        // one dialect this project's only real caller uses.
        //
        // Churn does not need a breakpoint budget. The prefix is rewritten and paid for either way.
        let turn1 = typical();
        let previous = PreviousPrompt::from_segments(&turn1);

        let mut turn2 = typical();
        turn2[1].hash = hash_text("you are a careful assistant. The time is 14:32:06.");

        let plan = CachePlanner::plan(&turn2, &previous, &profiles::openrouter_automatic());

        assert_eq!(plan.marks, vec![], "there is still nothing to place");
        assert!(plan.provider_caches_automatically);
        let Some(Warning::CacheChurn { segment, tokens, .. }) =
            plan.warnings.iter().find(|w| matches!(w, Warning::CacheChurn { .. })).cloned()
        else {
            panic!("churn must be reported without a breakpoint budget: {:?}", plan.warnings)
        };
        assert_eq!(segment.as_str(), "System:1");
        assert_eq!(tokens, 800);
    }

    #[test]
    fn a_prefix_below_an_automatic_providers_threshold_is_reported() {
        // Automatic caching is not free of the minimum: below the threshold the provider caches
        // nothing and says nothing, which is the same silent loss as on the explicit path.
        let small = vec![
            seg(0, SegmentKind::System, Stability::Static, 300, "short prompt"),
            seg(1, SegmentKind::History, Stability::Volatile, 100, "hi"),
        ];
        let plan =
            CachePlanner::plan(&small, &PreviousPrompt::none(), &profiles::openrouter_automatic());
        assert!(
            plan.warnings.contains(&Warning::BelowMinPrefix { have: 300, need: 1_024 }),
            "{:?}",
            plan.warnings
        );
    }

    #[test]
    fn an_automatic_provider_above_the_threshold_is_quiet() {
        let plan = CachePlanner::plan(
            &typical(),
            &PreviousPrompt::none(),
            &profiles::openrouter_automatic(),
        );
        assert!(plan.warnings.is_empty(), "{:?}", plan.warnings);
        assert!(plan.is_cached(), "the provider caches it even though Frey placed nothing");
    }

    #[test]
    fn a_stable_run_behind_a_volatile_segment_does_not_count_towards_an_automatic_prefix() {
        // The distinction that makes the automatic path different from the explicit one: a
        // provider matching the longest common prefix cannot skip a churning segment to reach the
        // stable content behind it, so 8,000 stable tokens sitting after a clock buy nothing.
        let segments = vec![
            seg(0, SegmentKind::System, Stability::Volatile, 50, "the time is 14:32:06"),
            seg(1, SegmentKind::History, Stability::Static, 8_000, "settled history"),
        ];
        let plan = CachePlanner::plan(
            &segments,
            &PreviousPrompt::none(),
            &profiles::openrouter_automatic(),
        );
        assert!(
            plan.warnings.iter().any(|w| matches!(w, Warning::Degraded { .. })),
            "nothing cacheable leads the prompt: {:?}",
            plan.warnings
        );
    }

    #[test]
    fn churn_is_not_reported_when_the_provider_caches_nothing() {
        // The one shape where churn genuinely costs nothing extra. Reporting it would train the
        // reader to ignore the warning that matters elsewhere.
        let turn1 = typical();
        let previous = PreviousPrompt::from_segments(&turn1);
        let mut turn2 = typical();
        turn2[1].hash = hash_text("changed");

        let plan = CachePlanner::plan(&turn2, &previous, &profiles::no_cache());
        assert_eq!(plan.warnings.len(), 1);
        assert!(matches!(plan.warnings[0], Warning::Degraded { .. }));
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
