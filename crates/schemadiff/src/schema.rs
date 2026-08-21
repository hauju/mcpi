//! JSON Schema–aware comparison.
//!
//! Every rule here answers one question: does this edit make the schema accept
//! (or promise) a *different set of values* than before, and does that break
//! somebody? The answer depends on which way the data flows, so the whole
//! walker is parameterised by [`Direction`].
//!
//! This is deliberately not a JSON Schema validator. It handles the constructs
//! MCP servers actually emit and refuses to guess about the rest — see the
//! composed-schema handling in [`diff_schema`].

use serde_json::{Map, Value};

use crate::{FieldChange, Severity};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Direction {
    /// A tool's `inputSchema` — the caller produces this data.
    Input,
    /// A tool's `outputSchema` — the caller consumes this data.
    Output,
}

impl Direction {
    /// The schema now describes *fewer* values.
    ///
    /// On the way in that rejects payloads which used to be accepted. On the
    /// way out it just means the caller sees a subset of what it already
    /// handled.
    fn restrict(self) -> Severity {
        match self {
            Self::Input => Severity::Breaking,
            Self::Output => Severity::Compatible,
        }
    }

    /// The schema now describes *more* values.
    ///
    /// Harmless on the way in. On the way out the caller can be handed a
    /// variant it has no branch for.
    fn relax(self) -> Severity {
        match self {
            Self::Input => Severity::Compatible,
            Self::Output => Severity::Breaking,
        }
    }
}

/// Bounds where a *higher* number describes fewer values.
const LOWER_BOUNDS: [&str; 5] = [
    "minimum",
    "exclusiveMinimum",
    "minLength",
    "minItems",
    "minProperties",
];

/// Bounds where a *lower* number describes fewer values.
const UPPER_BOUNDS: [&str; 5] = [
    "maximum",
    "exclusiveMaximum",
    "maxLength",
    "maxItems",
    "maxProperties",
];

/// Schema keywords that need subsumption checking to compare honestly.
const COMPOSED: [&str; 5] = ["oneOf", "anyOf", "allOf", "not", "$ref"];

/// Keywords that carry no constraint at all.
const DOCUMENTATION: [&str; 5] = ["description", "title", "default", "examples", "format"];

pub(crate) fn diff_schema(
    path: &str,
    before: &Value,
    after: &Value,
    dir: Direction,
    out: &mut Vec<FieldChange>,
) {
    if before == after {
        return;
    }

    let (b, a) = match (before.as_object(), after.as_object()) {
        (Some(b), Some(a)) => (b, a),
        // One side is absent or is a bare boolean schema. There is nothing to
        // walk, and "the schema was replaced" is the honest report.
        _ => {
            out.push(FieldChange::new(
                path,
                dir.restrict(),
                "schema replaced wholesale",
                nonnull(before),
                nonnull(after),
            ));
            return;
        }
    };

    // The nullable idiom first: pydantic writes every `Optional[x]` as
    // `anyOf: [x, null]`, so on a Python-built server most fields sit behind
    // an `anyOf` that is not really a union. Judged by the composed-schema
    // rule below, every edit inside one — a loosened enum, a reworded
    // description — would come back as unanalysable-and-therefore-breaking.
    if diff_nullable_idiom(path, b, a, dir, out) {
        return;
    }

    // Deciding whether one `oneOf` subsumes another needs a real subsumption
    // engine. Rather than guess, report it and lean breaking — a tool whose job
    // is catching breakage must not stay quiet about a change it cannot read.
    for key in COMPOSED {
        if b.get(key) != a.get(key) {
            out.push(FieldChange::new(
                join(path, key),
                Severity::Breaking,
                format!("`{key}` changed — composed schemas are not analysed, review by hand"),
                b.get(key),
                a.get(key),
            ));
        }
    }

    diff_type(path, b, a, dir, out);
    diff_enum(path, b, a, dir, out);
    diff_const(path, b, a, dir, out);
    diff_bounds(path, b, a, dir, out);
    diff_pattern(path, b, a, dir, out);
    diff_additional_properties(path, b, a, dir, out);
    diff_properties(path, b, a, dir, out);
    diff_array_items(path, b, a, dir, out);
    diff_definitions(path, b, a, dir, out);
    diff_documentation(path, b, a, out);
}

