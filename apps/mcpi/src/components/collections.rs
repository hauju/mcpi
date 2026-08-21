//! Saved call sequences: the sidebar list, and the runner.
//!
//! A collection is a smoke test — the handful of calls you make every time you
//! touch a server, saved so you can fire them at a new build in one click. The
//! payoff is the pass/fail summary read against a server whose contract just
//! moved.

use dioxus::prelude::*;
use dioxus_free_icons::Icon;
use dioxus_free_icons::icons::ld_icons::{LdChevronRight, LdFlaskConical, LdPlay, LdPlus, LdX};
use mcpstore::{Collection, Step, StepId};
use serde_json::Value;

use crate::state::{AppState, StepOutcome};

/// Sidebar section listing the selected server's collections.
#[component]
pub fn Collections() -> Element {
    let app = use_context::<AppState>();
    let collections = app.collections;

    if app.selected_server.read().is_none() {
        return rsx! {};
    }

    rsx! {
        section { class: "shrink-0 border-t border-base-300/60",
            details { class: "group", open: true,
                summary { class: "flex items-center gap-1.5 px-3 py-2 select-none",
                    Icon {
                        icon: LdChevronRight,
                        width: 11,
                        height: 11,
                        class: "text-base-content/35 transition-transform group-open:rotate-90",
                    }
                    h2 { class: "section-label flex-1", "Collections" }
                    button {
                        class: "btn btn-ghost btn-xs btn-square text-base-content/50 hover:text-base-content",
                        title: "New collection",
                        // The button lives inside the summary, so its click
                        // must not double as a fold/unfold.
                        onclick: move |evt| {
                            evt.prevent_default();
                            evt.stop_propagation();
                            app.create_collection("");
                        },
                        Icon { icon: LdPlus, width: 12, height: 12 }
                    }
                }
                div { class: "scroll-pane max-h-36 pb-1",
                    if collections.read().is_empty() {
                        p { class: "px-3 pb-3 text-xs text-base-content/40",
                            "Save a call from the form to build a smoke test."
                        }
                    }
                    for collection in collections.read().iter() {
                        CollectionRow { key: "{collection.id}", collection: collection.clone() }
                    }
                }
            }
        }
    }
}

#[component]
fn CollectionRow(collection: Collection) -> Element {
    let app = use_context::<AppState>();
    let id = collection.id;
    let selected = *app.open_collection.read() == Some(id);
    let row_class = if selected { "row row-active" } else { "row" };

    rsx! {
        button {
            class: "{row_class}",
            onclick: move |_| app.open_collection(id),
            Icon {
                icon: LdFlaskConical,
                width: 12,
                height: 12,
                class: "shrink-0 text-base-content/35",
            }
            span { class: "truncate flex-1 text-xs", "{collection.name}" }
        }
    }
}

