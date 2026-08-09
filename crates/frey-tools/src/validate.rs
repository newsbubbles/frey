//! Checking the model's arguments against the tool's schema, before the tool sees them.
//!
//! `Invocation`'s documentation has always said the tool layer validates arguments before dispatch,
//! on the grounds that strict schema adherence is not a guarantee on most providers. It said so
//! while nothing actually did it, which live testing found the hard way: a small model sent
//! `"arguments": "null"`, the provider adapter turned an unparseable argument string into
//! `Value::Null`, and that reached tool code as a perfectly ordinary — and completely empty —
//! argument object. Every tool author was left to hand-roll the same checks, and a tool whose
//! parameters are all optional would have run happily on garbage.
//!
//! This is a deliberately small subset of JSON Schema: `type`, `required`, `properties`, `enum`,
//! and `additionalProperties: false`. That is not a shortcut so much as the actual shape of tool
//! argument schemas, and a full validator would mean a heavyweight dependency in the hot path of
//! every tool call for constructs — `$ref`, `allOf`, `patternProperties` — that a tool definition
//! has no business containing.
//!
//! The output is a [`ToolError`] rather than a bool, because the model is the one who has to fix
//! it. Naming the field and saying what was expected turns a retry-with-identical-arguments into a
//! corrected call, which is the difference between a weak model being unusable and being slow.

use frey_core::error::{ToolError, ToolErrorKind};
use frey_core::tool_def::JsonSchema;
use serde_json::Value;

