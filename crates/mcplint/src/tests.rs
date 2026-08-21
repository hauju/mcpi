//! Each rule proven on a hand-built snapshot: one clean contract that must
//! stay silent, then one violation per rule.

use super::*;
use serde_json::json;

fn snapshot_with(tools: Value) -> Snapshot {
    serde_json::from_value(json!({
        "protocol_version": SPEC,
        "server_name": "fixture",
        "server_version": "0.0.0",
        "tools": tools,
    }))
    .expect("snapshot shape")
}

/// A tool that follows every rule, so any finding against it is a lint bug.
fn clean_tool() -> Value {
    json!({
        "description": "Searches the docs.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": "What to search for." }
            },
            "additionalProperties": false
        },
        "annotations": { "readOnlyHint": true }
    })
}

fn rules(findings: &[Finding]) -> Vec<&'static str> {
    findings.iter().map(|f| f.rule).collect()
}

#[test]
fn a_clean_contract_stays_silent() {
    let findings = lint(&snapshot_with(json!({ "search_docs": clean_tool() })));
    assert!(findings.is_empty(), "unexpected findings: {findings:?}");
}

#[test]
fn a_missing_input_schema_is_a_warning() {
    let findings = lint(&snapshot_with(json!({
        "broken": { "description": "d", "annotations": {} }
    })));
    assert!(rules(&findings).contains(&"input-schema-invalid"));
    assert_eq!(findings[0].level, Level::Warning);
}

#[test]
fn a_null_input_schema_is_named_as_null() {
    let findings = lint(&snapshot_with(json!({
        "broken": { "description": "d", "annotations": {}, "inputSchema": null }
    })));
    assert!(findings[0].fact.contains("null"));
}

#[test]
fn a_bad_name_is_a_note_that_quotes_the_character() {
    let mut tool = clean_tool();
    tool["inputSchema"] = json!({ "type": "object", "additionalProperties": false });
    let findings = lint(&snapshot_with(json!({ "search docs": tool })));
    let name = findings
        .iter()
        .find(|f| f.rule == "tool-name-format")
        .expect("name finding");
    assert_eq!(name.level, Level::Note);
    assert!(name.fact.contains("' '"), "fact was: {}", name.fact);
}

#[test]
fn an_overlong_name_reports_its_length() {
    let long = "a".repeat(129);
    let mut tool = clean_tool();
    tool["inputSchema"] = json!({ "type": "object", "additionalProperties": false });
    let mut tools = serde_json::Map::new();
    tools.insert(long, tool);
    let findings = lint(&snapshot_with(Value::Object(tools)));
    let name = findings
        .iter()
        .find(|f| f.rule == "tool-name-format")
        .expect("name finding");
    assert!(name.fact.contains("129"));
}

#[test]
fn an_open_no_arg_schema_is_a_note() {
    let findings = lint(&snapshot_with(json!({
        "ping": {
            "description": "d",
            "annotations": {},
            "inputSchema": { "type": "object" }
        }
    })));
    assert_eq!(rules(&findings), vec!["no-arg-schema-open"]);
}

#[test]
fn the_recommended_no_arg_form_is_silent() {
    let findings = lint(&snapshot_with(json!({
        "ping": {
            "description": "d",
            "annotations": {},
            "inputSchema": { "type": "object", "additionalProperties": false }
        }
    })));
    assert!(findings.is_empty(), "unexpected findings: {findings:?}");
}

#[test]
fn header_rules_catch_syntax_duplicates_and_number() {
    let findings = lint(&snapshot_with(json!({
        "call_api": {
            "description": "d",
            "annotations": {},
            "inputSchema": {
                "type": "object",
                "properties": {
                    "region": {
                        "type": "string",
                        "description": "d",
                        "x-mcp-header": "X Region"
                    },
                    "a": {
                        "type": "string",
                        "description": "d",
                        "x-mcp-header": "X-Tenant"
                    },
                    "b": {
                        "type": "string",
                        "description": "d",
                        "x-mcp-header": "x-tenant"
                    },
                    "amount": {
                        "type": "number",
                        "description": "d",
                        "x-mcp-header": "X-Amount"
                    }
                },
                "additionalProperties": false
            }
        }
    })));
    let rules = rules(&findings);
    assert!(rules.contains(&"header-name-syntax"), "{findings:?}");
    assert!(rules.contains(&"header-name-duplicate"), "{findings:?}");
    assert!(rules.contains(&"header-type"), "{findings:?}");
    // Client-side MUSTs: every header fact names the consequence.
    for finding in findings.iter().filter(|f| f.cite.contains("x-mcp-header")) {
        assert!(finding.fact.contains("drops this tool"), "{}", finding.fact);
    }
}

#[test]
fn a_header_behind_a_ref_is_still_checked() {
    let findings = lint(&snapshot_with(json!({
        "call_api": {
            "description": "d",
            "annotations": {},
            "inputSchema": {
                "type": "object",
                "properties": {
                    "auth": { "$ref": "#/$defs/Auth", "description": "d" }
                },
                "additionalProperties": false,
                "$defs": {
                    "Auth": { "type": "number", "x-mcp-header": "X-Auth" }
                }
            }
        }
    })));
    assert!(rules(&findings).contains(&"header-type"), "{findings:?}");
}

#[test]
fn a_declared_output_schema_must_be_an_object() {
    let mut tool = clean_tool();
    tool["outputSchema"] = json!("not a schema");
    let findings = lint(&snapshot_with(json!({ "search_docs": tool })));
    assert_eq!(rules(&findings), vec!["output-schema-invalid"]);
    assert_eq!(findings[0].level, Level::Warning);
}

#[test]
fn missing_descriptions_are_counted_by_name() {
    let findings = lint(&snapshot_with(json!({
        "quiet": {
            "annotations": {},
            "inputSchema": {
                "type": "object",
                "properties": {
                    "a": { "type": "string" },
                    "b": { "type": "string", "description": "d" }
                },
                "additionalProperties": false
            }
        }
    })));
    let rules = rules(&findings);
    assert!(rules.contains(&"tool-description-missing"));
    let properties = findings
        .iter()
        .find(|f| f.rule == "property-descriptions-missing")
        .expect("property finding");
    assert!(properties.fact.contains("1 of 2"), "{}", properties.fact);
    assert!(properties.fact.contains('a'));
}

#[test]
fn absent_annotations_aggregate_into_one_server_note() {
    let mut tool = clean_tool();
    tool.as_object_mut().unwrap().remove("annotations");
    let findings = lint(&snapshot_with(json!({
        "one": tool.clone(),
        "two": tool,
        "three": clean_tool(),
    })));
    let note = findings
        .iter()
        .find(|f| f.rule == "tool-annotations-absent")
        .expect("aggregate note");
    assert_eq!(note.tool, None);
    assert!(note.fact.contains("2 of 3"), "{}", note.fact);
}

#[test]
fn warnings_sort_before_notes_within_a_tool() {
    let findings = lint(&snapshot_with(json!({
        "bad name": { "description": "d", "annotations": {} }
    })));
    assert_eq!(findings[0].rule, "input-schema-invalid");
    assert_eq!(findings[0].level, Level::Warning);
}

#[test]
fn the_text_report_groups_by_tool_and_states_the_spec() {
    let findings = lint(&snapshot_with(json!({
        "broken": { "description": "d" }
    })));
    let text = to_text("fixture 0.0.0", &findings);
    assert!(text.contains(SPEC));
    assert!(text.contains("## broken"));
    assert!(text.contains("## server"), "annotations note group: {text}");
}