/// Unwrap the nullable idiom on both sides and judge the real schemas.
///
/// A wrapper is an `anyOf`/`oneOf` whose branches are one concrete schema plus
/// null branches — every `Option`/`Optional`/`.nullable()` out of schemars,
/// pydantic, and zod 4. The concrete branches are compared structurally under
/// the field's own path, and a change in nullability is reported as its own
/// widening or narrowing. `type: [x, "null"]` is recognised as the same fact,
/// so a server switching spellings does not light up the diff. Returns false —
/// leaving the composed-schema rule to fire — when either side is a genuine
/// union.
fn diff_nullable_idiom(
    path: &str,
    b: &Map<String, Value>,
    a: &Map<String, Value>,
    dir: Direction,
    out: &mut Vec<FieldChange>,
) -> bool {
    let (Some((b_core, b_null, b_wrapped)), Some((a_core, a_null, a_wrapped))) =
        (split_nullable(b), split_nullable(a))
    else {
        return false;
    };
    // Nothing wrapped: `diff_type` already judges the type-array spelling
    // correctly, with better-worded reports than a rewrite here would give.
    if !b_wrapped && !a_wrapped {
        return false;
    }

    if b_null != a_null {
        let (severity, note) = if a_null {
            (dir.relax(), "null is now accepted")
        } else {
            (dir.restrict(), "null is no longer accepted")
        };
        out.push(FieldChange::new(
            join(path, "nullable"),
            severity,
            note,
            Some(&Value::Bool(b_null)),
            Some(&Value::Bool(a_null)),
        ));
    }

    // Reported under the field's own path: the wrapper is spelling, and
    // `properties.x.enum` is where a reader will look, not `.anyOf.0.enum`.
    diff_schema(
        path,
        &Value::Object(b_core),
        &Value::Object(a_core),
        dir,
        out,
    );
    true
}

/// A schema reduced to its non-null core: (core, accepts null, had to peel an
/// `anyOf`/`oneOf` wrapper). `None` when the schema is genuinely composed.
fn split_nullable(map: &Map<String, Value>) -> Option<(Map<String, Value>, bool, bool)> {
    for key in ["anyOf", "oneOf"] {
        if !map.contains_key(key) {
            continue;
        }
        // A second composition keyword alongside the union is a real
        // combination, not the idiom.
        if COMPOSED.iter().any(|k| *k != key && map.contains_key(*k)) {
            return None;
        }
        let branches = map.get(key)?.as_array()?;
        let real: Vec<&Value> = branches.iter().filter(|b| !is_null_branch(b)).collect();
        let [only] = real.as_slice() else {
            return None;
        };
        let mut core = only.as_object()?.clone();
        // Siblings of the wrapper (title, description, default) still apply
        // to the field; fold them in so their edits are compared too.
        for (k, v) in map {
            if k != key && !core.contains_key(k) {
                core.insert(k.clone(), v.clone());
            }
        }
        let nullable = branches.iter().any(is_null_branch);
        return Some((core, nullable, true));
    }

    if COMPOSED.iter().any(|k| map.contains_key(*k)) {
        return None;
    }

    // The other spelling of the same fact: `type: [x, "null"]`.
    let mut core = map.clone();
    let mut nullable = false;
    if let Some(Value::Array(types)) = map.get("type") {
        let concrete: Vec<Value> = types
            .iter()
            .filter(|t| t.as_str() != Some("null"))
            .cloned()
            .collect();
        if concrete.len() != types.len() {
            nullable = true;
            let rewritten = match <[Value; 1]>::try_from(concrete) {
                Ok([single]) => single,
                Err(many) => Value::Array(many),
            };
            core.insert("type".into(), rewritten);
        }
    }
    Some((core, nullable, false))
}

fn is_null_branch(schema: &Value) -> bool {
    schema.get("type").and_then(Value::as_str) == Some("null")
}

