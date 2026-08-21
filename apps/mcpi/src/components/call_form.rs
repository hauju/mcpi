//! The generated argument form, and the raw JSON editor behind it.

use dioxus::prelude::*;
use dioxus_free_icons::Icon;
use dioxus_free_icons::icons::ld_icons::LdFlaskConical;
use serde_json::Value;

use crate::form::{self, Property, Widget};
use crate::state::{AppState, Selection, Tab};

#[component]
pub fn CallForm(selection: Selection) -> Element {
    let app = use_context::<AppState>();
    let mut raw = app.raw_form;

    let schema = app.schema_for(&selection);
    let properties = schema.as_ref().map(form::properties_of).unwrap_or_default();
    let running = *app.running.read();

    let verb = match selection.tab {
        Tab::Tools => "Call",
        Tab::Resources => "Read",
        Tab::Prompts => "Get",
    };

    rsx! {
        section { class: "border-t border-base-300/70",
            header { class: "flex items-center gap-2 px-4 py-2",
                h3 { class: "section-label flex-1", "Arguments" }
                if selection.tab != Tab::Resources {
                    button {
                        class: if *raw.read() { "btn btn-xs btn-active font-mono" } else { "btn btn-xs btn-ghost font-mono text-base-content/50 hover:text-base-content" },
                        title: "Edit the whole payload as JSON",
                        onclick: move |_| {
                            let current = *raw.read();
                            raw.set(!current);
                        },
                        "JSON"
                    }
                }
            }

            div { class: "px-4 pb-3 space-y-3.5",
                match selection.tab {
                    // A resource read takes only the URI, which is the selection.
                    Tab::Resources => rsx! {
                        p { class: "text-xs text-base-content/45", "This resource takes no arguments." }
                    },
                    _ if *raw.read() => rsx! {
                        RawEditor {}
                    },
                    _ if properties.is_empty() => rsx! {
                        p { class: "text-xs text-base-content/45", "This item takes no arguments." }
                    },
                    _ => rsx! {
                        for property in properties {
                            PropertyField {
                                key: "{property.name}",
                                path: vec![property.name.clone()],
                                property: property.clone(),
                            }
                        }
                    },
                }

                div { class: "flex gap-2",
                    button {
                        class: "btn btn-sm btn-primary flex-1",
                        disabled: running,
                        onclick: move |_| app.run(),
                        if running {
                            span { class: "loading loading-spinner loading-xs" }
                            "Running…"
                        } else {
                            "{verb}"
                        }
                    }
                    SaveToCollection {}
                }
            }
        }
    }
}

#[component]
fn PropertyField(path: Vec<String>, property: Property) -> Element {
    let app = use_context::<AppState>();
    let mut form = app.form;

    let current = form::get_path(&app.form.read(), &path).cloned();

    // One writer for every widget below, so "how does an edit reach the
    // payload" has exactly one answer.
    let write = {
        let path = path.clone();
        move |value: Value| form::set_path(&mut form.write(), &path, value)
    };

    rsx! {
        label { class: "block space-y-1",
            div { class: "flex items-baseline gap-1.5",
                // Required fields carry the weight; optional labels recede.
                // The asterisk stays neutral — red would read as a diff
                // verdict, and "must be filled" is not a severity.
                span {
                    class: if property.required { "text-xs font-mono text-base-content/90" } else { "text-xs font-mono text-base-content/55" },
                    "{property.name}"
                }
                if property.required {
                    span { class: "text-xs text-base-content/50", title: "Required", "*" }
                }
                if let Widget::Raw(reason) = &property.widget {
                    // A form limitation, not a contract change — so no amber.
                    span {
                        class: "badge badge-xs badge-ghost font-mono text-[10px] text-base-content/60",
                        title: "{reason}",
                        "raw"
                    }
                }
            }
            if let Some(description) = &property.description {
                p { class: "text-xs text-base-content/45 leading-snug", "{description}" }
            }

            // Objects are expanded here rather than inside `WidgetInput`
            // because only this scope knows the path to prefix onto each child.
            // Rendering them one level down without it would write every nested
            // field to the top of the payload.
            match &property.widget {
                Widget::Object(children) => rsx! {
                    div { class: "pl-3 border-l border-base-content/10 space-y-2.5 pt-1.5",
                        for child in children.clone() {
                            PropertyField {
                                key: "{child.name}",
                                path: child_path(&path, &child.name),
                                property: child,
                            }
                        }
                    }
                },
                _ => rsx! {
                    WidgetInput {
                        widget: property.widget.clone(),
                        current,
                        write: EventHandler::new(write),
                    }
                },
            }
        }
    }
}

/// Save the call as it currently stands into a collection.
///
/// The dropdown lists existing collections and offers a new one, so building a
/// smoke test never requires leaving the form you just filled in.
#[component]
fn SaveToCollection() -> Element {
    let app = use_context::<AppState>();
    let collections = app.collections;

    rsx! {
        div { class: "dropdown dropdown-top dropdown-end",
            div {
                class: "btn btn-sm gap-1.5",
                tabindex: 0,
                role: "button",
                title: "Save this call to a collection",
                Icon {
                    icon: LdFlaskConical,
                    width: 12,
                    height: 12,
                    class: "text-base-content/50",
                }
                "Save"
            }
            ul {
                class: "dropdown-content menu menu-sm bg-base-200 rounded-box z-10 w-52 p-1 shadow-lg border border-base-300",
                tabindex: 0,
                for collection in collections.read().iter() {
                    li { key: "{collection.id}",
                        a {
                            onclick: {
                                let id = collection.id;
                                move |_| app.add_current_to_collection(id)
                            },
                            "{collection.name}"
                        }
                    }
                }
                li {
                    a {
                        class: "opacity-70",
                        onclick: move |_| {
                            app.create_collection("");
                            // The new collection is the one just opened.
                            if let Some(id) = *app.open_collection.read() {
                                app.add_current_to_collection(id);
                            }
                        },
                        "New collection"
                    }
                }
            }
        }
    }
}

