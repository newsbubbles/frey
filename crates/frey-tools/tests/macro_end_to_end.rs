//! The `#[frey::tool]` macro, exercised the way a user would write it.
//!
//! The unit tests in `frey-macros` check the parsing. This checks the thing that actually matters:
//! that a plainly-written async function becomes a tool with a correct schema, and that the doc
//! comments on its parameters survive into that schema — because tool search matches on argument
//! names and descriptions, so a parameter whose description was lost is a tool that got harder to
//! find.

use frey_core::error::{ToolError, ToolErrorKind};
use frey_core::tool_def::CostHint;

/// Read a file from the workspace and return its contents as text.
#[frey_tools::tool(capabilities("fs:read(./src)"), cost_hint = "cheap")]
async fn fs_read(
    /// Path relative to the workspace root.
    path: String,
    /// Text encoding to decode with. Defaults to utf-8 when omitted.
    encoding: Option<String>,
) -> Result<String, ToolError> {
    if path.starts_with('/') {
        return Err(ToolError::new(
            ToolErrorKind::Denied,
            "absolute paths are outside the workspace",
        )
        .guide("Use a path relative to the workspace root."));
    }
    Ok(format!("contents of {path} as {}", encoding.unwrap_or_else(|| "utf-8".into())))
}

/// Delete every file under a path. There is no undo.
#[frey_tools::tool(capabilities("fs:write(./out)"), cost_hint = "destructive")]
async fn fs_purge(
    /// Directory to empty, relative to the workspace root.
    directory: String,
) -> Result<String, ToolError> {
    Ok(format!("purged {directory}"))
}

#[test]
fn a_plain_function_becomes_a_well_described_tool() {
    let def = FsReadTool::definition();
    assert_eq!(def.name.as_str(), "fs_read");
    assert!(def.description.starts_with("Read a file from the workspace"));
    assert_eq!(def.cost_hint, CostHint::Cheap);
}

#[test]
fn parameter_doc_comments_reach_the_schema() {
    // The whole reason the macro reads doc comments. Without these, the tool is invisible to a
    // search for "encoding" or "workspace root".
    let def = FsReadTool::definition();
    assert!(
        def.input_schema.undocumented_properties().is_empty(),
        "every parameter must be documented: {:?}",
        def.input_schema.as_value()
    );

    let searchable = def.searchable_text();
    assert!(searchable.contains("workspace root"), "{searchable}");
    assert!(searchable.contains("utf-8"), "{searchable}");
}

#[test]
fn the_generated_tool_is_discoverable_by_its_own_standard() {
    assert!(
        FsReadTool::definition().discoverability().is_clean(),
        "{:?}",
        FsReadTool::definition().discoverability().problems
    );
}

#[test]
fn declared_capabilities_become_real_capabilities() {
    let def = FsReadTool::definition();
    assert_eq!(def.capabilities.len(), 1);
    assert!(matches!(def.capabilities[0], frey_core::capability::Capability::FsRead(_)));
}

#[test]
fn a_destructive_tool_is_gated_by_default() {
    let def = FsPurgeTool::definition();
    assert_eq!(def.cost_hint, CostHint::Destructive);
    assert!(def.needs_approval_by_default());
    assert_eq!(frey_tools::layer::risk_of(&def), frey_core::error::Risk::High);

    // And the underlying function still works normally, gate or no gate.
    assert_eq!(pollster::block_on(fs_purge("out".into())).unwrap(), "purged out");
}

#[test]
fn valid_arguments_run_the_function() {
    let out = pollster::block_on(FsReadTool::invoke(
        serde_json::json!({"path": "src/main.rs", "encoding": "latin-1"}),
    ))
    .expect("valid arguments");
    assert_eq!(out.text, "contents of src/main.rs as latin-1");
}

#[test]
fn an_omitted_optional_argument_uses_the_functions_own_default() {
    let out = pollster::block_on(FsReadTool::invoke(serde_json::json!({"path": "a.txt"})))
        .expect("encoding is optional");
    assert!(out.text.ends_with("as utf-8"));
}

#[test]
fn bad_arguments_produce_an_error_the_model_can_act_on() {
    // Most providers' strict mode is best-effort, so this path is reached in practice and the
    // model needs the schema back, not just a complaint.
    let err = pollster::block_on(FsReadTool::invoke(serde_json::json!({"pth": "typo"})))
        .expect_err("a missing required field must fail");
    assert_eq!(err.kind(), ToolErrorKind::InvalidArgs);
    assert!(err.model().schema_hint.is_some(), "the model must be shown the right shape");
    assert!(err.model().guidance.is_some());
}

#[test]
fn the_functions_own_errors_pass_through_unchanged() {
    let err = pollster::block_on(FsReadTool::invoke(serde_json::json!({"path": "/etc/passwd"})))
        .expect_err("the function rejects absolute paths");
    assert_eq!(err.kind(), ToolErrorKind::Denied);
    assert!(err.model().guidance.as_deref().unwrap().contains("relative"));
}

#[test]
fn the_original_function_is_still_callable_directly() {
    // The macro adds; it does not replace. Unit-testing the logic should not require going through
    // JSON.
    let out = pollster::block_on(fs_read("a.txt".into(), None)).unwrap();
    assert!(out.contains("a.txt"));
}