fn diff_type(
    path: &str,
    b: &Map<String, Value>,
    a: &Map<String, Value>,
    dir: Direction,
    out: &mut Vec<FieldChange>,
) {
    let (before, after) = (b.get("type"), a.get("type"));
    if before == after {
        return;
    }
    let field = join(path, "type");

    match (type_set(before), type_set(after)) {
        (None, Some(_)) => out.push(FieldChange::new(
            field,
            dir.restrict(),
            "type constraint added",
            before,
            after,
        )),
        (Some(_), None) => out.push(FieldChange::new(
            field,
            dir.relax(),
            "type constraint removed",
            before,
            after,
        )),
        (Some(bt), Some(at)) => {
            let severity = if at.iter().all(|t| bt.contains(t)) {
                dir.restrict()
            } else if bt.iter().all(|t| at.contains(t)) {
                dir.relax()
            } else {
                Severity::Breaking
            };
            let note = match severity {
                _ if at.iter().all(|t| bt.contains(t)) => "type narrowed",
                _ if bt.iter().all(|t| at.contains(t)) => "type widened",
                _ => "type changed to an unrelated type",
            };
            out.push(FieldChange::new(field, severity, note, before, after));
        }
        (None, None) => {}
    }
}

fn type_set(v: Option<&Value>) -> Option<Vec<String>> {
    match v? {
        Value::String(s) => Some(vec![s.clone()]),
        Value::Array(items) => Some(
            items
                .iter()
                .filter_map(|x| x.as_str().map(str::to_string))
                .collect(),
        ),
        _ => None,
    }
}

fn diff_enum(
    path: &str,
    b: &Map<String, Value>,
    a: &Map<String, Value>,
    dir: Direction,
    out: &mut Vec<FieldChange>,
) {
    let (before, after) = (b.get("enum"), a.get("enum"));
    if before == after {
        return;
    }
    let field = join(path, "enum");

    match (
        before.and_then(Value::as_array),
        after.and_then(Value::as_array),
    ) {
        (None, Some(_)) => out.push(FieldChange::new(
            field,
            dir.restrict(),
            "value list added — only these values are allowed now",
            before,
            after,
        )),
        (Some(_), None) => out.push(FieldChange::new(
            field,
            dir.relax(),
            "value list removed",
            before,
            after,
        )),
        (Some(bv), Some(av)) => {
            let all_kept = bv.iter().all(|v| av.contains(v));
            let no_new = av.iter().all(|v| bv.contains(v));
            let (severity, note) = match (all_kept, no_new) {
                (true, false) => (dir.relax(), "values added to the list"),
                (false, true) => (dir.restrict(), "values removed from the list"),
                (true, true) => (Severity::Cosmetic, "value list reordered"),
                (false, false) => (
                    Severity::Breaking,
                    "value list both gained and lost entries",
                ),
            };
            out.push(FieldChange::new(field, severity, note, before, after));
        }
        (None, None) => {}
    }
}

fn diff_const(
    path: &str,
    b: &Map<String, Value>,
    a: &Map<String, Value>,
    dir: Direction,
    out: &mut Vec<FieldChange>,
) {
    let (before, after) = (b.get("const"), a.get("const"));
    if before == after {
        return;
    }
    let (severity, note) = match (before, after) {
        (None, Some(_)) => (dir.restrict(), "fixed value required"),
        (Some(_), None) => (dir.relax(), "fixed value no longer required"),
        _ => (Severity::Breaking, "fixed value changed"),
    };
    out.push(FieldChange::new(
        join(path, "const"),
        severity,
        note,
        before,
        after,
    ));
}

fn diff_bounds(
    path: &str,
    b: &Map<String, Value>,
    a: &Map<String, Value>,
    dir: Direction,
    out: &mut Vec<FieldChange>,
) {
    for (key, raising_restricts) in LOWER_BOUNDS
        .iter()
        .map(|k| (*k, true))
        .chain(UPPER_BOUNDS.iter().map(|k| (*k, false)))
    {
        let (before, after) = (b.get(key), a.get(key));
        if before == after {
            continue;
        }

        let (severity, note) = match (
            before.and_then(Value::as_f64),
            after.and_then(Value::as_f64),
        ) {
            (None, Some(_)) => (dir.restrict(), format!("`{key}` constraint added")),
            (Some(_), None) => (dir.relax(), format!("`{key}` constraint removed")),
            (Some(x), Some(y)) => {
                let tightened = if raising_restricts { y > x } else { y < x };
                if tightened {
                    (dir.restrict(), format!("`{key}` tightened"))
                } else {
                    (dir.relax(), format!("`{key}` loosened"))
                }
            }
            (None, None) => continue,
        };
        out.push(FieldChange::new(
            join(path, key),
            severity,
            note,
            before,
            after,
        ));
    }
}

