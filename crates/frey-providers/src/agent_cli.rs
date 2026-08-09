//! Delegating to an agent CLI that owns its own subscription.
//!
//! R4 of the founding brief asks for Claude-SDK / Codex-SDK style adapters so a user can ride an
//! existing subscription instead of paying per token. v0.1.1 shipped the [`AgentProvider`] trait, a
//! [`DelegatedTask`] type with a `timeout_ms` field nothing read, and a test stub. Dogfooding found
//! the obvious: the headline feature was a shape with nothing behind it.
//!
//! # Why this is a subprocess and not an HTTP client
//!
//! Anthropic's usage policy (2026-02-20) prohibits third-party applications from using subscription
//! OAuth credentials. Frey therefore never stores, mints, refreshes, or replays a vendor token —
//! there is nowhere in this module to put one, and [`tests::frey_supplies_no_credentials`] holds
//! that line. Delegation goes to the vendor's *own binary*, which authenticates itself however its
//! vendor decided. The user's subscription is used by the user's installed tool, which is the only
//! arrangement that is both useful and permitted.
//!
//! That constraint is also why [`AgentProvider`] has no completion method. A delegated agent runs
//! its own loop, its own tools, and its own sandbox. Frey did not mediate those tool calls and does
//! not pretend otherwise: [`AgentEvent::ToolUsed`] is documented as display-only, and the audit
//! record says the call was unmediated rather than implying Frey approved it.
//!
//! # Parsing is separated from spawning
//!
//! [`parse_event`] is a pure function from one line of output to at most one [`AgentEvent`], so the
//! whole wire format is testable against recorded output with no process, no subscription, and no
//! cost. Only [`AgentCli::delegate`] touches the operating system. This is the same split as
//! [`Dialect`](crate::dialect::Dialect) versus [`HttpProvider`](crate::HttpProvider), for the same
//! reason.

use std::process::Stdio;

use crate::streaming::async_stream;

use frey_core::ids::AgentId;
use frey_core::provider::{
    AgentEvent, AgentEventStream, AgentProvider, DelegatedTask, DelegationError,
};
use frey_core::usage::{Money, Usage};
use serde_json::Value;
use smol_str::SmolStr;

/// Which vendor's output format to expect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Flavour {
    /// Claude Code's `--output-format stream-json`: one JSON object per line, with `assistant`
    /// messages carrying content blocks and a final `result` object carrying cost.
    ClaudeCode,
}

/// An [`AgentProvider`] that runs a vendor's own CLI.
#[derive(Debug, Clone)]
pub struct AgentCli {
    id: AgentId,
    program: String,
    flavour: Flavour,
    extra_args: Vec<String>,
}

impl AgentCli {
    /// Delegate to Claude Code, assuming `claude` is on `PATH`.
    #[must_use]
    pub fn claude_code() -> Self {
        Self {
            id: AgentId::new("claude-code"),
            program: "claude".into(),
            flavour: Flavour::ClaudeCode,
            extra_args: Vec::new(),
        }
    }

    /// Use a specific executable — a versioned install, a wrapper script, or a test double.
    #[must_use]
    pub fn program(mut self, program: impl Into<String>) -> Self {
        self.program = program.into();
        self
    }

    /// Arguments appended after the ones this adapter derives from the task.
    #[must_use]
    pub fn args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.extra_args = args.into_iter().map(Into::into).collect();
        self
    }

    /// The full argument vector for `task`.
    ///
    /// Separated out so the command line is testable without running anything. Arguments are passed
    /// as a vector and never as a shell string: there is no shell here, so there is nothing for a
    /// prompt containing `;` or a backtick to escape into.
    #[must_use]
    pub fn argv(&self, task: &DelegatedTask) -> Vec<String> {
        let mut argv = match self.flavour {
            Flavour::ClaudeCode => vec![
                "-p".to_string(),
                task.prompt.clone(),
                "--output-format".to_string(),
                "stream-json".to_string(),
                "--verbose".to_string(),
            ],
        };

        if let Some(allowed) = &task.allowed_tools {
            // An empty allowlist means "no tools", which is a real and useful request. Sending no
            // flag at all would mean "every tool", so the two must not collapse.
            argv.push("--allowedTools".to_string());
            argv.push(allowed.iter().map(SmolStr::as_str).collect::<Vec<_>>().join(","));
        }

        argv.extend(self.extra_args.iter().cloned());
        argv
    }
}

