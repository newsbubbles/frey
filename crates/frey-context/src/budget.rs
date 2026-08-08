//! Fitting a prompt into the window.
//!
//! The rule that matters is not *how* eviction works but that it is **never silent**. A framework
//! that quietly drops the middle of a conversation produces bugs nobody can reproduce, because the
//! symptom (the model forgot something) appears far from the cause (a budget decision three turns
//! ago). Every decision here emits a [`Warning`] naming what went and why.
//!
//! Eviction order is fixed and boring, cheapest first:
//!
//! 1. definitions discovered earlier and not used since — free to re-discover;
//! 2. the oldest history above the floor — recoverable by summarising;
//! 3. refuse, naming what could not fit.
//!
//! Summarising and eliding oversized tool results happen a layer up, where there is something that
//! can call a model. Here everything is arithmetic on segments.

use frey_core::event::Warning;
use frey_core::ids::SegmentId;
use frey_core::provider_caps::ProviderCapabilities;
use frey_core::segment::{Segment, SegmentKind};

/// Minimum allocations that eviction may not touch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Floors {
    /// How many of the most recent history segments always survive. Dropping the last exchange
    /// makes an agent visibly incoherent, so this is a floor rather than a preference.
    pub recent_history: usize,
}

impl Default for Floors {
    fn default() -> Self {
        Self { recent_history: 4 }
    }
}

/// How much room the prompt has, and what may not be given up to get it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ContextBudget {
    /// The model's context window, in tokens.
    pub window: u32,
    /// Tokens set aside for the response.
    pub reserve_output: u32,
    /// Slack for tool results and definitions discovered mid-turn. Without this, a run fits
    /// perfectly right up to the moment the model calls a tool.
    pub reserve_headroom: u32,
    /// What eviction may not touch.
    pub floors: Floors,
}

impl ContextBudget {
    /// A budget derived from what the provider says it can do.
    #[must_use]
    pub fn from_capabilities(caps: &ProviderCapabilities) -> Self {
        let reserve_output = caps.max_output.min(caps.max_context / 4);
        Self {
            window: caps.max_context,
            reserve_output,
            reserve_headroom: caps.max_context / 10,
            floors: Floors::default(),
        }
    }

    /// The tokens available to the prompt itself.
    #[must_use]
    pub fn prompt_ceiling(&self) -> u32 {
        self.window.saturating_sub(self.reserve_output).saturating_sub(self.reserve_headroom)
    }
}

/// Why a segment was dropped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvictionReason {
    /// A definition discovered earlier and not used since. Cheapest to lose: it can be found again.
    StaleDiscovery,
    /// History above the floor. Recoverable by summarising, a layer up.
    OldHistory,
}

/// One segment that did not make it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Eviction {
    /// Which segment.
    pub id: SegmentId,
    /// Its label, so a warning can name it.
    pub label: smol_str::SmolStr,
    /// How many tokens it freed.
    pub tokens: u32,
    /// Why it went.
    pub reason: EvictionReason,
}

/// What fitting the prompt cost.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BudgetPlan {
    /// The segments that survived, in prompt order.
    pub keep: Vec<Segment>,
    /// What was dropped, in the order it was dropped.
    pub evicted: Vec<Eviction>,
    /// Tokens the surviving prompt occupies.
    pub tokens: u32,
    /// Diagnostics.
    pub warnings: Vec<Warning>,
}

/// The prompt cannot be made to fit.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error(
    "the prompt needs {needed} tokens but only {available} are available, and everything left is \
     protected: {protected}. Raise the window, lower reserve_output, or shrink the tool block."
)]
pub struct DoesNotFit {
    /// What the smallest possible prompt still needs.
    pub needed: u32,
    /// What the budget allows.
    pub available: u32,
    /// What could not be given up, as a human-readable list.
    pub protected: String,
}

/// Fits prompts into budgets.
#[derive(Debug, Clone, Copy, Default)]
pub struct Budgeter;

