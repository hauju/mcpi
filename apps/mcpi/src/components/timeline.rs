//! The recorded contract history for the selected server.
//!
//! Snapshot rows are only written when the contract moved, so every
//! consecutive pair here is a real change event — the timeline is the record
//! of "what happened to this server over time", which is the thing the live
//! diff banner cannot show once the connection is gone.
//!
//! Any recorded moment can be pinned under a name ("v1.2"), and any two
//! moments can be diffed against each other — including a baseline older than
//! the timeline window, which is the whole point of naming one.

use dioxus::prelude::*;
use dioxus_free_icons::Icon;
use dioxus_free_icons::icons::ld_icons::{LdChevronRight, LdPin};
use mcpstore::{SnapshotId, StoredSnapshot};
use schemadiff::SnapshotDiff;

use crate::state::AppState;

#[component]
pub fn SnapshotTimeline() -> Element {
    let app = use_context::<AppState>();

    // One diff per consecutive pair, newest first. Recomputed only when the
    // snapshot list changes; `schemadiff::diff` is pure, and the list is short
    // by construction since rows only exist where the contract moved.
    let diffs = use_memo(move || {
        let snapshots = app.snapshots.read();
        snapshots
            .windows(2)
            .map(|pair| schemadiff::diff(&pair[1].snapshot, &pair[0].snapshot))
            .collect::<Vec<SnapshotDiff>>()
    });

    let snapshots = app.snapshots.read();
    if snapshots.is_empty() {
        return rsx! {};
    }
    let oldest = snapshots.last().cloned();

    rsx! {
        section { class: "w-full max-w-sm mt-5 text-left space-y-1.5",
            h3 { class: "section-label", "Contract history" }
            div { class: "rounded-field border border-base-300 divide-y divide-base-300/60",
                for (index, diff) in diffs.read().iter().enumerate() {
                    ChangeRow {
                        key: "{snapshots[index].id}",
                        index,
                        snapshot_id: snapshots[index].id,
                        label: snapshots[index].label.clone(),
                        when: snapshots[index]
                            .taken_at
                            .with_timezone(&chrono::Local)
                            .format("%-d %b %Y %H:%M")
                            .to_string(),
                        total: diff.counts().total(),
                        breaking: diff.counts().breaking,
                    }
                }
                // The oldest snapshot heads no change pair, but it is still a
                // moment — and the first thing worth pinning ("this is what I
                // certified").
                if let Some(first) = oldest {
                    div { class: "group flex items-center",
                        span { class: "flex-1 px-3 py-2 text-xs text-base-content/40",
                            "First recorded {first.taken_at.with_timezone(&chrono::Local).format(\"%-d %b %Y\")}"
                        }
                        BaselinePin { snapshot_id: first.id, label: first.label.clone() }
                    }
                }
            }
            ComparePicker {}
        }
    }
}

/// One recorded change. Clicking opens the drawer on the pair's diff.
#[component]
fn ChangeRow(
    index: usize,
    snapshot_id: SnapshotId,
    label: Option<String>,
    when: String,
    total: usize,
    breaking: usize,
) -> Element {
    let app = use_context::<AppState>();

    let open = move |_| {
        let snapshots = app.snapshots.read();
        let (Some(after), Some(before)) = (snapshots.get(index), snapshots.get(index + 1)) else {
            return;
        };
        app.open_history_diff(before, after);
    };

    rsx! {
        div { class: "group flex items-center hover:bg-base-200/60 transition-colors",
            button {
                class: "flex-1 min-w-0 flex items-center gap-2 px-3 py-2 text-xs text-left",
                onclick: open,
                span { class: "text-base-content/60 shrink-0", "{when}" }
                span { class: "font-medium",
                    "{total} change"
                    {if total == 1 { "" } else { "s" }}
                }
                if breaking > 0 {
                    span { class: "badge badge-xs badge-error", "{breaking} breaking" }
                }
            }
            BaselinePin { snapshot_id, label }
            span { class: "flex items-center pr-3 text-base-content/40 group-hover:text-base-content transition-colors",
                Icon { icon: LdChevronRight, width: 12, height: 12 }
            }
        }
    }
}

