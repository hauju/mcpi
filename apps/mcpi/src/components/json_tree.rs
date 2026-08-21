//! Collapsible tree for JSON payloads.
//!
//! Responses land as `serde_json::Value`, and most tool results are JSON
//! stringified into a text block. Pretty-printed text wraps mid-token — a UUID
//! breaks at its own hyphens — and offers no way to fold a long array out of
//! the way. The tree does both, and the copy button hands back the original
//! document rather than the folded view.

use dioxus::prelude::*;
use dioxus_free_icons::Icon;
use dioxus_free_icons::icons::ld_icons::{LdCheck, LdChevronRight, LdCopy};
use serde_json::Value;

use crate::state::AppState;

/// Levels open by default: deep enough that a typical list response shows its
/// items, while whatever nests below them folds instead of flooding the pane.
const OPEN_DEPTH: usize = 3;

#[component]
pub fn JsonView(value: Value) -> Element {
    let app = use_context::<AppState>();
    let mut copied = use_signal(|| false);
    let payload = value.clone();

    rsx! {
        div { class: "relative group",
            button {
                class: "btn btn-ghost btn-xs absolute top-1 right-1 z-10 opacity-0 group-hover:opacity-100 transition-opacity",
                onclick: move |_| {
                    let text = serde_json::to_string_pretty(&payload)
                        .unwrap_or_else(|_| payload.to_string());
                    match arboard::Clipboard::new().and_then(|mut c| c.set_text(text)) {
                        Ok(()) => copied.set(true),
                        Err(e) => {
                            let mut notice = app.notice;
                            notice.set(Some(crate::state::Notice::error(format!(
                                "Could not reach the clipboard: {e}"
                            ))));
                        }
                    }
                },
                onmouseleave: move |_| copied.set(false),
                if copied() {
                    Icon { icon: LdCheck, width: 12, height: 12 }
                    "Copied"
                } else {
                    Icon { icon: LdCopy, width: 12, height: 12 }
                    "Copy"
                }
            }
            div { class: "code-block", Node { value, depth: 0 } }
        }
    }
}

#[component]
fn Node(#[props(default)] name: Option<String>, value: Value, depth: usize) -> Element {
    match value {
        Value::Object(entries) if !entries.is_empty() => rsx! {
            details { class: "json-node", open: depth < OPEN_DEPTH,
                summary { class: "flex items-center gap-1 cursor-default select-none",
                    Icon {
                        icon: LdChevronRight,
                        width: 10,
                        height: 10,
                        class: "json-chevron shrink-0 text-base-content/35",
                    }
                    Key { name: name.clone() }
                    span { class: "json-preview text-base-content/35",
                        "{{…}} {entries.len()} {plural(entries.len(), \"key\")}"
                    }
                }
                div { class: "ml-1 border-l border-base-300/60 pl-3",
                    for (key, child) in entries {
                        Node { key: "{key}", name: Some(key.clone()), value: child, depth: depth + 1 }
                    }
                }
            }
        },
        Value::Array(items) if !items.is_empty() => rsx! {
            details { class: "json-node", open: depth < OPEN_DEPTH,
                summary { class: "flex items-center gap-1 cursor-default select-none",
                    Icon {
                        icon: LdChevronRight,
                        width: 10,
                        height: 10,
                        class: "json-chevron shrink-0 text-base-content/35",
                    }
                    Key { name: name.clone() }
                    span { class: "json-preview text-base-content/35",
                        "[…] {items.len()} {plural(items.len(), \"item\")}"
                    }
                }
                div { class: "ml-1 border-l border-base-300/60 pl-3",
                    for (index, child) in items.into_iter().enumerate() {
                        Node { key: "{index}", name: Some(index.to_string()), value: child, depth: depth + 1 }
                    }
                }
            }
        },
        // Scalars, and the empty containers, which have nothing to unfold.
        other => rsx! {
            div { class: "flex items-start gap-1 pl-[14px]",
                Key { name }
                Scalar { value: other }
            }
        },
    }
}

/// The property name, when the node has one — array elements carry their index.
#[component]
fn Key(name: Option<String>) -> Element {
    let Some(name) = name else {
        return rsx! {};
    };
    rsx! {
        span { class: "shrink-0 text-base-content/70", "{name}" }
        span { class: "shrink-0 text-base-content/35", ":" }
    }
}

#[component]
fn Scalar(value: Value) -> Element {
    match value {
        Value::String(s) => {
            // A token without spaces must not wrap — browsers break at hyphens,
            // which is how UUIDs end up split mid-id. Prose wraps at spaces.
            let wrap = if s.contains(' ') {
                "whitespace-pre-wrap"
            } else {
                "whitespace-nowrap"
            };
            rsx! {
                span { class: "text-base-content/85 {wrap}", "\"{s}\"" }
            }
        }
        Value::Number(n) => rsx! {
            span { class: "text-accent/90", "{n}" }
        },
        Value::Bool(b) => rsx! {
            span { class: "text-accent/90", "{b}" }
        },
        Value::Null => rsx! {
            span { class: "italic text-base-content/40", "null" }
        },
        Value::Object(_) => rsx! {
            span { class: "text-base-content/40", "{{}}" }
        },
        Value::Array(_) => rsx! {
            span { class: "text-base-content/40", "[]" }
        },
    }
}

fn plural(n: usize, word: &str) -> String {
    if n == 1 {
        word.to_string()
    } else {
        format!("{word}s")
    }
}
