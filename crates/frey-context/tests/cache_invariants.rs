//! Property tests for the cache planner.
//!
//! The unit tests in `cache.rs` check specific situations. These check that the provider rules hold
//! for *every* prompt shape against *every* provider profile — which is the claim the framework
//! actually makes, and one that examples cannot establish.
//!
//! Each invariant below is a way to lose money or get a 400 from a provider, so each is stated as a
//! property rather than demonstrated once.

use frey_context::cache::{CachePlanner, PreviousPrompt};
use frey_context::hash::hash_text;
use frey_context::profiles;
use frey_core::ids::SegmentId;
use frey_core::provider_caps::ProviderCapabilities;
use frey_core::segment::{CacheTtl, Segment, SegmentKind, Stability, ttls_are_correctly_ordered};
use proptest::prelude::*;

fn any_kind() -> impl Strategy<Value = SegmentKind> {
    prop_oneof![
        Just(SegmentKind::Tools),
        Just(SegmentKind::System),
        Just(SegmentKind::SkillIndex),
        Just(SegmentKind::History),
        Just(SegmentKind::Discovered),
    ]
}

fn any_stability() -> impl Strategy<Value = Stability> {
    prop_oneof![Just(Stability::Static), Just(Stability::Slow), Just(Stability::Volatile)]
}

/// A prompt of up to twenty segments, with token counts spanning the interesting range: below every
/// minimum prefix, between them, and far above.
fn any_prompt() -> impl Strategy<Value = Vec<Segment>> {
    prop::collection::vec((any_kind(), any_stability(), 0u32..8_000, ".{0,12}"), 0..20).prop_map(
        |raw| {
            raw.into_iter()
                .enumerate()
                .map(|(i, (kind, stability, est_tokens, body))| Segment {
                    id: SegmentId(u32::try_from(i).expect("fewer than 20 segments")),
                    kind,
                    stability,
                    hash: hash_text(&body),
                    est_tokens,
                    label: format!("{kind:?}:{i}").into(),
                })
                .collect()
        },
    )
}

/// A previous turn built by perturbing some segments, so churn detection is exercised rather than
/// assumed away.
fn perturb(segments: &[Segment], mask: u32) -> PreviousPrompt {
    let perturbed: Vec<Segment> = segments
        .iter()
        .enumerate()
        .map(|(i, s)| {
            let mut s = s.clone();
            if mask >> (i % 32) & 1 == 1 {
                s.hash = hash_text(&format!("changed-{i}"));
            }
            s
        })
        .collect();
    PreviousPrompt::from_segments(&perturbed)
}

fn all_profiles() -> Vec<(&'static str, ProviderCapabilities)> {
    profiles::all()
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    /// Every rule that a provider would reject the request for, or silently charge extra for.
    #[test]
    fn provider_rules_hold_for_every_prompt_and_profile(
        segments in any_prompt(),
        mask in any::<u32>(),
    ) {
        let previous = perturb(&segments, mask);

        for (name, caps) in all_profiles() {
            let plan = CachePlanner::plan(&segments, &previous, &caps);
            let budget = caps.cache.breakpoint_budget() as usize;

            prop_assert!(
                plan.marks.len() <= budget,
                "{name}: {} marks exceeds the budget of {budget}",
                plan.marks.len()
            );

            prop_assert!(
                ttls_are_correctly_ordered(&plan.marks),
                "{name}: long-lived entries must precede short-lived ones, got {:?}",
                plan.marks
            );

            let ids: Vec<u32> = plan.marks.iter().map(|m| m.at.index()).collect();
            let mut sorted = ids.clone();
            sorted.sort_unstable();
            sorted.dedup();
            prop_assert_eq!(&ids, &sorted, "{}: marks must be ascending and unique", name);

            for mark in &plan.marks {
                let segment = segments
                    .iter()
                    .find(|s| s.id == mark.at)
                    .expect("a mark must name a segment in this prompt");

                prop_assert!(
                    segment.stability.is_cacheable(),
                    "{name}: a breakpoint on a volatile segment rewrites the cache every turn"
                );
                prop_assert!(
                    !previous.changed(segment),
                    "{name}: a breakpoint on a segment that changed since last turn is money burnt"
                );
                if !caps.cache.supports_ttl(CacheTtl::Long) {
                    prop_assert_eq!(
                        mark.ttl,
                        CacheTtl::Short,
                        "{}: asked for a lifetime this provider does not offer",
                        name
                    );
                }
            }

            if let (Some(min), true) = (caps.cache.min_prefix_tokens(), plan.caches_anything()) {
                prop_assert!(
                    plan.cached_prefix_tokens >= min,
                    "{name}: cached {} tokens, below the {min}-token minimum, so the provider \
                     would silently not cache it",
                    plan.cached_prefix_tokens
                );
            }
        }
    }

    /// A provider that cannot cache must never be sent a mark, whatever the prompt looks like.
    #[test]
    fn a_provider_without_caching_is_never_sent_a_breakpoint(
        segments in any_prompt(),
        mask in any::<u32>(),
    ) {
        let plan = CachePlanner::plan(&segments, &perturb(&segments, mask), &profiles::no_cache());
        prop_assert!(plan.marks.is_empty());
    }

    /// Planning is pure. Replay depends on it: the same journal must produce the same plan.
    #[test]
    fn planning_is_deterministic(segments in any_prompt(), mask in any::<u32>()) {
        let previous = perturb(&segments, mask);
        for (name, caps) in all_profiles() {
            let a = CachePlanner::plan(&segments, &previous, &caps);
            let b = CachePlanner::plan(&segments, &previous, &caps);
            prop_assert_eq!(a, b, "{} produced two different plans for one input", name);
        }
    }

    /// Silence means nothing was given up. Any plan that caches less than it could must say why,
    /// or a developer has no way to discover a prompt that quietly costs ten times what it should.
    #[test]
    fn caching_nothing_is_always_explained(segments in any_prompt(), mask in any::<u32>()) {
        prop_assume!(!segments.is_empty());
        let previous = perturb(&segments, mask);

        for (name, caps) in all_profiles() {
            let plan = CachePlanner::plan(&segments, &previous, &caps);
            if !plan.is_cached() {
                prop_assert!(
                    !plan.warnings.is_empty(),
                    "{name}: nothing is cached, by Frey or by the provider, and nothing said so"
                );
            }
        }
    }
}
