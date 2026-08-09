//! Tool plumbing for [Frey](https://github.com/newsbubbles/frey).
//!
//! Everything an agent can do arrives here — native functions, MCP tools, skill scripts,
//! sub-agents, remote peers — and passes through the same layers. That is what makes the security
//! layers unavoidable rather than opt-in: there is no second path to executing anything.
//!
//! ```
//! use frey_tools::prelude::*;
//! use frey_core::tool_def::{JsonSchema, ToolDefinition};
//!
//! let def = ToolDefinition::new(
//!     "fs_read",
//!     "Read a file from the workspace and return its contents",
//!     JsonSchema::empty_object(),
//! );
//! // Risk comes from what the tool declared, never from the model's account of its own intent.
//! assert_eq!(risk_of(&def), frey_core::error::Risk::Low);
//! ```

pub mod builtin;
pub mod layer;
pub mod registry;

pub use frey_macros::tool;

/// Implementation details the `#[frey::tool]` macro expands into.
///
/// Not a public API. It is `pub` because macro output must name it, and it is documented so that
/// anyone reading expanded code can tell what they are looking at.
#[doc(hidden)]
pub mod __private {
    pub use schemars;
    pub use serde;
    pub use serde_json::Value;

    use frey_core::capability::{Capability, HostPattern, PathScope, ProgramScope, SecretName};
    pub use frey_core::error::ToolError;
    use frey_core::error::ToolErrorKind;
    pub use frey_core::tool::ToolContent;
    pub use frey_core::tool_def::ToolDefinition;
    use frey_core::tool_def::{CallerPolicy, CostHint, JsonSchema, PresentationHint};

    /// Generate a JSON Schema for a type, as JSON Schema 2020-12.
    #[must_use]
    pub fn schema_for<T: schemars::JsonSchema>() -> JsonSchema {
        let schema = schemars::schema_for!(T);
        JsonSchema::new(serde_json::to_value(schema).unwrap_or_else(|_| serde_json::json!({})))
            .unwrap_or_else(|_| JsonSchema::empty_object())
    }

    /// Assemble a definition from what the macro parsed.
    #[must_use]
    pub fn build_definition(
        name: &str,
        description: &str,
        input_schema: JsonSchema,
        capabilities: &[&str],
        cost_hint: &str,
        caller: &str,
        presentation: &str,
    ) -> ToolDefinition {
        let mut def = ToolDefinition::new(name, description, input_schema);
        def.capabilities = capabilities.iter().filter_map(|c| parse_capability(c)).collect();
        def.cost_hint = match cost_hint {
            "pure" => CostHint::Pure,
            "expensive" => CostHint::Expensive,
            "destructive" => CostHint::Destructive,
            _ => CostHint::Cheap,
        };
        def.caller = match caller {
            "code" => CallerPolicy::CodeOnly,
            "both" => CallerPolicy::Both,
            _ => CallerPolicy::Direct,
        };
        def.presentation = match presentation {
            "always" => PresentationHint::Always,
            "code_only" => PresentationHint::CodeOnly,
            "hidden" => PresentationHint::Hidden,
            _ => PresentationHint::Deferred,
        };
        def
    }

    /// Parse a capability written as a short string in the attribute, e.g. `"fs:read(./src)"`.
    fn parse_capability(spec: &str) -> Option<Capability> {
        let (kind, arg) = match spec.split_once('(') {
            Some((k, rest)) => (k.trim(), rest.trim_end_matches(')').trim()),
            None => (spec.trim(), ""),
        };
        Some(match kind {
            "fs:read" => Capability::FsRead(PathScope::new([arg_or(arg, "./")]).ok()?),
            "fs:write" => Capability::FsWrite(PathScope::new([arg_or(arg, "./")]).ok()?),
            "net:egress" => Capability::NetEgress(HostPattern::new(arg).ok()?),
            "exec" => Capability::Exec(ProgramScope::new(arg.split(',').map(str::trim))),
            "secret" => Capability::Secret(SecretName(arg.into())),
            _ => return None,
        })
    }

    /// Test-only access to the capability parser, so its behaviour is pinned without making
    /// the parser itself part of the public API.
    #[doc(hidden)]
    #[must_use]
    pub fn parse_capability_for_test(spec: &str) -> Option<Capability> {
        parse_capability(spec)
    }

