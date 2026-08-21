//! Right pane: the selected item in full.
//!
//! Phase 4 shows the contract. The generated call form and the response pane
//! land here in Phase 5, below the schema.

use dioxus::prelude::*;
use dioxus_free_icons::Icon;
use dioxus_free_icons::icons::ld_icons::{LdChevronRight, LdCircleAlert, LdUnplug, LdX};
use serde_json::Value;

use crate::components::call_form::CallForm;
use crate::components::conformance::Conformance;
use crate::components::diff::ItemChanges;
use crate::components::response::Response;
use crate::components::timeline::SnapshotTimeline;
use crate::state::{AppState, Selection, Tab};

#[component]
pub fn Detail() -> Element {
    let app = use_context::<AppState>();

    let Some(connected) = app.active() else {
        return rsx! {
            Pane {
                EmptyState {
                    icon: rsx! {
                        Icon { icon: LdUnplug, width: 16, height: 16 }
                    },
                    message: "Nothing connected.",
                }
            }
        };
    };

    let Some(selection) = app.selection.read().clone() else {
        return rsx! {
            Pane { ServerSummary {} }
        };
    };

    let item = match selection.tab {
        Tab::Tools => connected.snapshot.tools.get(&selection.name),
        Tab::Resources => connected.snapshot.resources.get(&selection.name),
        Tab::Prompts => connected.snapshot.prompts.get(&selection.name),
    };

    let Some(item) = item.cloned() else {
        // The selection can outlive a reconnect that dropped the item.
        return rsx! {
            Pane {
                EmptyState {
                    icon: rsx! {
                        Icon { icon: LdCircleAlert, width: 16, height: 16 }
                    },
                    message: "That item is no longer advertised.",
                }
            }
        };
    };

    rsx! {
        Pane {
            ItemDetail { selection: selection.clone(), item }
        }
    }
}

#[component]
fn Pane(children: Element) -> Element {
    rsx! {
        // A shade above the browser: the pane holding the thing being worked
        // on reads as the raised surface. The divider is the browser's
        // border-r, so none is drawn here.
        aside { class: "w-96 shrink-0 flex flex-col bg-base-200/40",
            {children}
        }
    }
}

#[component]
fn ItemDetail(selection: Selection, item: Value) -> Element {
    let app = use_context::<AppState>();
    let description = item.get("description").and_then(Value::as_str);
    let title = item.get("title").and_then(Value::as_str);

    rsx! {
        header { class: "px-4 py-3 border-b border-base-300/70 space-y-1",
            div { class: "flex items-start gap-2",
                h2 { class: "flex-1 font-mono text-[13px] font-semibold break-all selectable leading-snug",
                    "{selection.name}"
                }
                // The only way back to the server summary — and with it the
                // snapshot timeline and baseline pinning — so it must exist.
                button {
                    class: "btn btn-ghost btn-xs btn-square shrink-0 text-base-content/50 hover:text-base-content",
                    title: "Back to the server overview",
                    onclick: move |_| app.clear_selection(),
                    Icon { icon: LdX, width: 14, height: 14 }
                }
            }
            if let Some(title) = title {
                p { class: "text-xs text-base-content/55", "{title}" }
            }
            if let Some(description) = description {
                p { class: "text-sm text-base-content/70 whitespace-pre-wrap selectable leading-relaxed",
                    "{description}"
                }
            }
        }

        div { class: "flex-1 scroll-pane min-h-0",
            // What moved comes first when something did: seeing a tool is
            // broken changes whether you want to call it at all.
            ItemChanges {}

            // Then how the tool's own declaration reads against the spec: a
            // violated MUST gets it dropped by a conforming client, which is
            // worth knowing before rather than after you try to call it.
            if selection.tab == Tab::Tools {
                Conformance { tool: Some(selection.name.clone()) }
            }

            // Otherwise the form leads, because inspecting an item is nearly
            // always a prelude to calling it.
            CallForm { selection: selection.clone() }
            Response {}

            div { class: "p-4 space-y-4 border-t border-base-300/70",
                match selection.tab {
                    Tab::Tools => rsx! {
                        if let Some(schema) = item.get("outputSchema") {
                            JsonBlock { label: "Output schema", value: schema.clone() }
                        }
                    },
                    Tab::Resources => rsx! {
                        KeyValues {
                            item: item.clone(),
                            keys: vec!["uri".into(), "mimeType".into(), "size".into()],
                        }
                    },
                    Tab::Prompts => rsx! {},
                }

                details { class: "group",
                    summary { class: "flex items-center gap-1 cursor-default select-none",
                        Icon {
                            icon: LdChevronRight,
                            width: 11,
                            height: 11,
                            class: "text-base-content/35 transition-transform group-open:rotate-90",
                        }
                        span { class: "section-label hover:text-base-content/70", "Raw definition" }
                    }
                    div { class: "mt-2",
                        JsonBlock { label: "", value: item.clone() }
                    }
                }
            }
        }
    }
}

