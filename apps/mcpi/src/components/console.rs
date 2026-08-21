//! The session console: every frame, `stderr` line, and notification, in order.
//!
//! This is the one surface in the app that interprets nothing. Everywhere else
//! leads with a judgement — the diff says "1 breaking", the response pane
//! renders a result — and that is right, because a judgement is what someone
//! squinting at two terminal windows does not have. But interpretation has a
//! floor: when a call fails for a reason the rendered result does not contain,
//! the exchange itself is the only remaining answer, and an inspector that
//! cannot show it sends you back to the terminal it replaced.
//!
//! A drawer rather than a fourth pane. The three panes are the workflow;
//! traffic is what you consult when the workflow produced something you did
//! not expect, and giving it permanent width would tax every session that went
//! fine to help the ones that did not.
//!
//! No colour, deliberately. Red and amber mean "the contract moved" everywhere
//! else in the app, and a `stderr` line is not a contract change — most servers
//! log routine startup chatter there. Direction is carried by an arrow and by
//! the kind label instead.

use dioxus::prelude::*;
use dioxus_free_icons::Icon;
use dioxus_free_icons::icons::ld_icons::{LdEraser, LdTerminal, LdX};

use crate::components::json_tree::JsonView;
use crate::state::{AppState, ConsoleLine, LineKind};

/// The kinds a person filters by, in the order the chips are laid out.
const KINDS: [LineKind; 5] = [
    LineKind::Sent,
    LineKind::Received,
    LineKind::Stderr,
    LineKind::Log,
    LineKind::Notice,
];

#[component]
pub fn Console() -> Element {
    let app = use_context::<AppState>();
    let mut open = app.console_open;

    if !*open.read() {
        return rsx! {};
    }

    // `None` means every kind, which is not the same as "all five selected":
    // it is what the pane opens as, and clicking a chip narrows to that one.
    let mut only = use_signal(|| None::<LineKind>);
    let mut needle = use_signal(String::new);

    // Filtered inside the borrow so a keystroke clones the lines that survive
    // it rather than the whole thousand-entry buffer.
    let filter = needle.read().to_lowercase();
    let kind = *only.read();
    let shown: Vec<(usize, ConsoleLine)> = match *app.selected_server.read() {
        Some(id) => app
            .console
            .read()
            .get(&id)
            .map(|lines| {
                lines
                    .iter()
                    .enumerate()
                    .filter(|(_, line)| kind.is_none_or(|kind| line.kind == kind))
                    .filter(|(_, line)| filter.is_empty() || matches(line, &filter))
                    .map(|(index, line)| (index, line.clone()))
                    .collect()
            })
            .unwrap_or_default(),
        None => Vec::new(),
    };

    rsx! {
        section {
            class: "fixed inset-x-0 bottom-0 z-40 flex h-[42vh] flex-col border-t border-base-300 bg-base-100 shadow-[0_-8px_24px_rgba(0,0,0,0.18)]",

            header { class: "flex items-center gap-2 border-b border-base-300/70 px-3 py-2",
                Icon {
                    icon: LdTerminal,
                    width: 13,
                    height: 13,
                    class: "shrink-0 text-base-content/40",
                }
                h2 { class: "section-label shrink-0", "Console" }

                div { class: "join",
                    button {
                        class: if only.read().is_none() {
                            "btn btn-xs join-item btn-active"
                        } else {
                            "btn btn-xs join-item"
                        },
                        onclick: move |_| only.set(None),
                        "all"
                    }
                    for kind in KINDS {
                        button {
                            key: "{kind:?}",
                            class: if *only.read() == Some(kind) {
                                "btn btn-xs join-item btn-active"
                            } else {
                                "btn btn-xs join-item"
                            },
                            onclick: move |_| {
                                let current = *only.peek();
                                only.set(if current == Some(kind) { None } else { Some(kind) });
                            },
                            "{kind.label()}"
                        }
                    }
                }

                input {
                    r#type: "search",
                    class: "input input-xs w-48",
                    placeholder: "Filter",
                    value: "{needle}",
                    oninput: move |e| needle.set(e.value()),
                }

                span { class: "ml-auto text-[11px] tabular-nums text-base-content/40",
                    "{shown.len()} line(s)"
                }
                button {
                    class: "btn btn-ghost btn-xs btn-square text-base-content/40 hover:text-base-content",
                    title: "Clear",
                    onclick: move |_| app.clear_console(),
                    Icon { icon: LdEraser, width: 13, height: 13 }
                }
                button {
                    class: "btn btn-ghost btn-xs btn-square text-base-content/50 hover:text-base-content",
                    title: "Close",
                    onclick: move |_| open.set(false),
                    Icon { icon: LdX, width: 14, height: 14 }
                }
            }

            div { class: "flex-1 min-h-0 scroll-pane font-mono text-[11px]",
                if shown.is_empty() {
                    p { class: "px-3 py-6 text-center text-xs font-sans text-base-content/40",
                        "Nothing here yet. Connect to a server and its traffic shows up as it happens."
                    }
                }
                for (index , line) in shown {
                    Line { key: "{index}", line }
                }
            }
        }
    }
}

/// Both halves of a line are searched: a method name is what you remember, but
/// the id or the argument you are hunting for is in the body.
fn matches(line: &ConsoleLine, needle: &str) -> bool {
    if line.summary.to_lowercase().contains(needle) {
        return true;
    }
    line.detail
        .as_ref()
        .is_some_and(|detail| detail.to_string().to_lowercase().contains(needle))
}

#[component]
fn Line(line: ConsoleLine) -> Element {
    let mut expanded = use_signal(|| false);
    let expandable = line.detail.is_some();

    // Only direction gets a glyph. A frame's kind is already in its label, and
    // two symbols per line makes a wall of punctuation to read past.
    let arrow = match line.kind {
        LineKind::Sent => "→",
        LineKind::Received => "←",
        _ => " ",
    };

    rsx! {
        div { class: "border-b border-base-300/40 last:border-0",
            div {
                class: if expandable {
                    "flex cursor-pointer items-baseline gap-2 px-3 py-1 hover:bg-base-200/60"
                } else {
                    "flex items-baseline gap-2 px-3 py-1"
                },
                onclick: move |_| {
                    if expandable {
                        let now = *expanded.peek();
                        expanded.set(!now);
                    }
                },
                span { class: "w-14 shrink-0 text-right text-base-content/35", "{line.kind.label()}" }
                span { class: "w-2 shrink-0 text-base-content/45", "{arrow}" }
                span { class: "min-w-0 flex-1 break-all selectable", "{line.summary}" }
            }
            if expandable && *expanded.read() {
                div { class: "px-3 pb-2 pl-[4.75rem]",
                    JsonView { value: line.detail.clone().unwrap_or_default() }
                }
            }
        }
    }
}
