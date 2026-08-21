//! Turning a tool's `inputSchema` into a form, and back into arguments.
//!
//! This deliberately handles a *subset* of JSON Schema — objects, primitives,
//! enums, arrays of primitives, nested objects — plus the composition idioms
//! the generators behind real MCP servers actually put on the wire: nullable
//! unions and `anyOf`-with-null (every `Option`/`Optional` field out of
//! schemars, pydantic, and zod 4), local `$ref`s into `$defs` (every named
//! struct and enum out of schemars and pydantic), and unions of `const`
//! branches (documented enums and literal unions). Anything genuinely
//! composed — two real types, a multi-schema `allOf`, a `not` — gets a raw
//! JSON editor for that field alone, carrying a visible badge. Reimplementing
//! a JSON Schema form library would be weeks of work, and a form that silently
//! mangles a payload it did not understand is worse than one that admits the
//! limit and hands over an editor.
//!
//! Arrays are edited as one entry per line rather than as add/remove rows. That
//! keeps every path a plain key path (no indices), and it means a list can be
//! pasted in whole — which is how people actually fill these in.

use serde_json::{Map, Value};

/// What widget a property's schema maps to.
#[derive(Debug, Clone, PartialEq)]
pub enum Widget {
    Text,
    Number {
        integer: bool,
    },
    Bool,
    /// A closed set of allowed values.
    Choice(Vec<Value>),
    /// One entry per line, each parsed as `item`.
    Lines {
        item: Box<Widget>,
    },
    Object(Vec<Property>),
    /// Not representable as a widget. The reason is shown to the user.
    Raw(&'static str),
}

impl Widget {
    fn is_primitive(&self) -> bool {
        matches!(
            self,
            Widget::Text | Widget::Number { .. } | Widget::Bool | Widget::Choice(_)
        )
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Property {
    pub name: String,
    pub title: Option<String>,
    pub description: Option<String>,
    pub required: bool,
    /// Whether the schema admits `null` alongside its real type. `null` is how
    /// a pydantic caller says "none", so a required nullable field is seeded to
    /// `null` rather than to a value the user never entered.
    pub nullable: bool,
    pub widget: Widget,
}

/// The properties of an object schema, in declaration order where the JSON
/// preserved one, alphabetical otherwise — required first either way, because
/// that is the order they have to be filled in.
pub fn properties_of(schema: &Value) -> Vec<Property> {
    properties_within(schema, schema, &[])
}

/// `properties_of` with the root schema threaded down, so a field's `$ref` can
/// be looked up in the root's `$defs`.
fn properties_within<'a>(
    schema: &'a Value,
    root: &'a Value,
    resolving: &[&'a str],
) -> Vec<Property> {
    let Some(object) = schema.as_object() else {
        return Vec::new();
    };
    let required: Vec<&str> = object
        .get("required")
        .and_then(Value::as_array)
        .map(|names| names.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();

    let mut properties: Vec<Property> = object
        .get("properties")
        .and_then(Value::as_object)
        .map(|properties| {
            properties
                .iter()
                .map(|(name, schema)| Property {
                    name: name.clone(),
                    title: string_field(schema, "title"),
                    description: string_field(schema, "description"),
                    required: required.contains(&name.as_str()),
                    nullable: accepts_null(schema),
                    widget: widget_within(schema, root, resolving),
                })
                .collect()
        })
        .unwrap_or_default();

    properties.sort_by_key(|p| !p.required);
    properties
}

/// [`properties_of`]'s per-field worker, with the root schema threaded down
/// for `$ref` lookups and the chain of refs currently being followed so a
/// self-referential schema terminates instead of recursing forever.
fn widget_within<'a>(schema: &'a Value, root: &'a Value, resolving: &[&'a str]) -> Widget {
    let Some(object) = schema.as_object() else {
        return Widget::Raw("this field has no usable schema");
    };

    // Composition first: a schema with `oneOf` may also carry a `type`, and the
    // composition is the binding part. More than one composition keyword on the
    // same schema is a genuine combination the form cannot honour.
    let composed: Vec<&str> = ["oneOf", "anyOf", "allOf", "not", "$ref"]
        .into_iter()
        .filter(|key| object.contains_key(*key))
        .collect();

    match composed.as_slice() {
        [] => {}

        // schemars and pydantic both put every named struct and enum behind a
        // local `$ref` into `$defs`. The definition travels in the same
        // document, so following the pointer recovers the field; only a ref
        // that leaves the document, or one that loops, is beyond the form.
        ["$ref"] => {
            let Some(pointer) = object.get("$ref").and_then(Value::as_str) else {
                return Widget::Raw("this field uses a composed schema");
            };
            if resolving.contains(&pointer) {
                return Widget::Raw("this field refers to itself");
            }
            return match resolve_local_ref(root, pointer) {
                Some(target) => {
                    let mut resolving = resolving.to_vec();
                    resolving.push(pointer);
                    widget_within(target, root, &resolving)
                }
                None => Widget::Raw("this field references a schema this document does not carry"),
            };
        }

        // pydantic before 2.9 wrapped a field's `$ref` in a one-element
        // `allOf` whenever the field carried a description or default. One
        // branch combines nothing — unwrap it.
        ["allOf"] => {
            return match object
                .get("allOf")
                .and_then(Value::as_array)
                .map(Vec::as_slice)
            {
                Some([only]) => widget_within(only, root, resolving),
                _ => Widget::Raw("this field uses a composed schema"),
            };
        }

        // A union. pydantic spells every `Optional[x]` as `anyOf: [x, null]`
        // (zod 4's `.nullable()` and schemars' `Option<Struct>` agree), so on
        // a Python-built server most fields arrive under an `anyOf`. One real
        // branch plus null branches is just that branch, optionally absent.
        // Failing that, schemars and zod 4 write enums whose values carry
        // descriptions as a union of `const` branches — a closed set.
        ["oneOf"] | ["anyOf"] => {
            let Some(branches) = object.get(composed[0]).and_then(Value::as_array) else {
                return Widget::Raw("this field uses a composed schema");
            };
            let real: Vec<&Value> = branches.iter().filter(|b| !is_null_schema(b)).collect();
            return match real.as_slice() {
                [only] => widget_within(only, root, resolving),
                [] => Widget::Raw("this field uses a composed schema"),
                many => match many.iter().map(|b| b.get("const").cloned()).collect() {
                    Some(values) => Widget::Choice(values),
                    // Two or more real branches without consts: a true union.
                    None => Widget::Raw("this field uses a composed schema"),
                },
            };
        }

        _ => return Widget::Raw("this field uses a composed schema"),
    }

    if let Some(values) = object.get("enum").and_then(Value::as_array) {
        return Widget::Choice(values.clone());
    }
    // A `const` is a one-value enum, which is how a literal type arrives.
    if let Some(value) = object.get("const") {
        return Widget::Choice(vec![value.clone()]);
    }

    // `["integer", "null"]` is what schemars emits for every `Option<T>`, so
    // most optional fields on a Rust-built server arrive as a union. Treating
    // those as unrepresentable would drop nearly every optional argument into a
    // raw JSON box — the form's whole value, lost to the most common idiom in
    // the ecosystem. A union that is one real type plus `null` is just that
    // type, optionally absent.
    let single_type = match object.get("type") {
        Some(Value::Array(types)) => {
            let named: Vec<&str> = types.iter().filter_map(Value::as_str).collect();
            let mut concrete = named.iter().filter(|t| **t != "null");
            match (concrete.next(), concrete.next()) {
                (Some(only), None) => Some(*only),
                // Two or more real types genuinely cannot map to one widget.
                _ => None,
            }
        }
        other => other.and_then(Value::as_str),
    };

    match single_type {
        Some("string") => Widget::Text,
        Some("integer") => Widget::Number { integer: true },
        Some("number") => Widget::Number { integer: false },
        Some("boolean") => Widget::Bool,
        Some("object") => Widget::Object(properties_within(schema, root, resolving)),
        Some("array") => match object.get("items") {
            Some(items) => {
                let item = widget_within(items, root, resolving);
                if item.is_primitive() {
                    Widget::Lines {
                        item: Box::new(item),
                    }
                } else {
                    Widget::Raw("this field is a list of structured values")
                }
            }
            None => Widget::Raw("this list does not say what it holds"),
        },
        // A union of two or more real types, or no `type` at all.
        _ => Widget::Raw("this field accepts more than one type"),
    }
}

/// Follow a `#/`-local JSON Pointer into the root schema. A ref that leaves
/// the document would need I/O, which a form cannot do.
fn resolve_local_ref<'a>(root: &'a Value, pointer: &str) -> Option<&'a Value> {
    pointer
        .strip_prefix("#/")?
        .split('/')
        .try_fold(root, |cursor, segment| {
            // JSON Pointer unescaping, in the order the RFC requires.
            cursor.get(segment.replace("~1", "/").replace("~0", "~"))
        })
}

