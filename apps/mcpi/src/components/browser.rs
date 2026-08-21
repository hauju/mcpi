//! Middle pane: what the connected server advertises.

use dioxus::prelude::*;
use dioxus_free_icons::Icon;
use dioxus_free_icons::icons::ld_icons::{
    LdCircleAlert, LdKeyRound, LdLock, LdLockOpen, LdServer, LdUnplug,
};
use serde_json::Value;

use crate::auth_hint::{self, AuthHint};
use crate::components::diff::DiffBanner;
use crate::components::timeline::SnapshotTimeline;
use crate::state::{AppState, Conn, Selection, Tab};

#[component]
pub fn Browser() -> Element {
    let app = use_context::<AppState>();

    let Some(id) = *app.selected_server.read() else {
        return rsx! {
            PaneState {
                icon: rsx! {
                    Icon { icon: LdServer, width: 18, height: 18 }
                },
                title: "No server selected",
                hint: "Pick a server from the library on the left, or add one to start inspecting.",
            }
        };
    };

    match app.conn(id) {
        Conn::Connected(_) => rsx! {
            Contract {}
        },
        Conn::Connecting => rsx! {
            ConnectingSkeleton {}
        },
        Conn::Authorizing => rsx! {
            PaneState {
                icon: rsx! {
                    Icon { icon: LdKeyRound, width: 18, height: 18 }
                },
                title: "Waiting for the browser…",
                hint: "Finish signing in in the tab that just opened. This window will connect on its own.",
                span { class: "loading loading-spinner loading-sm opacity-60 mt-1" }
            }
        },
        Conn::NeedsAuth { .. } => rsx! {
            PaneState {
                icon: rsx! {
                    Icon { icon: LdKeyRound, width: 18, height: 18 }
                },
                title: "This server requires you to sign in",
                hint: "Your browser will open to authorize. The result is kept in your keychain, so this is a one-time step.",
                button {
                    class: "btn btn-sm btn-primary mt-1",
                    onclick: move |_| app.sign_in(id),
                    "Sign in"
                }
            }
        },
        Conn::Failed(error) => rsx! {
            ConnectionFailed { id, error }
        },
        Conn::Disconnected => rsx! {
            PaneState {
                icon: rsx! {
                    Icon { icon: LdUnplug, width: 18, height: 18 }
                },
                title: "Not connected",
                button {
                    class: "btn btn-sm btn-primary mt-1",
                    onclick: move |_| app.connect(id),
                    "Connect"
                }
                // What the store remembers about this server outlives the
                // session, so a disconnected server is not a blank slate.
                SnapshotTimeline {}
            }
        },
    }
}

#[component]
fn Contract() -> Element {
    let app = use_context::<AppState>();
    let mut tab = app.tab;
    let mut filter = use_signal(String::new);

    let Some(connected) = app.active() else {
        return rsx! {};
    };
    let snapshot = &connected.snapshot;
    let active = *tab.read();

    let all: Vec<String> = match active {
        Tab::Tools => snapshot.tools.keys().cloned().collect(),
        Tab::Resources => snapshot.resources.keys().cloned().collect(),
        Tab::Prompts => snapshot.prompts.keys().cloned().collect(),
    };

    // Matched against name and description: a server with ninety tools is
    // normal, and scrolling is not a search strategy.
    let needle = filter.read().trim().to_lowercase();
    let names: Vec<String> = all
        .iter()
        .filter(|name| {
            needle.is_empty()
                || name.to_lowercase().contains(&needle)
                || summary_of(&connected.snapshot, active, name)
                    .is_some_and(|s| s.to_lowercase().contains(&needle))
        })
        .cloned()
        .collect();
    let filtered_out = !needle.is_empty() && names.len() < all.len();

    // Which items the diff flagged, so a changed tool is visible without
    // opening the diff drawer.
    let diff = connected.status.diff();
    let breaking: Vec<&str> = diff.map(|d| d.breaking_items()).unwrap_or_default();
    let changed: Vec<&str> = diff
        .map(|d| {
            d.tools
                .iter()
                .chain(&d.resources)
                .chain(&d.prompts)
                .map(schemadiff::ItemChange::name)
                .collect()
        })
        .unwrap_or_default();

    rsx! {
        section { class: "flex-1 min-w-0 flex flex-col border-r border-base-300/70",
            DiffBanner {}

            header { class: "flex items-center px-2 border-b border-base-300/70",
                for candidate in Tab::ALL {
                    button {
                        key: "{candidate:?}",
                        class: if candidate == active {
                            "px-3 py-2 text-[13px] font-medium text-base-content border-b-2 border-primary -mb-px"
                        } else {
                            "px-3 py-2 text-[13px] text-base-content/50 border-b-2 border-transparent -mb-px hover:text-base-content/80 transition-colors"
                        },
                        onclick: move |_| tab.set(candidate),
                        "{candidate.label()}"
                        span {
                            class: if candidate == active { "ml-1.5 font-mono text-[11px] text-base-content/50" } else { "ml-1.5 font-mono text-[11px] text-base-content/30" },
                            "{count(&connected.snapshot, candidate)}"
                        }
                    }
                }
            }

            div { class: "px-2 py-1.5 border-b border-base-300/70",
                input {
                    class: "input input-xs w-full",
                    r#type: "search",
                    placeholder: "Filter {active.label().to_lowercase()}…",
                    value: "{filter}",
                    oninput: move |e| filter.set(e.value()),
                }
            }

            div { class: "flex-1 scroll-pane py-1", "data-item-list": "true",
                if names.is_empty() && filtered_out {
                    p { class: "px-3 py-10 text-sm text-base-content/40 text-center",
                        "Nothing matches the filter."
                    }
                } else if names.is_empty() {
                    p { class: "px-3 py-10 text-sm text-base-content/40 text-center",
                        "This server advertises no {active.label().to_lowercase()}."
                    }
                }
                for name in names {
                    ItemRow {
                        key: "{name}",
                        name: name.clone(),
                        tab: active,
                        summary: summary_of(&connected.snapshot, active, &name),
                        changed: changed.contains(&name.as_str()),
                        breaking: breaking.contains(&name.as_str()),
                        auth: auth_hint_for(&app, &connected.snapshot, active, &name),
                    }
                }
            }
        }
    }
}

