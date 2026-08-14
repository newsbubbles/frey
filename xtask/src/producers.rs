//! The producer lint.
//!
//! **The standing form of the check that found most of this project's bugs.**
//!
//! Thirteen defects over two days shared one shape: a declared capability with no producer on the
//! path that mattered. The type existed. The documentation existed. Consumer-side tests existed.
//! Nothing ever constructed it.
//!
//! The cheapest detector for that class is a public enum variant no code ever builds, and it is
//! cheap enough to run on every push:
//!
//! | Variant | Constructions found by grep |
//! |---|---|
//! | `EventKind::Discovered` | one, in a test |
//! | `RunError::NeedsInput` | zero |
//! | `Item::Discovery` | zero |
//! | `Warning::RouteChanged` | zero, with a `Display` arm and a documentation page |
//!
//! The last one is the reason this is automated rather than remembered. It survived a capability
//! audit whose *entire method* was finding exactly this, because the sweep was run over two enums
//! and not over a third. A partial application of your own method is worse than not having it: the
//! report comes back clean.
//!
//! ## What it is not
//!
//! It is not a dead-code lint. `#[warn(dead_code)]` does not fire on a `pub` variant in a library
//! crate — there is no crate-local caller and there does not need to be, because a downstream user
//! might construct it. That is exactly the blind spot: for a variant Frey *itself* is supposed to
//! emit, "somebody downstream might build it" is not a defence.
//!
//! ## Deliberately crude
//!
//! Text, not syntax. It reads the enum declarations out of the source and greps for constructions.
//! A real answer needs `syn` and a resolver, and would be a worse trade: this runs in under a
//! second with no dependency, and the failure mode of a false positive is one line in `claims.toml`
//! saying why. It admits what it cannot see rather than being trusted further than it should be.

use std::collections::BTreeSet;
use std::path::Path;

/// A variant that is declared and never built.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Orphan {
    /// The enum.
    pub enum_name: String,
    /// The variant.
    pub variant: String,
    /// Where it is declared.
    pub file: String,
}

impl Orphan {
    /// The key used in `claims.toml` to acknowledge one deliberately.
    #[must_use]
    pub fn key(&self) -> String {
        format!("{}::{}", self.enum_name, self.variant)
    }
}

/// Which enums are swept.
///
/// An allowlist rather than everything public, because the claim being enforced is narrow: *Frey
/// emits this*. A `StopReason` variant only a provider can produce, or an error kind a caller
/// raises, is legitimately never constructed here. Sweep the enums that describe what Frey does,
/// and keep the list short enough that adding to it is a decision.
const SWEPT: &[&str] = &[
    "Warning",   // what Frey tells you about a run
    "EventKind", // what Frey says happened
    "RunError",  // how a run can end
    "Item",      // what can be in a prompt or a response
    // Added after the first sweep missed it: `Effect::InputSupplied` is a ninth orphan of exactly
    // the class the other eight acknowledge, and leaving `Effect` out of this list is the same
    // partial application of the method that let `Warning::RouteChanged` survive an audit whose
    // whole purpose was finding variants with no producer.
    "Effect", // what a run did that was not deterministic
];

/// Find every declared-and-never-constructed variant of the swept enums.
///
/// # Errors
/// Returns the first I/O failure. A lint that silently skips an unreadable file reports clean for
/// the wrong reason, which is the failure this whole module exists to prevent.
pub fn sweep(root: &Path) -> Result<Vec<Orphan>, std::io::Error> {
    let sources = rust_sources(&root.join("crates"))?;
    let mut bodies = Vec::with_capacity(sources.len());
    for path in &sources {
        bodies.push((path.clone(), std::fs::read_to_string(path)?));
    }

    let mut orphans = Vec::new();
    for (path, body) in &bodies {
        for (enum_name, variants) in declared_enums(body) {
            if !SWEPT.contains(&enum_name.as_str()) {
                continue;
            }
            for variant in variants {
                if !is_constructed(&bodies, &enum_name, &variant) {
                    orphans.push(Orphan {
                        enum_name: enum_name.clone(),
                        variant,
                        file: relative(root, path),
                    });
                }
            }
        }
    }
    orphans.sort();
    orphans.dedup();
    Ok(orphans)
}

