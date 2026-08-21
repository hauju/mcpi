//! The MCP inspector desktop app.

use std::path::PathBuf;
use std::sync::Arc;

use dioxus::desktop::muda::accelerator::{Accelerator, Code, Modifiers};
use dioxus::desktop::muda::{AboutMetadata, Menu, MenuItem, PredefinedMenuItem, Submenu};
use dioxus::desktop::tao::event::Event as TaoEvent;
use dioxus::desktop::{
    LogicalPosition, LogicalSize, WindowEvent, use_muda_event_handler, use_wry_event_handler,
};
use dioxus::prelude::*;
use dioxus_free_icons::Icon;
use dioxus_free_icons::icons::ld_icons::{
    LdCircleCheck, LdDatabaseZap, LdTerminal, LdTriangleAlert,
};
use mcpstore::Store;

mod auth_hint;
mod components;
mod config;
mod demo;
mod form;
mod import;
mod state;
#[cfg(test)]
mod tests;

use components::{
    Browser, CollectionRunner, Console, Detail, DiffDrawer, ImportDialog, Palette, ServerDialog,
    Sidebar, StatusDot,
};
use state::{AppState, ServerDraft, Tab};

const TAILWIND: Asset = asset!("/assets/tailwind.css");
const LOGO: Asset = asset!("/assets/logo.svg");

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,mcpi=debug".parse().expect("valid filter")),
        )
        .init();

    let mut window = dioxus::desktop::WindowBuilder::new().with_title("mcpi");
    window = match saved_geometry() {
        Some((position, size)) => window.with_position(position).with_inner_size(size),
        None => window.with_inner_size(LogicalSize::new(1280.0, 820.0)),
    };

    // The chrome is drawn by the app: the traffic lights float over the
    // sidebar rail, and the strip they sit in is `TitleBar`, the drag surface.
    #[cfg(target_os = "macos")]
    let window = {
        use dioxus::desktop::tao::platform::macos::WindowBuilderExtMacOS;
        window
            .with_titlebar_transparent(true)
            .with_title_hidden(true)
            .with_fullsize_content_view(true)
    };

    dioxus::LaunchBuilder::desktop()
        .with_cfg(
            dioxus::desktop::Config::new()
                .with_menu(app_menu())
                .with_window(window),
        )
        .launch(App);
}

/// `~/Library/Application Support/mcpi` on macOS. Defined in `mcpstore` so
/// the CLI opens the same database.
fn store_path() -> PathBuf {
    mcpstore::default_path()
}

/// Where the window was, remembered across launches. Lives next to the store.
fn window_state_path() -> PathBuf {
    mcpstore::default_path().with_file_name("window.json")
}

fn saved_geometry() -> Option<(LogicalPosition<f64>, LogicalSize<f64>)> {
    let text = std::fs::read_to_string(window_state_path()).ok()?;
    let value: serde_json::Value = serde_json::from_str(&text).ok()?;
    let field = |key: &str| value.get(key).and_then(serde_json::Value::as_f64);
    Some((
        LogicalPosition::new(field("x")?, field("y")?),
        // Floors keep a corrupted file from restoring an unusable sliver.
        LogicalSize::new(field("w")?.max(700.0), field("h")?.max(500.0)),
    ))
}

fn save_geometry(window: &dioxus::desktop::tao::window::Window) {
    let scale = window.scale_factor();
    let Ok(position) = window.outer_position() else {
        return;
    };
    let position = position.to_logical::<f64>(scale);
    let size = window.inner_size().to_logical::<f64>(scale);
    let _ = std::fs::write(
        window_state_path(),
        serde_json::json!({
            "x": position.x, "y": position.y,
            "w": size.width, "h": size.height,
        })
        .to_string(),
    );
}