/// The runner: steps on the left of their results, and a summary that answers
/// "did this build break anything I care about" in one line.
#[component]
pub fn CollectionRunner() -> Element {
    let app = use_context::<AppState>();

    let Some(id) = *app.open_collection.read() else {
        return rsx! {};
    };
    let collection = app.collections.read().iter().find(|c| c.id == id).cloned();
    let Some(collection) = collection else {
        return rsx! {};
    };

    let steps = app.steps.read().clone();
    let results = app.run_results.read().clone();
    let running = *app.running_collection.read();
    let connected = app.active().is_some();

    let failed = results.iter().filter(|r| r.is_error).count();
    let done = results.len();

    rsx! {
        div {
            class: "overlay-in fixed inset-0 z-50 flex items-start justify-center bg-black/60 backdrop-blur-[2px] p-8",
            // Focused on open so Esc lands here; keydowns from anything inside
            // bubble up to the same handler.
            tabindex: "0",
            autofocus: true,
            onkeydown: move |evt| {
                if evt.key() == Key::Escape {
                    app.close_collection();
                }
            },
            onclick: move |_| app.close_collection(),

            div {
                class: "dialog-in w-full max-w-2xl max-h-full flex flex-col rounded-box border border-base-300 bg-base-100 shadow-2xl",
                onclick: move |evt| evt.stop_propagation(),

                header { class: "flex items-center gap-3 px-5 py-3 border-b border-base-300",
                    Icon {
                        icon: LdFlaskConical,
                        width: 15,
                        height: 15,
                        class: "shrink-0 text-base-content/40",
                    }
                    input {
                        class: "input input-sm input-ghost font-semibold px-1 flex-1",
                        value: "{collection.name}",
                        onchange: move |e| {
                            app.rename_collection(id, &e.value());
                        },
                    }
                    if done > 0 && !running {
                        // Failure is solid, success is soft: a passing run
                        // should be one calm glance, a failing one should not.
                        span {
                            class: if failed > 0 { "badge badge-sm badge-error" } else { "badge badge-sm badge-soft badge-success" },
                            if failed > 0 { "{failed} of {done} failed" } else { "{done} passed" }
                        }
                    }
                    button {
                        class: "btn btn-sm btn-primary gap-1.5",
                        disabled: running || steps.is_empty() || !connected,
                        title: if connected { "Run every step in order" } else { "Connect to the server first" },
                        onclick: move |_| app.run_collection(id),
                        if running {
                            span { class: "loading loading-spinner loading-xs" }
                            "Running…"
                        } else {
                            Icon { icon: LdPlay, width: 12, height: 12 }
                            "Run"
                        }
                    }
                }

                div { class: "flex-1 scroll-pane px-5 py-4 space-y-2 min-h-0",
                    if steps.is_empty() {
                        div { class: "flex flex-col items-center gap-1.5 py-10 text-center",
                            Icon {
                                icon: LdFlaskConical,
                                width: 20,
                                height: 20,
                                class: "mb-1 text-base-content/25",
                            }
                            p { class: "text-sm text-base-content/70", "No steps yet" }
                            p { class: "max-w-xs text-xs text-base-content/40 leading-relaxed",
                                "Fill in a call in the detail pane and use “Save to collection”."
                            }
                        }
                    }
                    for (index, step) in steps.iter().enumerate() {
                        StepRow {
                            key: "{step.id}",
                            index: index + 1,
                            step: step.clone(),
                            outcome: results.iter().find(|r| r.step_id == step.id).cloned(),
                            // Steps run in order, so anything past the last
                            // result has not been reached yet.
                            pending: running && results.len() <= index,
                        }
                    }
                }

                footer { class: "flex items-center px-5 py-3 border-t border-base-300",
                    button {
                        class: "btn btn-sm btn-ghost text-error",
                        onclick: move |_| app.delete_collection(id),
                        "Delete collection"
                    }
                    button {
                        class: "btn btn-sm ml-auto",
                        onclick: move |_| app.close_collection(),
                        "Close"
                    }
                }
            }
        }
    }
}

#[component]
fn StepRow(index: usize, step: Step, outcome: Option<StepOutcome>, pending: bool) -> Element {
    let app = use_context::<AppState>();
    let id: StepId = step.id;

    rsx! {
        div { class: "group rounded-field border border-base-300 overflow-hidden",
            div { class: "flex items-center gap-2 px-3 py-2",
                span { class: "font-mono text-xs text-base-content/35 w-4 shrink-0 text-right",
                    "{index}"
                }
                span { class: "font-mono text-[13px] truncate flex-1", "{step.target}" }

                if pending {
                    span { class: "loading loading-spinner loading-xs opacity-40" }
                } else if let Some(outcome) = &outcome {
                    span { class: "text-[10px] font-mono text-base-content/35",
                        "{outcome.duration_ms}ms"
                    }
                    span {
                        class: if outcome.is_error { "badge badge-xs badge-error" } else { "badge badge-xs badge-soft badge-success" },
                        if outcome.is_error { "failed" } else { "passed" }
                    }
                }

                button {
                    class: "btn btn-ghost btn-xs btn-square text-base-content/25 group-hover:text-base-content/60 hover:text-base-content",
                    title: "Remove this step",
                    onclick: move |_| app.delete_step(id),
                    Icon { icon: LdX, width: 12, height: 12 }
                }
            }

            // Only failures expand by default: a passing run should be one
            // glance, and a failing one should already show you why.
            if let Some(outcome) = &outcome && outcome.is_error {
                pre {
                    class: "text-xs font-mono leading-snug bg-error/10 border-t border-error/20 px-3 py-2 overflow-x-auto selectable",
                    "{summarise(&outcome.response)}"
                }
            }
        }
    }
}

/// The one line worth showing for a failed step.
///
/// Errors arrive in three shapes depending on where they came from: a transport
/// failure, a JSON-RPC error, or a tool that set `isError` and explained itself
/// in its content blocks.
fn summarise(response: &Value) -> String {
    if let Some(error) = response.get("error").and_then(Value::as_str) {
        return error.to_string();
    }
    if let Some(text) = response
        .get("content")
        .and_then(Value::as_array)
        .and_then(|blocks| blocks.first())
        .and_then(|block| block.get("text"))
        .and_then(Value::as_str)
    {
        return text.to_string();
    }
    serde_json::to_string_pretty(response).unwrap_or_else(|_| response.to_string())
}