/// Enum name to variant names, for every `pub enum` in one file.
fn declared_enums(body: &str) -> Vec<(String, Vec<String>)> {
    let mut out = Vec::new();
    let mut lines = body.lines().peekable();
    while let Some(line) = lines.next() {
        let trimmed = line.trim_start();
        let Some(rest) = trimmed.strip_prefix("pub enum ") else { continue };
        let Some(name) = rest.split([' ', '{', '<']).next().filter(|n| !n.is_empty()) else {
            continue;
        };

        let mut variants = Vec::new();
        let mut depth = usize::from(line.contains('{'));
        for inner in lines.by_ref() {
            let t = inner.trim();
            // Variants sit at depth one. Anything nested is a field, not a variant.
            if depth == 1
                && let Some(first) = t.chars().next()
                && first.is_ascii_uppercase()
                && let Some(variant) = t.split(['(', '{', ',', ' ']).next()
                && !variant.is_empty()
            {
                variants.push(variant.to_string());
            }
            depth += t.matches('{').count();
            depth = depth.saturating_sub(t.matches('}').count());
            if depth == 0 {
                break;
            }
        }
        out.push((name.to_string(), variants));
    }
    out
}

/// Whether anything outside the declaring file's own `impl` blocks builds this variant.
///
/// Constructions are counted anywhere **except** inside a `#[cfg(test)]` module: a variant built
/// only by a test is precisely the case worth reporting, since the test then proves nothing about
/// whether the framework ever produces it.
fn is_constructed(bodies: &[(std::path::PathBuf, String)], enum_name: &str, variant: &str) -> bool {
    let qualified = format!("{enum_name}::{variant}");
    let shorthand = format!("Self::{variant}");
    for (_, body) in bodies {
        let production = strip_test_modules(body);
        let declares_here = production.contains(&format!("pub enum {enum_name}"));
        for line in production.lines() {
            let trimmed = line.trim_start();
            if trimmed.starts_with("//") || trimmed.starts_with("///") {
                continue;
            }
            // `Self::Variant` counts only in the file that declares the enum, and only where it is
            // being built rather than matched. Matching is consumption, and a variant that is only
            // ever matched on is exactly the orphan being hunted.
            if builds(line, &qualified) || (declares_here && builds(line, &shorthand)) {
                return true;
            }
        }
    }
    false
}

/// Whether this line *builds* the named variant rather than merely mentioning it.
///
/// The distinction the whole lint turns on, and getting it wrong in either direction is expensive:
/// count matches as constructions and the lint reports clean while nothing produces anything; miss
/// a construction inside a match arm's *body* and the lint cries wolf until somebody turns it off.
///
/// Positional, which is the one signal that separates the two reliably: in a pattern the variant
/// sits **left** of the `=>`, and in a construction it sits **right** of it. Everything else —
/// `matches!`, `if let`, a leading `|` — is a pattern context with no arrow to measure against.
fn builds(line: &str, needle: &str) -> bool {
    let Some(at) = line.find(needle) else { return false };
    let trimmed = line.trim_start();

    if trimmed.starts_with('|') || trimmed.starts_with("if let ") || trimmed.contains("matches!(") {
        return false;
    }
    // `Some(Effect::X { .. }) = &entry.effect` and friends: a destructuring `let`, not a build.
    if trimmed.starts_with("let ") && line.contains(" = &") {
        return false;
    }
    match line.find("=>") {
        // Left of the arrow is the pattern; right of it is the arm's body, where a build is a build.
        Some(arrow) => at > arrow,
        None => true,
    }
}