fn is_null_schema(schema: &Value) -> bool {
    schema.get("type").and_then(Value::as_str) == Some("null")
}

/// Whether the schema admits `null`, in either of the two spellings real
/// generators use: `type: [x, "null"]` or an `anyOf`/`oneOf` null branch.
fn accepts_null(schema: &Value) -> bool {
    let Some(object) = schema.as_object() else {
        return false;
    };
    if let Some(Value::Array(types)) = object.get("type")
        && types.iter().any(|t| t.as_str() == Some("null"))
    {
        return true;
    }
    ["anyOf", "oneOf"].iter().any(|key| {
        object
            .get(*key)
            .and_then(Value::as_array)
            .is_some_and(|branches| branches.iter().any(is_null_schema))
    })
}

/// Starting arguments for a schema.
///
/// Required fields are seeded so the payload is shaped correctly from the
/// start; optional fields stay absent so the server applies its own defaults
/// rather than receiving an empty value it never asked for.
pub fn seed(schema: &Value) -> Value {
    let mut out = Map::new();
    for property in properties_of(schema) {
        let field_schema = schema
            .get("properties")
            .and_then(|p| p.get(&property.name))
            .cloned()
            .unwrap_or(Value::Null);

        // pydantic stamps `"default": null` onto every `Optional[x] = None`
        // field. Copying that in would put an explicit null into every
        // optional payload, when absent is what "use your default" means.
        let default = field_schema.get("default").filter(|d| !d.is_null());
        if let Some(default) = default {
            out.insert(property.name, default.clone());
        } else if property.required {
            out.insert(property.name.clone(), seed_value(&property));
        }
    }
    Value::Object(out)
}

