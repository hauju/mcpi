//! Importing servers from a config file another client already uses.
//!
//! Two ways in, because two situations. A file this machine has is offered by
//! name — one click, no path to remember. Anything else (a coworker's config
//! pasted into chat, a registry entry copied off a web page) goes in the
//! textarea, which is the same reasoning the add/edit dialog uses for its
//! repeatable fields: what people have in hand is JSON text.
//!
//! Nothing imports without being seen first. The parsed list is shown with
//! caveats attached before anything is written, because a config file is
//! exactly the kind of input that contains entries this app cannot dial.

use dioxus::prelude::*;
use dioxus_free_icons::Icon;
use dioxus_free_icons::icons::ld_icons::{LdFileJson, LdTriangleAlert, LdX};
use mcpstore::TransportKind;

use crate::import::{self, Imported};
use crate::state::{AppState, Notice};

#[component]
pub fn ImportDialog() -> Element {
    let app = use_context::<AppState>();
    let mut open = app.import_open;

    if !*open.read() {
        return rsx! {};
    }

    let mut text = use_signal(String::new);
    let mut found = use_signal(Vec::<(Imported, bool)>::new);
    let mut error = use_signal(|| None::<String>);
    // Resolved once per open: the dialog is modal, so a file appearing on disk
    // while it is up cannot be acted on anyway.
    let sources = use_hook(import::sources);

    let mut load = move |source: String, parsed: Result<Vec<Imported>, String>| {
        text.set(source);
        match parsed {
            Ok(servers) => {
                found.set(servers.into_iter().map(|s| (s, true)).collect());
                error.set(None);
            }
            Err(message) => {
                found.set(Vec::new());
                error.set(Some(message));
            }
        }
    };

    let selected = found.read().iter().filter(|(_, on)| *on).count();

    rsx! {
        div {
            class: "overlay-in fixed inset-0 z-50 flex items-center justify-center bg-black/60 backdrop-blur-[2px] p-6",
            tabindex: "0",
            onkeydown: move |evt| {
                if evt.key() == Key::Escape {
                    open.set(false);
                }
            },
            onclick: move |_| open.set(false),

            div {
                class: "dialog-in w-full max-w-xl rounded-box border border-base-300 bg-base-100 shadow-2xl",
                onclick: move |evt| evt.stop_propagation(),

                header { class: "flex items-center px-5 py-3 border-b border-base-300",
                    h2 { class: "font-semibold flex-1", "Import servers" }
                    button {
                        class: "btn btn-ghost btn-xs btn-square text-base-content/50 hover:text-base-content",
                        title: "Close",
                        onclick: move |_| open.set(false),
                        Icon { icon: LdX, width: 14, height: 14 }
                    }
                }

                div { class: "px-5 py-4 space-y-4 max-h-[60vh] scroll-pane",
                    if sources.is_empty() {
                        p { class: "text-xs text-base-content/50",
                            "No config file from another MCP client was found on this machine. Paste one below."
                        }
                    } else {
                        div { class: "space-y-1.5",
                            span { class: "section-label text-base-content/60", "Found on this machine" }
                            for source in sources.iter().cloned() {
                                button {
                                    key: "{source.path.display()}",
                                    class: "row w-full text-left",
                                    onclick: move |_| {
                                        let parsed = import::read(&source.path);
                                        let raw = std::fs::read_to_string(&source.path)
                                            .unwrap_or_default();
                                        load(raw, parsed);
                                    },
                                    Icon {
                                        icon: LdFileJson,
                                        width: 13,
                                        height: 13,
                                        class: "shrink-0 text-base-content/40",
                                    }
                                    span { class: "shrink-0", "{source.label}" }
                                    span { class: "flex-1 truncate text-[11px] font-mono text-base-content/35",
                                        "{source.path.display()}"
                                    }
                                }
                            }
                        }
                    }

                    label { class: "block space-y-1",
                        div { class: "flex items-baseline gap-2",
                            span { class: "section-label text-base-content/60", "Or paste a config" }
                            span { class: "text-[10px] text-base-content/40",
                                "mcpServers / servers block, or a registry server.json"
                            }
                        }
                        textarea {
                            class: "textarea textarea-sm w-full font-mono",
                            rows: 5,
                            placeholder: "Paste the JSON here",
                            value: "{text}",
                            oninput: move |e| {
                                let raw = e.value();
                                if raw.trim().is_empty() {
                                    text.set(raw);
                                    found.set(Vec::new());
                                    error.set(None);
                                } else {
                                    let parsed = import::parse(&raw);
                                    load(raw, parsed);
                                }
                            },
                        }
                    }

                    if let Some(message) = error.read().clone() {
                        p { class: "text-sm text-error", "{message}" }
                    }

                    if !found.read().is_empty() {
                        div { class: "space-y-1",
                            span { class: "section-label text-base-content/60",
                                "{found.read().len()} found"
                            }
                            for (index , (server , picked)) in found.read().iter().enumerate() {
                                Row {
                                    key: "{index}-{server.name}",
                                    index,
                                    server: server.clone(),
                                    picked: *picked,
                                    toggle: move |i: usize| {
                                        if let Some((_, on)) = found.write().get_mut(i) {
                                            *on = !*on;
                                        }
                                    },
                                }
                            }
                        }
                    }
                }

                footer { class: "flex items-center gap-2 px-5 py-3 border-t border-base-300",
                    p { class: "text-[11px] text-base-content/40 flex-1",
                        "Imported servers are copies. Editing one here does not change the original file."
                    }
                    button {
                        class: "btn btn-sm btn-ghost",
                        onclick: move |_| open.set(false),
                        "Cancel"
                    }
                    button {
                        class: "btn btn-sm btn-primary",
                        disabled: selected == 0,
                        onclick: move |_| {
                            let picked: Vec<Imported> = found
                                .peek()
                                .iter()
                                .filter(|(_, on)| *on)
                                .map(|(s, _)| s.clone())
                                .collect();
                            let (added, skipped) = app.import_servers(picked);
                            let mut notice = app.notice;
                            notice
                                .set(
                                    Some(
                                        Notice::info(match (added, skipped) {
                                            (0, _) => "Everything selected was already saved.".to_string(),
                                            (n, 0) => format!("Imported {n} server(s)."),
                                            (n, s) => {
                                                format!("Imported {n} server(s); {s} already saved.")
                                            }
                                        }),
                                    ),
                                );
                            open.set(false);
                        },
                        if selected > 0 { "Import {selected}" } else { "Import" }
                    }
                }
            }
        }
    }
}

