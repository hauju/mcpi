//! The schema diff: what moved since the last time this server was connected.
//!
//! This is the feature the product exists for, so the presentation puts the
//! *judgement* first — "1 breaking" before any of the JSON — and only then the
//! evidence. A wall of before/after payloads is what the free inspector's users
//! already get by squinting at two terminal windows.

use dioxus::prelude::*;
use dioxus_free_icons::Icon;
use dioxus_free_icons::icons::ld_icons::{
    LdChevronRight, LdCircleCheck, LdGitCompareArrows, LdInfo, LdX,
};
use schemadiff::{ChangeKind, FieldChange, ItemChange, Severity, SnapshotDiff};
use serde_json::Value;

use crate::state::{AppState, ContractStatus};

/// Colours are reserved for severity throughout the app, so these functions
/// are the only place they are chosen. Breaking is solid, compatible is soft,
/// cosmetic is barely there — the badge volume matches the verdict.
fn severity_badge(severity: Severity) -> &'static str {
    match severity {
        Severity::Breaking => "badge badge-xs badge-error",
        Severity::Compatible => "badge badge-xs badge-soft badge-info",
        Severity::Cosmetic => "badge badge-xs badge-ghost",
    }
}

fn severity_label(severity: Severity) -> &'static str {
    match severity {
        Severity::Breaking => "breaking",
        Severity::Compatible => "compatible",
        Severity::Cosmetic => "cosmetic",
    }
}

/// The coloured left edge on a change card: readable from across the room,
/// before any label is.
fn severity_edge(severity: Severity) -> &'static str {
    match severity {
        Severity::Breaking => "border-l-2 border-l-error",
        Severity::Compatible => "border-l-2 border-l-info",
        Severity::Cosmetic => "border-l-2 border-l-base-300",
    }
}

/// A one-line summary above the item list, and the way into the full diff.
#[component]
pub fn DiffBanner() -> Element {
    let app = use_context::<AppState>();
    let mut open = app.diff_open;

    let Some(connected) = app.active() else {
        return rsx! {};
    };

    match &connected.status {
        // "Never seen before" and "seen, and identical" are different answers,
        // and only the second one has earned the green check.
        ContractStatus::First => rsx! {
            div { class: "flex items-center gap-2 px-3 py-1.5 text-xs text-base-content/50 bg-base-200/40 border-b border-base-300/60",
                Icon {
                    icon: LdInfo,
                    width: 12,
                    height: 12,
                    class: "shrink-0 text-base-content/35",
                }
                "First time connecting — nothing to compare against yet."
                // Waiting for a real change can take months; the sample is
                // how someone sees the feature before one happens.
                button {
                    class: "ml-auto flex items-center gap-0.5 text-base-content/55 hover:text-base-content transition-colors",
                    onclick: move |_| app.open_sample_diff(),
                    "See what a change looks like"
                    Icon { icon: LdChevronRight, width: 12, height: 12 }
                }
            }
        },
        ContractStatus::Unchanged => {
            // "Unchanged" used to be where the previous diff went to die: the
            // work was recorded, but reconnecting hid it. The way back in
            // belongs on exactly this line.
            let last_change = {
                let snapshots = app.snapshots.read();
                (snapshots.len() >= 2).then(|| (snapshots[1].clone(), snapshots[0].clone()))
            }
            .map(|(before, after)| {
                let when = after.taken_at.with_timezone(&chrono::Local).format("%-d %b");
                rsx! {
                    button {
                        class: "ml-auto flex items-center gap-0.5 text-base-content/55 hover:text-base-content transition-colors",
                        onclick: move |_| app.open_history_diff(&before, &after),
                        "Last change · {when}"
                        Icon { icon: LdChevronRight, width: 12, height: 12 }
                    }
                }
            });

            rsx! {
                div { class: "flex items-center gap-2 px-3 py-1.5 text-xs text-base-content/50 bg-base-200/40 border-b border-base-300/60",
                    Icon {
                        icon: LdCircleCheck,
                        width: 12,
                        height: 12,
                        class: "shrink-0 text-success/70",
                    }
                    "No changes since the last connect."
                    {last_change}
                }
            }
        }
        ContractStatus::Changed(diff) => {
            let counts = diff.counts();
            let breaking = counts.breaking;
            let (tone, icon_tone) = if breaking > 0 {
                (
                    "bg-error/10 border-b-error/20 border-l-error hover:bg-error/15",
                    "text-error",
                )
            } else {
                (
                    "bg-info/10 border-b-info/20 border-l-info hover:bg-info/15",
                    "text-info",
                )
            };

            rsx! {
                button {
                    class: "group w-full flex items-center gap-2.5 px-3 py-2 text-left text-xs border-b border-l-2 {tone} transition-colors",
                    onclick: move |_| open.set(true),
                    Icon {
                        icon: LdGitCompareArrows,
                        width: 14,
                        height: 14,
                        class: "shrink-0 {icon_tone}",
                    }
                    span { class: "font-medium text-[13px]",
                        "{counts.total()} contract change"
                        {if counts.total() == 1 { "" } else { "s" }}
                    }
                    if breaking > 0 {
                        span { class: "badge badge-xs badge-error", "{breaking} breaking" }
                    }
                    if counts.compatible > 0 {
                        span { class: "text-base-content/55", "{counts.compatible} compatible" }
                    }
                    if counts.cosmetic > 0 {
                        span { class: "text-base-content/40", "{counts.cosmetic} cosmetic" }
                    }
                    span { class: "ml-auto flex items-center gap-0.5 text-base-content/55 group-hover:text-base-content transition-colors",
                        "Review"
                        Icon { icon: LdChevronRight, width: 12, height: 12 }
                    }
                }
            }
        }
    }
}