impl Budgeter {
    /// Drop the cheapest things until the prompt fits.
    ///
    /// # Errors
    /// Returns [`DoesNotFit`] when everything remaining is protected — with the numbers, rather
    /// than truncating something and hoping nobody notices.
    pub fn fit(segments: &[Segment], budget: &ContextBudget) -> Result<BudgetPlan, DoesNotFit> {
        let ceiling = budget.prompt_ceiling();
        let mut keep: Vec<Segment> = segments.to_vec();
        let mut evicted = Vec::new();
        let mut warnings = Vec::new();

        let mut total: u32 = keep.iter().map(|s| s.est_tokens).sum();
        if total <= ceiling {
            return Ok(BudgetPlan { keep, evicted, tokens: total, warnings });
        }

        // 1. Discovered definitions. Free to lose: the model can search again.
        evict_while(
            &mut keep,
            &mut evicted,
            &mut total,
            ceiling,
            EvictionReason::StaleDiscovery,
            |s| s.kind == SegmentKind::Discovered,
        );

        // 2. History above the floor, oldest first.
        if total > ceiling {
            let history_ids: Vec<SegmentId> =
                keep.iter().filter(|s| s.kind == SegmentKind::History).map(|s| s.id).collect();
            let droppable: Vec<SegmentId> = history_ids
                .iter()
                .take(history_ids.len().saturating_sub(budget.floors.recent_history))
                .copied()
                .collect();
            evict_while(
                &mut keep,
                &mut evicted,
                &mut total,
                ceiling,
                EvictionReason::OldHistory,
                |s| droppable.contains(&s.id),
            );
        }

        if total > ceiling {
            let protected = keep
                .iter()
                .map(|s| format!("{} ({} tokens)", s.label, s.est_tokens))
                .collect::<Vec<_>>()
                .join(", ");
            return Err(DoesNotFit { needed: total, available: ceiling, protected });
        }

        if !evicted.is_empty() {
            let freed: u32 = evicted.iter().map(|e| e.tokens).sum();
            let used_percent = percent(total, ceiling);
            warnings.push(Warning::BudgetPressure {
                used_percent,
                action: format!(
                    "evicted {} segment(s) freeing {freed} tokens: {}",
                    evicted.len(),
                    evicted.iter().map(|e| e.label.as_str()).collect::<Vec<_>>().join(", ")
                )
                .into(),
            });
        }

        Ok(BudgetPlan { keep, evicted, tokens: total, warnings })
    }
}

fn evict_while(
    keep: &mut Vec<Segment>,
    evicted: &mut Vec<Eviction>,
    total: &mut u32,
    ceiling: u32,
    reason: EvictionReason,
    mut eligible: impl FnMut(&Segment) -> bool,
) {
    let mut i = 0;
    while i < keep.len() && *total > ceiling {
        if eligible(&keep[i]) {
            let segment = keep.remove(i);
            *total = total.saturating_sub(segment.est_tokens);
            evicted.push(Eviction {
                id: segment.id,
                label: segment.label,
                tokens: segment.est_tokens,
                reason,
            });
        } else {
            i += 1;
        }
    }
}

