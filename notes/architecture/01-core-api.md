# Frey — Core API (draft 1)

*Type sketches, not final code. The point is to pin down semantics before anything compiles.
Every type here lives in `frey-core` unless noted. `frey-core` depends only on
`serde`, `serde_json`, `schemars`, `thiserror`, `bytes`, and `smol_str`.*

---

## 1. The conversation model — items, not messages

Decided in research 02 §5. This is the highest-leverage decision in the codebase.

```rust
/// A single addressable unit of conversation state.
/// Providers are *projections* of this, never the source of truth.
#[non_exhaustive]
pub enum Item {
    Text(TextItem),
    Media(MediaItem),                 // image / audio / document / video
    Reasoning(ReasoningItem),
    ToolCall(ToolCallItem),
    ToolResult(ToolResultItem),
    Discovery(DiscoveryItem),         // tool_reference expansion, skill load, catalog delta
    /// Anything a provider emitted that we do not model. Round-tripped byte-for-byte.
    Opaque(OpaqueItem),
}

pub struct ReasoningItem {
    pub summary: Option<Tainted<String, ModelDerived>>,
    pub visibility: ReasoningVisibility,   // Plain | Redacted | Encrypted
    /// Provider-owned blob (e.g. OpenAI `encrypted_content`, Anthropic thinking signature).
    /// MUST be replayed verbatim on the next request or the model loses its chain of thought.
    pub carry: Option<ProviderCarry>,
}

pub struct ToolCallItem {
    pub id: CallId,                   // Anthropic tool_use.id | OpenAI call_id
    pub name: ToolName,
    pub args: serde_json::Value,
    pub caller: Caller,               // Direct | Code { runner_id } | Frontend | SubAgent
    pub presented_as: PresentationRef,// which catalog entry / discovery event produced it
}

pub struct ToolResultItem {
    pub id: CallId,
    pub outcome: ToolOutcomeWire,     // Ok(content) | Error(ModelMessage) | Denied(ModelMessage)
    pub elapsed: Duration,
    pub bytes_elided: u64,            // >0 means we truncated; the model is told how much
}

pub struct OpaqueItem {
    pub provider: ProviderId,
    pub kind: SmolStr,                // provider's own discriminator
    pub raw: Box<serde_json::value::RawValue>,
}
```

**Conformance test that must exist from commit 1:** for every provider, for a corpus of recorded
responses, `parse(raw) -> Vec<Item> -> serialize()` must be byte-identical to `raw`.
If that test is hard to write, the model is wrong.

```rust
pub struct Turn {
    pub role: Role,
    pub items: Vec<Item>,
    pub usage: Option<Usage>,
    pub marks: Vec<CacheMark>,        // advisory; the adapter realises them
}
```

---

## 2. Usage & money

```rust
pub struct Usage {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub reasoning: u64,
    /// Only what the provider actually reported. NEVER computed here.
    pub reported_cost: Option<Money>,
    pub raw: Box<serde_json::value::RawValue>,
}

pub struct CostEstimate { pub amount: Money, pub source: PricingSource, pub confidence: Confidence }
```

`UsageLedger` (in `frey-agent`) aggregates by `(run, turn, provider, model, tool, cache_class)`
and exports OTel `gen_ai.*` metrics. **Estimates are always labelled as estimates.**

Provider mapping (research 02):

| Frey field | Anthropic | OpenAI Responses | OpenRouter |
|---|---|---|---|
| `input` | `input_tokens` *(post-last-breakpoint only!)* | `input_tokens` | `prompt_tokens` |
| `cache_read` | `cache_read_input_tokens` | `input_tokens_details.cached_tokens` | `prompt_tokens_details.cached_tokens` |
| `cache_write` | `cache_creation_input_tokens` (+ `cache_creation.ephemeral_{5m,1h}_input_tokens`) | `cache_write_tokens` (GPT-5.6+) | `prompt_tokens_details.cache_write_tokens` |
| `reported_cost` | — (none) | — (none) | `cost` (+ `cost_details.upstream_inference_cost`, BYOK only) |

---

## 3. Cache marks & the plan