fn diff_pattern(
    path: &str,
    b: &Map<String, Value>,
    a: &Map<String, Value>,
    dir: Direction,
    out: &mut Vec<FieldChange>,
) {
    let (before, after) = (b.get("pattern"), a.get("pattern"));
    if before == after {
        return;
    }
    // Two regexes cannot be compared for subsumption here, so any edit to a
    // live pattern is treated as a restriction.
    let (severity, note) = match (before, after) {
        (None, Some(_)) => (dir.restrict(), "pattern constraint added"),
        (Some(_), None) => (dir.relax(), "pattern constraint removed"),
        _ => (
            dir.restrict(),
            "pattern changed — values matching the old one may be rejected",
        ),
    };
    out.push(FieldChange::new(
        join(path, "pattern"),
        severity,
        note,
        before,
        after,
    ));
}

fn diff_additional_properties(
    path: &str,
    b: &Map<String, Value>,
    a: &Map<String, Value>,
    dir: Direction,
    out: &mut Vec<FieldChange>,
) {
    let (before, after) = (b.get("additionalProperties"), a.get("additionalProperties"));
    if before == after {
        return;
    }
    // Absent means permitted, so a missing key reads as `true`.
    let permitted = |v: Option<&Value>| v.and_then(Value::as_bool).unwrap_or(true);
    let (severity, note) = match (permitted(before), permitted(after)) {
        (true, false) => (dir.restrict(), "extra properties are no longer accepted"),
        (false, true) => (dir.relax(), "extra properties are now accepted"),
        _ => (Severity::Cosmetic, "additionalProperties schema changed"),
    };
    out.push(FieldChange::new(
        join(path, "additionalProperties"),
        severity,
        note,
        before,
        after,
    ));
}

fn diff_properties(
    path: &str,
    b: &Map<String, Value>,
    a: &Map<String, Value>,
    dir: Direction,
    out: &mut Vec<FieldChange>,
) {
    let empty = Map::new();
    let before_props = b
        .get("properties")
        .and_then(Value::as_object)
        .unwrap_or(&empty);
    let after_props = a
        .get("properties")
        .and_then(Value::as_object)
        .unwrap_or(&empty);
    let before_required = required_names(b);
    let after_required = required_names(a);

    for (name, before_schema) in before_props {
        let field = join(path, &format!("properties.{name}"));
        match after_props.get(name) {
            None => out.push(FieldChange::new(
                field,
                // Breaking either way: on the way in the value stops being part
                // of the contract, on the way out the caller loses a field.
                Severity::Breaking,
                "property removed",
                Some(before_schema),
                None,
            )),
            Some(after_schema) => diff_schema(&field, before_schema, after_schema, dir, out),
        }
    }

    for (name, after_schema) in after_props {
        if before_props.contains_key(name) {
            continue;
        }
        let now_required = after_required.contains(name);
        let (severity, note) = if now_required {
            match dir {
                Direction::Input => (
                    Severity::Breaking,
                    "required property added — existing callers do not send it",
                ),
                Direction::Output => (Severity::Compatible, "guaranteed property added"),
            }
        } else {
            (Severity::Compatible, "optional property added")
        };
        out.push(FieldChange::new(
            join(path, &format!("properties.{name}")),
            severity,
            note,
            None,
            Some(after_schema),
        ));
    }

    // Properties that already existed and only changed their required status.
    // Newly added ones are skipped so a single edit is not reported twice.
    for name in &after_required {
        if before_required.contains(name) || !before_props.contains_key(name) {
            continue;
        }
        let (severity, note) = match dir {
            Direction::Input => (
                Severity::Breaking,
                "property became required — existing callers omit it",
            ),
            Direction::Output => (Severity::Compatible, "property is now always present"),
        };
        out.push(FieldChange::new(
            join(path, &format!("properties.{name}.required")),
            severity,
            note,
            Some(&Value::Bool(false)),
            Some(&Value::Bool(true)),
        ));
    }

    for name in &before_required {
        if after_required.contains(name) || !after_props.contains_key(name) {
            continue;
        }
        let (severity, note) = match dir {
            Direction::Input => (Severity::Compatible, "property became optional"),
            Direction::Output => (
                Severity::Breaking,
                "property is no longer guaranteed — it may be absent",
            ),
        };
        out.push(FieldChange::new(
            join(path, &format!("properties.{name}.required")),
            severity,
            note,
            Some(&Value::Bool(true)),
            Some(&Value::Bool(false)),
        ));
    }
}

