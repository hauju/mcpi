//! The endpoint diagnosis, shown inside the server dialog.
//!
//! Placed here rather than behind a separate screen because the moment you are
//! pasting a URL is the moment you are about to get it wrong — and the single
//! most common mistake, reaching for the documentation page instead of the
//! endpoint, is fixable in one click from the suggestion this produces.

use dioxus::prelude::*;
use dioxus_free_icons::Icon;
use dioxus_free_icons::icons::ld_icons::{LdCircleAlert, LdCircleCheck, LdInfo, LdTriangleAlert};
use probe::{EndpointKind, Severity};

use crate::state::AppState;

#[component]
pub fn EndpointCheck() -> Element {
    let app = use_context::<AppState>();
    let probing = *app.probing.read();
    let report = app.endpoint_report.read().clone();

    rsx! {
        div { class: "space-y-2",
            button {
                class: "btn btn-sm w-full",
                disabled: probing,
                onclick: move |_| app.check_endpoint(),
                if probing {
                    span { class: "loading loading-spinner loading-xs" }
                    "Checking…"
                } else {
                    "Check this endpoint"
                }
            }

            if let Some(report) = report {
                Verdict { report }
            }
        }
    }
}

#[component]
fn Verdict(report: probe::Report) -> Element {
    let app = use_context::<AppState>();

    let candidates = match &report.kind {
        Some(EndpointKind::WebPage { candidates }) => candidates.clone(),
        _ => Vec::new(),
    };

    rsx! {
        div { class: "rounded-field border border-base-300 bg-base-200/40 divide-y divide-base-300",

            // The identity line: what this actually is, before any judgement.
            if let Some(server) = &report.server {
                div { class: "px-3 py-2 flex items-baseline gap-2",
                    span { class: "font-mono text-[13px]", "{server.name}" }
                    span { class: "text-xs text-base-content/50", "{server.version}" }
                    if let Some(version) = &report.protocol_version {
                        span { class: "ml-auto font-mono text-[10px] text-base-content/40",
                            "MCP {version}"
                        }
                    }
                }
            }

            if report.findings.is_empty() {
                AllClear {}
            }
            for (index, finding) in report.findings.iter().enumerate() {
                FindingRow { key: "{index}", finding: finding.clone() }
            }

            // A suggestion is the only actionable output here, so it gets a
            // control rather than a sentence to copy out of.
            if !candidates.is_empty() {
                div { class: "px-3 py-2 space-y-1",
                    p { class: "section-label", "Try instead" }
                    for candidate in candidates {
                        button {
                            key: "{candidate}",
                            class: "block w-full text-left font-mono text-xs text-primary hover:underline truncate",
                            onclick: {
                                let candidate = candidate.clone();
                                move |_| app.use_suggested_url(&candidate)
                            },
                            "{candidate}"
                        }
                    }
                }
            }

            if !report.public_tools.is_empty() {
                div { class: "px-3 py-2",
                    p { class: "section-label mb-1", "Callable without signing in" }
                    p { class: "font-mono text-xs text-base-content/60 break-all",
                        "{report.public_tools.join(\", \")}"
                    }
                }
            }
        }
    }
}

#[component]
fn FindingRow(finding: probe::Finding) -> Element {
    // Severity keeps the same colours it has everywhere else in the app, so a
    // blocker here reads the same as a breaking change in the diff.
    let (tone, icon) = match finding.severity {
        Severity::Blocker => (
            "text-error",
            rsx! { Icon { icon: LdCircleAlert, width: 13, height: 13 } },
        ),
        Severity::Warning => (
            "text-warning",
            rsx! { Icon { icon: LdTriangleAlert, width: 13, height: 13 } },
        ),
        Severity::Note => (
            "text-info/70",
            rsx! { Icon { icon: LdInfo, width: 13, height: 13 } },
        ),
    };

    rsx! {
        div { class: "px-3 py-2 flex gap-2",
            span { class: "{tone} shrink-0 mt-0.5", {icon} }
            div { class: "min-w-0 space-y-0.5",
                p { class: "text-[13px] leading-snug", "{finding.title}" }
                p { class: "text-xs text-base-content/55 leading-snug", "{finding.detail}" }
            }
        }
    }
}

/// A single line for the healthy case, so "nothing is wrong" is still an answer.
#[component]
fn AllClear() -> Element {
    rsx! {
        div { class: "px-3 py-2 flex items-center gap-2 text-success",
            Icon { icon: LdCircleCheck, width: 13, height: 13 }
            span { class: "text-[13px]", "Reachable, and nothing stands in the way." }
        }
    }
}