    fn arg_or<'a>(arg: &'a str, fallback: &'a str) -> &'a str {
        if arg.is_empty() { fallback } else { arg }
    }

    /// Decode the model's arguments, turning a mismatch into an error the model can act on.
    ///
    /// # Errors
    /// Returns [`ToolError`] carrying the expected shape, because most providers' "strict" mode is
    /// best-effort and the model needs to be told exactly what it got wrong.
    pub fn decode_args<T: serde::de::DeserializeOwned + schemars::JsonSchema>(
        args: Value,
        tool: &str,
    ) -> Result<T, ToolError> {
        serde_json::from_value(args).map_err(|e| {
            ToolError::new(ToolErrorKind::InvalidArgs, format!("`{tool}` got bad arguments: {e}"))
                .guide("Re-read the schema and call again with the corrected arguments.")
                .schema_hint(schema_for::<T>().as_value().clone())
        })
    }

    /// Turn whatever a tool returned into content the model can read.
    pub fn into_content<T: IntoToolContent>(value: T) -> ToolContent {
        value.into_tool_content()
    }

    /// How a tool's return type becomes model-visible content.
    pub trait IntoToolContent {
        /// Render it.
        fn into_tool_content(self) -> ToolContent;
    }

    impl IntoToolContent for String {
        fn into_tool_content(self) -> ToolContent {
            ToolContent::text(self)
        }
    }

    impl IntoToolContent for ToolContent {
        fn into_tool_content(self) -> ToolContent {
            self
        }
    }

    impl IntoToolContent for Value {
        fn into_tool_content(self) -> ToolContent {
            ToolContent::text(self.to_string()).with_structured(self)
        }
    }
}

/// The types most callers want.
pub mod prelude {
    pub use crate::builtin::{AllowedProgram, InWorkspace, OnEgressAllowlist, ParsedJson};
    pub use crate::layer::{
        ApprovalLayer, ApprovalPolicy, PolicyLayer, RedactLayer, ToolService, TruncateLayer,
        risk_of,
    };
    pub use crate::registry::ToolRegistry;
    pub use crate::tool;
}

#[cfg(test)]
mod tests {
    use super::__private::{build_definition, parse_capability_for_test, schema_for};
    use frey_core::capability::Capability;
    use frey_core::tool_def::{CostHint, PresentationHint};

    #[derive(schemars::JsonSchema, serde::Deserialize)]
    #[allow(dead_code)]
    struct Args {
        /// Path relative to the workspace root.
        path: String,
        /// Text encoding, defaulting to utf-8.
        encoding: Option<String>,
    }

    #[test]
    fn generated_schemas_carry_parameter_descriptions() {
        // The reason the macro reads doc comments at all: tool search matches on argument names
        // and descriptions, so an undocumented parameter is lost search surface.
        let schema = schema_for::<Args>();
        assert!(schema.undocumented_properties().is_empty(), "{:?}", schema.as_value());
        assert!(schema.searchable_text().contains("workspace root"));
    }

    #[test]
    fn capability_strings_parse_into_real_capabilities() {
        assert!(matches!(parse_capability_for_test("fs:read(./src)"), Some(Capability::FsRead(_))));
        assert!(matches!(
            parse_capability_for_test("net:egress(api.github.com)"),
            Some(Capability::NetEgress(_))
        ));
        assert!(matches!(parse_capability_for_test("exec(git, cargo)"), Some(Capability::Exec(_))));
        assert_eq!(parse_capability_for_test("nonsense"), None);
        // Wildcards are rejected at construction, so a wildcard egress capability cannot exist.
        assert_eq!(parse_capability_for_test("net:egress(*.github.com)"), None);
    }

    #[test]
    fn definitions_default_to_deferred_and_cheap() {
        let def = build_definition(
            "fs_read",
            "Read a file from the workspace",
            schema_for::<Args>(),
            &[],
            "cheap",
            "direct",
            "deferred",
        );
        assert_eq!(def.presentation, PresentationHint::Deferred);
        assert_eq!(def.cost_hint, CostHint::Cheap);
    }
}
