//! What the connected server's own contract says about the spec.
//!
//! Renders `mcplint` findings, which the CLI has always printed and the app
//! never showed — the app being the surface people actually look at. The
//! crate's rule carries through unchanged: every finding is a fact with a
//! citation, and there is no score, grade, or total. A count would be a grade
//! wearing a number, and these facts are about other people's servers.

use dioxus::prelude::*;
use dioxus_free_icons::Icon;
use dioxus_free_icons::icons::ld_icons::LdBookOpen;
use mcplint::{Finding, Level};

use crate::state::AppState;

/// Findings about one tool, or — with `tool: None` — about the server itself.
///
/// `bare` drops the section's own border and padding, for the one caller that
/// already sits inside a padded column; the detail pane's other sections each
/// carry their own chrome, so that stays the default.
///
/// Linting is memoised on the snapshot rather than done per render: a server
/// with ninety tools is linted once per contract, not once per keystroke in
/// the filter box.
#[component]
pub fn Conformance(tool: Option<String>, #[props(default)] bare: bool) -> Element {
    let app = use_context::<AppState>();

    let findings = use_memo(move || {
        app.active()
            .map(|connected| mcplint::lint(&connected.snapshot))
            .unwrap_or_default()
    });

    let findings = findings.read();
    let mine: Vec<&Finding> = findings.iter().filter(|f| f.tool == tool).collect();
    if mine.is_empty() {
        return rsx! {};
    }

    let shell = if bare {
        "space-y-2"
    } else {
        "border-t border-base-300/70 px-4 py-3 space-y-2"
    };

    rsx! {
        section { class: "{shell}",
            div { class: "flex items-center gap-1.5",
                h3 { class: "section-label", "Spec conformance" }
                span { class: "text-[10px] text-base-content/35", "MCP {mcplint::SPEC}" }
            }
            ul { class: "space-y-1.5",
                for finding in mine {
                    li { key: "{finding.rule}", class: "flex items-start gap-2 text-xs",
                        // Level says how hard the spec word is — MUST or
                        // SHOULD — and nothing about how bad the server is,
                        // so it stays a text weight rather than a colour.
                        // Red and amber mean the contract moved.
                        span {
                            class: if finding.level == Level::Warning {
                                "shrink-0 mt-px font-semibold text-base-content/70 tabular-nums"
                            } else {
                                "shrink-0 mt-px text-base-content/35 tabular-nums"
                            },
                            title: if finding.level == Level::Warning {
                                "The spec says MUST"
                            } else {
                                "The spec says SHOULD"
                            },
                            if finding.level == Level::Warning { "MUST" } else { "SHOULD" }
                        }
                        div { class: "min-w-0",
                            p { class: "text-base-content/75 selectable", "{finding.fact}" }
                            p { class: "flex items-center gap-1 text-[10px] text-base-content/35",
                                Icon { icon: LdBookOpen, width: 10, height: 10 }
                                "{finding.cite} · {finding.rule}"
                            }
                        }
                    }
                }
            }
        }
    }
}
