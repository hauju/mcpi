//! Compares two MCP capability snapshots and classifies every change as
//! breaking, compatible, or cosmetic.
//!
//! A textual diff of two JSON schemas is close to worthless — schemas are large
//! and reorder freely. The useful output is a *judgement*: "one of these three
//! changes will break existing callers". That judgement is what this crate
//! produces, and everything else in the product hangs off it.
//!
//! Snapshots hold raw [`Value`]s rather than typed MCP structs so that fields
//! added by future revisions of the MCP spec still show up in a diff instead of
//! being silently dropped on deserialize. It also keeps this crate free of any
//! dependency on `rmcp`, which makes it trivially unit-testable.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

mod canonical;
mod export;
mod schema;
#[cfg(test)]
mod tests;

pub use canonical::canonical_json;
pub use export::to_markdown;
use schema::{Direction, diff_schema};

/// How much a change can hurt.
///
/// Ordered, so `max()` over a set of changes gives the headline severity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    /// Documentation and presentation only. Nothing observable changes.
    Cosmetic,
    /// Existing callers keep working.
    Compatible,
    /// Existing callers can break.
    Breaking,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ChangeKind {
    Added,
    Removed,
    Changed,
}

/// A single classified difference, addressed by a dotted path into the item's
/// JSON (`inputSchema.properties.limit.type`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FieldChange {
    pub path: String,
    pub kind: ChangeKind,
    pub severity: Severity,
    /// Why it was classified this way. Shown verbatim in the diff drawer, so it
    /// is written for the person reading it, not for a log.
    pub note: String,
    pub before: Option<Value>,
    pub after: Option<Value>,
    /// The engine could not read this change, so `severity` leans breaking
    /// rather than stay quiet. See [`SnapshotDiff::unconfirmed_breaking_items`].
    #[serde(default)]
    pub unanalysed: bool,
}

impl FieldChange {
    pub(crate) fn new(
        path: impl Into<String>,
        severity: Severity,
        note: impl Into<String>,
        before: Option<&Value>,
        after: Option<&Value>,
    ) -> Self {
        let kind = match (before, after) {
            (None, Some(_)) => ChangeKind::Added,
            (Some(_), None) => ChangeKind::Removed,
            _ => ChangeKind::Changed,
        };
        Self {
            path: path.into(),
            kind,
            severity,
            note: note.into(),
            before: before.cloned(),
            after: after.cloned(),
            unanalysed: false,
        }
    }
}

/// What happened to one tool, resource, or prompt.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum ItemChange {
    Added {
        name: String,
    },
    Removed {
        name: String,
    },
    Modified {
        name: String,
        fields: Vec<FieldChange>,
    },
}

impl ItemChange {
    pub fn name(&self) -> &str {
        match self {
            Self::Added { name } | Self::Removed { name } | Self::Modified { name, .. } => name,
        }
    }

    pub fn severity(&self) -> Severity {
        match self {
            // A new tool cannot break a caller that never knew about it.
            Self::Added { .. } => Severity::Compatible,
            Self::Removed { .. } => Severity::Breaking,
            Self::Modified { fields, .. } => fields
                .iter()
                .map(|f| f.severity)
                .max()
                .unwrap_or(Severity::Cosmetic),
        }
    }