/// The native menu bar. The accelerators here *are* the app's keyboard map —
/// the menu is how macOS both dispatches a shortcut and advertises it.
fn app_menu() -> Menu {
    let cmd = |code| Accelerator::new(Some(Modifiers::META), code);

    // The first submenu becomes the application menu; macOS titles it itself.
    let application = Submenu::new("mcpi", true);
    application
        .append_items(&[
            &PredefinedMenuItem::about(
                Some("About mcpi"),
                Some(AboutMetadata {
                    name: Some("mcpi".into()),
                    version: Some(env!("CARGO_PKG_VERSION").into()),
                    ..Default::default()
                }),
            ),
            &PredefinedMenuItem::separator(),
            &PredefinedMenuItem::hide(None),
            &PredefinedMenuItem::hide_others(None),
            &PredefinedMenuItem::show_all(None),
            &PredefinedMenuItem::separator(),
            // Custom rather than predefined: quitting saves the window
            // geometry first, and the predefined item exits before any code
            // gets to run.
            &MenuItem::with_id("quit", "Quit mcpi", true, Some(cmd(Code::KeyQ))),
        ])
        .expect("static menu");

    let file = Submenu::new("File", true);
    file.append_items(&[
        &MenuItem::with_id("new-server", "New Server…", true, Some(cmd(Code::KeyN))),
        &MenuItem::with_id("import", "Import from Another Client…", true, None),
        &PredefinedMenuItem::separator(),
        &PredefinedMenuItem::close_window(None),
    ])
    .expect("static menu");

    // Without these, ⌘C/⌘V/⌘A do nothing in webview inputs on macOS.
    let edit = Submenu::new("Edit", true);
    edit.append_items(&[
        &PredefinedMenuItem::undo(None),
        &PredefinedMenuItem::redo(None),
        &PredefinedMenuItem::separator(),
        &PredefinedMenuItem::cut(None),
        &PredefinedMenuItem::copy(None),
        &PredefinedMenuItem::paste(None),
        &PredefinedMenuItem::separator(),
        &PredefinedMenuItem::select_all(None),
    ])
    .expect("static menu");

    let go = Submenu::new("Go", true);
    go.append_items(&[
        &MenuItem::with_id("palette", "Jump to Anything…", true, Some(cmd(Code::KeyK))),
        &MenuItem::with_id("filter", "Filter Items", true, Some(cmd(Code::KeyF))),
        &MenuItem::with_id("console", "Console", true, Some(cmd(Code::Backquote))),
        &PredefinedMenuItem::separator(),
        &MenuItem::with_id("tab-tools", "Tools", true, Some(cmd(Code::Digit1))),
        &MenuItem::with_id("tab-resources", "Resources", true, Some(cmd(Code::Digit2))),
        &MenuItem::with_id("tab-prompts", "Prompts", true, Some(cmd(Code::Digit3))),
    ])
    .expect("static menu");

    let server = Submenu::new("Server", true);
    server
        .append_items(&[
            &MenuItem::with_id("call", "Call Selected Item", true, Some(cmd(Code::Enter))),
            &MenuItem::with_id("reconnect", "Reconnect", true, Some(cmd(Code::KeyR))),
        ])
        .expect("static menu");

    let window_menu = Submenu::new("Window", true);
    window_menu
        .append_items(&[
            &PredefinedMenuItem::minimize(None),
            &PredefinedMenuItem::maximize(None),
            &PredefinedMenuItem::fullscreen(None),
        ])
        .expect("static menu");

    let menu = Menu::new();
    menu.append_items(&[&application, &file, &edit, &go, &server, &window_menu])
        .expect("static menu");

    #[cfg(target_os = "macos")]
    window_menu.set_as_windows_menu_for_nsapp();

    menu
}

/// Plain ↑/↓ walk the item list from anywhere, but never while typing in a
/// field — the browser side filters, the Rust side decides.
const ARROW_KEYS: &str = r#"
window.addEventListener("keydown", (e) => {
    if (e.key !== "ArrowDown" && e.key !== "ArrowUp") return;
    if (e.metaKey || e.ctrlKey || e.altKey) return;
    const t = document.activeElement && document.activeElement.tagName;
    if (t === "INPUT" || t === "TEXTAREA" || t === "SELECT") return;
    e.preventDefault();
    dioxus.send(e.key === "ArrowDown" ? "next" : "prev");
});
"#;

