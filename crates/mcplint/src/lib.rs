//! Static lint of an MCP server's advertised contract.
//!
//! Pure logic over [`schemadiff::Snapshot`], the same raw-`Value` map the
//! differ reads — no I/O, no rmcp. That matters twice over: any of the four
//! CLI sources can be linted, and fields from spec revisions newer than our
//! client (`x-mcp-header`) are still present to check, because the snapshot
//! never deserialized them away.
//!
//! Every finding is a fact with a citation, never a verdict: `(rule, cite,
//! fact)` and a [`Level`] that says only whether the spec word behind it is
//! MUST or SHOULD. There is deliberately no score, grade, or total — these
//! findings appear next to other people's servers, and "4 tools return no
//! annotations" is durable where "quality: C−" starts a fight.
//!
//! Scope is the statically checkable half of the 2026-07-28 tool rules.
//! Behavioral rules — error-code semantics, `outputSchema` conformance of
//! live results, `tools/list` ordering — need a session and belong to `probe`.
//! Two static sub-rules are also out: name uniqueness (the snapshot is keyed
//! by name, so duplicates were collapsed before we could see them) and
//! `x-mcp-header` root-reachability (the spec's reachability definition is
//! more precise than we can honestly reimplement from memory).

use schemadiff::Snapshot;
use serde_json::Value;

#[cfg(test)]
mod tests;

/// The spec revision the rules cite.
pub const SPEC: &str = "2026-07-28";

/// How hard the spec word behind a rule is — nothing more.
///
/// `Warning` means a MUST is violated (including client-side MUSTs, which get
/// the tool dropped from `tools/list` by a conforming client). `Note` means a
/// SHOULD or a documented recommendation. There is no third tier and no
/// aggregate: severity here describes the spec, not the server.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Warning,
    Note,
}

/// One fact about the contract, with its citation.
#[derive(Debug, Clone, PartialEq)]
pub struct Finding {
    pub level: Level,
    /// Stable kebab-case rule id, e.g. `input-schema-missing`.
    pub rule: &'static str,
    /// Where the rule comes from, e.g. `tools §inputSchema`. The spec
    /// revision is [`SPEC`], carried once rather than repeated per finding.
    pub cite: &'static str,
    /// The tool the fact is about; `None` for a server-level fact.
    pub tool: Option<String>,
    pub fact: String,
}

/// Lint every tool in the snapshot. Findings come out grouped by tool (the
/// snapshot map is sorted), warnings before notes within a tool.
pub fn lint(snapshot: &Snapshot) -> Vec<Finding> {
    let mut findings = Vec::new();
    let mut without_annotations = 0usize;

    for (name, tool) in &snapshot.tools {
        let start = findings.len();
        let Some(tool_object) = tool.as_object() else {
            findings.push(Finding {
                level: Level::Warning,
                rule: "tool-shape",
                cite: "tools §Tool",
                tool: Some(name.clone()),
                fact: "the tool definition is not a JSON object".into(),
            });
            continue;
        };

        lint_name(name, &mut findings);
        lint_input_schema(name, tool_object.get("inputSchema"), &mut findings);
        lint_output_schema(name, tool_object.get("outputSchema"), &mut findings);
        lint_headers(name, tool_object.get("inputSchema"), &mut findings);
        lint_descriptions(name, tool_object, &mut findings);

        if tool_object.get("annotations").is_none() {
            without_annotations += 1;
        }

        findings[start..].sort_by_key(|f| f.level);
    }

    if without_annotations > 0 {
        findings.push(Finding {
            level: Level::Note,
            rule: "tool-annotations-absent",
            cite: "tools §ToolAnnotations",
            tool: None,
            fact: format!(
                "{without_annotations} of {} tools declare no annotations \
                 (readOnlyHint, destructiveHint, idempotentHint, openWorldHint)",
                snapshot.tools.len()
            ),
        });
    }

    findings
}

