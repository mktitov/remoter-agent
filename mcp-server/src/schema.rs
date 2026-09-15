//! JSON Schema sanitization for the advertised tool input schemas.
//!
//! rmcp 0.6 generates `input_schema` via schemars 0.8, which emits the OpenAPI
//! 3.0 `nullable` keyword: `{"type": "integer", "nullable": true}` for optional
//! scalars and, worse, an anyOf branch `{"const": null, "nullable": true}` for
//! optional enums (`taskKind` on create_task/update_task). `nullable` is not a
//! JSON Schema keyword — the MCP spec mandates JSON Schema for `inputSchema` —
//! and strict MCP clients reject such tools during argument validation, which
//! made `create_task`/`update_task` entirely unusable in them ("nullable"
//! cannot be used without "type").
//!
//! Rewriting at the single point where tools are advertised (`list_tools`)
//! avoids a major rmcp upgrade. The rewrite is semantics-preserving:
//! - `{"type": T, "nullable": true}` → `{"type": [T, "null"]}` (T string or array);
//! - `{"const": null, "nullable": true}` → `{"const": null}` (already valid);
//! - `"nullable": false` → dropped (the JSON Schema default).

use std::sync::Arc;

use rmcp::model::Tool;
use serde_json::{Map, Value};

/// Returns `tools` with every input schema rewritten to spec-conformant JSON
/// Schema (see module docs). Applied by `list_tools` after role gating.
pub fn sanitize_tools(tools: Vec<Tool>) -> Vec<Tool> {
    tools
        .into_iter()
        .map(|mut tool| {
            tool.input_schema = Arc::new(sanitize_tool_schema(&tool.input_schema));
            tool
        })
        .collect()
}

fn sanitize_tool_schema(schema: &Map<String, Value>) -> Map<String, Value> {
    match sanitize_value(&Value::Object(schema.clone())) {
        Value::Object(map) => map,
        _ => unreachable!("an object schema stays an object"),
    }
}

fn sanitize_value(value: &Value) -> Value {
    match value {
        Value::Object(map) => Value::Object(sanitize_object(map)),
        Value::Array(items) => Value::Array(items.iter().map(sanitize_value).collect()),
        scalar => scalar.clone(),
    }
}

fn sanitize_object(map: &Map<String, Value>) -> Map<String, Value> {
    let nullable = map.get("nullable") == Some(&Value::Bool(true));
    let mut out = Map::with_capacity(map.len());
    for (key, value) in map {
        if key == "nullable" {
            continue;
        }
        if nullable && key == "type" {
            out.insert(key.clone(), nullable_type(value));
            continue;
        }
        out.insert(key.clone(), sanitize_value(value));
    }
    out
}

/// `T` (string or array of strings) → the same with `"null"` added.
fn nullable_type(type_value: &Value) -> Value {
    match type_value {
        Value::String(t) => Value::Array(vec![Value::String(t.clone()), Value::String("null".into())]),
        Value::Array(types) => {
            let mut types = types.clone();
            if !types.iter().any(|t| t == "null") {
                types.push(Value::String("null".into()));
            }
            Value::Array(types)
        }
        other => other.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sanitize(v: Value) -> Value {
        sanitize_value(&v)
    }

    #[test]
    fn optional_scalar_becomes_type_union() {
        let schema = json!({"type": "integer", "format": "int32", "nullable": true});
        assert_eq!(
            sanitize(schema),
            json!({"type": ["integer", "null"], "format": "int32"})
        );
    }

    #[test]
    fn const_null_branch_loses_nullable() {
        let schema = json!({
            "anyOf": [
                {"$ref": "#/definitions/TaskKindParam"},
                {"const": null, "nullable": true}
            ]
        });
        assert_eq!(
            sanitize(schema),
            json!({"anyOf": [{"$ref": "#/definitions/TaskKindParam"}, {"const": null}]})
        );
    }

    #[test]
    fn nullable_false_is_dropped() {
        assert_eq!(
            sanitize(json!({"type": "string", "nullable": false})),
            json!({"type": "string"})
        );
    }

    #[test]
    fn existing_type_array_is_not_duplicated() {
        let schema = json!({"type": ["string", "null"], "nullable": true});
        assert_eq!(sanitize(schema), json!({"type": ["string", "null"]}));
    }

    #[test]
    fn nested_properties_are_reached() {
        let schema = json!({
            "type": "object",
            "properties": {
                "featureId": {"type": "integer", "nullable": true},
                "title": {"type": "string"}
            }
        });
        assert_eq!(
            sanitize(schema),
            json!({
                "type": "object",
                "properties": {
                    "featureId": {"type": ["integer", "null"]},
                    "title": {"type": "string"}
                }
            })
        );
    }

    #[test]
    fn definitions_subschemas_are_reached() {
        let schema = json!({
            "definitions": {"Thing": {"type": "object", "properties": {"x": {"type": "integer", "nullable": true}}}}
        });
        assert_eq!(
            sanitize(schema),
            json!({"definitions": {"Thing": {"type": "object", "properties": {"x": {"type": ["integer", "null"]}}}}})
        );
    }
}
