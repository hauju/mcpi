//! The add / edit server dialog.
//!
//! Repeatable fields (arguments, environment, headers) are edited as plain
//! textareas rather than as key/value widget lists. That is a deliberate v1
//! trade: people configuring MCP servers are copying these out of a JSON config
//! file, and a textarea takes a paste intact where a row editor would not.

use dioxus::prelude::*;
use dioxus_free_icons::Icon;
use dioxus_free_icons::icons::ld_icons::LdX;

use crate::components::endpoint_check::EndpointCheck;
use crate::config::Principal;
use crate::group;
use crate::state::{AppState, DraftKind, ServerDraft};

#[component]
pub fn ServerDialog() -> Element {
    let app = use_context::<AppState>();
    let mut draft_signal = app.draft;

    let Some(draft) = draft_signal.read().clone() else {
        return rsx! {};
    };

    let editing = draft.id;
    let title = if editing.is_some() {
        "Edit server"
    } else {
        "Add server"
    };

    // Every field writes back through `update` below, so the draft stays the
    // one source of truth and validation only has to look in one place.

    rsx! {
        div {
            class: "overlay-in fixed inset-0 z-50 flex items-center justify-center bg-black/60 backdrop-blur-[2px] p-6",
            // Keydowns bubble here from the fields, so Esc works while typing;
            // the name input's autofocus puts focus inside on open.
            tabindex: "0",
            onkeydown: move |evt| {
                if evt.key() == Key::Escape {
                    draft_signal.set(None);
                }
            },
            onclick: move |_| draft_signal.set(None),

            div {
                class: "dialog-in w-full max-w-lg rounded-box border border-base-300 bg-base-100 shadow-2xl",
                // Clicks inside must not reach the backdrop's dismiss handler.
                onclick: move |evt| evt.stop_propagation(),

                header { class: "flex items-center px-5 py-3 border-b border-base-300",
                    h2 { class: "font-semibold flex-1", "{title}" }
                    button {
                        class: "btn btn-ghost btn-xs btn-square text-base-content/50 hover:text-base-content",
                        title: "Close",
                        onclick: move |_| draft_signal.set(None),
                        Icon { icon: LdX, width: 14, height: 14 }
                    }
                }

                div { class: "px-5 py-4 space-y-4 max-h-[60vh] scroll-pane",
                    Field { label: "Name",
                        input {
                            class: "input input-sm w-full",
                            placeholder: "SeggWat Dev",
                            value: "{draft.name}",
                            autofocus: true,
                            oninput: move |e| update(draft_signal, |d, v| d.name = v, e.value()),
                        }
                    }

                    Field { label: "Transport",
                        div { class: "join",
                            for kind in [DraftKind::Http, DraftKind::Stdio, DraftKind::WebMcp] {
                                button {
                                    key: "{kind:?}",
                                    class: if draft.kind == kind {
                                        "btn btn-sm join-item btn-primary"
                                    } else {
                                        "btn btn-sm join-item"
                                    },
                                    onclick: move |_| {
                                        if let Some(d) = draft_signal.write().as_mut() {
                                            d.kind = kind;
                                            d.error = None;
                                        }
                                    },
                                    match kind {
                                        DraftKind::Stdio => "Local (stdio)",
                                        DraftKind::Http => "Remote (HTTP)",
                                        DraftKind::WebMcp => "Page (WebMCP)",
                                    }
                                }
                            }
                        }
                    }

                    if draft.kind == DraftKind::Stdio {
                        Field { label: "Command",
                            input {
                                class: "input input-sm w-full font-mono",
                                placeholder: "npx",
                                value: "{draft.command}",
                                oninput: move |e| update(draft_signal, |d, v| d.command = v, e.value()),
                            }
                        }
                        Field { label: "Arguments", hint: "One per line",
                            textarea {
                                class: "textarea textarea-sm w-full font-mono",
                                rows: 3,
                                placeholder: "-y\n@modelcontextprotocol/server-everything",
                                value: "{draft.args}",
                                oninput: move |e| update(draft_signal, |d, v| d.args = v, e.value()),
                            }
                        }
                        Field { label: "Environment", hint: "KEY=value, one per line",
                            textarea {
                                class: "textarea textarea-sm w-full font-mono",
                                rows: 2,
                                placeholder: "LOG_LEVEL=debug",
                                value: "{draft.env}",
                                oninput: move |e| update(draft_signal, |d, v| d.env = v, e.value()),
                            }
                        }
                        Field { label: "Working directory", hint: "Optional",
                            input {
                                class: "input input-sm w-full font-mono",
                                value: "{draft.cwd}",
                                oninput: move |e| update(draft_signal, |d, v| d.cwd = v, e.value()),
                            }
                        }
                    } else if draft.kind == DraftKind::WebMcp {
                        Field { label: "Page URL",
                            input {
                                class: "input input-sm w-full font-mono",
                                placeholder: "https://app.example.com",
                                value: "{draft.url}",
                                oninput: move |e| {
                                    let url = e.value();
                                    let suggested = group::suggest(
                                        &app.servers.read(),
                                        &url,
                                        draft_signal.read().as_ref().and_then(|d| d.id),
                                    );
                                    if let Some(d) = draft_signal.write().as_mut() {
                                        d.url = url;
                                        d.group_id = suggested;
                                        d.error = None;
                                    }
                                },
                            }
                        }
                        Field { label: "Read as", hint: "A page shows a stranger fewer tools",
                            div { class: "join",
                                for principal in [Principal::Anonymous, Principal::SignedIn] {
                                    button {
                                        key: "{principal:?}",
                                        class: if draft.principal == principal {
                                            "btn btn-sm join-item btn-primary"
                                        } else {
                                            "btn btn-sm join-item"
                                        },
                                        onclick: move |_| {
                                            if let Some(d) = draft_signal.write().as_mut() {
                                                d.principal = principal;
                                                d.error = None;
                                            }
                                        },
                                        "{principal.label()}"
                                    }
                                }
                            }
                        }
                        p { class: "text-xs opacity-50",
                            "mcpi opens its own Chrome — nothing to quit, no flags. "
                            if draft.principal == Principal::SignedIn {
                                "Signed-in scans run in a profile mcpi keeps, so you sign in once and it sticks. Use Sign in on the server when a scan comes back empty."
                            } else {
                                "Anonymous scans use a fresh profile every time, which is what CI reproduces."
                            }
                        }
                        p { class: "text-xs opacity-50",
                            "The two are separate contracts with separate history — comparing them would read every members-only tool as removed."
                        }
                    } else {
                        Field { label: "URL",
                            input {
                                class: "input input-sm w-full font-mono",
                                placeholder: "https://example.com/mcp",
                                value: "{draft.url}",
                                // Editing the URL retires the verdict below it,
                                // which was about the previous URL, and
                                // re-derives which entry this would join.
                                oninput: move |e| {
                                    let url = e.value();
                                    let suggested = group::suggest(
                                        &app.servers.read(),
                                        &url,
                                        draft_signal.read().as_ref().and_then(|d| d.id),
                                    );
                                    if let Some(d) = draft_signal.write().as_mut() {
                                        d.url = url;
                                        d.group_id = suggested;
                                        d.error = None;
                                    }
                                    let mut report = app.endpoint_report;
                                    report.set(None);
                                },
                            }
                        }
                        Field { label: "Headers", hint: "Header: value, one per line",
                            textarea {
                                class: "textarea textarea-sm w-full font-mono",
                                rows: 3,
                                placeholder: "Authorization: Bearer …",
                                value: "{draft.headers}",
                                oninput: move |e| update(draft_signal, |d, v| d.headers = v, e.value()),
                            }
                        }
                        p { class: "text-xs opacity-50",
                            "Headers are stored in the local database. If the server asks for OAuth instead, you will be offered a sign-in — those tokens go to your keychain, never here."
                        }

                        EndpointCheck {}
                    }

                    if draft.kind != DraftKind::Stdio {
                        GroupField { draft: draft.clone() }
                    }

                    if let Some(error) = &draft.error {
                        p { class: "text-sm text-error", "{error}" }
                    }
                }

                footer { class: "flex items-center gap-2 px-5 py-3 border-t border-base-300",
                    // Deleting takes two presses: the cascade also drops the
                    // server's snapshots, call history, and collections, and
                    // that accumulated record is the one thing a misclick
                    // cannot bring back.
                    if let (Some(id), true) = (editing, draft.confirm_delete) {
                        p { class: "text-xs text-base-content/70",
                            "Its snapshots, history, and collections go too."
                        }
                        div { class: "ml-auto flex gap-2",
                            button {
                                class: "btn btn-sm btn-ghost",
                                onclick: move |_| {
                                    if let Some(d) = draft_signal.write().as_mut() {
                                        d.confirm_delete = false;
                                    }
                                },
                                "Keep"
                            }
                            button {
                                class: "btn btn-sm btn-error",
                                onclick: move |_| {
                                    app.delete_server(id);
                                    draft_signal.set(None);
                                },
                                "Delete server"
                            }
                        }
                    } else {
                        if let Some(id) = editing {
                            button {
                                class: "btn btn-sm btn-ghost text-error",
                                onclick: move |_| {
                                    if let Some(d) = draft_signal.write().as_mut() {
                                        d.confirm_delete = true;
                                    }
                                },
                                "Delete"
                            }
                            if draft.kind == DraftKind::Http {
                                button {
                                    class: "btn btn-sm btn-ghost",
                                    title: "Forget the OAuth credentials stored in your keychain for this server",
                                    onclick: move |_| {
                                        app.sign_out(id);
                                        draft_signal.set(None);
                                    },
                                    "Sign out"
                                }
                            }
                        }
                        div { class: "ml-auto flex gap-2",
                            button {
                                class: "btn btn-sm btn-ghost",
                                onclick: move |_| draft_signal.set(None),
                                "Cancel"
                            }
                            button {
                                class: "btn btn-sm btn-primary",
                                onclick: move |_| app.save_draft(),
                                "Save"
                            }
                        }
                    }
                }
            }
        }
    }
}