/// SHOULD: 1–128 characters from `A–Z a–z 0–9 _ - .` — no spaces, commas,
/// or `/` (SEP-986 allowed `/` and capped at 64; the current spec supersedes
/// it, and these rules follow the spec).
fn lint_name(name: &str, findings: &mut Vec<Finding>) {
    let bad_char = name
        .chars()
        .find(|c| !(c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.')));
    let fact = if name.is_empty() {
        Some("the name is empty (SHOULD be 1–128 characters)".to_string())
    } else if name.chars().count() > 128 {
        Some(format!(
            "the name is {} characters (SHOULD be 1–128)",
            name.chars().count()
        ))
    } else {
        bad_char.map(|c| {
            format!("the name contains {c:?} (SHOULD use only A–Z, a–z, 0–9, `_`, `-`, `.`)")
        })
    };
    if let Some(fact) = fact {
        findings.push(Finding {
            level: Level::Note,
            rule: "tool-name-format",
            cite: "tools §name",
            tool: Some(name.to_string()),
            fact,
        });
    }
}

fn lint_input_schema(name: &str, schema: Option<&Value>, findings: &mut Vec<Finding>) {
    let mut warn = |fact: String| {
        findings.push(Finding {
            level: Level::Warning,
            rule: "input-schema-invalid",
            cite: "tools §inputSchema",
            tool: Some(name.to_string()),
            fact,
        });
    };

    let Some(schema) = schema else {
        warn("inputSchema is missing (MUST be a JSON Schema object)".into());
        return;
    };
    if schema.is_null() {
        warn("inputSchema is null (MUST be a JSON Schema object, never null)".into());
        return;
    }
    let Some(object) = schema.as_object() else {
        warn(format!(
            "inputSchema is {} (MUST be a JSON Schema object)",
            kind(schema)
        ));
        return;
    };

    // Arguments arrive as a named object; a schema typed anything else cannot
    // receive them. The spec's own recommended no-arg form is
    // `{"type":"object","additionalProperties":false}`.
    if let Some(declared) = object.get("type").and_then(Value::as_str)
        && declared != "object"
    {
        findings.push(Finding {
            level: Level::Note,
            rule: "input-schema-type",
            cite: "tools §inputSchema",
            tool: Some(name.to_string()),
            fact: format!(
                "inputSchema declares type {declared:?}; arguments are passed as a named object"
            ),
        });
    }

    let has_properties = object
        .get("properties")
        .and_then(Value::as_object)
        .is_some_and(|p| !p.is_empty());
    let closed = object.get("additionalProperties") == Some(&Value::Bool(false));
    if !has_properties && !closed {
        findings.push(Finding {
            level: Level::Note,
            rule: "no-arg-schema-open",
            cite: "tools §inputSchema",
            tool: Some(name.to_string()),
            fact: "takes no arguments but leaves the schema open — the recommended form is \
                   {\"type\":\"object\",\"additionalProperties\":false}"
                .into(),
        });
    }
}

/// Optional — but once declared it MUST be an object, same as `inputSchema`.
/// (Whether live results actually conform needs a call; that is probe work.)
fn lint_output_schema(name: &str, schema: Option<&Value>, findings: &mut Vec<Finding>) {
    let Some(schema) = schema else { return };
    if !schema.is_object() {
        findings.push(Finding {
            level: Level::Warning,
            rule: "output-schema-invalid",
            cite: "tools §outputSchema",
            tool: Some(name.to_string()),
            fact: format!(
                "outputSchema is {} (once declared, MUST be a JSON Schema object)",
                kind(schema)
            ),
        });
    }
}

/// The `x-mcp-header` rules are client-side MUSTs with the hardest
/// consequence on the page: a conforming client drops a violating tool from
/// `tools/list` entirely. Every fact here says so, because "why is my tool
/// missing in client X" is the question this rule answers.
fn lint_headers(name: &str, schema: Option<&Value>, findings: &mut Vec<Finding>) {
    let Some(schema) = schema else { return };

    let mut seen: Vec<String> = Vec::new();
    let mut markers = Vec::new();
    collect_header_markers(schema, &mut markers);

    for (header, property) in markers {
        let mut warn = |rule: &'static str, fact: String| {
            findings.push(Finding {
                level: Level::Warning,
                rule,
                cite: "tools §x-mcp-header",
                tool: Some(name.to_string()),
                fact: format!("{fact} — a conforming client drops this tool from tools/list"),
            });
        };

        let Some(header) = header.as_str() else {
            warn(
                "header-name-syntax",
                format!(
                    "x-mcp-header is {}, not a header-name string",
                    kind(&header)
                ),
            );
            continue;
        };
        if header.is_empty() || !header.chars().all(is_token_char) {
            warn(
                "header-name-syntax",
                format!("header name {header:?} is not an RFC 9110 token"),
            );
        }
        let lower = header.to_ascii_lowercase();
        if seen.contains(&lower) {
            warn(
                "header-name-duplicate",
                format!("header name {header:?} appears more than once (case-insensitively)"),
            );
        } else {
            seen.push(lower);
        }

        match property.get("type").and_then(Value::as_str) {
            Some("string" | "integer" | "boolean") => {}
            Some("number") => warn(
                "header-type",
                format!("the {header:?} property is type \"number\", which is forbidden"),
            ),
            Some(other) => warn(
                "header-type",
                format!(
                    "the {header:?} property is type {other:?} \
                     (must be string, integer, or boolean)"
                ),
            ),
            None => warn(
                "header-type",
                format!(
                    "the {header:?} property declares no single primitive type \
                     (must be string, integer, or boolean)"
                ),
            ),
        }
    }
}

/// Every object in the schema carrying an `x-mcp-header` key, wherever it
/// sits — including `$defs`, so a marker behind a `$ref` is still checked.
fn collect_header_markers(value: &Value, out: &mut Vec<(Value, Value)>) {
    match value {
        Value::Object(object) => {
            if let Some(header) = object.get("x-mcp-header") {
                out.push((header.clone(), value.clone()));
            }
            for child in object.values() {
                collect_header_markers(child, out);
            }
        }
        Value::Array(items) => {
            for child in items {
                collect_header_markers(child, out);
            }
        }
        _ => {}
    }
}

fn is_token_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || "!#$%&'*+-.^_`|~".contains(c)
}

