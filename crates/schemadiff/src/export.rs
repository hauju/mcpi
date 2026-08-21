//! Render a classified diff as Markdown, for pasting into a PR or changelog.
//!
//! The same rules as the diff drawer: judgement first ("1 breaking" before any
//! JSON), additions show only an `after` line and removals only a `before` one.
//! Lives here rather than in the app so a CLI can emit the identical artifact.

use crate::{ChangeKind, FieldChange, ItemChange, Severity, SnapshotDiff};

fn severity_label(severity: Severity) -> &'static str {
    match severity {
        Severity::Breaking => "breaking",
        Severity::Compatible => "compatible",
        Severity::Cosmetic => "cosmetic",
    }
}

/// Markdown for a whole diff. `subtitle` names the two moments being compared
/// ("mockserver · 12 Jun 2026 14:02 → 3 Aug 2026 09:15"); empty omits the line.
pub fn to_markdown(diff: &SnapshotDiff, subtitle: &str) -> String {
    let mut out = String::from("# Contract changes\n");

    if !subtitle.is_empty() {
        out.push_str(&format!("\n{subtitle}\n"));
    }

    let counts = diff.counts();
    if counts.total() == 0 {
        out.push_str("\nNo contract changes.\n");
    } else {
        let mut parts = Vec::new();
        if counts.breaking > 0 {
            parts.push(format!("**{} breaking**", counts.breaking));
        }
        if counts.compatible > 0 {
            parts.push(format!("{} compatible", counts.compatible));
        }
        if counts.cosmetic > 0 {
            parts.push(format!("{} cosmetic", counts.cosmetic));
        }
        out.push_str(&format!("\n{}\n", parts.join(" · ")));
    }

    group(&mut out, "Tools", &diff.tools);
    group(&mut out, "Resources", &diff.resources);
    group(&mut out, "Prompts", &diff.prompts);

    if !diff.capabilities.is_empty() {
        out.push_str("\n## Capabilities\n\n");
        for change in &diff.capabilities {
            field(&mut out, change);
        }
    }

    if diff.protocol_version.is_some() || diff.server_version.is_some() {
        out.push('\n');
        if let Some((before, after)) = &diff.protocol_version {
            out.push_str(&format!("Protocol version: {before} → {after}\n"));
        }
        if let Some((before, after)) = &diff.server_version {
            out.push_str(&format!("Server version: {before} → {after}\n"));
        }
    }

    out
}

fn group(out: &mut String, title: &str, changes: &[ItemChange]) {
    if changes.is_empty() {
        return;
    }
    out.push_str(&format!("\n## {title}\n"));
    for change in changes {
        let (verb, fields): (_, &[FieldChange]) = match change {
            ItemChange::Added { .. } => ("added", &[]),
            ItemChange::Removed { .. } => ("removed", &[]),
            ItemChange::Modified { fields, .. } => ("changed", fields),
        };
        let severity = severity_label(change.severity());
        out.push_str(&format!(
            "\n### {} · {verb} · {severity}\n",
            code(change.name())
        ));
        if !fields.is_empty() {
            out.push('\n');
            for change in fields {
                field(out, change);
            }
        }
    }
}

fn field(out: &mut String, change: &FieldChange) {
    let severity = match change.severity {
        Severity::Breaking => "**breaking**".to_string(),
        other => severity_label(other).to_string(),
    };
    out.push_str(&format!(
        "- {severity} {} — {}\n",
        code(&change.path),
        change.note
    ));
    if change.kind != ChangeKind::Added
        && let Some(before) = &change.before
    {
        out.push_str(&format!("  - before: {}\n", code(&before.to_string())));
    }
    if change.kind != ChangeKind::Removed
        && let Some(after) = &change.after
    {
        out.push_str(&format!("  - after: {}\n", code(&after.to_string())));
    }
}

/// Inline code that survives payloads containing backticks.
fn code(text: &str) -> String {
    if text.contains('`') {
        format!("`` {text} ``")
    } else {
        format!("`{text}`")
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::to_markdown;
    use crate::{FieldChange, ItemChange, Severity, SnapshotDiff};

    fn sample() -> SnapshotDiff {
        SnapshotDiff {
            tools: vec![
                ItemChange::Removed {
                    name: "legacy".into(),
                },
                ItemChange::Modified {
                    name: "search".into(),
                    fields: vec![
                        FieldChange::new(
                            "inputSchema.properties.limit",
                            Severity::Breaking,
                            "required property added — existing callers do not send it",
                            None,
                            Some(&json!({"type": "number"})),
                        ),
                        FieldChange::new(
                            "description",
                            Severity::Cosmetic,
                            "description changed",
                            Some(&json!("Search things")),
                            Some(&json!("Search all the things")),
                        ),
                    ],
                },
            ],
            capabilities: vec![FieldChange::new(
                "prompts",
                Severity::Compatible,
                "`prompts` capability added",
                None,
                Some(&json!({})),
            )],
            server_version: Some(("1.0.0".into(), "1.1.0".into())),
            ..Default::default()
        }
    }

    #[test]
    fn leads_with_the_judgement() {
        let md = to_markdown(&sample(), "fixture · then → now");
        // Counts are per item: the modified tool's cosmetic field is folded
        // into its breaking headline, same as the drawer.
        let counts = md.find("**2 breaking** · 1 compatible");
        let evidence = md.find("## Tools");
        assert!(counts.unwrap() < evidence.unwrap(), "{md}");
        assert!(md.contains("fixture · then → now"));
    }

    #[test]
    fn renders_every_group_and_field() {
        let md = to_markdown(&sample(), "");
        assert!(md.contains("### `legacy` · removed · breaking"), "{md}");
        assert!(md.contains("### `search` · changed · breaking"), "{md}");
        assert!(
            md.contains("- **breaking** `inputSchema.properties.limit` — required property added"),
            "{md}"
        );
        assert!(md.contains("## Capabilities"), "{md}");
        assert!(md.contains("Server version: 1.0.0 → 1.1.0"), "{md}");
    }

    #[test]
    fn additions_omit_before_and_removals_omit_after() {
        let md = to_markdown(&sample(), "");
        assert!(md.contains("  - after: `{\"type\":\"number\"}`"), "{md}");
        assert!(!md.contains("- before: `null`"), "{md}");
        assert!(md.contains("  - before: `\"Search things\"`"), "{md}");
    }

    #[test]
    fn an_empty_diff_says_so() {
        let md = to_markdown(&SnapshotDiff::default(), "");
        assert!(md.contains("No contract changes."), "{md}");
    }
}
