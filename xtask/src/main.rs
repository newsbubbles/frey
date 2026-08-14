//! Repository checks that are not tests.
//!
//! ```text
//! cargo xtask producers   # public variants nothing ever constructs
//! cargo xtask claims      # claims.toml against the tree
//! cargo xtask check       # both, which is what CI runs
//! ```
//!
//! These are not unit tests because they are checks *about* the codebase rather than about its
//! behaviour, and because both of them need to read the whole tree — which a test in one crate
//! cannot do, and which is exactly the blind spot that let a declared-and-never-built enum variant
//! survive an audit designed to find declared-and-never-built enum variants.

mod claims;
mod conformance;
mod producers;

use std::path::PathBuf;
use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    let command = args.get(1).map(String::as_str).unwrap_or("check");
    let root = root();

    match command {
        "producers" => run_producers(&root),
        "claims" => run_claims(&root),
        "conformance" => conformance::run(&root, &args[2..]),
        "check" => {
            let a = run_producers(&root);
            let b = run_claims(&root);
            if a == ExitCode::SUCCESS && b == ExitCode::SUCCESS {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            }
        }
        other => {
            eprintln!(
                "unknown command `{other}`\nusage: cargo xtask [check|producers|claims|conformance]"
            );
            ExitCode::FAILURE
        }
    }
}

fn root() -> PathBuf {
    // `CARGO_MANIFEST_DIR` is `<root>/xtask`.
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .map_or_else(|| PathBuf::from("."), Path::to_path_buf)
}

use std::path::Path;

fn run_producers(root: &Path) -> ExitCode {
    let orphans = match producers::sweep(root) {
        Ok(orphans) => orphans,
        Err(error) => {
            eprintln!("producer lint could not read the tree: {error}");
            return ExitCode::FAILURE;
        }
    };

    let acknowledged = claims::Claims::load(&root.join("claims.toml"))
        .map(|c| c.acknowledged_orphans())
        .unwrap_or_default();
    let unknown = producers::unacknowledged(&orphans, &acknowledged);

    println!("producers: {} orphan(s), {} acknowledged", orphans.len(), acknowledged.len());
    for orphan in &orphans {
        let known = if acknowledged.contains(&orphan.key()) { "known" } else { " NEW " };
        println!("  [{known}] {:<28} {}", orphan.key(), orphan.file);
    }

    if unknown.is_empty() {
        return ExitCode::SUCCESS;
    }
    eprintln!(
        "\n{} public variant(s) are declared and never constructed, with no row in claims.toml.\n\
         \n\
         This is the shape of every capability bug this project has had: a type that exists, a doc\n\
         that describes it, tests that consume it, and nothing that produces it. Either write the\n\
         producer, or add a claims.toml row with `acknowledges_orphan` saying why it is a design\n\
         and not a feature.",
        unknown.len()
    );
    ExitCode::FAILURE
}

fn run_claims(root: &Path) -> ExitCode {
    let path = root.join("claims.toml");
    let claims = match claims::Claims::load(&path) {
        Ok(claims) => claims,
        Err(error) => {
            eprintln!("{error}");
            return ExitCode::FAILURE;
        }
    };

    let today = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() / 86_400)
        .unwrap_or(0);

    let counts = claims.counts();
    let total: usize = counts.iter().map(|(_, n)| n).sum();
    println!("claims: {total} row(s)");
    for (label, count) in counts {
        println!("  {count:>3}  {label}");
    }

    // The unevidenced rows, in full, with what would settle each. This is the part of the artifact
    // that is worth publishing: a list of things the repository says about itself and cannot yet
    // demonstrate, maintained by the same person who wrote the README.
    let open: Vec<&claims::Claim> =
        claims.claim.iter().filter(|c| c.status == claims::Status::DeclaredOnly).collect();
    if !open.is_empty() {
        println!(
            "
unevidenced:"
        );
        for claim in open {
            println!("  {} — {}", claim.id, claim.claim);
            if let Some(next) = &claim.would_settle {
                println!("      would settle: {next}");
            }
            for place in &claim.stated_in {
                println!("      stated in: {place}");
            }
        }
    }

    let problems = claims::check(&claims, root, today);
    if problems.is_empty() {
        return ExitCode::SUCCESS;
    }
    eprintln!("\n{} problem(s):", problems.len());
    for problem in &problems {
        eprintln!("  - {problem}");
    }
    ExitCode::FAILURE
}