    /// Every breaking field is one the engine could not read.
    fn breaks_only_unanalysed(&self) -> bool {
        match self {
            Self::Modified { fields, .. } => fields
                .iter()
                .filter(|f| f.severity == Severity::Breaking)
                .all(|f| f.unanalysed),
            Self::Added { .. } | Self::Removed { .. } => false,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Counts {
    pub breaking: usize,
    pub compatible: usize,
    pub cosmetic: usize,
}

impl Counts {
    pub fn total(&self) -> usize {
        self.breaking + self.compatible + self.cosmetic
    }

    fn add(&mut self, severity: Severity) {
        match severity {
            Severity::Breaking => self.breaking += 1,
            Severity::Compatible => self.compatible += 1,
            Severity::Cosmetic => self.cosmetic += 1,
        }
    }
}

/// One server's advertised capabilities at a point in time.
///
/// Keyed maps rather than lists: servers do not promise a stable order for
/// `tools/list`, so comparing positionally would report churn on every connect.
/// `BTreeMap` also makes serialization deterministic, which the store's digest
/// depends on.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Snapshot {
    pub protocol_version: String,
    pub server_name: String,
    pub server_version: String,
    /// Raw `ServerCapabilities` object.
    #[serde(default)]
    pub capabilities: Value,
    /// Tool name → the full object returned by `tools/list`.
    #[serde(default)]
    pub tools: BTreeMap<String, Value>,
    /// Resource URI → the full object returned by `resources/list`.
    #[serde(default)]
    pub resources: BTreeMap<String, Value>,
    /// Prompt name → the full object returned by `prompts/list`.
    #[serde(default)]
    pub prompts: BTreeMap<String, Value>,
    /// Event name → the full object returned by `events/list` (the MCP events
    /// extension). Omitted when empty so that the digest of every server
    /// without events is unchanged — otherwise each one would record a
    /// phantom contract change on its next connect.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub events: BTreeMap<String, Value>,
}

impl Snapshot {
    /// Stable string for content-addressing this snapshot.
    ///
    /// The store hashes this and only writes a new row when it moves, so a
    /// server reconnected fifty times a day still leaves one row per actual
    /// change. Everything the user can see is included — a bare version bump
    /// with no contract change is still worth recording as a distinct
    /// observation.
    pub fn digest_source(&self) -> String {
        let value = serde_json::to_value(self)
            .expect("Snapshot holds only JSON-representable types, so this cannot fail");
        canonical_json(&value)
    }

    /// [`Self::digest_source`] minus what moves without the contract moving:
    /// the `server_version` string, each item's `_meta`, and the order of
    /// value lists inside tool schemas.
    ///
    /// For a store that should keep one row per contract change rather than
    /// one per deploy. Every exclusion is something [`diff`] already ignores
    /// or calls cosmetic, and only at the places its walker reaches — so two
    /// snapshots with one contract digest never differ by a breaking,
    /// compatible, or instruction change. A `_meta` *property* of an input
    /// schema, or an `enum` inside a real `oneOf`, still moves the digest.
    pub fn contract_digest_source(&self) -> String {
        let mut value = serde_json::to_value(self)
            .expect("Snapshot holds only JSON-representable types, so this cannot fail");
        let Value::Object(map) = &mut value else {
            unreachable!("Snapshot serializes to an object");
        };
        map.remove("server_version");
        for kind in ["tools", "resources", "prompts", "events"] {
            let Some(Value::Object(items)) = map.get_mut(kind) else {
                continue;
            };
            for item in items.values_mut().filter_map(Value::as_object_mut) {
                item.remove("_meta");
                let schemas: &[&str] = match kind {
                    "tools" => &["inputSchema", "outputSchema"],
                    "events" => &["inputSchema", "payloadSchema"],
                    _ => &[],
                };
                for key in schemas {
                    if let Some(schema) = item.get_mut(*key) {
                        schema::normalize(schema);
                    }
                }
            }
        }
        canonical_json(&value)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SnapshotDiff {
    pub tools: Vec<ItemChange>,
    pub resources: Vec<ItemChange>,
    pub prompts: Vec<ItemChange>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub events: Vec<ItemChange>,
    pub capabilities: Vec<FieldChange>,
    /// `(before, after)` when the negotiated protocol version moved.
    pub protocol_version: Option<(String, String)>,
    /// `(before, after)` when the server reported a new version string.
    pub server_version: Option<(String, String)>,
}

impl SnapshotDiff {
    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
            && self.resources.is_empty()
            && self.prompts.is_empty()
            && self.events.is_empty()
            && self.capabilities.is_empty()
            && self.protocol_version.is_none()
            && self.server_version.is_none()
    }

    /// The worst severity in the diff, or `None` when nothing changed.
    ///
    /// Version strings are deliberately excluded: a bumped version number is an
    /// observation, not a change to the contract.
    pub fn severity(&self) -> Option<Severity> {
        self.tools
            .iter()
            .chain(&self.resources)
            .chain(&self.prompts)
            .chain(&self.events)
            .map(ItemChange::severity)
            .chain(self.capabilities.iter().map(|f| f.severity))
            .max()
    }

    pub fn counts(&self) -> Counts {
        let mut counts = Counts::default();
        for item in self
            .tools
            .iter()
            .chain(&self.resources)
            .chain(&self.prompts)
            .chain(&self.events)
        {
            counts.add(item.severity());
        }
        for field in &self.capabilities {
            counts.add(field.severity);
        }
        counts
    }

    /// Names of items whose *instructions* were rewritten — a changed
    /// `description` or `annotations` — while the callable contract may not
    /// have moved at all.
    ///
    /// Deliberately not a severity, and deliberately not folded into one. A
    /// rewritten description is `Cosmetic`, and that stays correct: no
    /// caller's code breaks. But the description *is* the instruction an agent
    /// follows, and `annotations` is what it consents on, so a silent edit to
    /// either is the shape a rug pull takes. Severity answers "will my code
    /// still work"; this answers "did the instructions move". Collapsing them
    /// would make one of the two questions unanswerable.
    ///
    /// Naming the fact is the whole contribution. Two snapshots cannot show
    /// whether an edit was innocent, and every consumer of this — the site
    /// included — must say what changed and never that a server is malicious.
    ///
    /// Only `Changed` counts. A tool that gains a description it never had
    /// reads as `Added`, and a first description is documentation, not a
    /// rewrite.
    pub fn instruction_rewrites(&self) -> Vec<&str> {
        self.tools
            .iter()
            .chain(&self.resources)
            .chain(&self.prompts)
            .chain(&self.events)
            .filter_map(|item| match item {
                ItemChange::Modified { name, fields } => fields
                    .iter()
                    .any(|field| {
                        field.kind == ChangeKind::Changed
                            && INSTRUCTION_FIELDS.contains(&field.path.as_str())
                    })
                    .then_some(name.as_str()),
                ItemChange::Added { .. } | ItemChange::Removed { .. } => None,
            })
            .collect()
    }

    /// Names of items with at least one breaking change, for badging the list.
    pub fn breaking_items(&self) -> Vec<&str> {
        self.tools
            .iter()
            .chain(&self.resources)
            .chain(&self.prompts)
            .chain(&self.events)
            .filter(|i| i.severity() == Severity::Breaking)
            .map(ItemChange::name)
            .collect()
    }

    /// The part of [`Self::breaking_items`] this diff can prove.
    pub fn confirmed_breaking_items(&self) -> Vec<&str> {
        self.breaking_by_confirmation(false)
    }

    /// The part of [`Self::breaking_items`] this diff cannot prove: a composed
    /// schema the engine does not analyse, or a removed resource.
    ///
    /// Deliberately not a severity, for the same reason as
    /// [`Self::instruction_rewrites`]. `Breaking` stays the answer for anything
    /// that must not pass silently, so CI still fails on these. This answers a
    /// different question, "can we state this publicly as fact". A resource
    /// list is data, so a removal is usually a row going away, and a changed
    /// `oneOf` may only have widened. Either can still break a caller, so a
    /// consumer must say these need review and never that they are safe.
    pub fn unconfirmed_breaking_items(&self) -> Vec<&str> {
        self.breaking_by_confirmation(true)
    }

    fn breaking_by_confirmation(&self, unconfirmed: bool) -> Vec<&str> {
        let contract = |item: &&ItemChange| item.breaks_only_unanalysed() == unconfirmed;
        let resource = |item: &&ItemChange| {
            (item.breaks_only_unanalysed() || matches!(item, ItemChange::Removed { .. }))
                == unconfirmed
        };
        self.tools
            .iter()
            .filter(contract)
            .chain(self.resources.iter().filter(resource))
            .chain(self.prompts.iter().filter(contract))
            .chain(self.events.iter().filter(contract))
            .filter(|i| i.severity() == Severity::Breaking)
            .map(ItemChange::name)
            .collect()
    }
}

/// Compare two snapshots of the same server.
pub fn diff(before: &Snapshot, after: &Snapshot) -> SnapshotDiff {
    SnapshotDiff {
        tools: diff_items(&before.tools, &after.tools, diff_tool),
        resources: diff_items(&before.resources, &after.resources, diff_resource),
        prompts: diff_items(&before.prompts, &after.prompts, diff_prompt),
        events: diff_items(&before.events, &after.events, diff_event),
        capabilities: diff_capabilities(&before.capabilities, &after.capabilities),
        protocol_version: pair(&before.protocol_version, &after.protocol_version),
        server_version: pair(&before.server_version, &after.server_version),
    }
}

fn pair(before: &str, after: &str) -> Option<(String, String)> {
    (before != after).then(|| (before.to_string(), after.to_string()))
}

fn diff_items(
    before: &BTreeMap<String, Value>,
    after: &BTreeMap<String, Value>,
    diff_one: fn(&Value, &Value) -> Vec<FieldChange>,
) -> Vec<ItemChange> {
    let mut changes = Vec::new();

    for (name, before_item) in before {
        match after.get(name) {
            None => changes.push(ItemChange::Removed { name: name.clone() }),
            Some(after_item) => {
                let fields = diff_one(before_item, after_item);
                if !fields.is_empty() {
                    changes.push(ItemChange::Modified {
                        name: name.clone(),
                        fields,
                    });
                }
            }
        }
    }

    for name in after.keys() {
        if !before.contains_key(name) {
            changes.push(ItemChange::Added { name: name.clone() });
        }
    }

    changes.sort_by(|a, b| a.name().cmp(b.name()));
    changes
}

/// Fields whose only effect is on how an item reads to a human.
const PROSE_FIELDS: [&str; 4] = ["description", "title", "annotations", "_meta"];

/// The subset of [`PROSE_FIELDS`] an agent acts on rather than merely displays:
/// `description` is the instruction it follows, `annotations` is what it
/// consents on. `title` and `_meta` are left out — they move a label, not a
/// behaviour, and including them would make [`SnapshotDiff::instruction_rewrites`]
/// fire on cosmetic churn until nobody reads it.
const INSTRUCTION_FIELDS: [&str; 2] = ["description", "annotations"];

fn diff_tool(before: &Value, after: &Value) -> Vec<FieldChange> {
    let mut out = Vec::new();
    let (b, a) = match (before.as_object(), after.as_object()) {
        (Some(b), Some(a)) => (b, a),
        _ => return out,
    };

    diff_prose(b, a, &mut out);

    // The two schemas describe traffic flowing in opposite directions, so the
    // same structural edit means opposite things on each.
    diff_schema(
        "inputSchema",
        b.get("inputSchema").unwrap_or(&Value::Null),
        a.get("inputSchema").unwrap_or(&Value::Null),
        Direction::Input,
        &mut out,
    );

    match (b.get("outputSchema"), a.get("outputSchema")) {
        (None, Some(after_schema)) => out.push(FieldChange::new(
            "outputSchema",
            Severity::Compatible,
            "output schema added — callers that ignored the result are unaffected",
            None,
            Some(after_schema),
        )),
        (Some(before_schema), None) => out.push(FieldChange::new(
            "outputSchema",
            Severity::Breaking,
            "output schema removed — structured results are no longer guaranteed",
            Some(before_schema),
            None,
        )),
        (Some(before_schema), Some(after_schema)) => diff_schema(
            "outputSchema",
            before_schema,
            after_schema,
            Direction::Output,
            &mut out,
        ),
        (None, None) => {}
    }

    out
}

/// An event type from the MCP events extension. Its two schemas mirror a
/// tool's: `inputSchema` is what a subscriber sends (and re-sends on every
/// webhook refresh), `payloadSchema` is the `data` it receives.
fn diff_event(before: &Value, after: &Value) -> Vec<FieldChange> {
    let mut out = Vec::new();
    let (b, a) = match (before.as_object(), after.as_object()) {
        (Some(b), Some(a)) => (b, a),
        _ => return out,
    };

    diff_prose(b, a, &mut out);

    diff_schema(
        "inputSchema",
        b.get("inputSchema").unwrap_or(&Value::Null),
        a.get("inputSchema").unwrap_or(&Value::Null),
        Direction::Input,
        &mut out,
    );
    diff_schema(
        "payloadSchema",
        b.get("payloadSchema").unwrap_or(&Value::Null),
        a.get("payloadSchema").unwrap_or(&Value::Null),
        Direction::Output,
        &mut out,
    );

    let modes = |o: &serde_json::Map<String, Value>| -> Vec<Value> {
        o.get("delivery")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
    };
    let (before_modes, after_modes) = (modes(b), modes(a));
    for mode in &before_modes {
        if !after_modes.contains(mode) {
            out.push(FieldChange::new(
                format!("delivery.{}", mode.as_str().unwrap_or_default()),
                Severity::Breaking,
                "delivery mode dropped — subscribers using it can no longer subscribe",
                Some(mode),
                None,
            ));
        }
    }
    for mode in &after_modes {
        if !before_modes.contains(mode) {
            out.push(FieldChange::new(
                format!("delivery.{}", mode.as_str().unwrap_or_default()),
                Severity::Compatible,
                "delivery mode added",
                None,
                Some(mode),
            ));
        }
    }

    out
}

fn diff_resource(before: &Value, after: &Value) -> Vec<FieldChange> {
    let mut out = Vec::new();
    let (b, a) = match (before.as_object(), after.as_object()) {
        (Some(b), Some(a)) => (b, a),
        _ => return out,
    };

    diff_prose(b, a, &mut out);

    // Consumers branch on mimeType to decide how to parse the body.
    if b.get("mimeType") != a.get("mimeType") {
        out.push(FieldChange::new(
            "mimeType",
            Severity::Breaking,
            "MIME type changed — consumers parse the body based on this",
            b.get("mimeType"),
            a.get("mimeType"),
        ));
    }

    out
}

fn diff_prompt(before: &Value, after: &Value) -> Vec<FieldChange> {
    let mut out = Vec::new();
    let (b, a) = match (before.as_object(), after.as_object()) {
        (Some(b), Some(a)) => (b, a),
        _ => return out,
    };

    diff_prose(b, a, &mut out);

    let empty = Vec::new();
    let before_args = b
        .get("arguments")
        .and_then(Value::as_array)
        .unwrap_or(&empty);
    let after_args = a
        .get("arguments")
        .and_then(Value::as_array)
        .unwrap_or(&empty);

    let arg_name = |v: &Value| {
        v.get("name")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string()
    };
    let is_required = |v: &Value| v.get("required").and_then(Value::as_bool).unwrap_or(false);

    for before_arg in before_args {
        let name = arg_name(before_arg);
        let path = format!("arguments.{name}");
        match after_args.iter().find(|a| arg_name(a) == name) {
            None => out.push(FieldChange::new(
                &path,
                Severity::Breaking,
                "argument removed",
                Some(before_arg),
                None,
            )),
            Some(after_arg) => {
                match (is_required(before_arg), is_required(after_arg)) {
                    (false, true) => out.push(FieldChange::new(
                        format!("{path}.required"),
                        Severity::Breaking,
                        "argument became required — existing callers omit it",
                        Some(&Value::Bool(false)),
                        Some(&Value::Bool(true)),
                    )),
                    (true, false) => out.push(FieldChange::new(
                        format!("{path}.required"),
                        Severity::Compatible,
                        "argument became optional",
                        Some(&Value::Bool(true)),
                        Some(&Value::Bool(false)),
                    )),
                    _ => {}
                }
                if before_arg.get("description") != after_arg.get("description") {
                    out.push(FieldChange::new(
                        format!("{path}.description"),
                        Severity::Cosmetic,
                        "description changed",
                        before_arg.get("description"),
                        after_arg.get("description"),
                    ));
                }
            }
        }
    }

    for after_arg in after_args {
        let name = arg_name(after_arg);
        if before_args.iter().any(|b| arg_name(b) == name) {
            continue;
        }
        let (severity, note) = if is_required(after_arg) {
            (
                Severity::Breaking,
                "required argument added — existing callers do not send it",
            )
        } else {
            (Severity::Compatible, "optional argument added")
        };
        out.push(FieldChange::new(
            format!("arguments.{name}"),
            severity,
            note,
            None,
            Some(after_arg),
        ));
    }

    out
}

fn diff_prose(
    before: &serde_json::Map<String, Value>,
    after: &serde_json::Map<String, Value>,
    out: &mut Vec<FieldChange>,
) {
    for field in PROSE_FIELDS {
        if before.get(field) != after.get(field) {
            out.push(FieldChange::new(
                field,
                Severity::Cosmetic,
                format!("{field} changed"),
                before.get(field),
                after.get(field),
            ));
        }
    }
}

fn diff_capabilities(before: &Value, after: &Value) -> Vec<FieldChange> {
    let mut out = Vec::new();
    let empty = serde_json::Map::new();
    let b = before.as_object().unwrap_or(&empty);
    let a = after.as_object().unwrap_or(&empty);

    for (name, before_value) in b {
        match a.get(name) {
            None => out.push(FieldChange::new(
                name,
                Severity::Breaking,
                format!("`{name}` capability withdrawn"),
                Some(before_value),
                None,
            )),
            Some(after_value) if after_value != before_value => out.push(FieldChange::new(
                name,
                Severity::Compatible,
                format!("`{name}` capability options changed"),
                Some(before_value),
                Some(after_value),
            )),
            Some(_) => {}
        }
    }

    for (name, after_value) in a {
        if !b.contains_key(name) {
            out.push(FieldChange::new(
                name,
                Severity::Compatible,
                format!("`{name}` capability added"),
                None,
                Some(after_value),
            ));
        }
    }

    out.sort_by(|x, y| x.path.cmp(&y.path));
    out
}
