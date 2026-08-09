//! Skills, and the ladder that keeps them cheap.
//!
//! A skill is a directory with a `SKILL.md` at its root, following the open Agent Skills format.
//! The mechanism that makes it worth having is **progressive disclosure**: at startup an agent
//! loads only each skill's name and description — roughly a hundred tokens — and reads the full
//! instructions only when a task matches. Referenced files and bundled scripts load later still.
//!
//! That is the same mechanism as deferred tool loading at a different altitude, which is why skills
//! share the selector, the budget and the search index rather than getting their own.
//!
//! # Skills are a trust boundary
//!
//! A `SKILL.md` is text someone else wrote, and its `scripts/` are code someone else wrote. Supply
//! chain attacks on skill registries are published literature, not a hypothetical. So a skill from
//! outside an operator-declared trusted root reaches a prompt as low-integrity data, and **a skill
//! cannot grant itself capabilities** — it can only request them, and a request surfaces at install
//! time rather than mid-run.

use frey_core::ids::SkillId;
use frey_core::taint::{Provenance, Tainted, Untrusted};
use smol_str::SmolStr;

/// How much of a skill is currently loaded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Rung {
    /// Name and description only. What every skill costs at startup.
    Index,
    /// The full `SKILL.md` body.
    Instructions,
    /// A referenced file, read on demand.
    Reference,
}

/// The token budget the format recommends for a skill's full body.
///
/// Not enforced as a hard limit — a skill that exceeds it still works — but reported, because a
/// skill that costs as much as the conversation defeats the point of the ladder.
pub const RECOMMENDED_BODY_TOKENS: u32 = 5_000;

/// A skill's front matter: what the index rung costs.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SkillIndexEntry {
    /// Its identifier.
    pub id: SkillId,
    /// Short name.
    pub name: String,
    /// When to use it. This is what the selector matches on, so it is the only text that must be
    /// good.
    pub description: String,
    /// Roughly what loading the full body would cost.
    pub body_tokens: u32,
    /// Whether it came from a trusted root.
    pub trusted: bool,
}

impl SkillIndexEntry {
    /// Roughly what this entry costs at the index rung.
    #[must_use]
    pub fn index_tokens(&self) -> u32 {
        u32::try_from((self.name.len() + self.description.len()).div_ceil(4)).unwrap_or(u32::MAX)
    }

    /// Text a search should index for this skill.
    #[must_use]
    pub fn searchable_text(&self) -> String {
        format!("{} {}", self.name, self.description)
    }
}

/// A skill was not usable.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum SkillError {
    /// The file had no front matter.
    #[error("`{path}` has no YAML front matter; a skill needs at least a name and a description")]
    NoFrontMatter {
        /// Which file.
        path: String,
    },
    /// A required field was missing.
    #[error("`{path}` is missing `{field}`")]
    MissingField {
        /// Which file.
        path: String,
        /// Which field.
        field: &'static str,
    },
    /// The skill asked for capabilities it may not have.
    #[error(
        "`{path}` requests capabilities ({requested}) that were not granted at install time. A \
         skill can request capabilities; it cannot grant them to itself."
    )]
    UngrantedCapabilities {
        /// Which file.
        path: String,
        /// What it asked for.
        requested: String,
    },
}

/// A parsed skill, before anything beyond the index has been loaded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skill {
    /// The index entry.
    pub entry: SkillIndexEntry,
    /// The full body, kept out of the prompt until the ladder calls for it.
    body: String,
    /// Capabilities the skill asked for.
    pub requested_capabilities: Vec<SmolStr>,
    /// Where it came from.
    pub source: SmolStr,
}

impl Skill {
    /// The full instructions, labelled by trust.
    ///
    /// A skill from a trusted root is operator-authored and reaches the prompt as such. Anything
    /// else is someone else's text, and it is labelled that way whatever it claims about itself.
    #[must_use]
    pub fn instructions(&self) -> Untrusted<String> {
        Tainted::with_provenance(
            self.body.clone(),
            Provenance::new(format!("skill:{}", self.entry.id)),
        )
    }

    /// Whether the body exceeds what the format recommends.
    #[must_use]
    pub fn is_oversized(&self) -> bool {
        self.entry.body_tokens > RECOMMENDED_BODY_TOKENS
    }
}

