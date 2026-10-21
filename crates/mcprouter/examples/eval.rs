//! Golden-set evaluation of tool selection.
//!
//! Answers the question the production ledger cannot: when the model called a
//! tool, was that the *right* tool? The ledger only knows what the router
//! returned and what was called next, so it cannot tell a good selection from a
//! model working around a bad one. Here the answer is labelled up front.
//!
//! Two arms, because the interesting comparison is not "is the router good" but
//! "is the model earning its place":
//!
//! * `bm25` — the lexical shortlist alone, no model, no API calls, free.
//! * `jev`  — the shipping router: one choice plus three gate nouls.
//!
//! If the arms tie on a catalog this size, the model is buying `p_none`, the
//! flat signal and the verdict, not ranking — worth knowing before tuning
//! anything.
//!
//! ```sh
//! cargo run -p mcprouter --example eval -- golden.json          # bm25 only
//! TYPESAFE_API_KEY=… cargo run -p mcprouter --example eval -- golden.json
//! ```
//!
//! The golden file is `{ catalog: [{server, name, key, description,
//! input_schema}], cases: [{request, expect: [key]}] }`. An empty `expect` is a
//! request the catalog cannot serve, or one that wants no tool at all: there the
//! score is whether the router *said so* rather than what it ranked.

use std::collections::HashSet;
use std::sync::Arc;

use mcprouter::bm25::Bm25;
use mcprouter::router::Verdict;
use mcprouter::{Entry, Jev, Router, RouterSettings};
use rmcp::model::Tool;
use serde::Deserialize;

/// Tools returned per request. The router's own default, so the arms are scored
/// on the same budget the gateway actually spends.
const K: usize = 5;

#[derive(Deserialize)]
struct Golden {
    catalog: Vec<CatalogTool>,
    cases: Vec<Case>,
}

#[derive(Deserialize)]
struct CatalogTool {
    server: String,
    name: String,
    key: String,
    description: Option<String>,
    input_schema: serde_json::Value,
}

#[derive(Deserialize)]
struct Case {
    request: String,
    expect: Vec<String>,
}

/// One case's outcome, in the terms the arms have in common.
struct Scored {
    /// Rank of the first expected tool in the full ranking, if it is there.
    first_rank: Option<usize>,
    /// How many of the expected tools the arm returned.
    found: usize,
    /// How many returned tools nobody asked for.
    noise: usize,
    /// Returned anything at all — the only thing that matters when nothing fits.
    returned: usize,
    /// The router said the list was not a straight answer. `None` for an arm
    /// that has no opinion to give.
    flagged: Option<bool>,
}

#[derive(Default)]
struct Totals {
    hit_at_1: usize,
    all_found: usize,
    recall: f64,
    noise: usize,
    ranked: usize,
    rank_sum: usize,
    rank_n: usize,
    flagged_when_nothing_fits: usize,
    returned_when_nothing_fits: usize,
}

fn score(
    ranking: &[String],
    returned: &[String],
    expect: &[String],
    flagged: Option<bool>,
) -> Scored {
    let wanted: HashSet<&str> = expect.iter().map(String::as_str).collect();
    Scored {
        first_rank: ranking.iter().position(|k| wanted.contains(k.as_str())),
        found: returned
            .iter()
            .filter(|k| wanted.contains(k.as_str()))
            .count(),
        noise: returned
            .iter()
            .filter(|k| !wanted.contains(k.as_str()))
            .count(),
        returned: returned.len(),
        flagged,
    }
}

fn accumulate(t: &mut Totals, case: &Case, s: &Scored, ranking: &[String]) {
    if case.expect.is_empty() {
        t.returned_when_nothing_fits += s.returned;
        if s.flagged == Some(true) {
            t.flagged_when_nothing_fits += 1;
        }
        return;
    }
    t.ranked += 1;
    t.noise += s.noise;
    t.recall += s.found as f64 / case.expect.len() as f64;
    if s.found == case.expect.len() {
        t.all_found += 1;
    }
    if ranking
        .first()
        .is_some_and(|k| case.expect.iter().any(|e| e == k))
    {
        t.hit_at_1 += 1;
    }
    if let Some(r) = s.first_rank {
        t.rank_sum += r + 1;
        t.rank_n += 1;
    }
}