impl AgentProvider for AgentCli {
    fn id(&self) -> AgentId {
        self.id.clone()
    }

    async fn delegate(&self, task: DelegatedTask) -> Result<AgentEventStream, DelegationError> {
        use tokio::io::AsyncBufReadExt;

        let argv = self.argv(&task);
        let id = self.id.clone();

        let mut child = tokio::process::Command::new(&self.program)
            .args(&argv)
            .current_dir(&task.workspace)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| DelegationError::Unavailable {
                agent: id.clone(),
                detail: format!("could not start `{}`: {e}", self.program),
            })?;

        let stdout = child.stdout.take().ok_or_else(|| DelegationError::Unavailable {
            agent: id.clone(),
            detail: "the child produced no stdout".into(),
        })?;
        let flavour = self.flavour;
        let timeout = std::time::Duration::from_millis(task.timeout_ms);

        let stream = async_stream(move |mut yielder| async move {
            let mut lines = tokio::io::BufReader::new(stdout).lines();
            let deadline = tokio::time::Instant::now() + timeout;
            let mut finished = false;

            loop {
                match tokio::time::timeout_at(deadline, lines.next_line()).await {
                    // The timeout `DelegatedTask` has always described and nothing enforced. The
                    // child is killed rather than left running: an abandoned agent process keeps
                    // spending the user's subscription on work nobody will read.
                    Err(_elapsed) => {
                        let _ = child.start_kill();
                        yielder
                            .send(AgentEvent::Failed {
                                detail: format!(
                                    "agent `{id}` produced no output for {}ms",
                                    timeout.as_millis()
                                ),
                            })
                            .await;
                        return;
                    }
                    Ok(Err(e)) => {
                        yielder
                            .send(AgentEvent::Failed {
                                detail: format!("could not read output: {e}"),
                            })
                            .await;
                        return;
                    }
                    Ok(Ok(None)) => break,
                    Ok(Ok(Some(line))) => {
                        for event in parse_event(flavour, &line) {
                            if matches!(event, AgentEvent::Finished { .. }) {
                                finished = true;
                            }
                            yielder.send(event).await;
                        }
                    }
                }
            }

            // A child that dies without saying it finished has failed, and saying so beats a
            // truncated transcript that looks complete. The exit status is the only evidence left.
            if !finished {
                let detail = match child.wait().await {
                    Ok(status) if status.success() => {
                        "the agent exited successfully but never reported a result".to_string()
                    }
                    Ok(status) => format!("the agent exited with {status}"),
                    Err(e) => format!("the agent could not be waited on: {e}"),
                };
                yielder.send(AgentEvent::Failed { detail }).await;
            }
        });

        Ok(Box::pin(stream))
    }
}

/// Parse one line of agent output into zero or more events.
///
/// Returns a vector because a single assistant message can carry several content blocks — text and
/// two tool uses in one line is ordinary — and flattening them here keeps the stream in the order
/// the agent produced them.
#[must_use]
pub fn parse_event(flavour: Flavour, line: &str) -> Vec<AgentEvent> {
    let line = line.trim();
    if line.is_empty() {
        return Vec::new();
    }
    let Ok(value) = serde_json::from_str::<Value>(line) else {
        // Not every line is JSON: a CLI may print a banner or a warning. Dropping unparseable lines
        // is deliberate, because treating one as a failure would make an agent's first cosmetic
        // change break every delegation.
        return Vec::new();
    };

    match flavour {
        Flavour::ClaudeCode => parse_claude_code(&value),
    }
}