/// A file with every `#[cfg(test)]` module removed.
fn strip_test_modules(body: &str) -> String {
    let mut out = String::with_capacity(body.len());
    let mut lines = body.lines().peekable();
    while let Some(line) = lines.next() {
        if line.trim_start().starts_with("#[cfg(test)]") {
            // Skip forward to the end of the item this attribute decorates.
            let mut depth = 0usize;
            let mut entered = false;
            for inner in lines.by_ref() {
                depth += inner.matches('{').count();
                if depth > 0 {
                    entered = true;
                }
                depth = depth.saturating_sub(inner.matches('}').count());
                if entered && depth == 0 {
                    break;
                }
            }
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }
    out
}

fn rust_sources(dir: &Path) -> Result<Vec<std::path::PathBuf>, std::io::Error> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(current) = stack.pop() {
        if !current.exists() {
            continue;
        }
        for entry in std::fs::read_dir(&current)? {
            let path = entry?.path();
            if path.is_dir() {
                if path.file_name().is_some_and(|n| n == "target") {
                    continue;
                }
                stack.push(path);
            } else if path.extension().is_some_and(|e| e == "rs") {
                out.push(path);
            }
        }
    }
    out.sort();
    Ok(out)
}

fn relative(root: &Path, path: &Path) -> String {
    path.strip_prefix(root).unwrap_or(path).display().to_string().replace('\\', "/")
}

/// Report orphans against the set acknowledged in `claims.toml`.
///
/// Returns the ones that are **not** acknowledged. An orphan with a row saying why it exists and
/// what would settle it is a known gap; an orphan with no row is a capability nobody noticed was
/// fiction.
#[must_use]
pub fn unacknowledged(orphans: &[Orphan], acknowledged: &BTreeSet<String>) -> Vec<Orphan> {
    orphans.iter().filter(|o| !acknowledged.contains(&o.key())).cloned().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_variant_only_a_test_builds_is_reported() {
        let source = r"
pub enum Warning {
    Real,
    Fictional,
}
#[cfg(test)]
mod tests {
    fn x() { let _ = Warning::Fictional; }
}
";
        let bodies = vec![(std::path::PathBuf::from("a"), source.to_string())];
        assert!(!is_constructed(&bodies, "Warning", "Fictional"));
    }

    #[test]
    fn a_variant_that_is_only_matched_on_is_reported() {
        // The `RouteChanged` shape exactly: a `Display` arm, a doc page, and nothing that builds it.
        let source = r"
pub enum Warning {
    RouteChanged,
}
impl std::fmt::Display for Warning {
    fn fmt(&self, f: &mut Formatter) -> Result {
        match self {
            Self::RouteChanged => write!(f, 'the router moved this call'),
        }
    }
}
";
        let bodies = vec![(std::path::PathBuf::from("a"), source.to_string())];
        assert!(!is_constructed(&bodies, "Warning", "RouteChanged"));
    }

    #[test]
    fn a_construction_in_the_body_of_a_match_arm_counts() {
        // The false positive that would have made this lint useless on its first run: `run.rs`
        // builds `EventKind::ToolCallFinished` inside a match arm, and a rule that skipped any line
        // containing `=>` reported the loop's own busiest event as never produced.
        assert!(builds(
            "            _ => EventKind::ToolCallFinished {",
            "EventKind::ToolCallFinished"
        ));
        assert!(!builds(
            "            Self::RouteChanged { from, to } => write!(",
            "Self::RouteChanged"
        ));
    }

    #[test]
    fn a_variant_something_actually_builds_is_not_reported() {
        let declaring = "pub enum Warning {\n    Churn,\n}\n";
        let user = "fn f() { warnings.push(Warning::Churn); }\n";
        let bodies = vec![
            (std::path::PathBuf::from("a"), declaring.to_string()),
            (std::path::PathBuf::from("b"), user.to_string()),
        ];
        assert!(is_constructed(&bodies, "Warning", "Churn"));
    }

    #[test]
    fn variants_are_read_out_of_a_declaration_with_fields() {
        let source = "pub enum E {\n    A { x: u32 },\n    B(String),\n    C,\n}\n";
        let found = declared_enums(source);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].1, vec!["A", "B", "C"]);
    }
}
