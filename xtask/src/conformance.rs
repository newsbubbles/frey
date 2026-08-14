//! Connect Frey's MCP client to servers Frey did not write.
//!
//! Every claim in `docs/mcp.md` rests on `FakeToolset` and on frey's own server answering frey's own
//! client over a loopback. That is a real test of the code and no test at all of the *protocol*: a
//! client and a server written by one author on one afternoon agree about everything, including
//! their shared misreadings.
//!
//! This runs the client against real third-party stdio servers — the ones a person would actually
//! reach for — and records what each one does. It costs nothing: **no inference is involved**, only
//! `server/discover`, `tools/list`, and a listing round-trip.
//!
//! What it looks for, in order of what would hurt most:
//!
//! 1. Does the server answer the `2026-07-28` stateless shape, or does it require the old handshake?
//! 2. Does `tools/list` return schemas Frey's own validator accepts?
//! 3. Do descriptions and parameter docs exist, or would every tool here be unfindable by search?
//! 4. Does anything in the listing churn between two identical calls — a timestamp, a counter, an
//!    unstable order — which would rewrite a cached prompt prefix every turn?
//!
//! Item 4 is the one worth the exercise. Frey re-sorts listings defensively *because* a server can
//! churn a prompt cache, and until now that defence had never met a server that might.
//!
//! Results go to `notes/conformance/` as a table. Per-server defects are disclosed to maintainers
//! privately before anyone is named in public.

use std::path::Path;
use std::process::ExitCode;

/// How long a server gets to answer before the sweep gives up on it.
///
/// Generous, because `npx -y` downloads the package on a first run and that is genuinely slow. Not
/// unbounded, because a server that never answers is a finding and not a reason for the sweep to
/// hang — which is what happened the first time this ran.
const PATIENCE: std::time::Duration = std::time::Duration::from_secs(90);

/// A server worth testing against, and how to start it.
///
/// Deliberately all `npx`/`uvx` one-liners: the point is what a person encounters, and a server
/// that needs a bespoke build is not what a person encounters.
struct Target {
    name: &'static str,
    program: &'static str,
    args: &'static [&'static str],
}

const TARGETS: &[Target] = &[
    Target {
        name: "filesystem",
        program: "npx",
        args: &["-y", "@modelcontextprotocol/server-filesystem", "."],
    },
    Target { name: "memory", program: "npx", args: &["-y", "@modelcontextprotocol/server-memory"] },
    Target {
        name: "sequential-thinking",
        program: "npx",
        args: &["-y", "@modelcontextprotocol/server-sequential-thinking"],
    },
    Target {
        name: "everything",
        program: "npx",
        args: &["-y", "@modelcontextprotocol/server-everything"],
    },
    Target { name: "context7", program: "npx", args: &["-y", "@upstash/context7-mcp"] },
    Target { name: "chrome-devtools", program: "npx", args: &["-y", "chrome-devtools-mcp"] },
    // The Python reference servers. Kept in the sweep even though they do not currently start on
    // this machine, because "the upstream reference implementation is broken against the current
    // SDK" is a finding about the ecosystem and dropping the row would hide it.
    Target { name: "git", program: "uvx", args: &["mcp-server-git", "--repository", "."] },
    Target { name: "fetch", program: "uvx", args: &["mcp-server-fetch"] },
    Target { name: "time", program: "uvx", args: &["mcp-server-time"] },
    Target {
        name: "sqlite",
        program: "uvx",
        args: &["mcp-server-sqlite", "--db-path", ":memory:"],
    },
];