/// One parsed entry, with whatever will not work about it stated up front.
#[component]
fn Row(index: usize, server: Imported, picked: bool, toggle: EventHandler<usize>) -> Element {
    let kind = match server.kind {
        TransportKind::Stdio => "stdio",
        TransportKind::Http => "http",
    };
    // The command or URL, because two entries called `filesystem` are told
    // apart by what they run, not by their name.
    let detail = match server.kind {
        TransportKind::Stdio => {
            let command = server.config["command"].as_str().unwrap_or_default();
            let args = server.config["args"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str())
                        .collect::<Vec<_>>()
                        .join(" ")
                })
                .unwrap_or_default();
            format!("{command} {args}").trim_end().to_string()
        }
        TransportKind::Http => server.config["url"]
            .as_str()
            .unwrap_or_default()
            .to_string(),
    };

    rsx! {
        label { class: "row cursor-pointer items-start gap-2",
            input {
                r#type: "checkbox",
                class: "checkbox checkbox-xs mt-0.5 shrink-0",
                checked: picked,
                onchange: move |_| toggle.call(index),
            }
            div { class: "min-w-0 flex-1",
                div { class: "flex items-baseline gap-2",
                    span { class: "truncate", "{server.name}" }
                    span { class: "badge badge-xs badge-ghost shrink-0", "{kind}" }
                }
                p { class: "truncate text-[11px] font-mono text-base-content/40", "{detail}" }
                if let Some(caveat) = &server.caveat {
                    p { class: "flex items-center gap-1 text-[11px] text-base-content/55",
                        Icon {
                            icon: LdTriangleAlert,
                            width: 11,
                            height: 11,
                            class: "shrink-0 text-base-content/40",
                        }
                        "{caveat}"
                    }
                }
            }
        }
    }
}