fn percent(used: u32, of: u32) -> u8 {
    if of == 0 {
        return 100;
    }
    u8::try_from((u64::from(used) * 100 / u64::from(of)).min(100)).unwrap_or(100)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hash::hash_text;
    use crate::profiles;
    use frey_core::segment::Stability;

    fn seg(id: u32, kind: SegmentKind, tokens: u32) -> Segment {
        Segment {
            id: SegmentId(id),
            kind,
            stability: Stability::Static,
            hash: hash_text(&format!("{id}")),
            est_tokens: tokens,
            label: format!("{kind:?}:{id}").into(),
        }
    }

    fn budget(window: u32) -> ContextBudget {
        ContextBudget {
            window,
            reserve_output: window / 10,
            reserve_headroom: 0,
            floors: Floors { recent_history: 2 },
        }
    }

    #[test]
    fn a_prompt_that_fits_is_left_alone() {
        let segments = vec![seg(0, SegmentKind::Tools, 100), seg(1, SegmentKind::History, 100)];
        let plan = Budgeter::fit(&segments, &budget(10_000)).unwrap();
        assert_eq!(plan.keep.len(), 2);
        assert!(plan.evicted.is_empty());
        assert!(plan.warnings.is_empty(), "no decision means no noise");
    }

    #[test]
    fn discovered_definitions_go_before_history_does() {
        let segments = vec![
            seg(0, SegmentKind::Tools, 400),
            seg(1, SegmentKind::History, 200),
            seg(2, SegmentKind::History, 200),
            seg(3, SegmentKind::Discovered, 300),
        ];
        // Ceiling is 900; the prompt is 1,100. Dropping the discovery alone is enough.
        let plan = Budgeter::fit(&segments, &budget(1_000)).unwrap();
        assert_eq!(plan.evicted.len(), 1);
        assert_eq!(plan.evicted[0].reason, EvictionReason::StaleDiscovery);
        assert_eq!(plan.evicted[0].id, SegmentId(3));
        assert!(plan.keep.iter().all(|s| s.kind != SegmentKind::Discovered));
    }

    #[test]
    fn history_is_evicted_oldest_first_and_the_floor_holds() {
        let segments = vec![
            seg(0, SegmentKind::Tools, 300),
            seg(1, SegmentKind::History, 300),
            seg(2, SegmentKind::History, 300),
            seg(3, SegmentKind::History, 300),
            seg(4, SegmentKind::History, 300),
        ];
        // Ceiling 900, prompt 1,500: two history segments must go, and the floor of two protects
        // the most recent pair.
        let plan = Budgeter::fit(&segments, &budget(1_000)).unwrap();
        assert_eq!(plan.evicted.len(), 2);
        assert_eq!(
            plan.evicted.iter().map(|e| e.id).collect::<Vec<_>>(),
            vec![SegmentId(1), SegmentId(2)],
            "oldest first"
        );
        assert!(
            plan.keep.iter().any(|s| s.id == SegmentId(4)),
            "the most recent exchange survives, or the agent looks amnesiac"
        );
    }

    #[test]
    fn eviction_is_never_silent() {
        let segments =
            vec![seg(0, SegmentKind::Discovered, 2_000), seg(1, SegmentKind::Tools, 100)];
        let plan = Budgeter::fit(&segments, &budget(1_000)).unwrap();
        let Warning::BudgetPressure { action, .. } = &plan.warnings[0] else {
            panic!("expected budget pressure, got {:?}", plan.warnings)
        };
        assert!(action.contains("Discovered:0"), "the warning must name what went: {action}");
        assert!(action.contains("2000"), "and what it saved");
    }

    #[test]
    fn an_impossible_prompt_fails_with_numbers_rather_than_truncating() {
        // A tool block alone that exceeds the window. Truncating it would produce a model that
        // calls tools it can no longer see the definitions for.
        let segments = vec![seg(0, SegmentKind::Tools, 50_000)];
        let err = Budgeter::fit(&segments, &budget(1_000)).unwrap_err();
        assert_eq!(err.needed, 50_000);
        assert_eq!(err.available, 900);
        assert!(err.protected.contains("Tools:0"));
        assert!(format!("{err}").contains("shrink the tool block"), "and how to fix it");
    }

    #[test]
    fn the_budget_leaves_room_for_the_response_and_for_tool_results() {
        let caps = profiles::opus5();
        let budget = ContextBudget::from_capabilities(&caps);
        assert_eq!(budget.window, 200_000);
        assert_eq!(budget.reserve_output, 50_000, "capped at a quarter of the window");
        assert_eq!(budget.reserve_headroom, 20_000);
        assert_eq!(budget.prompt_ceiling(), 130_000);
    }

    #[test]
    fn the_ceiling_is_never_exceeded_for_any_prompt_that_fits_at_all() {
        // A cheap stand-in for the property test: many shapes, one invariant.
        for tools in [0u32, 500, 5_000] {
            for history_count in 0..8u32 {
                let mut segments = vec![seg(0, SegmentKind::Tools, tools)];
                for i in 0..history_count {
                    segments.push(seg(i + 1, SegmentKind::History, 400));
                }
                if let Ok(plan) = Budgeter::fit(&segments, &budget(10_000)) {
                    assert!(
                        plan.tokens <= budget(10_000).prompt_ceiling(),
                        "tools={tools} history={history_count} produced {} tokens",
                        plan.tokens
                    );
                }
            }
        }
    }
}