/// The value a required field starts from.
///
/// A nullable field starts at `null`: for pydantic's `Optional[str]` with no
/// default — required, but happy with none — `null` is a value the tool said
/// it accepts, where `""` or `0` is data the user never entered.
fn seed_value(property: &Property) -> Value {
    if property.nullable {
        Value::Null
    } else {
        zero(&property.widget)
    }
}

fn zero(widget: &Widget) -> Value {
    match widget {
        Widget::Text => Value::String(String::new()),
        Widget::Number { .. } => Value::from(0),
        Widget::Bool => Value::Bool(false),
        Widget::Choice(values) => values.first().cloned().unwrap_or(Value::Null),
        Widget::Lines { .. } => Value::Array(Vec::new()),
        Widget::Object(properties) => Value::Object(
            properties
                .iter()
                .filter(|p| p.required)
                .map(|p| (p.name.clone(), seed_value(p)))
                .collect(),
        ),
        Widget::Raw(_) => Value::Null,
    }
}

/// MCP prompts describe their arguments as a flat list, not a JSON Schema.
/// Synthesising one lets prompts reuse the whole form.
pub fn schema_for_prompt_arguments(arguments: Option<&Value>) -> Value {
    let mut properties = Map::new();
    let mut required = Vec::new();

    for argument in arguments.and_then(Value::as_array).into_iter().flatten() {
        let Some(name) = argument.get("name").and_then(Value::as_str) else {
            continue;
        };
        let mut field = Map::new();
        field.insert("type".into(), Value::String("string".into()));
        if let Some(description) = argument.get("description") {
            field.insert("description".into(), description.clone());
        }
        properties.insert(name.to_string(), Value::Object(field));

        if argument
            .get("required")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            required.push(Value::String(name.to_string()));
        }
    }

    serde_json::json!({
        "type": "object",
        "properties": properties,
        "required": required,
    })
}

// ── Reading and writing by path ─────────────────────────────────────────────

pub fn get_path<'a>(value: &'a Value, path: &[String]) -> Option<&'a Value> {
    let mut cursor = value;
    for key in path {
        cursor = cursor.get(key)?;
    }
    Some(cursor)
}