/// Check `args` against `schema`.
///
/// # Errors
/// Returns a [`ToolError`] of kind [`ToolErrorKind::InvalidArgs`] naming the first problem found,
/// with guidance the model can act on.
pub fn check_arguments(schema: &JsonSchema, args: &Value) -> Result<(), ToolError> {
    let schema = schema.as_value();

    // An object schema with no declared properties accepts anything; that is how a no-argument tool
    // is spelled, and models signal "no arguments" as `null`, `{}`, and occasionally `"null"`.
    let properties = schema.get("properties").and_then(Value::as_object);
    let required: Vec<&str> = schema
        .get("required")
        .and_then(Value::as_array)
        .map(|r| r.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();

    let Some(object) = args.as_object() else {
        if args.is_null() && required.is_empty() {
            return Ok(());
        }
        return Err(ToolError::new(
            ToolErrorKind::InvalidArgs,
            format!("arguments must be a JSON object, but were {}", describe(args)),
        )
        .guide(if required.is_empty() {
            "Send `{}` when a tool takes no arguments.".to_string()
        } else {
            format!("Send a JSON object with these fields: {}.", required.join(", "))
        }));
    };

    for name in &required {
        if !object.contains_key(*name) || object.get(*name).is_some_and(Value::is_null) {
            return Err(ToolError::new(
                ToolErrorKind::InvalidArgs,
                format!("required argument `{name}` is missing"),
            )
            .guide(format!(
                "Call the tool again with `{name}` set. Required arguments: {}.",
                required.join(", ")
            )));
        }
    }

    let Some(properties) = properties else { return Ok(()) };

    if schema.get("additionalProperties") == Some(&Value::Bool(false)) {
        for key in object.keys() {
            if !properties.contains_key(key) {
                let known: Vec<&str> = properties.keys().map(String::as_str).collect();
                return Err(ToolError::new(
                    ToolErrorKind::InvalidArgs,
                    format!("`{key}` is not an argument this tool accepts"),
                )
                .guide(format!("Accepted arguments are: {}.", known.join(", "))));
            }
        }
    }

    for (key, value) in object {
        let Some(spec) = properties.get(key) else { continue };
        // An absent optional argument sent explicitly as null is the same as not sending it.
        if value.is_null() {
            continue;
        }

        if let Some(expected) = spec.get("type").and_then(Value::as_str)
            && !matches_type(expected, value)
        {
            return Err(ToolError::new(
                ToolErrorKind::InvalidArgs,
                format!("`{key}` must be {}, but was {}", article(expected), describe(value)),
            )
            .guide(format!("Send `{key}` as {}.", article(expected))));
        }

        if let Some(choices) = spec.get("enum").and_then(Value::as_array)
            && !choices.contains(value)
        {
            let rendered: Vec<String> = choices.iter().map(ToString::to_string).collect();
            return Err(ToolError::new(
                ToolErrorKind::InvalidArgs,
                format!("`{key}` is not one of the permitted values"),
            )
            .guide(format!("`{key}` must be one of: {}.", rendered.join(", "))));
        }
    }

    Ok(())
}

/// Whether `value` satisfies a JSON Schema `type` keyword.
fn matches_type(expected: &str, value: &Value) -> bool {
    match expected {
        "object" => value.is_object(),
        "array" => value.is_array(),
        "string" => value.is_string(),
        "boolean" => value.is_boolean(),
        // `integer` is a number with no fractional part. Models routinely send `3.0` meaning `3`,
        // and rejecting that is pedantry that costs a round trip.
        "integer" => value.as_i64().is_some() || value.as_f64().is_some_and(|f| f.fract() == 0.0),
        "number" => value.is_number(),
        "null" => value.is_null(),
        // An unrecognised type keyword is not the model's fault, so it is not the model's problem.
        _ => true,
    }
}

/// A human name for what arrived, for the error message.
fn describe(value: &Value) -> String {
    match value {
        Value::Null => "null".into(),
        Value::Bool(_) => "a boolean".into(),
        Value::Number(_) => "a number".into(),
        Value::String(s) => format!("the string {s:?}"),
        Value::Array(_) => "an array".into(),
        Value::Object(_) => "an object".into(),
    }
}

/// `"a string"`, `"an object"` — so messages read as sentences.
fn article(type_name: &str) -> String {
    match type_name {
        "object" | "array" | "integer" => format!("an {type_name}"),
        other => format!("a {other}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn schema(value: serde_json::Value) -> JsonSchema {
        JsonSchema::new(value).unwrap()
    }

    fn station_schema() -> JsonSchema {
        schema(serde_json::json!({
            "type": "object",
            "properties": {
                "station": {"type": "string"},
                "offset": {"type": "integer"}
            },
            "required": ["station"],
            "additionalProperties": false
        }))
    }

    #[test]
    fn well_formed_arguments_pass() {
        let args = serde_json::json!({"station": "alpha", "offset": 3});
        assert!(check_arguments(&station_schema(), &args).is_ok());
    }

    /// The exact shape that reached tool code during live testing. A small model sent
    /// `"arguments": "null"`; the adapter parsed that to `Value::Null`; nothing objected.
    #[test]
    fn null_arguments_are_refused_when_the_tool_requires_some() {
        let error = check_arguments(&station_schema(), &Value::Null).unwrap_err();
        assert_eq!(error.kind(), ToolErrorKind::InvalidArgs);
        assert!(
            error.model().summary.contains("must be a JSON object"),
            "{}",
            error.model().summary
        );
        assert!(
            error.model().guidance.as_deref().is_some_and(|g| g.contains("station")),
            "the guidance names the field the model has to supply"
        );
    }

    /// A tool that takes no arguments is the common case for a lister, and models spell "nothing"
    /// three different ways. None of them is an error.
    #[test]
    fn null_is_fine_when_nothing_is_required() {
        let empty = schema(serde_json::json!({"type": "object", "properties": {}}));
        assert!(check_arguments(&empty, &Value::Null).is_ok());
        assert!(check_arguments(&empty, &serde_json::json!({})).is_ok());
    }

    #[test]
    fn a_missing_required_argument_is_named() {
        let args = serde_json::json!({"offset": 3});
        let error = check_arguments(&station_schema(), &args).unwrap_err();
        assert!(error.model().summary.contains("`station` is missing"));
    }

    /// Explicitly passing null for a required field is the same failure as omitting it, and models
    /// do this constantly.
    #[test]
    fn an_explicit_null_does_not_satisfy_a_required_argument() {
        let args = serde_json::json!({"station": null});
        assert!(check_arguments(&station_schema(), &args).is_err());
    }

    #[test]
    fn a_wrong_type_says_what_was_wanted_and_what_arrived() {
        let args = serde_json::json!({"station": 42});
        let error = check_arguments(&station_schema(), &args).unwrap_err();
        let summary = error.model().summary.clone();
        assert!(summary.contains("`station` must be a string"), "{summary}");
        assert!(summary.contains("a number"), "{summary}");
    }

    /// `3.0` means `3`. Rejecting it is pedantry that costs a round trip, and models emit it often
    /// because JSON has one number type.
    #[test]
    fn a_whole_float_satisfies_an_integer() {
        let args = serde_json::json!({"station": "alpha", "offset": 3.0});
        assert!(check_arguments(&station_schema(), &args).is_ok());

        let fractional = serde_json::json!({"station": "alpha", "offset": 3.5});
        assert!(check_arguments(&station_schema(), &fractional).is_err());
    }

    #[test]
    fn an_unknown_argument_is_refused_and_the_real_ones_listed() {
        let args = serde_json::json!({"station": "alpha", "statoin": "typo"});
        let error = check_arguments(&station_schema(), &args).unwrap_err();
        let guidance = error.model().guidance.clone().unwrap_or_default();
        assert!(guidance.contains("station"), "{guidance}");
        assert!(guidance.contains("offset"), "{guidance}");
    }

    /// Without `additionalProperties: false` the schema is permissive, and Frey does not invent a
    /// stricter contract than the tool author wrote.
    #[test]
    fn extra_arguments_are_allowed_when_the_schema_permits_them() {
        let permissive = schema(serde_json::json!({
            "type": "object",
            "properties": {"station": {"type": "string"}},
            "required": ["station"]
        }));
        let args = serde_json::json!({"station": "alpha", "whatever": 1});
        assert!(check_arguments(&permissive, &args).is_ok());
    }

    #[test]
    fn an_enum_lists_the_permitted_values() {
        let with_enum = schema(serde_json::json!({
            "type": "object",
            "properties": {"mode": {"type": "string", "enum": ["read", "write"]}}
        }));
        let error =
            check_arguments(&with_enum, &serde_json::json!({"mode": "delete"})).unwrap_err();
        let guidance = error.model().guidance.clone().unwrap_or_default();
        assert!(guidance.contains("read"), "{guidance}");
        assert!(guidance.contains("write"), "{guidance}");
    }

    /// An optional argument explicitly set to null is how models say "not applicable". It must not
    /// trip the type check.
    #[test]
    fn an_optional_null_is_ignored_rather_than_type_checked() {
        let args = serde_json::json!({"station": "alpha", "offset": null});
        assert!(check_arguments(&station_schema(), &args).is_ok());
    }
}