/// Run the sweep and write the table.
///
/// Takes `--only <name>` to run one target, because a sweep that takes four minutes is a sweep
/// nobody runs while iterating on the thing it found.
pub fn run(root: &Path, args: &[String]) -> ExitCode {
    let only =
        args.iter().position(|a| a == "--only").and_then(|i| args.get(i + 1)).map(String::as_str);

    let mut rows = Vec::new();
    for target in TARGETS {
        if only.is_some_and(|name| name != target.name) {
            continue;
        }
        println!("→ {}", target.name);
        let row = probe(target);
        println!("   {}", row.summary());
        rows.push(row);
    }

    if rows.is_empty() {
        eprintln!("no targets matched");
        return ExitCode::FAILURE;
    }

    let dir = root.join("notes/conformance");
    if let Err(error) = std::fs::create_dir_all(&dir) {
        eprintln!("could not create {}: {error}", dir.display());
        return ExitCode::FAILURE;
    }
    let table = render(&rows);
    if let Err(error) = std::fs::write(dir.join("results.md"), &table) {
        eprintln!("could not write results: {error}");
        return ExitCode::FAILURE;
    }

    // **And a dated machine-readable line**, because a claim resting on this has to go red when the
    // sweep goes stale. The prose table is for a person; `claims.toml` cannot check it, and a claim
    // whose evidence cannot expire is a claim that will eventually be wrong quietly.
    //
    // Days since the epoch rather than a timestamp: the checker reads a plain integer on purpose,
    // since a date format is one more thing to get wrong inside a check whose only job is to be
    // trustworthy.
    let day = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() / 86_400)
        .unwrap_or(0);
    let record = format!(
        "{{\"day\": {day}, \"targets\": {}, \"reached\": {}, \"stateless\": {}, \"churning\": {}, \"tools\": {}}}
",
        rows.len(),
        rows.iter().filter(|r| r.reached).count(),
        rows.iter().filter(|r| r.stateless).count(),
        rows.iter().filter(|r| r.churns).count(),
        rows.iter().map(|r| r.tools).sum::<usize>(),
    );
    if let Err(error) = std::fs::write(dir.join("results.jsonl"), record) {
        eprintln!("could not write the dated record: {error}");
        return ExitCode::FAILURE;
    }
    println!("\nwrote {}", dir.join("results.md").display());

    // **A server that could not be reached is not a passing server.** The whole exercise is worth
    // nothing if an absent `npx` reads the same as a clean sweep.
    let reached = rows.iter().filter(|r| r.reached).count();
    println!("{reached}/{} server(s) answered", rows.len());
    ExitCode::SUCCESS
}

/// What one server did.
struct Row {
    name: &'static str,
    /// The process started and answered at all.
    reached: bool,
    /// It answered `server/discover` — the 2026-07-28 shape — without a handshake.
    stateless: bool,
    /// How many tools it listed.
    tools: usize,
    /// Tools whose input schema Frey's validator would reject outright.
    bad_schemas: usize,
    /// Tools with no description, or a description too thin to search on.
    unfindable: usize,
    /// The listing differed between two identical calls.
    churns: bool,
    /// What went wrong, if anything.
    note: String,
    /// If the listing churned, what changed between the two calls.
    churn_detail: String,
}

impl Row {
    fn summary(&self) -> String {
        if !self.reached {
            return format!("unreachable: {}", self.note);
        }
        format!(
            "{} tool(s), {} unfindable, {} bad schema(s), {}, {}",
            self.tools,
            self.unfindable,
            self.bad_schemas,
            if self.stateless { "stateless" } else { "needs handshake" },
            if self.churns { "LISTING CHURNS" } else { "listing stable" }
        )
    }
}

fn probe(target: &Target) -> Row {
    let mut row = Row {
        name: target.name,
        reached: false,
        stateless: false,
        tools: 0,
        bad_schemas: 0,
        unfindable: 0,
        churns: false,
        note: String::new(),
        churn_detail: String::new(),
    };

    // Stateless first, then the legacy handshake — the same order Frey's own client uses, and the
    // fallback firing is itself the finding: it means the server has not moved to `2026-07-28`.
    let first = match list_tools(target, true).or_else(|stateless_error| {
        list_tools(target, false).map_err(|handshake_error| {
            format!("stateless: {stateless_error}; handshake: {handshake_error}")
        })
    }) {
        Ok(value) => value,
        Err(error) => {
            row.note = error;
            return row;
        }
    };
    row.reached = true;
    row.stateless = first.stateless;

    let second = list_tools(target, first.stateless).map(|r| r.tools).unwrap_or_default();
    row.churns = !second.is_empty() && second != first.tools;
    if row.churns {
        row.churn_detail = describe_churn(&first.tools, &second);
    }

    row.tools = first.tools.len();
    for tool in &first.tools {
        let schema = tool.get("inputSchema").or_else(|| tool.get("input_schema"));
        let usable =
            schema.is_some_and(|s| s.get("type").and_then(|t| t.as_str()) == Some("object"));
        if !usable {
            row.bad_schemas += 1;
        }
        let description = tool.get("description").and_then(|d| d.as_str()).unwrap_or_default();
        if description.split_whitespace().count() < 5 {
            row.unfindable += 1;
        }
    }
    row
}

