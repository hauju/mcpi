//! Sidebar section: recent calls against the selected server.
//!
//! Clicking a row loads it back into the form without running it — that is the
//! edit-and-replay path, and it is the more common one. Replay runs it as-is.

use dioxus::prelude::*;
use dioxus_free_icons::Icon;
use dioxus_free_icons::icons::ld_icons::{LdChevronRight, LdRotateCcw};
use mcpstore::CallRow;

use crate::state::AppState;

#[component]
pub fn History() -> Element {
    let app = use_context::<AppState>();
    let history = app.history;

    if app.selected_server.read().is_none() {
        return rsx! {};
    }

    rsx! {
        section { class: "shrink-0 border-t border-base-300/60",
            // A native disclosure, open by default: the section can be folded
            // away to give a crowded library its space back, with no state of
            // our own to manage. Dioxus never rewrites the `open` attribute, so
            // the user's toggle sticks.
            details { class: "group", open: true,
                summary { class: "flex items-center gap-1.5 px-3 py-2 select-none",
                    Icon {
                        icon: LdChevronRight,
                        width: 11,
                        height: 11,
                        class: "text-base-content/35 transition-transform group-open:rotate-90",
                    }
                    h2 { class: "section-label flex-1", "History" }
                    span { class: "font-mono text-[10px] text-base-content/35",
                        "{history.read().len()}"
                    }
                }
                div { class: "scroll-pane max-h-56 pb-1",
                    if history.read().is_empty() {
                        p { class: "px-3 pb-3 text-xs text-base-content/40",
                            "Calls you make show up here."
                        }
                    }
                    for call in history.read().iter() {
                        HistoryRow { key: "{call.id}", call: call.clone() }
                    }
                }
            }
        }
    }
}

#[component]
fn HistoryRow(call: CallRow) -> Element {
    let app = use_context::<AppState>();

    rsx! {
        div { class: "group/row flex items-center",
            button {
                class: "row flex-1 min-w-0 block",
                title: "Load these arguments back into the form",
                onclick: {
                    let call = call.clone();
                    move |_| app.load_from_history(&call)
                },
                div { class: "flex items-center gap-1.5",
                    span {
                        class: if call.is_error { "size-1.5 rounded-full bg-error shrink-0" } else { "size-1.5 rounded-full bg-success/60 shrink-0" },
                    }
                    span { class: "font-mono text-xs truncate flex-1", "{call.target}" }
                    span { class: "text-[10px] font-mono text-base-content/35 shrink-0",
                        "{call.duration_ms}ms"
                    }
                }
            }
            button {
                class: "btn btn-ghost btn-xs btn-square mr-1 text-base-content/25 group-hover/row:text-base-content/60 hover:text-base-content",
                title: "Run it again",
                onclick: {
                    let call = call.clone();
                    move |_| app.replay(&call)
                },
                Icon { icon: LdRotateCcw, width: 12, height: 12 }
            }
        }
    }
}