/// Write `new` at `path`, creating intermediate objects as needed.
///
/// A `Null` value removes the key instead, which is how an optional field goes
/// back to being absent.
pub fn set_path(value: &mut Value, path: &[String], new: Value) {
    let Some((last, parents)) = path.split_last() else {
        *value = new;
        return;
    };

    let mut cursor = value;
    for key in parents {
        if !cursor.is_object() {
            *cursor = Value::Object(Map::new());
        }
        cursor = cursor
            .as_object_mut()
            .expect("just ensured this is an object")
            .entry(key.clone())
            .or_insert_with(|| Value::Object(Map::new()));
    }

    if !cursor.is_object() {
        *cursor = Value::Object(Map::new());
    }
    let object = cursor.as_object_mut().expect("just ensured");
    if new.is_null() {
        object.remove(last);
    } else {
        object.insert(last.clone(), new);
    }
}

/// Parse the textarea form of a list back into an array.
pub fn parse_lines(text: &str, item: &Widget) -> Value {
    Value::Array(
        text.lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(|line| parse_scalar(line, item))
            .collect(),
    )
}

/// Render an array back into the textarea form.
pub fn unparse_lines(value: Option<&Value>) -> String {
    value
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .map(|item| match item {
                    Value::String(s) => s.clone(),
                    other => other.to_string(),
                })
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
}

pub fn parse_scalar(text: &str, widget: &Widget) -> Value {
    match widget {
        Widget::Number { integer: true } => text
            .parse::<i64>()
            .map(Value::from)
            // Keep what was typed rather than silently substituting a zero: the
            // server's error about a bad type is more useful than a value the
            // user never entered.
            .unwrap_or_else(|_| Value::String(text.to_string())),
        Widget::Number { integer: false } => text
            .parse::<f64>()
            .ok()
            .and_then(serde_json::Number::from_f64)
            .map(Value::Number)
            .unwrap_or_else(|| Value::String(text.to_string())),
        Widget::Bool => Value::Bool(matches!(text, "true" | "1")),
        _ => Value::String(text.to_string()),
    }
}

