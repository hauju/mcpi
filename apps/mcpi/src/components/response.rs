//! What came back from the last call.
//!
//! Rendering is driven off the JSON rather than off typed MCP results, because
//! the same component has to show a tool result, a resource read, a prompt, and
//! a stored history row — all of which reach it as `Value`.

use dioxus::prelude::*;
use dioxus_free_icons::Icon;
use dioxus_free_icons::icons::ld_icons::LdChevronRight;
use serde_json::Value;

use crate::components::json_tree::JsonView;
use crate::state::{AppState, CallOutcome};

#[component]
pub fn Response() -> Element {
    let app = use_context::<AppState>();
    let Some(outcome) = app.outcome.read().clone() else {
        return rsx! {};
    };

    rsx! {
        section { class: "border-t border-base-300/70",
            header { class: "flex items-center gap-2 px-4 py-2",
                h3 { class: "section-label flex-1", "Response" }
                // Failure is solid, success is soft: green should confirm,
                // not compete with a red that means something broke.
                if outcome.is_error {
                    span { class: "badge badge-xs badge-error", "error" }
                } else {
                    span { class: "badge badge-xs badge-soft badge-success", "ok" }
                }
                span { class: "text-[10px] font-mono text-base-content/40", "{outcome.duration_ms}ms" }
            }
            div { class: "px-4 pb-4 space-y-3",
                Body { outcome: outcome.clone() }
                details { class: "group",
                    summary { class: "flex items-center gap-1 cursor-default select-none",
                        Icon {
                            icon: LdChevronRight,
                            width: 11,
                            height: 11,
                            class: "text-base-content/35 transition-transform group-open:rotate-90",
                        }
                        span { class: "section-label hover:text-base-content/70", "Raw response" }
                    }
                    pre { class: "code-block mt-2", "{pretty(&outcome.response)}" }
                }
            }
        }
    }
}

#[component]
fn Body(outcome: CallOutcome) -> Element {
    let response = &outcome.response;

    // A transport-level failure is stored as a bare `{ "error": ... }`, which
    // has no content blocks to render.
    if let Some(error) = response.get("error").and_then(Value::as_str) {
        return rsx! {
            p { class: "text-sm text-error font-mono whitespace-pre-wrap selectable", "{error}" }
        };
    }

    // Tool results carry `content`; resource reads carry `contents`; prompts
    // carry `messages`, each wrapping a block under `content`.
    let blocks: Vec<Value> = if let Some(content) = array(response, "content") {
        content
    } else if let Some(contents) = array(response, "contents") {
        contents
    } else if let Some(messages) = array(response, "messages") {
        messages
            .iter()
            .filter_map(|m| m.get("content").cloned())
            .collect()
    } else {
        Vec::new()
    };

    rsx! {
        if blocks.is_empty() {
            p { class: "text-xs text-base-content/45", "No content returned." }
        }
        for (index, block) in blocks.into_iter().enumerate() {
            Block { key: "{index}", block }
        }
        if let Some(structured) = response.get("structuredContent") {
            section { class: "space-y-1.5",
                h4 { class: "section-label", "Structured content" }
                JsonView { value: structured.clone() }
            }
        }
    }
}

#[component]
fn Block(block: Value) -> Element {
    // Text first: it is what almost every server returns. Most of it is JSON
    // stringified into the block, which gets the tree; anything that does not
    // parse is prose and reads as prose.
    if let Some(text) = block.get("text").and_then(Value::as_str) {
        if let Ok(parsed) = serde_json::from_str::<Value>(text)
            && (parsed.is_object() || parsed.is_array())
        {
            return rsx! {
                JsonView { value: parsed }
            };
        }
        return rsx! {
            pre { class: "code-block whitespace-pre-wrap", "{text}" }
        };
    }

    let mime = block.get("mimeType").and_then(Value::as_str).unwrap_or("");
    if let Some(data) = block.get("data").and_then(Value::as_str) {
        if mime.starts_with("image/") {
            return rsx! {
                img {
                    class: "max-w-full rounded-field border border-base-300",
                    src: "data:{mime};base64,{data}",
                    alt: "Returned image",
                }
            };
        }
        return rsx! {
            p { class: "text-xs text-base-content/55 font-mono", "{mime} · {data.len()} base64 chars" }
        };
    }

    if let Some(blob) = block.get("blob").and_then(Value::as_str) {
        return rsx! {
            p { class: "text-xs text-base-content/55 font-mono", "binary · {blob.len()} base64 chars" }
        };
    }

    // An embedded resource wraps its payload one level down.
    if let Some(resource) = block.get("resource") {
        return rsx! {
            Block { block: resource.clone() }
        };
    }

    rsx! {
        JsonView { value: block }
    }
}

fn array(value: &Value, key: &str) -> Option<Vec<Value>> {
    value.get(key).and_then(Value::as_array).cloned()
}

fn pretty(value: &Value) -> String {
    serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string())
}