fn parse_claude_code(value: &Value) -> Vec<AgentEvent> {
    let mut events = Vec::new();

    match value.get("type").and_then(Value::as_str) {
        Some("assistant") => {
            let message = value.get("message").unwrap_or(&Value::Null);
            if let Some(content) = message.get("content").and_then(Value::as_array) {
                for block in content {
                    match block.get("type").and_then(Value::as_str) {
                        Some("text") => {
                            if let Some(text) = block.get("text").and_then(Value::as_str)
                                && !text.is_empty()
                            {
                                events.push(AgentEvent::Text(text.to_string()));
                            }
                        }
                        Some("tool_use") => {
                            if let Some(name) = block.get("name").and_then(Value::as_str) {
                                events.push(AgentEvent::ToolUsed { name: name.into() });
                            }
                        }
                        _ => {}
                    }
                }
            }
            if let Some(usage) = message.get("usage") {
                events.push(AgentEvent::Usage(Box::new(claude_usage(usage, None))));
            }
        }
        Some("result") => {
            let is_error = value.get("is_error").and_then(Value::as_bool).unwrap_or(false);
            // The vendor reports cost here or not at all. Frey does not estimate another agent's
            // spend: an invented number in a ledger is worse than a gap, because a gap is visible.
            let cost = value.get("total_cost_usd").and_then(Value::as_f64);
            if let Some(usage) = value.get("usage") {
                events.push(AgentEvent::Usage(Box::new(claude_usage(usage, cost))));
            } else if let Some(cost) = cost {
                events.push(AgentEvent::Usage(Box::new(Usage {
                    reported_cost: Some(Money::usd(cost)),
                    ..Usage::default()
                })));
            }
            events.push(AgentEvent::Finished { ok: !is_error });
        }
        Some("error") => {
            let detail = value
                .get("error")
                .and_then(Value::as_str)
                .or_else(|| value.get("message").and_then(Value::as_str))
                .unwrap_or("the agent reported an error with no detail");
            events.push(AgentEvent::Failed { detail: detail.to_string() });
        }
        // `system`/`init` and `user` (tool results echoed back) carry nothing Frey should report as
        // its own. Silence here is correct rather than lossy.
        _ => {}
    }

    events
}