```rust
pub struct CacheMark { pub at: SegmentId, pub ttl: CacheTtl, pub priority: u8 }
pub enum CacheTtl { Short, Long }     // Short≈5m/30m, Long≈1h/24h — provider maps it

pub struct Segment {
    pub id: SegmentId,
    pub kind: SegmentKind,            // Tools | System | Skills | History | Discovered
    pub stability: Stability,         // Static | Slow | Volatile
    pub hash: Blake3,
    pub est_tokens: u64,
}

pub struct CachePlan {
    pub segments: Vec<Segment>,
    pub breakpoints: Vec<SegmentId>,  // ordered; Long before Short (Anthropic rule)
    pub warnings: Vec<CacheWarning>,  // ChurnDetected{segment, prev_hash}, BelowMinPrefix{need},
                                      // TooManyBreakpoints, BreakpointOnVolatile
}
```

`CachePlanner::plan(&catalog, &history, &ProviderCapabilities) -> CachePlan` is a **pure function**
— trivially unit-testable, and the place every provider quirk from research 02 §1–3 is encoded.

---

## 4. Taint / information flow

```rust
pub struct Tainted<T, L: Label> { value: T, prov: Provenance, _l: PhantomData<L> }

pub trait Label { const INTEGRITY: Integrity; const CONFIDENTIALITY: Confidentiality; }

pub struct Trusted;        // operator-authored: system prompt, config, code
pub struct ModelDerived;   // the model wrote it
pub struct LowIntegrity;   // tool output, fetched page, MCP description, sub-agent output
pub struct Confidential;   // read from a secret-scoped capability

pub struct Provenance { pub source: SourceId, pub via: Vec<SourceId>, pub at: SeqId }

impl<T, L: Label> Tainted<T, L> {
    pub fn map<U>(self, f: impl FnOnce(T) -> U) -> Tainted<U, L>;   // label propagates
    /// The ONLY way to raise integrity. Every call site is enumerable via `grep declassify`
    /// and every invocation is written to the audit log with file/line.
    #[track_caller]
    pub fn declassify(self, why: Declassification, auth: &Authority) -> T;
}

/// Parsers are the honest declassifiers: narrowing the type IS the check.
pub trait Validator<T> { fn validate(&self, raw: &str) -> Result<T, ValidationError>; }
```

Session-level Rule of Two (Meta, research 04 §2):

```rust
pub struct SessionPowers { untrusted_input: bool, confidential_access: bool, mutating_egress: bool }
impl SessionPowers {
    pub fn check(&self) -> Result<(), RuleOfTwoViolation>;  // all three ⇒ Err
}
```
Resolution options when all three are held: refuse, require human escalation (recorded),
or fork a fresh sub-session that drops one power. All three are supported; the default is
**require escalation**.

---

## 5. Capabilities

```rust
#[non_exhaustive]
pub enum Capability {
    FsRead(PathScope), FsWrite(PathScope),
    NetEgress(HostPattern),           // concrete hosts only; resolved once at sandbox start
    Exec(ProgramScope),
    Secret(SecretName),               // resolved by the supervisor — never handed to the tool
    Spend(Budget),
    Mcp(ServerId, ToolPattern),
    Delegate(AgentId),
}

pub struct Grant { pub cap: Capability, pub granted_by: Authority, pub expires: Option<Instant> }
```
No ambient authority: a tool that did not declare `fs:write` cannot obtain it at runtime, and
`Secret` is never materialised inside a sandbox — the supervisor performs the authenticated call
on the tool's behalf (Cloudflare Code Mode's binding model, research 01 §4).

---

## 6. Errors (R9) — typed by audience