fn child_path(parent: &[String], name: &str) -> Vec<String> {
    let mut path = parent.to_vec();
    path.push(name.to_string());
    path
}

#[component]
fn WidgetInput(widget: Widget, current: Option<Value>, write: EventHandler<Value>) -> Element {
    match widget {
        Widget::Text => {
            let value = current
                .as_ref()
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            rsx! {
                input {
                    class: "input input-sm w-full font-mono",
                    value: "{value}",
                    oninput: move |e| write.call(Value::String(e.value())),
                }
            }
        }
        Widget::Number { integer } => {
            let value = current
                .as_ref()
                .map(|v| match v {
                    Value::String(s) => s.clone(),
                    other => other.to_string(),
                })
                .unwrap_or_default();
            rsx! {
                input {
                    class: "input input-sm w-full font-mono",
                    r#type: "text",
                    inputmode: if integer { "numeric" } else { "decimal" },
                    value: "{value}",
                    oninput: move |e| write.call(form::parse_scalar(&e.value(), &Widget::Number { integer })),
                }
            }
        }
        Widget::Bool => {
            let checked = current.as_ref().and_then(Value::as_bool).unwrap_or(false);
            rsx! {
                input {
                    class: "toggle toggle-sm",
                    r#type: "checkbox",
                    checked,
                    onchange: move |e| write.call(Value::Bool(e.checked())),
                }
            }
        }
        Widget::Choice(values) => {
            let selected = current.as_ref().map(scalar_text).unwrap_or_default();
            let options = values.clone();
            rsx! {
                select {
                    class: "select select-sm w-full font-mono",
                    onchange: move |e| {
                        let picked = e.value();
                        // Send back the original typed value, not its rendering,
                        // so a numeric enum stays numeric.
                        let value = options
                            .iter()
                            .find(|v| scalar_text(v) == picked)
                            .cloned()
                            .unwrap_or(Value::String(picked));
                        write.call(value);
                    },
                    for option in values.iter() {
                        option {
                            key: "{scalar_text(option)}",
                            value: "{scalar_text(option)}",
                            selected: scalar_text(option) == selected,
                            "{scalar_text(option)}"
                        }
                    }
                }
            }
        }
        Widget::Lines { item } => {
            let text = form::unparse_lines(current.as_ref());
            rsx! {
                textarea {
                    class: "textarea textarea-sm w-full font-mono",
                    rows: 3,
                    placeholder: "One per line",
                    value: "{text}",
                    oninput: move |e| write.call(form::parse_lines(&e.value(), &item)),
                }
            }
        }
        // `PropertyField` expands objects structurally, so this arm is a
        // fallback rather than the normal path: treat it like any other schema
        // the form cannot lay out.
        Widget::Object(_) | Widget::Raw(_) => {
            let text = current
                .as_ref()
                .map(|v| serde_json::to_string(v).unwrap_or_default())
                .unwrap_or_default();
            rsx! {
                textarea {
                    class: "textarea textarea-sm w-full font-mono",
                    rows: 2,
                    placeholder: "JSON",
                    value: "{text}",
                    oninput: move |e| {
                        let raw = e.value();
                        // An empty box means "leave the field out" rather than
                        // "send null".
                        if raw.trim().is_empty() {
                            write.call(Value::Null);
                        } else if let Ok(parsed) = serde_json::from_str::<Value>(&raw) {
                            write.call(parsed);
                        }
                    },
                }
            }
        }
    }
}

/// The whole payload as JSON.
///
/// The buffer is local so a half-typed edit is not destroyed on every
/// keystroke; it re-syncs only when the payload moves underneath it, which is
/// what happens when a call is loaded from history.
#[component]
fn RawEditor() -> Element {
    let app = use_context::<AppState>();
    let mut form = app.form;
    let mut text = use_signal(|| pretty(&app.form.read()));
    let mut error = use_signal(|| Option::<String>::None);

    use_effect(move || {
        let current = form.read().clone();
        let buffer_describes_it =
            serde_json::from_str::<Value>(&text.peek()).ok().as_ref() == Some(&current);
        if !buffer_describes_it {
            text.set(pretty(&current));
            error.set(None);
        }
    });

    rsx! {
        div { class: "space-y-1",
            textarea {
                class: "textarea textarea-sm w-full font-mono",
                rows: 8,
                value: "{text}",
                oninput: move |e| {
                    let typed = e.value();
                    match serde_json::from_str::<Value>(&typed) {
                        Ok(parsed) => {
                            error.set(None);
                            form.set(parsed);
                        }
                        // Keep the text; only refuse to publish it.
                        Err(e) => error.set(Some(e.to_string())),
                    }
                    text.set(typed);
                },
            }
            if let Some(message) = error.read().as_ref() {
                p { class: "text-xs text-error font-mono", "{message}" }
            }
        }
    }
}

fn pretty(value: &Value) -> String {
    serde_json::to_string_pretty(value).unwrap_or_else(|_| "{}".into())
}

/// How a scalar reads in a `<select>`: strings bare, everything else as JSON.
fn scalar_text(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}