/// Community layer, not spec — but description absence measurably degrades
/// tool selection, and "absent" is a fact.
fn lint_descriptions(
    name: &str,
    tool: &serde_json::Map<String, Value>,
    findings: &mut Vec<Finding>,
) {
    let described = tool
        .get("description")
        .and_then(Value::as_str)
        .is_some_and(|d| !d.trim().is_empty());
    if !described {
        findings.push(Finding {
            level: Level::Note,
            rule: "tool-description-missing",
            cite: "tools §description",
            tool: Some(name.to_string()),
            fact: "the tool has no description".into(),
        });
    }

    let Some(properties) = tool
        .get("inputSchema")
        .and_then(|s| s.get("properties"))
        .and_then(Value::as_object)
    else {
        return;
    };
    let missing: Vec<&str> = properties
        .iter()
        .filter(|(_, p)| {
            !p.get("description")
                .and_then(Value::as_str)
                .is_some_and(|d| !d.trim().is_empty())
        })
        .map(|(key, _)| key.as_str())
        .collect();
    if !missing.is_empty() {
        findings.push(Finding {
            level: Level::Note,
            rule: "property-descriptions-missing",
            cite: "tools §inputSchema",
            tool: Some(name.to_string()),
            fact: format!(
                "{} of {} argument properties have no description: {}",
                missing.len(),
                properties.len(),
                missing.join(", ")
            ),
        });
    }
}

fn kind(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    }
}

/// Plain-text report: findings grouped by tool, warnings first, the spec
/// revision stated once. The CLI prints this verbatim; any future surface
/// must render the same facts rather than inventing its own summary.
pub fn to_text(subtitle: &str, findings: &[Finding]) -> String {
    let warnings = findings
        .iter()
        .filter(|f| f.level == Level::Warning)
        .count();
    let notes = findings.len() - warnings;

    let mut out = String::new();
    out.push_str(&format!("# Lint: {subtitle}\n\nSpec {SPEC} · "));
    if findings.is_empty() {
        out.push_str("no findings from the static tool rules.\n");
        return out;
    }
    out.push_str(&format!(
        "{warnings} warning{}, {notes} note{}\n",
        plural(warnings),
        plural(notes)
    ));

    let mut current: Option<Option<&str>> = None;
    for finding in findings {
        let tool = finding.tool.as_deref();
        if current != Some(tool) {
            current = Some(tool);
            out.push_str(&format!("\n## {}\n", tool.unwrap_or("server")));
        }
        let level = match finding.level {
            Level::Warning => "warning",
            Level::Note => "note",
        };
        out.push_str(&format!(
            "- {level} · {} — {}  [{}]\n",
            finding.rule, finding.fact, finding.cite
        ));
    }
    out
}

fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}
