//! The claims table, and the check that keeps it honest.
//!
//! `claims.toml` is one row per thing this repository says about itself, each with a status and —
//! for anything above `declared-only` — a `settled_by` naming what makes it true.
//!
//! It exists because a README is a snapshot and code is not. Two claims in this one were *wrong*
//! rather than optimistic when the capability audit found them, and both had been wrong since the
//! day they were written. A table with a machine-checked link from claim to evidence is the only
//! version of a README that can rot loudly.
//!
//! **The check is on the evidence, not on the file.** A checker that verifies `settled_by` points
//! at a file that exists goes green forever while every claim underneath it decays. So:
//!
//! * `settled_by = "test:some_test_name"` must resolve to a `fn some_test_name` in the tree. CI runs
//!   the whole suite anyway, so a named test that exists is a named test that passed.
//! * `settled_by = "results:notes/evidence/x.jsonl"` must resolve to a file whose newest record is
//!   inside `stale_after_days`. Evidence with a date on it goes red on its own.
//! * `status = "operated"` additionally requires `stale_after_days`, because an operational claim
//!   with no expiry is a claim about a machine that may have been switched off in March.
//!
//! The honest count is published. A third of the rows saying UNEVIDENCED is the point of the
//! artifact, not an embarrassment in it.

use std::collections::BTreeSet;
use std::path::Path;

/// What kind of backing a claim has.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Status {
    /// A named test proves it, and CI runs that test.
    Settled,
    /// A running system has demonstrated it, with a timestamped record.
    Operated,
    /// Tests pass and nothing has ever run it in anger.
    TestedOnly,
    /// It exists as a type, a trait or a document, and nothing produces it.
    DeclaredOnly,
    /// It was claimed, it was wrong, and the claim has been withdrawn.
    Retracted,
}

impl Status {
    /// Whether this status obliges the row to name its evidence.
    #[must_use]
    pub fn needs_evidence(self) -> bool {
        matches!(self, Self::Settled | Self::Operated)
    }

    /// How the published count labels it.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Settled => "settled",
            Self::Operated => "operated",
            Self::TestedOnly => "tested-only",
            Self::DeclaredOnly => "UNEVIDENCED",
            Self::Retracted => "retracted",
        }
    }
}

/// One thing the repository says about itself.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct Claim {
    /// Short stable key.
    pub id: String,
    /// The claim, as a reader would encounter it.
    pub claim: String,
    /// Where a reader encounters it.
    #[serde(default)]
    pub stated_in: Vec<String>,
    /// What kind of backing it has.
    pub status: Status,
    /// `test:<fn name>`, `doctest:<path>`, or `results:<path>`. Required above `tested-only`.
    #[serde(default)]
    pub settled_by: Option<String>,
    /// How long a results file stays evidence. Required for `operated`.
    #[serde(default)]
    pub stale_after_days: Option<u32>,
    /// What would move it up a row.
    #[serde(default)]
    pub would_settle: Option<String>,
    /// A producer-lint orphan this row deliberately acknowledges.
    #[serde(default)]
    pub acknowledges_orphan: Option<String>,
}

/// The whole table.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct Claims {
    /// Every row.
    pub claim: Vec<Claim>,
}

impl Claims {
    /// Parse `claims.toml`.
    ///
    /// # Errors
    /// Returns a human-readable message. This is a repository check, not a library.
    pub fn load(path: &Path) -> Result<Self, String> {
        let body = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
        toml::from_str(&body).map_err(|e| format!("{}: {e}", path.display()))
    }

    /// The orphans this table acknowledges.
    #[must_use]
    pub fn acknowledged_orphans(&self) -> BTreeSet<String> {
        self.claim.iter().filter_map(|c| c.acknowledges_orphan.clone()).collect()
    }

    /// `status → count`, in the order they are published.
    #[must_use]
    pub fn counts(&self) -> Vec<(&'static str, usize)> {
        [
            Status::Settled,
            Status::Operated,
            Status::TestedOnly,
            Status::DeclaredOnly,
            Status::Retracted,
        ]
        .into_iter()
        .map(|s| (s.label(), self.claim.iter().filter(|c| c.status == s).count()))
        .collect()
    }
}