/// Which lock, if any, this item has earned.
///
/// Observed beats declared: once a call has actually been refused there is
/// nothing left to guess about, and the weaker mark would only dilute it.
/// Tools only — the call log is keyed by tool, and a resource read failing is
/// a different conversation.
fn auth_hint_for(
    app: &AppState,
    snapshot: &mcpclient::Snapshot,
    tab: Tab,
    name: &str,
) -> Option<AuthHint> {
    if tab != Tab::Tools {
        return None;
    }
    if let Some(at) = app.auth_failures.read().get(name) {
        return Some(AuthHint::Observed(*at));
    }
    // The full description, not `summary_of`'s first sentence: servers put the
    // credential requirement last, after they have finished selling the tool.
    snapshot
        .tools
        .get(name)?
        .get("description")
        .and_then(Value::as_str)
        .filter(|d| auth_hint::declares_auth(d))
        .map(|_| AuthHint::Declared)
}

#[component]
fn ItemRow(
    name: String,
    tab: Tab,
    summary: Option<String>,
    changed: bool,
    breaking: bool,
    auth: Option<AuthHint>,
) -> Element {
    let app = use_context::<AppState>();
    let selection = app.selection;

    let selected = selection
        .read()
        .as_ref()
        .is_some_and(|s| s.tab == tab && s.name == name);

    let row_class = if selected { "row row-active" } else { "row" };

    // get_/list_/update_ prefixes repeat down the whole list; dimming the
    // shared verb leaves the object as the thing the eye scans.
    let (dim, bright) = match tab {
        Tab::Tools => match name.split_once('_') {
            Some((verb, rest)) => (format!("{verb}_"), rest.to_string()),
            None => (String::new(), name.clone()),
        },
        _ => (String::new(), name.clone()),
    };

    let pick = {
        let name = name.clone();
        move |_| {
            app.select_item(Selection {
                tab,
                name: name.clone(),
            })
        }
    };

    rsx! {
        button { class: "{row_class} block", onclick: pick,
            div { class: "flex items-center gap-2",
                span { class: "font-mono text-[13px] truncate",
                    span { class: "text-base-content/50", "{dim}" }
                    "{bright}"
                }
                // Breaking is solid, changed is soft: severity keeps its own
                // volume order even inside a quiet list.
                if breaking {
                    span { class: "badge badge-xs badge-error shrink-0", "breaking" }
                } else if changed {
                    span { class: "badge badge-xs badge-soft badge-warning shrink-0", "changed" }
                }
                // Deliberately not red or amber: those two colours mean the
                // contract moved, everywhere in this app. A tool needing a key
                // is the server working as intended, not a severity.
                match auth {
                    Some(AuthHint::Observed(at)) => {
                        let when = at.format("%-d %b %Y").to_string();
                        rsx! {
                            span {
                                class: "shrink-0 flex items-center gap-1 text-[10px] text-base-content/55",
                                title: "Last call was refused for want of credentials, {when}",
                                Icon { icon: LdLock, width: 11, height: 11 }
                                "auth"
                            }
                        }
                    }
                    Some(AuthHint::Declared) => rsx! {
                        span {
                            class: "shrink-0 text-base-content/30",
                            title: "This tool's description says it needs credentials — not yet confirmed by a call",
                            Icon { icon: LdLockOpen, width: 11, height: 11 }
                        }
                    },
                    None => rsx! {},
                }
            }
            if let Some(summary) = summary {
                div { class: "text-xs text-base-content/45 truncate", "{summary}" }
            }
        }
    }
}