/// A snapshot's pinned name, or the button to give it one.
///
/// Editing is a small inline input: Enter saves, Escape cancels, and clearing
/// the text unpins. No modal — naming a moment is a two-second action.
#[component]
fn BaselinePin(snapshot_id: SnapshotId, label: Option<String>) -> Element {
    let app = use_context::<AppState>();
    let mut editing = use_signal(|| false);
    let mut text = use_signal(String::new);

    if *editing.read() {
        return rsx! {
            input {
                class: "input input-xs w-24 font-mono mx-1 shrink-0",
                value: "{text}",
                autofocus: true,
                placeholder: "v1.2",
                title: "Enter saves · empty unpins · Esc cancels",
                oninput: move |evt| text.set(evt.value()),
                onkeydown: move |evt| match evt.key() {
                    Key::Enter => {
                        let name = text.read().trim().to_string();
                        app.set_baseline(
                            snapshot_id,
                            (!name.is_empty()).then_some(name.as_str()),
                        );
                        editing.set(false);
                    }
                    Key::Escape => editing.set(false),
                    _ => {}
                },
            }
        };
    }

    match label {
        Some(name) => {
            let seed = name.clone();
            rsx! {
                button {
                    class: "badge badge-xs badge-neutral badge-soft font-mono shrink-0 cursor-pointer",
                    title: "Rename or unpin this baseline",
                    onclick: move |_| {
                        text.set(seed.clone());
                        editing.set(true);
                    },
                    "{name}"
                }
            }
        }
        None => rsx! {
            button {
                class: "opacity-0 group-hover:opacity-100 btn btn-ghost btn-xs btn-square text-base-content/40 hover:text-base-content shrink-0",
                title: "Pin as a named baseline",
                onclick: move |_| {
                    text.set(String::new());
                    editing.set(true);
                },
                Icon { icon: LdPin, width: 12, height: 12 }
            }
        },
    }
}

/// Diff any two recorded moments, not just consecutive ones.
///
/// The choices include pinned snapshots the timeline window has moved past —
/// "current against the v1.2 we certified in April" is the question named
/// baselines exist to answer.
#[component]
fn ComparePicker() -> Element {
    let app = use_context::<AppState>();

    let choices = use_memo(move || {
        let mut all = app.snapshots.read().clone();
        for pinned in app.baselines.read().iter() {
            if all.iter().all(|s| s.id != pinned.id) {
                all.push(pinned.clone());
            }
        }
        all.sort_by_key(|s| std::cmp::Reverse(s.id));
        all
    });

    let mut from = use_signal(|| None::<SnapshotId>);
    let mut to = use_signal(|| None::<SnapshotId>);

    let list = choices.read();
    if list.len() < 2 {
        return rsx! {};
    }
    // Defaults are the two most recent moments; a selection that no longer
    // exists (the window moved, a pin was cleared) falls back the same way.
    let resolve = |picked: Option<SnapshotId>, fallback: SnapshotId| {
        picked
            .filter(|id| list.iter().any(|s| s.id == *id))
            .unwrap_or(fallback)
    };
    let from_id = resolve(*from.read(), list[1].id);
    let to_id = resolve(*to.read(), list[0].id);

    let compare = move |_| {
        let list = choices.read();
        let (Some(a), Some(b)) = (
            list.iter().find(|s| s.id == from_id),
            list.iter().find(|s| s.id == to_id),
        ) else {
            return;
        };
        // Time flows one way: the older snapshot is always "before", so the
        // classification reads forward whatever the pick order.
        let (before, after) = if a.id <= b.id { (a, b) } else { (b, a) };
        app.open_history_diff(before, after);
    };

    rsx! {
        div { class: "flex items-center gap-1.5 text-xs",
            select {
                class: "select select-xs flex-1 min-w-0",
                title: "Older snapshot",
                onchange: move |evt| from.set(evt.value().parse().ok()),
                for s in list.iter() {
                    option { key: "{s.id}", value: "{s.id}", selected: s.id == from_id, "{stamp(s)}" }
                }
            }
            span { class: "text-base-content/40 shrink-0", "→" }
            select {
                class: "select select-xs flex-1 min-w-0",
                title: "Newer snapshot",
                onchange: move |evt| to.set(evt.value().parse().ok()),
                for s in list.iter() {
                    option { key: "{s.id}", value: "{s.id}", selected: s.id == to_id, "{stamp(s)}" }
                }
            }
            button {
                class: "btn btn-xs shrink-0",
                disabled: from_id == to_id,
                onclick: compare,
                "Compare"
            }
        }
    }
}

fn stamp(s: &StoredSnapshot) -> String {
    let when = s
        .taken_at
        .with_timezone(&chrono::Local)
        .format("%-d %b %Y %H:%M");
    match &s.label {
        Some(label) => format!("{when} · {label}"),
        None => when.to_string(),
    }
}