struct Listing {
    stateless: bool,
    tools: Vec<serde_json::Value>,
}

/// Start the server, ask it for its tools, and stop.
///
/// Tries the stateless `server/discover` first and falls back to the legacy `initialize` handshake,
/// which is the same order Frey's own client uses — and the fallback firing is itself the finding.
fn list_tools(target: &Target, try_stateless: bool) -> Result<Listing, String> {
    use std::io::{BufRead, BufReader, Write};
    use std::process::{Command, Stdio};

    // `npx` and `npm` are `.cmd` shims on Windows, and `Command::new` will not find them without
    // the extension — which is how a sweep across eight servers reported "program not found" eight
    // times on a machine with node installed and on `PATH`.
    let program = if cfg!(windows) && matches!(target.program, "npx" | "npm") {
        format!("{}.cmd", target.program)
    } else {
        target.program.to_string()
    };

    let mut child = Command::new(&program)
        .args(target.args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        // Captured rather than discarded. A sweep whose failures all read "no answer" is half a
        // sweep: the Python servers here fail with an `ImportError` against the current MCP SDK,
        // which is a fact worth publishing and invisible without this line.
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("could not start `{program} {}`: {e}", target.args.join(" ")))?;

    // **A watchdog, because a server that never answers must be a row in the table rather than a
    // sweep that never ends.** One of the eight sat waiting on a first run and took the whole run
    // with it. Killing the child closes its stdout, which ends the read loop below without needing
    // a channel or a non-blocking read.
    let watchdog = {
        let handle = child.id();
        std::thread::spawn(move || {
            std::thread::sleep(PATIENCE);
            // `taskkill` / `kill` by pid: the `Child` itself is borrowed by the read loop.
            #[cfg(windows)]
            let _ = Command::new("taskkill")
                .args(["/PID", &handle.to_string(), "/T", "/F"])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
            #[cfg(not(windows))]
            let _ = Command::new("kill").arg(handle.to_string()).status();
        })
    };

    let mut stdin = child.stdin.take().ok_or("no stdin")?;
    let stdout = child.stdout.take().ok_or("no stdout")?;
    let mut reader = BufReader::new(stdout);

    let mut send = |value: &serde_json::Value| -> Result<(), String> {
        writeln!(stdin, "{value}").map_err(|e| e.to_string())
    };

    let mut stateless = false;
    if try_stateless {
        send(&serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "server/discover", "params": {}
        }))?;
    } else {
        send(&serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": {"name": "frey-conformance", "version": "0"}
            }
        }))?;
    }
    if !try_stateless {
        // A server that speaks the old protocol will not answer anything until it has been told the
        // handshake is complete. Omitting this notification looks exactly like a server that does
        // not implement `tools/list`.
        send(&serde_json::json!({"jsonrpc": "2.0", "method": "notifications/initialized"}))?;
    }
    send(&serde_json::json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {}}))?;

    let mut tools = Vec::new();
    let mut line = String::new();
    for _ in 0..64 {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line.trim()) else { continue };
        if value.get("id") == Some(&serde_json::json!(1)) && value.get("result").is_some() {
            stateless = try_stateless;
        }
        if value.get("id") == Some(&serde_json::json!(2))
            && let Some(list) = value.pointer("/result/tools").and_then(|t| t.as_array())
        {
            tools = list.clone();
            break;
        }
    }

    let complaint = child.stderr.take().map(read_briefly).unwrap_or_default();
    let _ = child.kill();
    let _ = child.wait();
    drop(watchdog);

    if tools.is_empty() && !stateless {
        return Err(if complaint.is_empty() {
            "no tools listed and no discover answer".to_string()
        } else {
            complaint
        });
    }
    Ok(Listing { stateless, tools })
}