```rust
pub struct ToolError {
    pub kind: ToolErrorKind,
    /// Goes back into the context. This is the model's *only* view of the failure.
    pub model: ModelMessage,
    /// Logged/traced. Never enters the context.
    pub operator: Diagnostic,
    /// Optional UI surface.
    pub user: Option<Presentation>,
    pub retry: RetryDirective,
}

pub struct ModelMessage {
    pub summary: String,
    /// The custom "further instruction" from the brief: what the model should DO next.
    pub guidance: Option<String>,
    pub suggested_tools: Vec<ToolName>,
    pub schema_hint: Option<serde_json::Value>,
}

pub enum ToolErrorKind { InvalidArgs, NotFound, Denied, Conflict, Timeout, Transient, Fatal, Cancelled }
pub enum RetryDirective { Never, Immediate { budgeted: bool }, After(Duration), RequiresInput }

pub enum ToolOutcome {
    Ok(Tainted<Content, LowIntegrity>),
    Failed(ToolError),                        // model sees it, may retry
    Denied(ToolError),                        // policy said no; model told, operator alerted
    NeedsInput(InputRequests),                // → MRTR / AG-UI interrupt / durable suspend
}
```

Ergonomic constructors so the common case is one line:

```rust
Err(tool_err!(NotFound, "no file at {path}")
        .guide("List the directory with `fs_list` before reading.")
        .suggest(["fs_list"]))
```

Provider-side, the same audience split applies: `ProviderError::{Auth, Billing, RateLimit,
Overloaded, BadRequest, Protocol, Network}` where **`Auth` and `Billing` are non-retryable and
loud** (OpenRouter 402 silently degrading runs is a known, verified failure mode).

---

## 7. Tools & toolsets

```rust
#[dynosaur::dynosaur(DynTool)]
pub trait Tool: Send + Sync + 'static {
    fn definition(&self) -> &ToolDefinition;
    async fn call(&self, inv: Invocation, cx: &ToolCx) -> ToolOutcome;
}

pub struct ToolDefinition {
    pub name: ToolName,
    pub description: String,
    pub input_schema: schemars::Schema,       // JSON Schema 2020-12
    pub output_schema: Option<schemars::Schema>,
    pub capabilities: Vec<Capability>,
    pub caller: CallerPolicy,                 // Direct | CodeOnly | Both  → maps to allowed_callers
    pub presentation: PresentationHint,       // Always | Deferred | CodeOnly | Hidden
    pub cost_hint: CostHint,                  // Pure | Cheap | Expensive | Destructive
    pub examples: Vec<ToolExample>,           // maps to Anthropic input_examples
}

#[dynosaur::dynosaur(DynToolset)]
pub trait Toolset: Send + Sync + 'static {
    async fn definitions(&self, cx: &StepCx) -> Result<Vec<ToolDefinition>, ToolsetError>;
    async fn call(&self, inv: Invocation, cx: &ToolCx) -> ToolOutcome;
    fn instructions(&self, cx: &StepCx) -> Option<String> { None }
    async fn for_run(&self, cx: &RunCx) -> Result<Arc<dyn DynToolset>, ToolsetError>;
}
```

Layers are `tower::Layer`s over `Service<Invocation, Response = ToolOutcome, Error = Infallible>`
(errors are *values*, because the model must see them). Shipped stack in
`ToolStack::production()`: Namespace → Filter → Policy → Approval → Retry → Timeout →
Concurrency → Cache → Sandbox → Redact → Audit.

`CostHint::Destructive` is not decoration: `ApprovalLayer` defaults to requiring approval for it,
and the approval prompt shows the **literal** action (argv, URL, SQL) — never a natural-language
summary, per research 04 §2 layer 8.

---

## 8. Providers

```rust
#[dynosaur::dynosaur(DynModelProvider)]
pub trait ModelProvider: Send + Sync + 'static {
    fn id(&self) -> ProviderId;
    fn capabilities(&self, model: &ModelId) -> ProviderCapabilities;
    async fn complete(&self, req: Request, cx: &CallCx) -> Result<Response, ProviderError>;
    async fn stream(&self, req: Request, cx: &CallCx)
        -> Result<BoxStream<'static, Result<StreamEvent, ProviderError>>, ProviderError>;
    fn count_tokens(&self, req: &Request) -> Result<TokenCount, ProviderError>;
}

pub struct ProviderCapabilities {
    pub tool_search: ToolSearchSupport,       // Native{max_results} | None
    pub programmatic_tool_calling: bool,
    pub cache: CacheSupport,                  // Explicit{max_breakpoints, ttls, min_prefix_fn} | Automatic{min} | None
    pub reasoning: ReasoningSupport,          // None | Opaque | Encrypted
    pub strict_schema: StrictSupport,         // Always | Attempted | None
    pub parallel_tool_calls: bool,
    pub modalities: ModalitySet,
    pub max_context: u32,
    pub max_output: u32,
}
```