/// The shape of the pane that is about to appear, not a spinner: the eye can
/// stay where the content will land, and a fast connect barely flashes it.
#[component]
fn ConnectingSkeleton() -> Element {
    const NAME_WIDTHS: [&str; 4] = ["w-32", "w-44", "w-28", "w-40"];
    const SUMMARY_WIDTHS: [&str; 4] = ["w-4/5", "w-3/5", "w-2/3", "w-1/2"];

    rsx! {
        section { class: "flex-1 min-w-0 flex flex-col border-r border-base-300/70",
            header { class: "flex items-center gap-4 px-3 py-2.5 border-b border-base-300/70",
                div { class: "skeleton h-3.5 w-14" }
                div { class: "skeleton h-3.5 w-20 opacity-60" }
                div { class: "skeleton h-3.5 w-16 opacity-60" }
            }
            div { class: "px-2 py-1.5 border-b border-base-300/70",
                div { class: "skeleton h-6 w-full" }
            }
            div { class: "flex-1 overflow-hidden py-1",
                // Fading toward the bottom keeps the placeholder reading as
                // "loading", not as nine identical mystery rows.
                for row in 0..9 {
                    div {
                        key: "{row}",
                        class: "px-3 py-2 space-y-1.5",
                        style: "opacity: {1.0 - row as f32 * 0.1}",
                        div { class: "skeleton h-3 {NAME_WIDTHS[row % 4]}" }
                        div { class: "skeleton h-2.5 {SUMMARY_WIDTHS[(row + 1) % 4]} opacity-70" }
                    }
                }
            }
        }
    }
}

#[component]
fn ConnectionFailed(id: i64, error: String) -> Element {
    let app = use_context::<AppState>();
    rsx! {
        PaneState {
            icon: rsx! {
                Icon { icon: LdCircleAlert, width: 18, height: 18 }
            },
            failed: true,
            title: "Could not connect",
            // `break-all` matters: causes routinely contain a long unbroken URL
            // or type path, which would otherwise run off the pane.
            p { class: "max-w-md text-center text-xs text-base-content/60 font-mono break-all selectable",
                "{error}"
            }
            div { class: "mt-1 flex gap-1.5",
                button {
                    class: "btn btn-sm",
                    onclick: move |_| app.connect(id),
                    "Try again"
                }
                // Only offered when there is something to read. A stdio child
                // that failed to start usually said why on its own `stderr`,
                // and that line is the answer far more often than the
                // transport error above it is.
                if app.console_has_lines() {
                    button {
                        class: "btn btn-sm btn-ghost",
                        onclick: move |_| {
                            let mut open = app.console_open;
                            open.set(true);
                        },
                        "Show console"
                    }
                }
            }
        }
    }
}

/// A full-pane state: one glyph, one verdict line, one hint. These are the
/// first screens a new user meets, so they explain themselves instead of
/// floating a bare sentence in the void.
#[component]
fn PaneState(
    icon: Element,
    title: String,
    #[props(default = None)] hint: Option<String>,
    #[props(default = false)] failed: bool,
    children: Element,
) -> Element {
    // The ring goes red only for an actual failure; every other state keeps
    // severity colours out of play.
    let ring = if failed {
        "border-error/30 bg-error/10 text-error"
    } else {
        "border-base-300/70 bg-base-200/40 text-base-content/40"
    };

    rsx! {
        section { class: "flex-1 flex flex-col items-center justify-center gap-1.5 p-8 border-r border-base-300/70 text-center",
            div { class: "mb-1.5 flex size-10 items-center justify-center rounded-full border {ring}",
                {icon}
            }
            p { class: "text-sm font-medium text-base-content/80", "{title}" }
            if let Some(hint) = hint {
                p { class: "max-w-xs text-xs text-base-content/45 leading-relaxed", "{hint}" }
            }
            {children}
        }
    }
}

fn count(snapshot: &mcpclient::Snapshot, tab: Tab) -> usize {
    match tab {
        Tab::Tools => snapshot.tools.len(),
        Tab::Resources => snapshot.resources.len(),
        Tab::Prompts => snapshot.prompts.len(),
    }
}

/// The one line worth showing under an item's name in the list.
fn summary_of(snapshot: &mcpclient::Snapshot, tab: Tab, name: &str) -> Option<String> {
    let item = match tab {
        Tab::Tools => snapshot.tools.get(name),
        Tab::Resources => snapshot.resources.get(name),
        Tab::Prompts => snapshot.prompts.get(name),
    }?;
    item.get("description")
        .and_then(Value::as_str)
        // Descriptions are often multi-line; the row has one. Stopping at the
        // first sentence keeps the CSS ellipsis from landing mid-word.
        .map(|d| {
            let line = d.lines().next().unwrap_or(d);
            match line.find(". ") {
                Some(dot) => line[..=dot].to_string(),
                None => line.to_string(),
            }
        })
        .filter(|d| !d.is_empty())
}