fn claude_usage(value: &Value, cost: Option<f64>) -> Usage {
    let get = |key: &str| value.get(key).and_then(Value::as_u64).unwrap_or(0);
    let mut usage = Usage {
        // Anthropic's `input_tokens` already excludes cached tokens, so unlike the OpenAI-shaped
        // adapters nothing is subtracted here. Getting this backwards double-counts every cached
        // prefix in the ledger.
        input: get("input_tokens"),
        output: get("output_tokens"),
        cache_read: get("cache_read_input_tokens"),
        cache_write: get("cache_creation_input_tokens"),
        ..Usage::default()
    };
    usage.reported_cost = cost.map(Money::usd);
    usage
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task(prompt: &str) -> DelegatedTask {
        DelegatedTask {
            prompt: prompt.to_string(),
            workspace: std::env::temp_dir(),
            allowed_tools: None,
            timeout_ms: 30_000,
        }
    }

    fn events(lines: &[&str]) -> Vec<AgentEvent> {
        lines.iter().flat_map(|l| parse_event(Flavour::ClaudeCode, l)).collect()
    }

    /// The property Anthropic's usage policy makes non-negotiable: Frey never holds, forwards, or
    /// mints a vendor credential. There is nowhere in the public API to put one, and nothing is
    /// added to the child's command line. The child authenticates itself.
    ///
    /// Written as a test rather than a comment because the failure mode is someone adding an
    /// `api_key` field in good faith to "make it easier".
    #[test]
    fn frey_supplies_no_credentials() {
        let argv = AgentCli::claude_code().argv(&task("summarise the readme"));
        let joined = argv.join(" ").to_lowercase();
        for forbidden in ["--api-key", "api_key", "authorization", "bearer", "sk-ant", "token"] {
            assert!(
                !joined.contains(forbidden),
                "`{forbidden}` must never reach the argv: {joined}"
            );
        }
    }

    /// A prompt is one argv element, so shell metacharacters in it are inert. There is no shell
    /// here to escape into — which is why the prompt is never interpolated into a command string.
    #[test]
    fn a_prompt_is_one_argument_however_hostile_it_looks() {
        let nasty = "read the file; rm -rf / && echo `whoami`";
        let argv = AgentCli::claude_code().argv(&task(nasty));
        assert!(argv.contains(&nasty.to_string()), "passed through whole: {argv:?}");
    }

    /// An empty allowlist means "no tools", which is a real request. Sending no flag would mean
    /// "every tool", so collapsing the two would silently grant everything.
    #[test]
    fn an_empty_allowlist_is_not_the_same_as_no_allowlist() {
        let mut none = task("go");
        none.allowed_tools = Some(Vec::new());
        let with_empty = AgentCli::claude_code().argv(&none);
        assert!(with_empty.iter().any(|a| a == "--allowedTools"));

        let unset = AgentCli::claude_code().argv(&task("go"));
        assert!(!unset.iter().any(|a| a == "--allowedTools"));
    }

    #[test]
    fn assistant_text_and_tool_use_arrive_in_order() {
        let line = r#"{"type":"assistant","message":{"role":"assistant","content":[
            {"type":"text","text":"Looking now."},
            {"type":"tool_use","id":"t1","name":"Read","input":{"file_path":"a.rs"}}
        ]}}"#;
        let got = events(&[line]);
        assert_eq!(got[0], AgentEvent::Text("Looking now.".into()));
        assert_eq!(got[1], AgentEvent::ToolUsed { name: "Read".into() });
    }

    /// Anthropic's `input_tokens` excludes cached tokens, unlike the OpenAI-shaped adapters where
    /// they are subtracted. Getting this backwards double-counts every cached prefix in the ledger.
    #[test]
    fn cached_tokens_are_not_subtracted_for_this_vendor() {
        let line = r#"{"type":"assistant","message":{"content":[],"usage":{
            "input_tokens":100,"output_tokens":50,
            "cache_read_input_tokens":900,"cache_creation_input_tokens":200}}}"#;
        let got = events(&[line]);
        let AgentEvent::Usage(usage) = &got[0] else { panic!("expected usage, got {got:?}") };
        assert_eq!(usage.input, 100, "reported as given, not reduced");
        assert_eq!(usage.cache_read, 900);
        assert_eq!(usage.cache_write, 200);
        assert_eq!(usage.total_input(), 1_200);
    }

    #[test]
    fn a_result_reports_the_vendors_cost_and_finishes() {
        let line = r#"{"type":"result","subtype":"success","is_error":false,
            "total_cost_usd":0.0421,"usage":{"input_tokens":10,"output_tokens":20}}"#;
        let got = events(&[line]);
        let AgentEvent::Usage(usage) = &got[0] else { panic!("expected usage: {got:?}") };
        assert_eq!(usage.reported_cost, Some(Money::usd(0.0421)));
        assert_eq!(got[1], AgentEvent::Finished { ok: true });
    }

    /// Frey does not estimate another agent's spend. A vendor that reports no cost produces no
    /// number, because an invented one in a ledger is worse than a gap — a gap is visible.
    #[test]
    fn a_result_without_a_cost_invents_none() {
        let line = r#"{"type":"result","is_error":false,"usage":{"input_tokens":10}}"#;
        let got = events(&[line]);
        let AgentEvent::Usage(usage) = &got[0] else { panic!("expected usage: {got:?}") };
        assert_eq!(usage.reported_cost, None);
    }

    #[test]
    fn an_error_result_is_not_reported_as_success() {
        let line = r#"{"type":"result","subtype":"error_max_turns","is_error":true}"#;
        assert_eq!(events(&[line]), vec![AgentEvent::Finished { ok: false }]);
    }

    /// A CLI prints banners, warnings and progress that are not JSON. Treating one as a failure
    /// would make a cosmetic change in the vendor's output break every delegation.
    #[test]
    fn non_json_chatter_is_ignored_rather_than_fatal() {
        assert!(events(&["Welcome to Claude Code!", "", "   "]).is_empty());
    }

    /// `system`/`init` and the echoed `user` turns carry nothing Frey should claim as its own
    /// event. Silence is the correct output, not a fallback.
    #[test]
    fn housekeeping_lines_produce_nothing() {
        let init = r#"{"type":"system","subtype":"init","tools":["Read","Bash"]}"#;
        let echoed = r#"{"type":"user","message":{"content":[{"type":"tool_result"}]}}"#;
        assert!(events(&[init, echoed]).is_empty());
    }

    /// A missing binary must say which one, so the answer is "install it" rather than "something
    /// went wrong". This exercises the real spawn path.
    #[test]
    fn a_missing_binary_names_itself() {
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let cli = AgentCli::claude_code().program("frey-no-such-agent-binary");
        let result = runtime.block_on(cli.delegate(task("hello")));
        let Err(DelegationError::Unavailable { detail, .. }) = result else {
            panic!("expected Unavailable");
        };
        assert!(detail.contains("frey-no-such-agent-binary"), "{detail}");
    }
}