fn required_names(schema: &Map<String, Value>) -> Vec<String> {
    schema
        .get("required")
        .and_then(Value::as_array)
        .map(|names| {
            names
                .iter()
                .filter_map(|n| n.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

fn diff_array_items(
    path: &str,
    b: &Map<String, Value>,
    a: &Map<String, Value>,
    dir: Direction,
    out: &mut Vec<FieldChange>,
) {
    let (before, after) = (b.get("items"), a.get("items"));
    if before == after {
        return;
    }
    match (before, after) {
        (Some(bi), Some(ai)) => diff_schema(&join(path, "items"), bi, ai, dir, out),
        (before, after) => out.push(FieldChange::new(
            join(path, "items"),
            dir.restrict(),
            "item schema added or removed",
            before,
            after,
        )),
    }
}

/// The named definitions that `$ref`s elsewhere in the schema point into.
///
/// schemars and pydantic put every named struct and enum behind a local `$ref`
/// into `$defs`, and the ref string itself almost never changes — the
/// definition behind it is where the contract actually moves. Skipping these
/// maps would let a server gut an enum inside `$defs` without a single
/// reported change.
fn diff_definitions(
    path: &str,
    b: &Map<String, Value>,
    a: &Map<String, Value>,
    dir: Direction,
    out: &mut Vec<FieldChange>,
) {
    for key in ["$defs", "definitions"] {
        let empty = Map::new();
        let before_defs = b.get(key).and_then(Value::as_object).unwrap_or(&empty);
        let after_defs = a.get(key).and_then(Value::as_object).unwrap_or(&empty);

        for (name, before_def) in before_defs {
            let field = join(path, &format!("{key}.{name}"));
            match after_defs.get(name) {
                Some(after_def) => diff_schema(&field, before_def, after_def, dir, out),
                None => {
                    // Removing a definition something still points at breaks
                    // the schema outright; removing one nothing references is
                    // cleanup, and flagging it would cry wolf on refactors.
                    let pointer = format!("#/{key}/{name}");
                    let (severity, note) = if a.values().any(|v| mentions_ref(v, &pointer)) {
                        (
                            Severity::Breaking,
                            "definition removed — a `$ref` still points at it",
                        )
                    } else {
                        (Severity::Cosmetic, "unreferenced definition removed")
                    };
                    out.push(FieldChange::new(
                        field,
                        severity,
                        note,
                        Some(before_def),
                        None,
                    ));
                }
            }
        }

        for (name, after_def) in after_defs {
            if !before_defs.contains_key(name) {
                out.push(FieldChange::new(
                    join(path, &format!("{key}.{name}")),
                    // A definition on its own constrains nothing; whatever
                    // newly references it is reported at the referencing site.
                    Severity::Compatible,
                    "definition added",
                    None,
                    Some(after_def),
                ));
            }
        }
    }
}

fn mentions_ref(value: &Value, pointer: &str) -> bool {
    match value {
        Value::Object(map) => map.iter().any(|(key, inner)| {
            (key == "$ref" && inner.as_str() == Some(pointer)) || mentions_ref(inner, pointer)
        }),
        Value::Array(items) => items.iter().any(|item| mentions_ref(item, pointer)),
        _ => false,
    }
}

fn diff_documentation(
    path: &str,
    b: &Map<String, Value>,
    a: &Map<String, Value>,
    out: &mut Vec<FieldChange>,
) {
    for key in DOCUMENTATION {
        if b.get(key) != a.get(key) {
            out.push(FieldChange::new(
                join(path, key),
                Severity::Cosmetic,
                format!("`{key}` changed"),
                b.get(key),
                a.get(key),
            ));
        }
    }
}

fn join(path: &str, key: &str) -> String {
    if path.is_empty() {
        key.to_string()
    } else {
        format!("{path}.{key}")
    }
}

fn nonnull(v: &Value) -> Option<&Value> {
    (!v.is_null()).then_some(v)
}
