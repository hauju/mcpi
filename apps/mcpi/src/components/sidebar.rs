//! Left pane: the saved server library.

use dioxus::prelude::*;
use dioxus_free_icons::Icon;
use dioxus_free_icons::icons::ld_icons::{
    LdFolderInput, LdKeyRound, LdPencil, LdPlay, LdPlus, LdServer, LdSquare,
};
use mcpstore::ServerRow;

use crate::components::collections::Collections;
use crate::components::history::History;
use crate::state::{AppState, Conn, ServerDraft, Status};

#[component]
pub fn Sidebar() -> Element {
    let app = use_context::<AppState>();
    let servers = app.servers;
    let mut draft = app.draft;
    let mut import_open = app.import_open;

    rsx! {
        // The rail sits one shade below both work surfaces so the three panes
        // read as library → contract → item, left to right.
        aside { class: "w-64 shrink-0 flex flex-col border-r border-base-300/70 bg-well",
            header { class: "flex items-center px-3 py-2 border-b border-base-300/60",
                h1 { class: "section-label flex-1", "Servers" }
                button {
                    class: "btn btn-ghost btn-xs btn-square text-base-content/50 hover:text-base-content",
                    title: "Import from another MCP client",
                    onclick: move |_| import_open.set(true),
                    Icon { icon: LdFolderInput, width: 13, height: 13 }
                }
                button {
                    class: "btn btn-ghost btn-xs btn-square text-base-content/50 hover:text-base-content",
                    title: "Add server",
                    onclick: move |_| draft.set(Some(ServerDraft::default())),
                    Icon { icon: LdPlus, width: 13, height: 13 }
                }
            }

            // The library owns the leftover height; History and Collections are
            // pinned sections below it, so a long server list never squeezes
            // them into unusable slivers (or vice versa).
            div { class: "flex-1 min-h-0 scroll-pane py-1",
                if servers.read().is_empty() {
                    EmptyLibrary {}
                }
                for server in servers.read().iter() {
                    ServerItem { key: "{server.id}", server: server.clone() }
                }
            }

            History {}
            Collections {}
        }
    }
}

/// The first thing a new user sees: say what goes here and offer the one
/// action that makes the rest of the app exist.
#[component]
fn EmptyLibrary() -> Element {
    let app = use_context::<AppState>();
    let mut draft = app.draft;
    let mut import_open = app.import_open;

    rsx! {
        div { class: "flex flex-col items-center gap-1.5 px-4 py-10 text-center",
            Icon {
                icon: LdServer,
                width: 20,
                height: 20,
                class: "mb-1 text-base-content/25",
            }
            p { class: "text-sm text-base-content/70", "No servers yet" }
            p { class: "text-xs text-base-content/40 leading-relaxed",
                "Add a local command or a remote URL to start inspecting."
            }
            div { class: "mt-2 flex gap-1.5",
                button {
                    class: "btn btn-xs btn-primary",
                    onclick: move |_| draft.set(Some(ServerDraft::default())),
                    "Add server"
                }
                button {
                    class: "btn btn-xs btn-ghost",
                    onclick: move |_| import_open.set(true),
                    "Import"
                }
            }
        }
    }
}

#[component]
fn ServerItem(server: ServerRow) -> Element {
    let app = use_context::<AppState>();
    let id = server.id;

    let conn = app.conn(id);
    let selected = *app.selected_server.read() == Some(id);

    // A breaking change is the one thing worth interrupting the eye for, so it
    // is the only state that gets a colour other than the connection dot.
    let breaking = conn
        .connected()
        .and_then(|c| c.status.diff())
        .map(|d| d.counts().breaking)
        .unwrap_or(0);

    let row_class = if selected { "row row-active" } else { "row" };

    rsx! {
        div { class: "group flex items-center",
            button {
                class: "{row_class} flex-1 min-w-0",
                onclick: move |_| app.select_server(id),
                StatusDot { status: conn.status() }
                span { class: "flex-1 truncate", "{server.name}" }
                if breaking > 0 {
                    span {
                        class: "badge badge-xs badge-error shrink-0",
                        title: "{breaking} breaking change(s) since the last connect",
                        "{breaking}"
                    }
                }
            }
            ServerActions { server: server.clone() }
        }
    }
}

/// Shared with the title bar and the palette, so one colour map answers
/// "what does a connection state look like" everywhere.
#[component]
pub fn StatusDot(status: Status) -> Element {
    let (class, title) = match status {
        Status::Connected => ("bg-success", "Connected"),
        Status::Connecting => ("bg-warning animate-pulse", "Connecting"),
        // Not a failure — an action waiting to be taken.
        Status::NeedsAuth => ("bg-info", "Needs sign-in"),
        Status::Failed => ("bg-error", "Failed"),
        Status::Disconnected => ("bg-base-content/25", "Not connected"),
    };
    rsx! {
        span { class: "size-2 rounded-full shrink-0 {class}", title: "{title}" }
    }
}

/// Connect / disconnect and the edit affordance.
///
/// Always visible rather than revealed on hover: a hidden action is an
/// undiscoverable one, and two 12px glyphs at low opacity keep the list quiet
/// enough to read past. Hovering the row merely brightens them.
#[component]
fn ServerActions(server: ServerRow) -> Element {
    let app = use_context::<AppState>();
    let mut draft = app.draft;
    let id = server.id;
    let conn = app.conn(id);

    rsx! {
        div { class: "flex items-center pr-1.5",
            match conn {
                Conn::Connected(_) => rsx! {
                    button {
                        class: "btn btn-ghost btn-xs btn-square text-base-content/40 group-hover:text-base-content/70 hover:text-base-content",
                        title: "Disconnect",
                        onclick: move |_| app.disconnect(id),
                        Icon { icon: LdSquare, width: 12, height: 12 }
                    }
                },
                Conn::Connecting | Conn::Authorizing => rsx! {
                    span { class: "flex size-6 items-center justify-center",
                        span { class: "loading loading-spinner loading-xs opacity-60" }
                    }
                },
                Conn::NeedsAuth { .. } => rsx! {
                    button {
                        class: "btn btn-ghost btn-xs btn-square text-info/70 hover:text-info",
                        title: "Sign in to this server",
                        onclick: move |_| app.sign_in(id),
                        Icon { icon: LdKeyRound, width: 13, height: 13 }
                    }
                },
                _ => rsx! {
                    button {
                        class: "btn btn-ghost btn-xs btn-square text-base-content/40 group-hover:text-base-content/70 hover:text-base-content",
                        title: "Connect",
                        onclick: move |_| app.connect(id),
                        Icon { icon: LdPlay, width: 13, height: 13 }
                    }
                },
            }
            button {
                class: "btn btn-ghost btn-xs btn-square text-base-content/25 group-hover:text-base-content/60 hover:text-base-content",
                title: "Edit",
                onclick: {
                    let server = server.clone();
                    move |_| draft.set(Some(ServerDraft::from_row(&server)))
                },
                Icon { icon: LdPencil, width: 12, height: 12 }
            }
        }
    }
}