/// Shown when a server is connected but no item is selected: the handshake is
/// itself information worth reading.
#[component]
fn ServerSummary() -> Element {
    let app = use_context::<AppState>();
    let Some(connected) = app.active() else {
        return rsx! {};
    };
    let snapshot = &connected.snapshot;

    rsx! {
        header { class: "px-4 py-3 border-b border-base-300/70 space-y-0.5",
            h2 { class: "text-sm font-semibold selectable", "{snapshot.server_name}" }
            p { class: "text-xs text-base-content/45 font-mono",
                "v{snapshot.server_version} · MCP {snapshot.protocol_version}"
            }
        }
        div { class: "flex-1 scroll-pane p-4 space-y-4",
            if let Some(instructions) = &connected.instructions {
                section { class: "space-y-1.5",
                    h3 { class: "section-label", "Instructions" }
                    p { class: "text-sm text-base-content/75 whitespace-pre-wrap selectable leading-relaxed",
                        "{instructions}"
                    }
                }
            }
            // Facts about the contract as a whole, not any one tool.
            Conformance { tool: None, bare: true }
            JsonBlock { label: "Capabilities", value: snapshot.capabilities.clone() }
            // Pinning the snapshot this connect just recorded happens here —
            // "certify what I'm looking at" must not require disconnecting.
            SnapshotTimeline {}
            p { class: "text-xs text-base-content/35", "Select an item to inspect it." }
        }
    }
}

#[component]
fn JsonBlock(label: String, value: Value) -> Element {
    let text = serde_json::to_string_pretty(&value).unwrap_or_else(|_| value.to_string());
    rsx! {
        section { class: "space-y-1.5",
            if !label.is_empty() {
                h3 { class: "section-label", "{label}" }
            }
            pre { class: "code-block", "{text}" }
        }
    }
}

#[component]
fn KeyValues(item: Value, keys: Vec<String>) -> Element {
    rsx! {
        dl { class: "space-y-1.5 text-sm",
            for key in keys {
                if let Some(value) = item.get(&key).filter(|v| !v.is_null()) {
                    div { key: "{key}", class: "flex gap-3",
                        dt { class: "w-24 shrink-0 section-label pt-0.5", "{key}" }
                        dd { class: "font-mono text-xs break-all selectable",
                            "{value.as_str().map(str::to_string).unwrap_or_else(|| value.to_string())}"
                        }
                    }
                }
            }
        }
    }
}

#[component]
fn EmptyState(icon: Element, message: String) -> Element {
    rsx! {
        div { class: "flex-1 flex flex-col items-center justify-center gap-1.5 p-6 text-center",
            div { class: "mb-1 flex size-9 items-center justify-center rounded-full border border-base-300/70 bg-base-200/40 text-base-content/40",
                {icon}
            }
            p { class: "text-sm text-base-content/50", "{message}" }
        }
    }
}
