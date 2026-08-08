//! How a tool is described, and how it is presented to a model.
//!
//! The description is not documentation. It is the *search index*: Anthropic's tool search matches
//! on tool names, descriptions, **argument names, and argument descriptions**, so a tool whose
//! parameters are undocumented is invisible once a catalog grows past the point where everything
//! fits in context. That makes discoverability a measurable property of a catalog rather than a
//! style preference, which is what [`ToolDefinition::discoverability`] exists to measure.

use serde_json::Value;
use smol_str::SmolStr;

use crate::capability::Capability;
use crate::ids::ToolName;

/// A JSON Schema 2020-12 document.
///
/// Held as a plain [`Value`] rather than a `schemars` type so that `frey-core` stays dependency
/// light and so that MCP's wire format — which since `2026-07-28` accepts any 2020-12 keywords — is
/// representable exactly. `frey-macros` generates these with `schemars`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub struct JsonSchema(Value);

/// A value was not usable as a schema.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SchemaError {
    /// The document was not a JSON object.
    #[error("a JSON Schema must be an object, got {0}")]
    NotAnObject(&'static str),
}

impl JsonSchema {
    /// Wrap a JSON Schema document.
    ///
    /// # Errors
    /// Returns [`SchemaError::NotAnObject`] if the value is not an object.
    pub fn new(value: Value) -> Result<Self, SchemaError> {
        if value.is_object() {
            Ok(Self(value))
        } else {
            Err(SchemaError::NotAnObject(type_name(&value)))
        }
    }

    /// A schema taking no arguments.
    #[must_use]
    pub fn empty_object() -> Self {
        Self(serde_json::json!({"type": "object", "properties": {}}))
    }

    /// The underlying document.
    #[must_use]
    pub fn as_value(&self) -> &Value {
        &self.0
    }

    /// The declared property names, in document order.
    #[must_use]
    pub fn property_names(&self) -> Vec<&str> {
        self.0
            .get("properties")
            .and_then(Value::as_object)
            .map(|m| m.keys().map(String::as_str).collect())
            .unwrap_or_default()
    }

    /// Property names that have no `description`. These are the ones invisible to tool search.
    #[must_use]
    pub fn undocumented_properties(&self) -> Vec<&str> {
        self.0
            .get("properties")
            .and_then(Value::as_object)
            .map(|m| {
                m.iter()
                    .filter(|(_, v)| {
                        v.get("description").and_then(Value::as_str).is_none_or(str::is_empty)
                    })
                    .map(|(k, _)| k.as_str())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Every word a tool search would index from this schema: property names and their
    /// descriptions.
    #[must_use]
    pub fn searchable_text(&self) -> String {
        let Some(props) = self.0.get("properties").and_then(Value::as_object) else {
            return String::new();
        };
        let mut out = String::new();
        for (name, spec) in props {
            out.push_str(name);
            out.push(' ');
            if let Some(d) = spec.get("description").and_then(Value::as_str) {
                out.push_str(d);
                out.push(' ');
            }
        }
        out
    }
}

fn type_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    }
}

/// Who may invoke a tool. Maps to Anthropic's `allowed_callers`.
///
/// Anthropic's documentation is explicit that `allowed_callers` is **not** a security boundary —
/// it guides the model. Frey therefore enforces this client-side in the policy layer, which is a
/// real, statable security property rather than a restatement of the provider's hint.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CallerPolicy {
    /// The model calls it directly.
    #[default]
    Direct,
    /// Only reachable from a code-mode script.
    CodeOnly,
    /// Either. Anthropic advise against this: picking one gives the model clearer guidance.
    Both,
}

impl CallerPolicy {
    /// Whether a direct model call is permitted.
    #[must_use]
    pub fn allows_direct(self) -> bool {
        matches!(self, Self::Direct | Self::Both)
    }

    /// Whether a call from inside a code-mode sandbox is permitted.
    #[must_use]
    pub fn allows_code(self) -> bool {
        matches!(self, Self::CodeOnly | Self::Both)
    }
}

/// How a capability should occupy the context window.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PresentationHint {
    /// Always in the stable prefix. Reserve for the three to five hottest tools.
    Always,
    /// Indexed for search; the definition is injected only when discovered.
    #[default]
    Deferred,
    /// Never presented as a callable tool; reachable only from code mode.
    CodeOnly,
    /// Present in the registry but invisible this step.
    Hidden,
}