/// Offer to show this server beside one already in the library.
///
/// Only ever appears when there is a real candidate: a site's endpoint and its
/// page share an origin, which is how the suggestion arrives pre-filled. It
/// groups the two for display only — each keeps its own contract history, and
/// that is what makes grouping safe to offer at all.
#[component]
fn GroupField(draft: ServerDraft) -> Element {
    let app = use_context::<AppState>();
    let mut draft_signal = app.draft;

    let servers = app.servers.read().clone();
    let target_id = draft
        .group_id
        .or_else(|| group::suggest(&servers, &draft.url, draft.id));
    let Some(target) = target_id.and_then(|id| servers.iter().find(|r| r.id == id).cloned()) else {
        return rsx! {};
    };

    let grouped = draft.group_id == Some(target.id);
    let id = target.id;

    rsx! {
        Field { label: "Grouping",
            label { class: "flex items-center gap-2 cursor-pointer",
                input {
                    r#type: "checkbox",
                    class: "checkbox checkbox-xs",
                    checked: grouped,
                    onchange: move |_| {
                        if let Some(d) = draft_signal.write().as_mut() {
                            d.group_id = if grouped { None } else { Some(id) };
                            d.error = None;
                        }
                    },
                }
                span { class: "text-sm",
                    "Show beside "
                    span { class: "font-medium", "{target.name}" }
                    " as one entry"
                }
            }
            p { class: "text-xs opacity-50 mt-1",
                "Same site, two surfaces. They stay separate contracts with separate history — this only joins them in the list."
            }
        }
    }
}

/// Apply one field's edit to the open draft and clear any stale error.
///
/// A free function over the (`Copy`) signal rather than a closure: a closure
/// capturing the signal would be `FnMut`, and every `oninput` below needs its
/// own copy.
fn update(
    mut draft: Signal<Option<ServerDraft>>,
    mutate: fn(&mut ServerDraft, String),
    value: String,
) {
    if let Some(draft) = draft.write().as_mut() {
        mutate(draft, value);
        draft.error = None;
    }
}

#[component]
fn Field(
    label: String,
    #[props(default = None)] hint: Option<String>,
    children: Element,
) -> Element {
    rsx! {
        label { class: "block space-y-1",
            div { class: "flex items-baseline gap-2",
                span { class: "section-label text-base-content/60", "{label}" }
                if let Some(hint) = hint {
                    span { class: "text-[10px] text-base-content/40", "{hint}" }
                }
            }
            {children}
        }
    }
}