/// The full diff, as a modal.
///
/// Shows either the live connection's diff or a past one opened from the
/// snapshot timeline — the latter takes precedence and does not require a
/// connection at all, which is what keeps a recorded change reachable after
/// disconnect.
#[component]
pub fn DiffDrawer() -> Element {
    let app = use_context::<AppState>();
    let mut open = app.diff_open;
    let mut history_view = app.history_view;
    // Which export button last succeeded, so it can say "Copied" in place —
    // the notice banner is styled for failures.
    let mut copied = use_signal(|| None::<&'static str>);

    if !*open.read() {
        return rsx! {};
    }

    let (subtitle, diff) = if let Some(view) = history_view.read().as_ref() {
        (view.subtitle.clone(), (*view.diff).clone())
    } else {
        let Some(connected) = app.active() else {
            return rsx! {};
        };
        let Some(diff) = connected.status.diff().cloned() else {
            return rsx! {};
        };
        (
            format!(
                "{} · since the previous connect",
                connected.snapshot.server_name
            ),
            diff,
        )
    };

    let mut close = move || {
        open.set(false);
        history_view.set(None);
        copied.set(None);
    };

    let counts = diff.counts();

    let copy_markdown = {
        let diff = diff.clone();
        let subtitle = subtitle.clone();
        move |_| copy(app, copied, "md", schemadiff::to_markdown(&diff, &subtitle))
    };
    let copy_json = {
        let diff = diff.clone();
        move |_| {
            copy(
                app,
                copied,
                "json",
                serde_json::to_string_pretty(&diff).unwrap_or_default(),
            )
        }
    };

    rsx! {
        div {
            class: "overlay-in fixed inset-0 z-50 flex items-start justify-center bg-black/60 backdrop-blur-[2px] p-8",
            // Focused on open so Esc lands here; keydowns from anything inside
            // bubble up to the same handler.
            tabindex: "0",
            autofocus: true,
            onkeydown: move |evt| {
                if evt.key() == Key::Escape {
                    close();
                }
            },
            onclick: move |_| close(),

            div {
                class: "dialog-in w-full max-w-3xl max-h-full flex flex-col rounded-box border border-base-300 bg-base-100 shadow-2xl",
                onclick: move |evt| evt.stop_propagation(),

                header { class: "flex items-center gap-3 px-5 py-3.5 border-b border-base-300",
                    Icon {
                        icon: LdGitCompareArrows,
                        width: 16,
                        height: 16,
                        class: "shrink-0 text-base-content/45",
                    }
                    div { class: "flex-1 min-w-0",
                        h2 { class: "font-semibold leading-tight", "Contract changes" }
                        p { class: "text-xs text-base-content/50 font-mono truncate",
                            "{subtitle}"
                        }
                    }
                    div { class: "flex items-center gap-2 text-xs",
                        if counts.breaking > 0 {
                            span { class: "badge badge-xs badge-error", "{counts.breaking} breaking" }
                        }
                        if counts.compatible > 0 {
                            span { class: "badge badge-xs badge-soft badge-info",
                                "{counts.compatible} compatible"
                            }
                        }
                        if counts.cosmetic > 0 {
                            span { class: "badge badge-xs badge-ghost", "{counts.cosmetic} cosmetic" }
                        }
                    }
                    button {
                        class: "btn btn-ghost btn-xs btn-square text-base-content/50 hover:text-base-content",
                        title: "Close",
                        onclick: move |_| close(),
                        Icon { icon: LdX, width: 14, height: 14 }
                    }
                }

                div { class: "flex-1 scroll-pane px-5 py-4 space-y-5 min-h-0",
                    Group { title: "Tools", changes: diff.tools.clone() }
                    Group { title: "Resources", changes: diff.resources.clone() }
                    Group { title: "Prompts", changes: diff.prompts.clone() }

                    if !diff.capabilities.is_empty() {
                        section { class: "space-y-2",
                            h3 { class: "section-label", "Capabilities" }
                            div { class: "rounded-field border border-base-300 divide-y divide-base-300/60",
                                for (index, change) in diff.capabilities.iter().enumerate() {
                                    Field { key: "{index}", change: change.clone() }
                                }
                            }
                        }
                    }

                    VersionNotes { diff: diff.clone() }
                }

                footer { class: "flex items-center gap-2 px-5 py-3 border-t border-base-300",
                    // The exported artifact is the diff leaving the app — a
                    // classified change pasted into a PR or piped into a tool.
                    button {
                        class: "btn btn-ghost btn-sm",
                        onclick: copy_markdown,
                        if *copied.read() == Some("md") { "Copied ✓" } else { "Copy as Markdown" }
                    }
                    button {
                        class: "btn btn-ghost btn-sm",
                        onclick: copy_json,
                        if *copied.read() == Some("json") { "Copied ✓" } else { "Copy as JSON" }
                    }
                    button { class: "btn btn-sm ml-auto", onclick: move |_| close(), "Close" }
                }
            }
        }
    }
}

#[component]
fn Group(title: String, changes: Vec<ItemChange>) -> Element {
    if changes.is_empty() {
        return rsx! {};
    }
    let count = changes.len();
    rsx! {
        section { class: "space-y-2",
            div { class: "flex items-baseline gap-2",
                h3 { class: "section-label", "{title}" }
                span { class: "font-mono text-[10px] text-base-content/35", "{count}" }
            }
            for change in changes {
                Item { key: "{change.name()}", change }
            }
        }
    }
}

#[component]
fn Item(change: ItemChange) -> Element {
    let severity = change.severity();
    let (verb, fields) = match &change {
        ItemChange::Added { .. } => ("added", Vec::new()),
        ItemChange::Removed { .. } => ("removed", Vec::new()),
        ItemChange::Modified { fields, .. } => ("changed", fields.clone()),
    };

    rsx! {
        div { class: "rounded-field border border-base-300 {severity_edge(severity)} overflow-hidden",
            div { class: "flex items-center gap-2 px-3 py-1.5 bg-base-200/50",
                span { class: "font-mono text-[13px] truncate", "{change.name()}" }
                span { class: "text-xs text-base-content/45", "{verb}" }
                span { class: "ml-auto {severity_badge(severity)}", "{severity_label(severity)}" }
            }
            if !fields.is_empty() {
                div { class: "divide-y divide-base-300/60",
                    for (index, field) in fields.into_iter().enumerate() {
                        Field { key: "{index}", change: field }
                    }
                }
            }
        }
    }
}

/// One classified field change, with the note first and the payload second.
#[component]
fn Field(change: FieldChange) -> Element {
    rsx! {
        div { class: "px-3 py-2 space-y-1.5",
            div { class: "flex items-baseline gap-2",
                code { class: "text-xs font-mono text-base-content/80 break-all selectable",
                    "{change.path}"
                }
                span { class: "ml-auto shrink-0 {severity_badge(change.severity)}",
                    "{severity_label(change.severity)}"
                }
            }
            p { class: "text-xs text-base-content/65", "{change.note}" }

            // Removals and additions have only one side; showing an empty
            // counterpart would read as "changed to nothing".
            if change.kind != ChangeKind::Added && let Some(before) = &change.before {
                Line {
                    marker: "-",
                    tone: "bg-error/10 text-error/90",
                    value: before.clone(),
                }
            }
            if change.kind != ChangeKind::Removed && let Some(after) = &change.after {
                Line {
                    marker: "+",
                    tone: "bg-success/10 text-success/90",
                    value: after.clone(),
                }
            }
        }
    }
}

#[component]
fn Line(marker: String, tone: String, value: Value) -> Element {
    rsx! {
        pre {
            class: "text-xs font-mono leading-snug rounded-sm px-2 py-1 overflow-x-auto selectable {tone}",
            "{marker} {compact(&value)}"
        }
    }
}

/// Version movement is reported but carries no severity: a bumped version
/// number is an observation, not a change to the contract.
#[component]
fn VersionNotes(diff: SnapshotDiff) -> Element {
    if diff.protocol_version.is_none() && diff.server_version.is_none() {
        return rsx! {};
    }
    rsx! {
        section { class: "space-y-1 text-xs text-base-content/50 font-mono",
            if let Some((before, after)) = &diff.protocol_version {
                p { "Protocol version: {before} → {after}" }
            }
            if let Some((before, after)) = &diff.server_version {
                p { "Server version: {before} → {after}" }
            }
        }
    }
}

/// Changes shown inline in the detail pane for the selected item.
#[component]
pub fn ItemChanges() -> Element {
    let app = use_context::<AppState>();
    let Some(connected) = app.active() else {
        return rsx! {};
    };
    let Some(selection) = app.selection.read().clone() else {
        return rsx! {};
    };
    let Some(change) = connected.change_for(&selection) else {
        return rsx! {};
    };

    // The section itself takes the severity tint, so the interruption is
    // visible before a single word is read.
    let tint = match change.severity() {
        Severity::Breaking => "bg-error/5",
        Severity::Compatible => "bg-info/5",
        Severity::Cosmetic => "",
    };

    rsx! {
        section { class: "border-t border-base-300/70 px-4 py-3 space-y-2 {tint}",
            h3 { class: "section-label", "Changed since last connect" }
            Item { change }
        }
    }
}

/// Put text on the system clipboard, flagging which button did it.
fn copy(
    app: AppState,
    mut copied: Signal<Option<&'static str>>,
    which: &'static str,
    text: String,
) {
    match arboard::Clipboard::new().and_then(|mut c| c.set_text(text)) {
        Ok(()) => copied.set(Some(which)),
        Err(e) => {
            let mut notice = app.notice;
            notice.set(Some(crate::state::Notice::error(format!(
                "Could not reach the clipboard: {e}"
            ))));
        }
    }
}

/// Schemas nest deeply; one line each keeps a long list scannable, and the
/// full payload is a click away in the item's raw definition.
fn compact(value: &Value) -> String {
    const LIMIT: usize = 240;
    let text = match value {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    };
    if text.chars().count() <= LIMIT {
        return text;
    }
    let truncated: String = text.chars().take(LIMIT).collect();
    format!("{truncated}…")
}
