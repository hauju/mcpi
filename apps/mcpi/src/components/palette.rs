//! ⌘K: jump to any server or advertised item without touching the mouse.

use dioxus::prelude::*;

use crate::components::sidebar::StatusDot;
use crate::state::{AppState, Selection, Status, Tab};

/// One row the palette can execute.
#[derive(Clone, PartialEq)]
enum Entry {
    Server {
        id: mcpstore::ServerId,
        status: Status,
    },
    Item {
        tab: Tab,
        name: String,
    },
}

#[component]
pub fn Palette() -> Element {
    let app = use_context::<AppState>();
    let mut open = app.palette_open;
    let mut query = use_signal(String::new);
    let mut active = use_signal(|| 0usize);

    if !*open.read() {
        return rsx! {};
    }

    // (score, label, entry) — servers first on an empty query, then the
    // connected server's items in tab order. Sorting is stable, so ties keep
    // that order.
    let needle = query.read().clone();
    let mut entries: Vec<(i32, String, Entry)> = Vec::new();
    for server in app.servers.read().iter() {
        if let Some(score) = score(&needle, &server.name) {
            entries.push((
                score,
                server.name.clone(),
                Entry::Server {
                    id: server.id,
                    status: app.conn(server.id).status(),
                },
            ));
        }
    }
    if let Some(connected) = app.active() {
        for (tab, names) in [
            (Tab::Tools, connected.snapshot.tools.keys()),
            (Tab::Resources, connected.snapshot.resources.keys()),
            (Tab::Prompts, connected.snapshot.prompts.keys()),
        ] {
            for name in names {
                if let Some(score) = score(&needle, name) {
                    entries.push((
                        score,
                        name.clone(),
                        Entry::Item {
                            tab,
                            name: name.clone(),
                        },
                    ));
                }
            }
        }
    }
    entries.sort_by_key(|(score, _, _)| std::cmp::Reverse(*score));
    entries.truncate(50);

    let row = active().min(entries.len().saturating_sub(1));
    let execute = {
        let entries = entries.clone();
        move |index: usize| {
            let Some((_, _, entry)) = entries.get(index) else {
                return;
            };
            match entry {
                Entry::Server { id, status } => {
                    app.select_server(*id);
                    // A palette jump means "take me there": a cold server gets
                    // dialled rather than landing on a "Connect" button.
                    if !matches!(status, Status::Connected | Status::Connecting) {
                        app.connect(*id);
                    }
                }
                Entry::Item { tab, name } => {
                    let mut tab_signal = app.tab;
                    tab_signal.set(*tab);
                    app.select_item(Selection {
                        tab: *tab,
                        name: name.clone(),
                    });
                }
            }
            open.set(false);
            query.set(String::new());
            active.set(0);
        }
    };

    let count = entries.len();
    let mut execute_on_enter = execute.clone();

    rsx! {
        div {
            class: "overlay-in fixed inset-0 z-[60] flex items-start justify-center bg-black/60 backdrop-blur-[2px] pt-[14vh] px-6",
            onclick: move |_| {
                open.set(false);
                query.set(String::new());
                active.set(0);
            },
            div {
                class: "dialog-in w-full max-w-lg flex flex-col max-h-[50vh] rounded-box border border-base-300 bg-base-100 shadow-2xl overflow-hidden",
                onclick: move |evt| evt.stop_propagation(),

                input {
                    class: "w-full px-4 py-3 text-sm bg-transparent outline-none border-b border-base-300/70 placeholder:text-base-content/35",
                    r#type: "text",
                    autofocus: true,
                    placeholder: "Jump to a tool, resource, prompt, or server…",
                    value: "{query}",
                    oninput: move |e| {
                        query.set(e.value());
                        active.set(0);
                    },
                    onkeydown: move |e| {
                        match e.key() {
                            Key::Escape => {
                                open.set(false);
                                query.set(String::new());
                                active.set(0);
                            }
                            Key::ArrowDown => {
                                e.prevent_default();
                                if count > 0 {
                                    active.set((row + 1) % count);
                                    reveal_active();
                                }
                            }
                            Key::ArrowUp => {
                                e.prevent_default();
                                if count > 0 {
                                    active.set((row + count - 1) % count);
                                    reveal_active();
                                }
                            }
                            Key::Enter => execute_on_enter(row),
                            _ => {}
                        }
                    },
                }

                div { class: "flex-1 scroll-pane py-1",
                    if entries.is_empty() {
                        p { class: "px-4 py-6 text-sm text-base-content/40 text-center", "Nothing matches." }
                    }
                    for (index, (_, label, entry)) in entries.iter().enumerate() {
                        PaletteRow {
                            key: "{kind_label(entry)}-{label}",
                            label: label.clone(),
                            entry: entry.clone(),
                            active: index == row,
                            index,
                            on_hover: move |i| active.set(i),
                            on_pick: execute.clone(),
                        }
                    }
                }

                footer { class: "flex items-center gap-3 px-4 py-1.5 border-t border-base-300/70 text-[10px] text-base-content/35",
                    span { "↑↓ navigate" }
                    span { "↵ open" }
                    span { "esc close" }
                }
            }
        }
    }
}

#[component]
fn PaletteRow(
    label: String,
    entry: Entry,
    active: bool,
    index: usize,
    on_hover: EventHandler<usize>,
    on_pick: Callback<usize>,
) -> Element {
    let row_class = if active { "row row-active" } else { "row" };

    rsx! {
        button {
            class: "{row_class}",
            "data-palette-active": if active { "true" },
            onmouseenter: move |_| on_hover.call(index),
            onclick: move |_| on_pick.call(index),
            if let Entry::Server { status, .. } = &entry {
                StatusDot { status: *status }
            }
            span { class: "font-mono text-[13px] truncate", "{label}" }
            span { class: "ml-auto shrink-0 text-[10px] uppercase tracking-wide text-base-content/30",
                "{kind_label(&entry)}"
            }
        }
    }
}

fn kind_label(entry: &Entry) -> &'static str {
    match entry {
        Entry::Server { .. } => "server",
        Entry::Item {
            tab: Tab::Tools, ..
        } => "tool",
        Entry::Item {
            tab: Tab::Resources,
            ..
        } => "resource",
        Entry::Item {
            tab: Tab::Prompts, ..
        } => "prompt",
    }
}

/// Keep the active row on screen while arrowing. Deferred a frame so the
/// selector sees the row *after* Dioxus applies the render.
fn reveal_active() {
    let _ = document::eval(
        "requestAnimationFrame(() => \
         document.querySelector('[data-palette-active]')?.scrollIntoView({block: 'nearest'}))",
    );
}

/// Prefix beats substring beats subsequence; shorter names win ties. `None`
/// is a non-match, and an empty query matches everything equally.
fn score(needle: &str, hay: &str) -> Option<i32> {
    if needle.trim().is_empty() {
        return Some(0);
    }
    let hay_lower = hay.to_lowercase();
    let needle = needle.trim().to_lowercase();
    let length_penalty = hay.len().min(100) as i32;
    if hay_lower.starts_with(&needle) {
        return Some(300 - length_penalty);
    }
    if hay_lower.contains(&needle) {
        return Some(200 - length_penalty);
    }
    let mut haystack = hay_lower.chars();
    needle
        .chars()
        .all(|c| haystack.by_ref().any(|h| h == c))
        .then_some(100 - length_penalty)
}