fn string_field(schema: &Value, key: &str) -> Option<String> {
    schema
        .get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A field schema on its own, as the tests mostly exercise one: no root
    /// means a `$ref` has nowhere to resolve, exactly like the real entry
    /// point when a schema refs a definition it does not carry.
    fn widget_for(schema: &Value) -> Widget {
        widget_within(schema, schema, &[])
    }

    #[test]
    fn primitives_map_to_their_widgets() {
        assert_eq!(widget_for(&json!({"type": "string"})), Widget::Text);
        assert_eq!(
            widget_for(&json!({"type": "integer"})),
            Widget::Number { integer: true }
        );
        assert_eq!(
            widget_for(&json!({"type": "number"})),
            Widget::Number { integer: false }
        );
        assert_eq!(widget_for(&json!({"type": "boolean"})), Widget::Bool);
    }

    #[test]
    fn an_enum_beats_its_type() {
        assert_eq!(
            widget_for(&json!({"type": "string", "enum": ["a", "b"]})),
            Widget::Choice(vec![json!("a"), json!("b")])
        );
    }

    #[test]
    fn a_list_of_primitives_is_editable_but_a_list_of_objects_is_not() {
        assert_eq!(
            widget_for(&json!({"type": "array", "items": {"type": "string"}})),
            Widget::Lines {
                item: Box::new(Widget::Text)
            }
        );
        assert!(matches!(
            widget_for(&json!({"type": "array", "items": {"type": "object"}})),
            Widget::Raw(_)
        ));
    }

    #[test]
    fn a_union_of_consts_is_a_choice_not_raw() {
        // A `oneOf` of `const` branches is a closed set of values — schemars
        // writes every enum whose variants carry doc comments this way, and
        // zod 4 does the same for literal unions (as `anyOf`). Offering the
        // consts as a select preserves the whole constraint, so this is not a
        // schema the form has to refuse.
        let schema = json!({"type": "string", "oneOf": [{"const": "a"}, {"const": "b"}]});
        assert_eq!(
            widget_for(&schema),
            Widget::Choice(vec![json!("a"), json!("b")])
        );
    }

    #[test]
    fn a_zod4_literal_union_is_a_choice() {
        // `z.union([z.literal("a"), z.literal("b")])` through the TS SDK's
        // zod 4 path (Mini toJSONSchema, draft-7 target), verified against
        // zod 3.25 / @modelcontextprotocol/sdk 1.30.
        let schema = json!({
            "anyOf": [
                {"type": "string", "const": "a"},
                {"type": "string", "const": "b"},
            ]
        });
        assert_eq!(
            widget_for(&schema),
            Widget::Choice(vec![json!("a"), json!("b")])
        );
    }

    #[test]
    fn a_true_union_still_falls_back_to_raw() {
        // `str | int` in pydantic, `z.union([z.string(), z.number()])` in
        // zod 4: genuinely two widgets, so the honest answer is still raw.
        assert!(matches!(
            widget_for(&json!({"anyOf": [{"type": "string"}, {"type": "integer"}]})),
            Widget::Raw(_)
        ));
    }

    #[test]
    fn a_nullable_union_keeps_its_real_widget() {
        // `Option<T>` in a schemars-generated schema. Falling back to raw here
        // would put nearly every optional argument on a Rust-built MCP server
        // into a JSON textarea.
        assert_eq!(
            widget_for(&json!({"type": ["string", "null"]})),
            Widget::Text
        );
        assert_eq!(
            widget_for(&json!({"type": ["integer", "null"]})),
            Widget::Number { integer: true }
        );
        assert_eq!(
            widget_for(&json!({"type": ["null", "boolean"]})),
            Widget::Bool,
            "order within the union must not matter"
        );
    }

    #[test]
    fn a_nullable_object_still_expands_its_properties() {
        let schema = json!({
            "type": ["object", "null"],
            "properties": { "inner": {"type": "string"} },
        });
        match widget_for(&schema) {
            Widget::Object(properties) => assert_eq!(properties[0].name, "inner"),
            other => panic!("expected an object, got {other:?}"),
        }
    }

    #[test]
    fn a_union_of_two_real_types_is_still_raw() {
        // Nothing sensible to render: it is genuinely two different widgets.
        assert!(matches!(
            widget_for(&json!({"type": ["string", "integer"]})),
            Widget::Raw(_)
        ));
    }

    #[test]
    fn a_nullable_enum_still_offers_its_choices() {
        // The shape SeggWat's `status` and `type` filters actually arrive in.
        assert_eq!(
            widget_for(&json!({"type": ["string", "null"], "enum": ["New", "Active"]})),
            Widget::Choice(vec![json!("New"), json!("Active")])
        );
    }

    #[test]
    fn a_pydantic_optional_field_is_editable() {
        // pydantic v2 spells every `Optional[x]` as `anyOf: [x, null]` — never
        // as a type union — so before this unwrap, every optional field on
        // every Python-built MCP server landed in a raw JSON box. Verified
        // against pydantic 2.12.
        assert_eq!(
            widget_for(&json!({
                "anyOf": [{"type": "string"}, {"type": "null"}],
                "default": null,
                "title": "Optional Str",
            })),
            Widget::Text
        );
        assert_eq!(
            widget_for(&json!({"anyOf": [{"type": "null"}, {"type": "integer"}]})),
            Widget::Number { integer: true },
            "branch order must not matter"
        );
    }

    #[test]
    fn a_pydantic_optional_literal_offers_its_choices() {
        // `Optional[Literal["a", "b"]] = None` — the enum rides inside the
        // non-null branch.
        assert_eq!(
            widget_for(&json!({
                "anyOf": [
                    {"enum": ["a", "b"], "type": "string"},
                    {"type": "null"},
                ],
                "default": null,
            })),
            Widget::Choice(vec![json!("a"), json!("b")])
        );
    }

    #[test]
    fn a_schemars_enum_ref_resolves_through_defs() {
        // schemars (and pydantic) put every named enum behind a local `$ref`;
        // the definition travels in the same schema. Verified against
        // schemars 1.2.2, the version rmcp 3.0 serves schemas with.
        let schema = json!({
            "$defs": { "Status": {"enum": ["New", "Active"], "type": "string"} },
            "type": "object",
            "properties": { "status": {"$ref": "#/$defs/Status"} },
        });
        let properties = properties_of(&schema);
        assert_eq!(
            properties[0].widget,
            Widget::Choice(vec![json!("New"), json!("Active")])
        );
    }

    #[test]
    fn a_schemars_nested_struct_ref_expands_its_properties() {
        let schema = json!({
            "$defs": {
                "Nested": {
                    "type": "object",
                    "properties": { "inner": {"type": "string"} },
                    "required": ["inner"],
                }
            },
            "type": "object",
            "properties": { "nested": {"$ref": "#/$defs/Nested"} },
        });
        match &properties_of(&schema)[0].widget {
            Widget::Object(children) => {
                assert_eq!(children[0].name, "inner");
                assert_eq!(children[0].widget, Widget::Text);
            }
            other => panic!("expected an object, got {other:?}"),
        }
    }

    #[test]
    fn an_optional_nested_model_unwraps_and_resolves() {
        // `Option<Struct>` in schemars and `Optional[Model]` in pydantic both
        // arrive as `anyOf: [$ref, null]` — the two idioms composed.
        let schema = json!({
            "$defs": { "Status": {"enum": ["New"], "type": "string"} },
            "type": "object",
            "properties": {
                "status": {
                    "anyOf": [{"$ref": "#/$defs/Status"}, {"type": "null"}],
                    "default": null,
                }
            },
        });
        assert_eq!(
            properties_of(&schema)[0].widget,
            Widget::Choice(vec![json!("New")])
        );
    }

    #[test]
    fn an_old_pydantic_allof_wrapped_ref_resolves() {
        // pydantic 2.0–2.8 wrapped a field's `$ref` in a one-element `allOf`
        // whenever the field carried a description or default. Verified
        // against pydantic 2.4.
        let schema = json!({
            "$defs": { "Status": {"enum": ["new"], "type": "string"} },
            "type": "object",
            "properties": {
                "status": {
                    "allOf": [{"$ref": "#/$defs/Status"}],
                    "default": "new",
                    "description": "doc",
                }
            },
        });
        assert_eq!(
            properties_of(&schema)[0].widget,
            Widget::Choice(vec![json!("new")])
        );
    }

    #[test]
    fn a_schemars_documented_enum_is_a_choice() {
        // An enum whose variants carry doc comments becomes a `oneOf` of
        // `const` branches rather than a bare `enum` list (schemars 1.2.2).
        let schema = json!({
            "$defs": {
                "Mode": {
                    "oneOf": [
                        {"const": "Fast", "description": "Runs fast.", "type": "string"},
                        {"const": "Slow", "description": "Runs slow.", "type": "string"},
                    ]
                }
            },
            "type": "object",
            "properties": { "mode": {"$ref": "#/$defs/Mode"} },
        });
        assert_eq!(
            properties_of(&schema)[0].widget,
            Widget::Choice(vec![json!("Fast"), json!("Slow")])
        );
    }

    #[test]
    fn a_self_referential_schema_stops_at_the_cycle() {
        // A recursive type (trees, linked nodes) refs its own definition. The
        // walk must terminate, and the honest widget for the cycle is raw.
        let schema = json!({
            "$defs": {
                "Node": {
                    "type": "object",
                    "properties": {
                        "name": {"type": "string"},
                        "child": {"$ref": "#/$defs/Node"},
                    },
                }
            },
            "type": "object",
            "properties": { "root": {"$ref": "#/$defs/Node"} },
        });
        match &properties_of(&schema)[0].widget {
            Widget::Object(children) => {
                let child = children.iter().find(|p| p.name == "child").unwrap();
                assert!(matches!(child.widget, Widget::Raw(_)));
            }
            other => panic!("expected an object, got {other:?}"),
        }
    }

    #[test]
    fn a_ref_that_leaves_the_document_is_raw() {
        // A remote ref would need I/O, and a pointer to nothing is a broken
        // schema; both get the badge rather than a guess.
        assert!(matches!(
            widget_for(&json!({"$ref": "https://example.com/schema.json"})),
            Widget::Raw(_)
        ));
        assert!(matches!(
            widget_for(&json!({"$ref": "#/$defs/Missing"})),
            Widget::Raw(_)
        ));
    }

    #[test]
    fn required_properties_sort_first() {
        let schema = json!({
            "type": "object",
            "properties": {
                "alpha": {"type": "string"},
                "zulu": {"type": "string"},
            },
            "required": ["zulu"],
        });
        let names: Vec<String> = properties_of(&schema).into_iter().map(|p| p.name).collect();
        assert_eq!(names, ["zulu", "alpha"]);
    }

    #[test]
    fn seeding_fills_required_fields_and_leaves_optional_ones_absent() {
        let schema = json!({
            "type": "object",
            "properties": {
                "query": {"type": "string"},
                "limit": {"type": "integer"},
            },
            "required": ["query"],
        });
        let seeded = seed(&schema);
        assert_eq!(seeded["query"], json!(""));
        assert!(
            seeded.get("limit").is_none(),
            "an optional field must stay absent so the server applies its own default"
        );
    }

    #[test]
    fn a_default_is_used_even_for_an_optional_field() {
        let schema = json!({
            "type": "object",
            "properties": { "mode": {"type": "string", "default": "fast"} },
        });
        assert_eq!(seed(&schema)["mode"], json!("fast"));
    }

    #[test]
    fn a_pydantic_null_default_does_not_seed_an_explicit_null() {
        // pydantic stamps `"default": null` onto every `Optional[x] = None`
        // field. Seeding that in would start every Python tool call with a
        // payload full of explicit nulls; absent is what lets the server apply
        // its own default.
        let schema = json!({
            "type": "object",
            "properties": {
                "note": {
                    "anyOf": [{"type": "string"}, {"type": "null"}],
                    "default": null,
                }
            },
        });
        assert!(seed(&schema).get("note").is_none());
    }

    #[test]
    fn a_required_nullable_field_seeds_null_not_a_fabricated_value() {
        // pydantic's `Optional[int]` without a default is *required* — the
        // caller must send it, but may send null. Seeding `0` would put a
        // number the user never entered into the payload; `null` is the value
        // the tool itself said means "none".
        let schema = json!({
            "type": "object",
            "properties": {
                "limit": { "anyOf": [{"type": "integer"}, {"type": "null"}] },
                "tag": { "type": ["string", "null"] },
            },
            "required": ["limit", "tag"],
        });
        let seeded = seed(&schema);
        assert_eq!(seeded["limit"], Value::Null);
        assert_eq!(seeded["tag"], Value::Null, "both nullable spellings count");
    }

    #[test]
    fn seeding_a_required_enum_picks_its_first_value() {
        let schema = json!({
            "type": "object",
            "properties": { "mode": {"type": "string", "enum": ["fast", "slow"]} },
            "required": ["mode"],
        });
        assert_eq!(seed(&schema)["mode"], json!("fast"));
    }

    #[test]
    fn paths_read_and_write_through_nesting() {
        let mut value = json!({});
        set_path(&mut value, &["outer".into(), "inner".into()], json!("here"));
        assert_eq!(value, json!({"outer": {"inner": "here"}}));
        assert_eq!(
            get_path(&value, &["outer".into(), "inner".into()]),
            Some(&json!("here"))
        );
    }

    #[test]
    fn writing_null_removes_the_key() {
        let mut value = json!({"keep": 1, "drop": 2});
        set_path(&mut value, &["drop".into()], Value::Null);
        assert_eq!(value, json!({"keep": 1}));
    }

    #[test]
    fn a_path_through_a_non_object_is_replaced_rather_than_panicking() {
        // Happens when raw-JSON mode leaves a scalar where the schema wants an
        // object, then the user switches back to the form.
        let mut value = json!({"outer": "not an object"});
        set_path(&mut value, &["outer".into(), "inner".into()], json!(1));
        assert_eq!(value, json!({"outer": {"inner": 1}}));
    }

    #[test]
    fn lists_round_trip_through_the_textarea_form() {
        let parsed = parse_lines("one\ntwo\n\n three ", &Widget::Text);
        assert_eq!(parsed, json!(["one", "two", "three"]));
        assert_eq!(unparse_lines(Some(&parsed)), "one\ntwo\nthree");
    }

    #[test]
    fn numeric_lists_parse_as_numbers() {
        assert_eq!(
            parse_lines("1\n2", &Widget::Number { integer: true }),
            json!([1, 2])
        );
    }

    #[test]
    fn an_unparseable_number_keeps_what_was_typed() {
        // Substituting 0 would send a value the user never entered; letting the
        // server reject "12abc" tells them the truth.
        assert_eq!(
            parse_scalar("12abc", &Widget::Number { integer: true }),
            json!("12abc")
        );
    }

    #[test]
    fn prompt_arguments_become_a_usable_schema() {
        let arguments = json!([
            {"name": "path", "required": true, "description": "File"},
            {"name": "style"},
        ]);
        let schema = schema_for_prompt_arguments(Some(&arguments));
        let properties = properties_of(&schema);

        assert_eq!(properties.len(), 2);
        assert_eq!(properties[0].name, "path", "required sorts first");
        assert!(properties[0].required);
        assert!(!properties[1].required);
        assert_eq!(properties[0].widget, Widget::Text);
    }
}