fn report(name: &str, t: &Totals, no_fit_cases: usize) {
    let n = t.ranked.max(1) as f64;
    println!("\n{name}");
    println!(
        "  hit@1            {:>5.0}%  ({}/{})",
        100.0 * t.hit_at_1 as f64 / n,
        t.hit_at_1,
        t.ranked
    );
    println!("  recall@{K}         {:>5.0}%", 100.0 * t.recall / n);
    println!(
        "  every tool found {:>5.0}%  ({}/{})",
        100.0 * t.all_found as f64 / n,
        t.all_found,
        t.ranked
    );
    println!(
        "  mean rank        {:>5.2}   (of the first expected tool, 1-based; {} of {} found at all)",
        t.rank_sum as f64 / t.rank_n.max(1) as f64,
        t.rank_n,
        t.ranked
    );
    println!(
        "  noise            {:>5.2}   unwanted tools returned per request",
        t.noise as f64 / n
    );
    if no_fit_cases > 0 {
        println!(
            "  nothing fits     {:>5.0}%  flagged ({}/{}), {:.1} tools returned anyway",
            100.0 * t.flagged_when_nothing_fits as f64 / no_fit_cases as f64,
            t.flagged_when_nothing_fits,
            no_fit_cases,
            t.returned_when_nothing_fits as f64 / no_fit_cases as f64
        );
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args().nth(1).ok_or("usage: eval <golden.json>")?;
    let golden: Golden = serde_json::from_str(&std::fs::read_to_string(&path)?)?;
    let entries: Vec<Entry> = golden
        .catalog
        .iter()
        .map(|t| Entry {
            key: t.key.clone(),
            server: t.server.clone(),
            tool: Tool::new(
                t.name.clone(),
                t.description.clone().unwrap_or_default(),
                Arc::new(t.input_schema.as_object().cloned().unwrap_or_default()),
            ),
        })
        .collect();
    let no_fit_cases = golden.cases.iter().filter(|c| c.expect.is_empty()).count();
    println!(
        "{} tools, {} cases ({} of them with nothing to find)",
        entries.len(),
        golden.cases.len(),
        no_fit_cases
    );

    // --- bm25 -------------------------------------------------------------
    // The cumulative cutoff is the knob that decides how many tools a request
    // gets, so a sweep over it is the cheapest experiment there is.
    let mut settings = RouterSettings::default();
    if let Ok(c) = std::env::var("EVAL_CUMULATIVE") {
        settings.cumulative = c.parse()?;
        println!("cumulative = {}", settings.cumulative);
    }
    let onelines: Vec<String> = entries
        .iter()
        .map(|e| e.oneline(settings.max_description_chars))
        .collect();
    let index = Bm25::new(onelines.iter().map(String::as_str));
    let mut bm25 = Totals::default();
    for case in &golden.cases {
        let ranked: Vec<String> = index
            .top(&case.request, entries.len())
            .into_iter()
            .map(|i| entries[i].key.clone())
            .collect();
        let returned: Vec<String> = ranked.iter().take(K).cloned().collect();
        // Lexical ranking has no opinion about whether anything fits.
        let s = score(&ranked, &returned, &case.expect, None);
        accumulate(&mut bm25, case, &s, &returned);
    }
    report("bm25 (no model)", &bm25, no_fit_cases);

    // --- jev --------------------------------------------------------------
    let Ok(api_key) = std::env::var(mcprouter::jev::API_KEY_ENV) else {
        println!(
            "\n{} unset — skipping the model arm.",
            mcprouter::jev::API_KEY_ENV
        );
        return Ok(());
    };
    let mut jev = Jev::new(api_key, mcprouter::jev::DEFAULT_MODEL)?;
    if let Ok(base) = std::env::var(mcprouter::jev::BASE_URL_ENV) {
        jev = jev.with_base_url(base);
    }
    let router = Router::new(jev, settings.clone(), &entries);
    let mut model = Totals::default();
    let mut tokens = 0u64;
    let mut latency = Vec::new();
    for case in &golden.cases {
        let routing = router.route(&entries, &case.request, Some(K)).await?;
        tokens += routing.input_tokens;
        latency.push(routing.latency_ms);
        let ranked: Vec<String> = routing.candidates.iter().map(|c| c.key.clone()).collect();
        let returned: Vec<String> = routing
            .selected
            .iter()
            .map(|&i| routing.candidates[i].key.clone())
            .collect();
        let flagged = Some(routing.verdict != Verdict::Ranked);
        let s = score(&ranked, &returned, &case.expect, flagged);
        if !case.expect.is_empty() && s.found < case.expect.len() {
            println!(
                "  miss  {:<60} want {:?} got {:?} [{:?}]",
                case.request, case.expect, returned, routing.verdict
            );
        }
        accumulate(&mut model, case, &s, &returned);
    }
    report("jev (choice + gate nouls)", &model, no_fit_cases);
    latency.sort_unstable();
    println!(
        "  cost             {:>5} input tokens total, {} ms median latency",
        tokens,
        latency[latency.len() / 2]
    );
    Ok(())
}