/// Parse a `SKILL.md`.
///
/// # Errors
/// Returns [`SkillError`] when the front matter is missing or incomplete, or when the skill asks for
/// capabilities that were not granted when it was installed.
pub fn parse_skill(
    path: &str,
    text: &str,
    granted: &[SmolStr],
    trusted: bool,
) -> Result<Skill, SkillError> {
    let rest = text
        .strip_prefix("---")
        .ok_or_else(|| SkillError::NoFrontMatter { path: path.to_string() })?;
    let (front, body) = rest
        .split_once("\n---")
        .ok_or_else(|| SkillError::NoFrontMatter { path: path.to_string() })?;

    let mut name = None;
    let mut description = None;
    let mut requested = Vec::new();

    for line in front.lines() {
        let Some((key, value)) = line.split_once(':') else { continue };
        let value = value.trim().trim_matches(['"', '\'']).to_string();
        match key.trim() {
            "name" => name = Some(value),
            "description" => description = Some(value),
            "capabilities" => {
                requested = value
                    .trim_matches(['[', ']'])
                    .split(',')
                    .map(|c| SmolStr::new(c.trim().trim_matches(['"', '\''])))
                    .filter(|c| !c.is_empty())
                    .collect();
            }
            _ => {}
        }
    }

    let name = name.ok_or(SkillError::MissingField { path: path.to_string(), field: "name" })?;
    let description = description
        .ok_or(SkillError::MissingField { path: path.to_string(), field: "description" })?;

    // A skill cannot grant itself capabilities. Anything it asks for that was not granted at
    // install time is a refusal, not a prompt mid-run — because a mid-run prompt is exactly where
    // an injected instruction would like to be answered.
    let ungranted: Vec<&SmolStr> = requested.iter().filter(|c| !granted.contains(c)).collect();
    if !ungranted.is_empty() {
        return Err(SkillError::UngrantedCapabilities {
            path: path.to_string(),
            requested: ungranted.iter().map(|c| c.as_str()).collect::<Vec<_>>().join(", "),
        });
    }

    let body = body.trim_start_matches(['-', '\n']).trim().to_string();
    let body_tokens = u32::try_from(body.len().div_ceil(4)).unwrap_or(u32::MAX);

    Ok(Skill {
        entry: SkillIndexEntry {
            id: SkillId::new(name.to_ascii_lowercase().replace(' ', "-")),
            name,
            description,
            body_tokens,
            trusted,
        },
        body,
        requested_capabilities: requested,
        source: path.into(),
    })
}

/// What a set of skills costs at each rung.
///
/// The point of the ladder is that the first number stays small however many skills exist. This is
/// what makes that checkable rather than asserted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LadderCost {
    /// What every skill costs, always.
    pub index_tokens: u32,
    /// What loading every skill's body would cost.
    pub all_bodies_tokens: u32,
}