/// Roughly what calling a tool costs, and how badly it could go.
///
/// Drives the default approval policy and the risk shown to a human. Derived from the tool's
/// declaration, **never** from the model's own opinion of what it is about to do.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Default,
    serde::Serialize,
    serde::Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum CostHint {
    /// No side effects, cheap, safe to memoise.
    Pure,
    /// Reads the world. Cheap.
    #[default]
    Cheap,
    /// Slow or costs money.
    Expensive,
    /// Changes state irreversibly, or leaves the machine.
    Destructive,
}

/// An example invocation. Maps to Anthropic's `input_examples`, which are expanded alongside a
/// definition when tool search discovers it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ToolExample {
    /// What the example demonstrates.
    pub description: String,
    /// The arguments.
    pub args: Value,
}

/// Everything Frey knows about a tool.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ToolDefinition {
    /// The name the model calls, after namespacing.
    pub name: ToolName,
    /// What it does, in the model's terms. Indexed by tool search.
    pub description: String,
    /// Argument schema.
    pub input_schema: JsonSchema,
    /// Result schema, when the tool has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_schema: Option<JsonSchema>,
    /// What the tool needs in order to run. Nothing else is available to it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub capabilities: Vec<Capability>,
    /// Who may call it.
    #[serde(default)]
    pub caller: CallerPolicy,
    /// How it should occupy context.
    #[serde(default)]
    pub presentation: PresentationHint,
    /// How expensive and how dangerous.
    #[serde(default)]
    pub cost_hint: CostHint,
    /// Example invocations.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub examples: Vec<ToolExample>,
}

impl ToolDefinition {
    /// A minimal definition: name, description, and argument schema.
    pub fn new(
        name: impl Into<ToolName>,
        description: impl Into<String>,
        input_schema: JsonSchema,
    ) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            input_schema,
            output_schema: None,
            capabilities: Vec::new(),
            caller: CallerPolicy::default(),
            presentation: PresentationHint::default(),
            cost_hint: CostHint::default(),
            examples: Vec::new(),
        }
    }

    /// Everything a tool search would index: name, description, argument names, and argument
    /// descriptions.
    #[must_use]
    pub fn searchable_text(&self) -> String {
        format!("{} {} {}", self.name, self.description, self.input_schema.searchable_text())
    }

    /// The namespace prefix, if the name has one (`github_list_issues` → `github`).
    ///
    /// Consistent prefixes let one search match a whole service's tools, which is why they are
    /// checked rather than merely suggested.
    #[must_use]
    pub fn namespace(&self) -> Option<&str> {
        self.name.as_str().split_once('_').map(|(head, _)| head)
    }

    /// Whether a human should approve calls by default.
    #[must_use]
    pub fn needs_approval_by_default(&self) -> bool {
        self.cost_hint == CostHint::Destructive
            || self.capabilities.iter().any(Capability::is_mutating_or_egress)
    }

    /// Assess how findable this tool is once the catalog outgrows the context window.
    #[must_use]
    pub fn discoverability(&self) -> DiscoverabilityReport {
        let mut problems = Vec::new();

        if self.description.trim().is_empty() {
            problems.push(Discoverability::NoDescription);
        } else if self.description.split_whitespace().count() < MIN_DESCRIPTION_WORDS {
            problems.push(Discoverability::ThinDescription {
                words: self.description.split_whitespace().count(),
            });
        }

        let undocumented: Vec<SmolStr> =
            self.input_schema.undocumented_properties().into_iter().map(SmolStr::new).collect();
        if !undocumented.is_empty() {
            problems.push(Discoverability::UndocumentedParameters { names: undocumented });
        }

        if self.namespace().is_none() && self.presentation == PresentationHint::Deferred {
            problems.push(Discoverability::NoNamespace);
        }

        DiscoverabilityReport { problems }
    }
}

/// Descriptions shorter than this are treated as too thin to retrieve reliably.
const MIN_DESCRIPTION_WORDS: usize = 6;

/// A reason a tool would be hard to find.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Discoverability {
    /// No description at all.
    NoDescription,
    /// Too few words to match a paraphrased query.
    ThinDescription {
        /// How many words the description has.
        words: usize,
    },
    /// Parameters with no `description`. Tool search indexes these, so an undocumented parameter is
    /// lost search surface.
    UndocumentedParameters {
        /// Which parameters.
        names: Vec<SmolStr>,
    },
    /// A deferred tool with no `service_` prefix, so one search cannot match its whole group.
    NoNamespace,
}

/// The result of a discoverability check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoverabilityReport {
    /// Everything that would hurt retrieval.
    pub problems: Vec<Discoverability>,
}