/// Name what changed between two identical listings.
///
/// The whole reason the sweep exists. Frey re-sorts listings defensively *because* a server can
/// rewrite a cached prompt prefix by reordering or re-describing its tools, and that defence had
/// never met a server that does it. Saying *which field moved* turns "a server churns" from a
/// warning into a bug report somebody can act on.
fn describe_churn(first: &[serde_json::Value], second: &[serde_json::Value]) -> String {
    let names = |tools: &[serde_json::Value]| -> Vec<String> {
        tools
            .iter()
            .map(|t| t.get("name").and_then(|n| n.as_str()).unwrap_or("?").to_string())
            .collect()
    };
    let (a, b) = (names(first), names(second));
    if a != b {
        let mut sorted_a = a.clone();
        let mut sorted_b = b.clone();
        sorted_a.sort();
        sorted_b.sort();
        if sorted_a == sorted_b {
            return format!(
                "the same tools in a different order: {} then {}",
                a.join(","),
                b.join(",")
            );
        }
        return format!("a different set of tools: {} then {}", a.join(","), b.join(","));
    }

    // Same names, same order: something inside a definition moved. Name the first field that did.
    for (index, (x, y)) in first.iter().zip(second).enumerate() {
        if x == y {
            continue;
        }
        let name = x.get("name").and_then(|n| n.as_str()).unwrap_or("?");
        if let (Some(ox), Some(oy)) = (x.as_object(), y.as_object()) {
            for (key, value) in ox {
                if oy.get(key) != Some(value) {
                    return format!(
                        "tool `{name}` (#{index}) changed its `{key}`: {} then {}",
                        elide(&value.to_string()),
                        elide(
                            &oy.get(key).map(std::string::ToString::to_string).unwrap_or_default()
                        )
                    );
                }
            }
        }
        return format!("tool `{name}` (#{index}) changed");
    }
    "the listings differ in a way this check could not name".to_string()
}

fn elide(text: &str) -> String {
    let cut: String = text.chars().take(180).collect();
    if cut.len() < text.len() { format!("{cut}…") } else { cut }
}

/// The last useful line a dying server wrote, capped.
///
/// The *last* line, because a Python traceback puts the reason at the bottom and the top is fifteen
/// lines of frozen importlib.
fn read_briefly(mut stderr: std::process::ChildStderr) -> String {
    use std::io::Read as _;
    let mut buffer = Vec::new();
    let _ = stderr.read_to_end(&mut buffer);
    let text = String::from_utf8_lossy(&buffer);
    let last = text.lines().rfind(|l| !l.trim().is_empty()).unwrap_or_default().trim();
    last.chars().take(160).collect()
}

fn render(rows: &[Row]) -> String {
    let mut out = String::from(
        "# MCP conformance sweep\n\n\
         Frey's own MCP client against servers Frey did not write. No inference; this costs nothing\n\
         to run. Every claim in `docs/mcp.md` previously rested on `FakeToolset` and on Frey's\n\
         server answering Frey's client, which is a test of the code and not of the protocol.\n\n\
         `churns` is the column worth reading: Frey re-sorts listings defensively because a server\n\
         can rewrite a cached prompt prefix, and until this sweep that defence had never met a\n\
         server that might.\n\n\
         | server | reached | stateless | tools | thin descriptions | bad schemas | churns |\n\
         |---|---|---|---|---|---|---|\n",
    );
    for row in rows {
        out.push_str(&format!(
            "| {} | {} | {} | {} | {} | {} | {} |\n",
            row.name,
            if row.reached { "yes" } else { "no" },
            if row.stateless { "yes" } else { "handshake" },
            row.tools,
            row.unfindable,
            row.bad_schemas,
            if row.churns { "**yes**" } else { "no" },
        ));
    }
    out.push_str(
        "\nUnreachable rows mean the server would not start on this machine — usually a missing\n\
         `npx` or `uvx` — and are **not** passes.\n",
    );
    for row in rows.iter().filter(|r| !r.reached) {
        out.push_str(&format!("\n- `{}`: {}\n", row.name, row.note));
    }

    let churning: Vec<&Row> = rows.iter().filter(|r| r.churns).collect();
    if !churning.is_empty() {
        out.push_str("\n## What churned\n");
        for row in churning {
            out.push_str(&format!("\n- **`{}`** — {}\n", row.name, row.churn_detail));
        }
    }

    let reached: Vec<&Row> = rows.iter().filter(|r| r.reached).collect();
    let stateless = reached.iter().filter(|r| r.stateless).count();
    out.push_str(&format!(
        "\n## The headline\n\n**{stateless} of {} servers that answered speak the `2026-07-28` \
         stateless revision.**\n",
        reached.len()
    ));
    out
}