/// Measure the ladder for a set of skills.
#[must_use]
pub fn ladder_cost(skills: &[Skill]) -> LadderCost {
    LadderCost {
        index_tokens: skills.iter().map(|s| s.entry.index_tokens()).sum(),
        all_bodies_tokens: skills.iter().map(|s| s.entry.body_tokens).sum(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A realistically sized skill. A two-line fixture would make the ladder look pointless: the
    /// claim is that the index stays small next to a body of the size the format actually
    /// recommends, so the fixture has to be that size for the test to mean anything.
    const SKILL: &str = r#"---
name: Release Checklist
description: Use when cutting a release, to run the pre-flight checks in the right order
---

# Release Checklist

Run these in order. Each step assumes the previous one passed; if one fails, stop and fix it
rather than continuing, because a half-finished release is harder to undo than a delayed one.

## 1. Verify the working tree

Confirm the branch is the release branch and the tree is clean. Uncommitted changes at this point
almost always mean something was fixed locally and never pushed, and it will be missing from the
artefact.

## 2. Run the full suite

Run every tier, not just the fast ones. The integration tier is the one that catches platform
differences, and it is exactly the tier people skip when they are in a hurry to release.

## 3. Check the changelog

Every user-visible change since the last tag needs an entry. Read the commit log rather than
trusting memory. Entries should say what changed for a user, not what changed in the code.

## 4. Verify the version

The version in the manifest, the changelog heading, and the tag must agree. A mismatch here is
discovered by users rather than by tooling.

## 5. Build the artefacts

Build for every supported target. Check that each one runs, not merely that it compiled.

## 6. Tag and push

Tag with the version, push the tag, and watch the release pipeline to completion. Do not walk away
before it finishes: a failed publish leaves a tag pointing at something that was never released.
"#;

    fn parse(text: &str, granted: &[&str], trusted: bool) -> Result<Skill, SkillError> {
        let granted: Vec<SmolStr> = granted.iter().map(|g| SmolStr::new(*g)).collect();
        parse_skill("skills/release/SKILL.md", text, &granted, trusted)
    }

    #[test]
    fn the_index_rung_costs_far_less_than_the_body() {
        // The entire point of the ladder. If this ratio ever inverts, progressive disclosure has
        // stopped paying for itself.
        let skill = parse(SKILL, &[], true).unwrap();
        assert!(
            skill.entry.index_tokens() * 2 < skill.entry.body_tokens,
            "index {} vs body {}",
            skill.entry.index_tokens(),
            skill.entry.body_tokens
        );
    }

    #[test]
    fn the_description_is_what_a_selector_matches_on() {
        let skill = parse(SKILL, &[], true).unwrap();
        assert!(skill.entry.searchable_text().contains("cutting a release"));
        assert_eq!(skill.entry.id.as_str(), "release-checklist");
    }

    #[test]
    fn a_skill_from_outside_a_trusted_root_is_someone_elses_text() {
        // Whatever it says about itself. Supply-chain attacks on skill registries are published
        // literature, so provenance is recorded and integrity is not raised.
        let skill = parse(SKILL, &[], false).unwrap();
        assert!(!skill.entry.trusted);

        let instructions = skill.instructions();
        assert_eq!(instructions.label().0, frey_core::taint::IntegrityLevel::Low);
        assert_eq!(instructions.provenance().origin.as_str(), "skill:release-checklist");
    }

    #[test]
    fn a_skill_cannot_grant_itself_capabilities() {
        let greedy = SKILL.replace(
            "description: Use when",
            "capabilities: [\"exec\", \"net:egress\"]\ndescription: Use when",
        );

        let err = parse(&greedy, &[], true).unwrap_err();
        let message = format!("{err}");
        assert!(message.contains("exec"), "{message}");
        assert!(message.contains("cannot grant them to itself"), "{message}");

        // Granted at install time, it loads.
        assert!(parse(&greedy, &["exec", "net:egress"], true).is_ok());
    }

    #[test]
    fn ungranted_capabilities_are_refused_at_load_rather_than_prompted_for_mid_run() {
        // A mid-run prompt is exactly where an injected instruction would like to be answered.
        let greedy = SKILL
            .replace("description: Use when", "capabilities: [\"exec\"]\ndescription: Use when");
        assert!(matches!(parse(&greedy, &[], true), Err(SkillError::UngrantedCapabilities { .. })));
    }

    #[test]
    fn missing_front_matter_says_what_is_needed() {
        let err = parse("# Just a heading\n", &[], true).unwrap_err();
        assert!(format!("{err}").contains("name and a description"));
    }

    #[test]
    fn a_missing_field_names_itself() {
        let no_description = "---\nname: Thing\n---\nbody";
        let err = parse(no_description, &[], true).unwrap_err();
        assert!(matches!(err, SkillError::MissingField { field: "description", .. }));
    }

    #[test]
    fn an_oversized_body_is_reported_rather_than_refused() {
        // A skill that costs as much as the conversation defeats the ladder, but refusing to load it
        // would be worse than telling the author.
        let mut long = String::from("---\nname: Long\ndescription: A very long skill\n---\n");
        long.push_str(&"x".repeat((RECOMMENDED_BODY_TOKENS as usize + 100) * 4));
        let skill = parse(&long, &[], true).unwrap();
        assert!(skill.is_oversized());
        assert!(!parse(SKILL, &[], true).unwrap().is_oversized());
    }

    #[test]
    fn the_index_stays_small_as_the_catalog_grows() {
        // Twenty skills must not cost twenty bodies at startup. This is the claim the whole
        // mechanism makes, so it gets a test rather than a paragraph.
        let skills: Vec<Skill> = (0..20).map(|_| parse(SKILL, &[], true).unwrap()).collect();
        let cost = ladder_cost(&skills);
        assert!(
            cost.index_tokens * 2 < cost.all_bodies_tokens,
            "index {} vs bodies {}",
            cost.index_tokens,
            cost.all_bodies_tokens
        );
    }
}