`AgentProvider` is deliberately a *different* trait — it cannot be asked for token completion:

```rust
pub trait AgentProvider: Send + Sync + 'static {
    fn id(&self) -> AgentId;
    /// Spawns/attaches to the vendor's own process. Frey never sees the vendor's credentials.
    async fn delegate(&self, task: DelegatedTask, cx: &RunCx)
        -> Result<BoxStream<'static, AgentEvent>, DelegationError>;
}
```
Implementations: `ClaudeAgent` (Agent SDK / `claude -p`), `CodexAgent` (Codex CLI/SDK).
Both are **subprocess adapters**. Frey never stores, mints, or replays a vendor subscription
OAuth token; see research 02 §4 for the policy basis.

### Config-defined providers (R3, no Rust required)

```toml
[provider.my-vllm]
dialect   = "openai_chat"          # openai_responses | openai_chat | anthropic_messages
base_url  = "https://llm.internal/v1"
auth      = { kind = "bearer", env = "MY_VLLM_KEY" }

[provider.my-vllm.capabilities]    # overrides the dialect defaults
cache = "none"
strict_schema = "none"
parallel_tool_calls = false

[provider.my-vllm.pricing]
input = "0.0"; output = "0.0"      # estimates only; reported_cost stays None
```

---

## 9. Events (internal bus **is** the AG-UI model)

```rust
#[non_exhaustive]
pub enum Event {
    RunStarted { run: RunId, parent: Option<SpanId> },
    TurnStarted { turn: TurnId },
    TextDelta { text: String },                  // droppable under backpressure
    ReasoningDelta { text: String },             // droppable
    ToolCallStarted { call: CallId, name: ToolName, args_preview: Json },
    ToolCallFinished { call: CallId, outcome: OutcomeSummary },
    Discovery { found: Vec<ToolName>, via: SearchKind },
    NeedsInput(InputRequests),                   // approvals, MRTR, frontend tools
    StateDelta(JsonPatch),                       // AG-UI shared state
    UsageUpdated(Usage),
    Warning(FreyWarning),                        // cache churn, budget pressure, degraded capability
    RunFinished { usage: UsageLedgerSnapshot, cost: Option<CostEstimate> },
}
```
Backpressure rule: **presentation events (`*Delta`) may be dropped; semantic events may not.**
That is the fix for the surveyed "streaming through agent trees" gap.

---

## 10. The 90% case

```rust
use frey::prelude::*;

#[frey::tool(description = "Get the weather at a location")]
async fn get_weather(
    /// City name or "lat,lon".
    location: String,
) -> Result<Weather, ToolError> { /* … */ }

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let agent = Agent::builder()
        .model("anthropic:claude-opus-5")          // or "openrouter:…", "agent:claude"
        .system("You are a careful assistant.")
        .tool(get_weather)
        .mcp("github", McpServer::http("https://mcp.github.com"))  // stateless 2026-07-28
        .skills_dir("./skills")
        .context_budget(ContextBudget::auto())     // planner picks breakpoints
        .sandbox(Sandbox::workspace("./work"))     // fails closed if unavailable
        .build()?;

    let run = agent.run("What's the weather in Rabat, and open an issue if it's over 40C?").await?;
    println!("{}", run.output_text());
    println!("{}", run.usage().explain());          // per-tool, per-turn, cache hit rate, cost
    Ok(())
}
```

Everything in that snippet that could silently cost money or leak data is *visible*:
the model string names the provider, `sandbox()` is mandatory for `exec`-capable tools,
and `usage().explain()` is one call.

---

## 11. What is deliberately NOT here in v1

- A2A (declare "planned", don't half-ship interop).
- A vector-store zoo — one trait, one reference impl, adapters live outside the workspace.
- A durable-execution engine — the run journal covers replay; Temporal/Restate adapters later.
- Fine-tuning, evals-as-a-service, a hosted gateway (and note: OpenRouter ToS §7 forbids
  reselling model access, so a Frey gateway must never become a resale surface).
