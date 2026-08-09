//! What Frey does that a string-concatenating framework cannot.
//!
//! Run with `cargo run -p frey --example cache_planning`.
//!
//! Two turns of an ordinary agent, once with a stable system prompt and once with a clock in it.
//! The second costs about eleven times as much on Anthropic pricing, no provider reports an error,
//! and the only symptom is the bill at the end of the month. Frey names the segment and says why.

use frey::prelude::*;
use frey_core::ids::SegmentId;
use frey_core::segment::{Segment, SegmentKind, Stability};

fn segment(id: u32, kind: SegmentKind, stability: Stability, tokens: u32, body: &str) -> Segment {
    Segment {
        id: SegmentId(id),
        kind,
        stability,
        hash: hash_text(body),
        est_tokens: tokens,
        label: format!("{kind:?}").to_lowercase().into(),
    }
}

/// A realistic prompt: a large tool block, a system prompt, and one turn of conversation.
fn prompt(system: &str) -> Vec<Segment> {
    vec![
        segment(0, SegmentKind::Tools, Stability::Static, 12_000, "42 tool definitions"),
        segment(1, SegmentKind::System, Stability::Static, 800, system),
        segment(2, SegmentKind::History, Stability::Volatile, 240, "what changed this week?"),
    ]
}

fn main() {
    let caps = frey::profiles::opus5();

    println!("== a stable system prompt ==");
    let turn1 = prompt("You are a careful assistant.");
    let previous = PreviousPrompt::from_segments(&turn1);
    let plan = CachePlanner::plan(&prompt("You are a careful assistant."), &previous, &caps);

    println!("  breakpoint after segment: {:?}", plan.marks.first().map(|m| m.at.index()));
    println!("  cached prefix: {} tokens", plan.cached_prefix_tokens);
    println!("  warnings: {}", plan.warnings.len());

    println!("\n== the same prompt, with a clock in it ==");
    let churned = CachePlanner::plan(
        &prompt("You are a careful assistant. The time is 14:32:06."),
        &previous,
        &caps,
    );

    println!("  cached prefix: {} tokens", churned.cached_prefix_tokens);
    for warning in &churned.warnings {
        if let Warning::CacheChurn { segment, tokens, advice } = warning {
            println!("  churn in `{segment}`: {tokens} tokens rewritten every turn");
            println!("    {advice}");
        }
    }

    let lost = plan.cached_prefix_tokens - churned.cached_prefix_tokens;
    println!("\n  {lost} tokens dropped out of the cached prefix.");
    println!(
        "  On Anthropic pricing a cache read costs a tenth of an input token, so those tokens now\n  \
         cost roughly ten times what they did. No provider reports this; the bill is the only sign."
    );

    println!("\n== the same prompt on a model with a higher minimum ==");
    // Haiku 4.5 caches from 4,096 tokens rather than 512. A prompt that caches well on one model
    // can silently not cache at all on another from the same vendor.
    let small = vec![
        segment(0, SegmentKind::System, Stability::Static, 600, "a short system prompt"),
        segment(1, SegmentKind::History, Stability::Volatile, 100, "hello"),
    ];
    for (name, caps) in [("opus5", frey::profiles::opus5()), ("haiku45", frey::profiles::haiku45())]
    {
        let plan = CachePlanner::plan(&small, &PreviousPrompt::none(), &caps);
        println!(
            "  {name:<8} caches: {:<5}  (minimum prefix {:?})",
            plan.caches_anything(),
            caps.cache.min_prefix_tokens()
        );
    }
}