impl DiscoverabilityReport {
    /// Whether the tool is well described.
    #[must_use]
    pub fn is_clean(&self) -> bool {
        self.problems.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capability::{Capability, PathScope};

    fn schema_with_docs() -> JsonSchema {
        JsonSchema::new(serde_json::json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "Path relative to the workspace root."},
                "encoding": {"type": "string", "description": "Text encoding, defaulting to utf-8."}
            },
            "required": ["path"]
        }))
        .unwrap()
    }

    #[test]
    fn schemas_must_be_objects() {
        assert!(JsonSchema::new(serde_json::json!("a string")).is_err());
        assert!(JsonSchema::new(serde_json::json!([1, 2])).is_err());
        assert!(JsonSchema::new(serde_json::json!({})).is_ok());
    }

    #[test]
    fn searchable_text_includes_argument_names_and_descriptions() {
        let def = ToolDefinition::new(
            "fs_read",
            "Read a file from the workspace and return its contents",
            schema_with_docs(),
        );
        let text = def.searchable_text();
        // Anthropic's tool search matches all four of these fields.
        assert!(text.contains("fs_read"), "name");
        assert!(text.contains("Read a file"), "description");
        assert!(text.contains("encoding"), "argument name");
        assert!(text.contains("utf-8"), "argument description");
    }

    #[test]
    fn undocumented_parameters_are_reported_as_lost_search_surface() {
        let schema = JsonSchema::new(serde_json::json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "Where to read from."},
                "mode": {"type": "string"},
                "limit": {"type": "integer", "description": ""}
            }
        }))
        .unwrap();
        let mut undocumented = schema.undocumented_properties();
        undocumented.sort_unstable();
        assert_eq!(undocumented, ["limit", "mode"], "an empty description counts as missing");
    }

    #[test]
    fn discoverability_flags_the_things_that_actually_hurt_retrieval() {
        let bad = ToolDefinition::new(
            "doit",
            "Does it",
            JsonSchema::new(serde_json::json!({
                "type": "object",
                "properties": {"x": {"type": "string"}}
            }))
            .unwrap(),
        );
        let report = bad.discoverability();
        assert!(!report.is_clean());
        assert!(report.problems.contains(&Discoverability::ThinDescription { words: 2 }));
        assert!(
            report
                .problems
                .contains(&Discoverability::UndocumentedParameters { names: vec!["x".into()] })
        );
        assert!(report.problems.contains(&Discoverability::NoNamespace));
    }

    #[test]
    fn a_well_described_namespaced_tool_is_clean() {
        let good = ToolDefinition::new(
            "fs_read",
            "Read a file from the workspace and return its contents as text",
            schema_with_docs(),
        );
        assert!(good.discoverability().is_clean(), "{:?}", good.discoverability().problems);
        assert_eq!(good.namespace(), Some("fs"));
    }

    #[test]
    fn caller_policy_maps_to_allowed_callers() {
        assert!(CallerPolicy::Direct.allows_direct());
        assert!(!CallerPolicy::Direct.allows_code());
        assert!(CallerPolicy::CodeOnly.allows_code());
        assert!(!CallerPolicy::CodeOnly.allows_direct());
        assert!(CallerPolicy::Both.allows_direct() && CallerPolicy::Both.allows_code());
    }

    #[test]
    fn approval_defaults_come_from_the_declaration_not_the_model() {
        let mut def =
            ToolDefinition::new("fs_read", "Read a file from the workspace", schema_with_docs());
        def.capabilities = vec![Capability::FsRead(PathScope::new(["./"]).unwrap())];
        assert!(!def.needs_approval_by_default(), "reading is not gated by default");

        def.capabilities = vec![Capability::FsWrite(PathScope::new(["./"]).unwrap())];
        assert!(def.needs_approval_by_default(), "writing is");

        let mut destructive =
            ToolDefinition::new("db_drop", "Drop a database table permanently", schema_with_docs());
        destructive.cost_hint = CostHint::Destructive;
        assert!(destructive.needs_approval_by_default(), "so is anything declared destructive");
    }

    #[test]
    fn definitions_round_trip() {
        let mut def = ToolDefinition::new(
            "github_list_issues",
            "List open issues on a GitHub repository, newest first",
            schema_with_docs(),
        );
        def.presentation = PresentationHint::Deferred;
        def.caller = CallerPolicy::CodeOnly;
        def.cost_hint = CostHint::Expensive;
        def.examples = vec![ToolExample {
            description: "issues on the frey repo".into(),
            args: serde_json::json!({"path": "newsbubbles/frey"}),
        }];

        let decoded: ToolDefinition =
            serde_json::from_str(&serde_json::to_string(&def).unwrap()).unwrap();
        assert_eq!(decoded, def);
    }
}
