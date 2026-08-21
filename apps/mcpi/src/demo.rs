//! The built-in sample change.
//!
//! The diff only demonstrates itself when a server actually changes, which can
//! take months of real use — this pair of hand-written snapshots is how anyone
//! sees the feature on demand. The diff is computed through `schemadiff::diff`
//! at the moment it is opened, never stored, so the sample can never drift
//! from what the classifier actually reports.
//!
//! The schemas are shaped like what real generators emit — `$defs` behind
//! `$ref`s, a required list, description strings — and each severity class
//! gets its own tool, because an item is reported at its worst field's
//! severity: a compatible edit inside a breaking tool would be evidence under
//! that tool, not a row of its own.

use serde_json::json;

use crate::state::HistoryView;

pub fn sample_view() -> HistoryView {
    // Breaking twice over: a new required argument, and an enum gutted behind
    // a `$ref` that itself never changed.
    let search_docs_before = json!({
        "name": "search_docs",
        "description": "Search the documentation index.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": "Search terms" },
                "status": { "$ref": "#/$defs/Status" }
            },
            "required": ["query"],
            "$defs": {
                "Status": { "type": "string", "enum": ["draft", "published", "archived"] }
            }
        }
    });
    let search_docs_after = json!({
        "name": "search_docs",
        "description": "Search the documentation index.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": "Search terms" },
                "status": { "$ref": "#/$defs/Status" },
                "workspace": { "type": "string", "description": "Workspace slug" }
            },
            "required": ["query", "workspace"],
            "$defs": {
                "Status": { "type": "string", "enum": ["draft", "published"] }
            }
        }
    });

    // Compatible: an optional argument appeared; existing callers are fine.
    let get_page_before = json!({
        "name": "get_page",
        "description": "Fetch one page by id.",
        "inputSchema": {
            "type": "object",
            "properties": { "id": { "type": "string" } },
            "required": ["id"]
        }
    });
    let get_page_after = json!({
        "name": "get_page",
        "description": "Fetch one page by id.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "id": { "type": "string" },
                "include_drafts": { "type": "boolean", "description": "Also match unpublished pages" }
            },
            "required": ["id"]
        }
    });

    // Cosmetic: only the prose moved.
    let list_tags_before = json!({
        "name": "list_tags",
        "description": "List tags.",
        "inputSchema": { "type": "object", "properties": {} }
    });
    let list_tags_after = json!({
        "name": "list_tags",
        "description": "List every tag in use, most recent first.",
        "inputSchema": { "type": "object", "properties": {} }
    });

    // And one removal, which needs no `after` at all.
    let delete_page = json!({
        "name": "delete_page",
        "description": "Delete a page by id.",
        "inputSchema": {
            "type": "object",
            "properties": { "id": { "type": "string" } },
            "required": ["id"]
        }
    });

    let before = snapshot(
        "1.4.0",
        vec![
            ("search_docs", search_docs_before),
            ("get_page", get_page_before),
            ("list_tags", list_tags_before),
            ("delete_page", delete_page),
        ],
    );
    let after = snapshot(
        "1.5.0",
        vec![
            ("search_docs", search_docs_after),
            ("get_page", get_page_after),
            ("list_tags", list_tags_after),
        ],
    );

    HistoryView {
        subtitle: "sample-server · what a recorded change looks like".into(),
        diff: Box::new(schemadiff::diff(&before, &after)),
    }
}

fn snapshot(version: &str, tools: Vec<(&str, serde_json::Value)>) -> schemadiff::Snapshot {
    let mut snapshot = schemadiff::Snapshot {
        protocol_version: "2025-06-18".into(),
        server_name: "sample-server".into(),
        server_version: version.into(),
        ..Default::default()
    };
    for (name, tool) in tools {
        snapshot.tools.insert(name.into(), tool);
    }
    snapshot
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The sample's one job is showing every severity class at once. If a
    /// classifier change empties one of these, the sample needs new material,
    /// not a weaker assertion.
    #[test]
    fn the_sample_shows_all_three_severities() {
        let view = sample_view();
        let counts = view.diff.counts();
        assert!(counts.breaking > 0, "sample lost its breaking changes");
        assert!(counts.compatible > 0, "sample lost its compatible change");
        assert!(counts.cosmetic > 0, "sample lost its cosmetic change");
    }
}