#[component]
fn App() -> Element {
    // Opening the store is the one failure the app cannot route around, so it
    // gets its own screen rather than a toast over an empty shell.
    let store = use_hook(|| match Store::open(&store_path()) {
        Ok(store) => Ok(Arc::new(store)),
        Err(e) => Err(format!("{e}")),
    });

    let store = match store {
        Ok(store) => store,
        Err(message) => return rsx! { FatalError { message } },
    };

    let app = use_context_provider(|| AppState::new(store));

    use_hook(|| app.reload_servers());

    let desktop_for_menu = dioxus::desktop::window();
    use_muda_event_handler(move |event| match event.id().0.as_str() {
        "new-server" => {
            let mut draft = app.draft;
            draft.set(Some(ServerDraft::default()));
        }
        "console" => {
            let mut open = app.console_open;
            let now = *open.peek();
            open.set(!now);
        }
        "import" => {
            let mut open = app.import_open;
            open.set(true);
        }
        "palette" => {
            let mut open = app.palette_open;
            let now = *open.peek();
            open.set(!now);
        }
        "filter" => {
            let _ = document::eval(r#"document.querySelector('input[type="search"]')?.focus()"#);
        }
        "tab-tools" => {
            let mut tab = app.tab;
            tab.set(Tab::Tools);
        }
        "tab-resources" => {
            let mut tab = app.tab;
            tab.set(Tab::Resources);
        }
        "tab-prompts" => {
            let mut tab = app.tab;
            tab.set(Tab::Prompts);
        }
        "call" => app.run(),
        "reconnect" => {
            let id = *app.selected_server.peek();
            if let Some(id) = id {
                app.connect(id);
            }
        }
        "quit" => {
            save_geometry(&desktop_for_menu.window);
            desktop_for_menu.close();
        }
        _ => {}
    });

    // The red button and ⌘W come through here; ⌘Q is the branch above.
    let desktop_for_close = dioxus::desktop::window();
    use_wry_event_handler(move |event, _| {
        if let TaoEvent::WindowEvent {
            event: WindowEvent::CloseRequested,
            ..
        } = event
        {
            save_geometry(&desktop_for_close.window);
        }
    });

    use_future(move || async move {
        let mut keys = document::eval(ARROW_KEYS);
        while let Ok(step) = keys.recv::<String>().await {
            if app.overlay_open() {
                continue;
            }
            app.select_adjacent(if step == "next" { 1 } else { -1 });
            let _ = document::eval(
                "requestAnimationFrame(() => \
                 document.querySelector('[data-item-list] .row-active')?.scrollIntoView({block: 'nearest'}))",
            );
        }
    });

    rsx! {
        document::Stylesheet { href: TAILWIND }

        div { class: "h-screen flex flex-col bg-base-100 text-base-content font-sans",
            TitleBar {}
            Notice {}
            div { class: "flex-1 flex min-h-0",
                Sidebar {}
                Browser {}
                Detail {}
            }
            ServerDialog {}
            ImportDialog {}
            Console {}
            DiffDrawer {}
            CollectionRunner {}
            Palette {}
        }
    }
}

/// The strip the traffic lights sit in: drag surface, double-click to zoom,
/// and the one place the chrome says which server it is looking at.
#[component]
fn TitleBar() -> Element {
    let app = use_context::<AppState>();
    let mut open = app.palette_open;
    let mut console = app.console_open;
    let mut last_press = use_signal(|| None::<std::time::Instant>);

    let selected = *app.selected_server.read();
    let current = selected.and_then(|id| {
        app.servers
            .read()
            .iter()
            .find(|s| s.id == id)
            .map(|s| (s.name.clone(), app.conn(id).status()))
    });

    rsx! {
        header {
            class: "h-9 shrink-0 flex items-stretch border-b border-base-300/70 select-none",
            onmousedown: move |_| {
                // tao has no double-click event for a drag region, so one is
                // reconstructed from two presses inside the usual interval.
                let now = std::time::Instant::now();
                let double = last_press
                    .peek()
                    .is_some_and(|t| now.duration_since(t).as_millis() < 350);
                last_press.set(if double { None } else { Some(now) });
                let win = dioxus::desktop::window();
                if double {
                    let maximized = win.window.is_maximized();
                    win.window.set_maximized(!maximized);
                } else {
                    win.drag();
                }
            },
            // Matches the sidebar rail so the lights sit on its tone; the
            // left padding clears the traffic lights.
            div { class: "w-64 shrink-0 flex items-center gap-1.5 pl-20 bg-well border-r border-base-300/70",
                img { src: LOGO, class: "size-5 rounded-[5px] opacity-80" }
                span { class: "text-xs font-medium tracking-wide text-base-content/45", "mcpi" }
            }
            div { class: "relative flex-1 flex items-center justify-center gap-2 bg-base-100",
                if let Some((name, status)) = current {
                    StatusDot { status }
                    span { class: "text-xs text-base-content/50", "{name}" }
                }
                button {
                    class: "absolute right-24 btn btn-ghost btn-xs btn-square text-base-content/35 hover:text-base-content/70",
                    title: "Console (\u{2318}`)",
                    onmousedown: move |e| e.stop_propagation(),
                    onclick: move |_| {
                        let now = *console.peek();
                        console.set(!now);
                    },
                    Icon { icon: LdTerminal, width: 13, height: 13 }
                }
                button {
                    class: "absolute right-2 btn btn-ghost btn-xs gap-1 text-base-content/35 hover:text-base-content/70",
                    title: "Jump to anything",
                    onmousedown: move |e| e.stop_propagation(),
                    onclick: move |_| {
                        let now = *open.peek();
                        open.set(!now);
                    },
                    kbd { class: "kbd kbd-xs", "⌘" }
                    kbd { class: "kbd kbd-xs", "K" }
                }
            }
        }
    }
}

#[component]
fn FatalError(message: String) -> Element {
    rsx! {
        document::Stylesheet { href: TAILWIND }
        div { class: "h-screen flex items-center justify-center bg-base-100 text-base-content p-8",
            div { class: "max-w-md flex flex-col items-center space-y-3 text-center",
                div { class: "flex size-12 items-center justify-center rounded-full border border-error/30 bg-error/10 text-error",
                    Icon { icon: LdDatabaseZap, width: 22, height: 22 }
                }
                h1 { class: "text-lg font-semibold", "The local database could not be opened" }
                p { class: "text-xs font-mono text-base-content/60 selectable break-all", "{message}" }
                p { class: "text-sm text-base-content/60",
                    "Saved servers, history, and schema snapshots all live here, so the app cannot start without it."
                }
            }
        }
    }
}

/// A floating toast for messages that belong to no particular pane. Fixed
/// positioning keeps it out of the layout — three panes must not reflow
/// because a step was added to a collection. Confirmations dismiss themselves;
/// failures wait to be read, and confirmations must not borrow their styling —
/// red means something went wrong everywhere else in the app.
#[component]
fn Notice() -> Element {
    let app = use_context::<AppState>();
    let mut notice = app.notice;

    // The timer is armed via `tokio::spawn`: Dioxus polls `spawn` futures on
    // its own scheduler, where an inline `tokio::time` timer never fires (see
    // `off_scheduler` in state.rs).
    use_effect(move || {
        let Some(current) = notice.read().clone() else {
            return;
        };
        if current.error {
            return;
        }
        spawn(async move {
            let _ = tokio::spawn(tokio::time::sleep(std::time::Duration::from_secs(4))).await;
            if notice.peek().as_ref() == Some(&current) {
                notice.set(None);
            }
        });
    });

    let Some(message) = notice.read().clone() else {
        return rsx! {};
    };

    let wrap = if message.error {
        "flex items-center gap-2.5 rounded-box border border-error/40 bg-base-200 px-4 py-2.5 text-sm shadow-lg"
    } else {
        "flex items-center gap-2.5 rounded-box border border-base-300/70 bg-base-200 px-4 py-2.5 text-sm shadow-lg"
    };

    rsx! {
        div { class: "toast-in fixed bottom-4 right-4 z-50 max-w-md {wrap}",
            if message.error {
                Icon {
                    icon: LdTriangleAlert,
                    width: 14,
                    height: 14,
                    class: "text-error shrink-0",
                }
            } else {
                Icon {
                    icon: LdCircleCheck,
                    width: 14,
                    height: 14,
                    class: "text-base-content/50 shrink-0",
                }
            }
            span { class: "flex-1 selectable", "{message.text}" }
            button {
                class: "btn btn-ghost btn-xs",
                onclick: move |_| notice.set(None),
                "Dismiss"
            }
        }
    }
}