/// Check every row against the tree, and return the problems.
///
/// `now_days` is days since the Unix epoch, passed in rather than read, so the check is a pure
/// function and its own tests do not depend on the day they run.
#[must_use]
pub fn check(claims: &Claims, root: &Path, now_days: u64) -> Vec<String> {
    let mut problems = Vec::new();
    let mut seen = BTreeSet::new();
    let sources = all_rust(root);

    for claim in &claims.claim {
        if !seen.insert(claim.id.clone()) {
            problems.push(format!("`{}` is declared twice", claim.id));
        }

        if claim.status.needs_evidence() && claim.settled_by.is_none() {
            problems.push(format!(
                "`{}` is `{}` and names no evidence. That is the state this table exists to make \
                 impossible: raise it to a status it has earned, or name what settles it.",
                claim.id,
                claim.status.label()
            ));
            continue;
        }
        if claim.status == Status::Operated && claim.stale_after_days.is_none() {
            problems.push(format!(
                "`{}` is `operated` with no `stale_after_days`. An operational claim with no \
                 expiry is a claim about a machine that may have been switched off months ago.",
                claim.id
            ));
        }

        let Some(evidence) = &claim.settled_by else { continue };
        if let Some(name) = evidence.strip_prefix("test:") {
            let needle = format!("fn {name}(");
            if !sources.iter().any(|body| body.contains(&needle)) {
                problems.push(format!(
                    "`{}` is settled by test `{name}`, and no `fn {name}` exists. A renamed test \
                     silently unsettles the claim it was written for.",
                    claim.id
                ));
            }
        } else if let Some(rel) = evidence.strip_prefix("doctest:") {
            // A `compile_fail` doctest is the strongest evidence this repository has for the taint
            // claims — the *compiler* proves them, on every platform, rather than a snapshot of
            // diagnostic text. It had no way to be cited, so the row cited a runtime test that did
            // not establish the claim. A vocabulary that cannot express your best evidence pushes
            // you towards worse evidence.
            match std::fs::read_to_string(root.join(rel)) {
                Err(_) => problems.push(format!(
                    "`{}` is settled by doctests in `{rel}`, which does not exist.",
                    claim.id
                )),
                Ok(body) if !body.contains("```compile_fail") => problems.push(format!(
                    "`{}` is settled by `compile_fail` doctests in `{rel}` and there are none.",
                    claim.id
                )),
                Ok(_) => {}
            }
        } else if let Some(rel) = evidence.strip_prefix("results:") {
            match newest_record_day(&root.join(rel)) {
                None => problems.push(format!(
                    "`{}` is settled by results at `{rel}`, which is missing or has no dated \
                     record in it.",
                    claim.id
                )),
                Some(day) => {
                    let limit = u64::from(claim.stale_after_days.unwrap_or(u32::MAX));
                    if now_days.saturating_sub(day) > limit {
                        problems.push(format!(
                            "`{}` rests on results at `{rel}` last written {} days ago, past its \
                             {limit}-day window. The claim has not been disproved; it has stopped \
                             being evidenced, which is the thing this table measures.",
                            claim.id,
                            now_days.saturating_sub(day)
                        ));
                    }
                }
            }
        } else {
            problems.push(format!(
                "`{}` has `settled_by = \"{evidence}\"`, which is none of `test:`, `doctest:` or \
                 `results:`",
                claim.id
            ));
        }
    }
    problems
}

/// The newest `"day": N` field in a JSONL results file, where `N` is days since the epoch.
///
/// Deliberately not a timestamp parser. Evidence files in this repository are written by Frey's own
/// tooling and carry a plain integer day, because a date format is one more thing to get wrong in a
/// check whose whole job is to be trustworthy.
fn newest_record_day(path: &Path) -> Option<u64> {
    let body = std::fs::read_to_string(path).ok()?;
    body.lines()
        .filter_map(|line| {
            let at = line.find("\"day\"")?;
            let rest = &line[at + 5..];
            let digits: String = rest
                .chars()
                .skip_while(|c| !c.is_ascii_digit())
                .take_while(char::is_ascii_digit)
                .collect();
            digits.parse::<u64>().ok()
        })
        .max()
}

fn all_rust(root: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let mut stack = vec![root.join("crates"), root.join("xtask")];
    while let Some(current) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&current) else { continue };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                if path.file_name().is_some_and(|n| n == "target") {
                    continue;
                }
                stack.push(path);
            } else if path.extension().is_some_and(|e| e == "rs")
                && let Ok(body) = std::fs::read_to_string(&path)
            {
                out.push(body);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn one(status: Status, settled_by: Option<&str>, stale: Option<u32>) -> Claims {
        Claims {
            claim: vec![Claim {
                id: "x".into(),
                claim: "something".into(),
                stated_in: Vec::new(),
                status,
                settled_by: settled_by.map(str::to_string),
                stale_after_days: stale,
                would_settle: None,
                acknowledges_orphan: None,
            }],
        }
    }

    #[test]
    fn a_settled_claim_naming_no_evidence_is_a_problem() {
        let problems = check(&one(Status::Settled, None, None), Path::new("."), 0);
        assert_eq!(problems.len(), 1);
        assert!(problems[0].contains("names no evidence"));
    }

    #[test]
    fn a_declared_only_claim_needs_nothing() {
        assert!(check(&one(Status::DeclaredOnly, None, None), Path::new("."), 0).is_empty());
    }

    #[test]
    fn an_operated_claim_must_expire() {
        let claims = one(Status::Operated, Some("results:notes/evidence/none.jsonl"), None);
        let problems = check(&claims, Path::new("."), 0);
        assert!(problems.iter().any(|p| p.contains("stale_after_days")), "{problems:?}");
    }

    #[test]
    fn a_renamed_test_unsettles_its_claim() {
        // The failure mode a file-existence check cannot see, and the reason this resolves the test
        // *name*: renaming a test is a normal refactor, and it silently detaches the only thing
        // standing behind a public claim.
        let claims = one(Status::Settled, Some("test:definitely_not_a_test_in_this_tree"), None);
        let problems = check(&claims, Path::new("."), 0);
        assert_eq!(problems.len(), 1);
        assert!(problems[0].contains("silently unsettles"));
    }

    #[test]
    fn stale_results_go_red_on_their_own() {
        let dir = std::env::temp_dir().join("frey-claims-test");
        std::fs::create_dir_all(&dir).expect("tempdir");
        let file = dir.join("evidence.jsonl");
        std::fs::write(&file, "{\"day\": 100, \"ok\": true}\n").expect("write");

        let claims = Claims {
            claim: vec![Claim {
                id: "x".into(),
                claim: "runs nightly".into(),
                stated_in: Vec::new(),
                status: Status::Operated,
                settled_by: Some("evidence.jsonl".to_string()).map(|f| format!("results:{f}")),
                stale_after_days: Some(7),
                would_settle: None,
                acknowledges_orphan: None,
            }],
        };
        assert!(check(&claims, &dir, 105).is_empty(), "five days old is inside a seven-day window");
        let problems = check(&claims, &dir, 120);
        assert_eq!(problems.len(), 1);
        assert!(problems[0].contains("stopped being evidenced"), "{problems:?}");
    }
}
