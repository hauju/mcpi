//! Tool selection: one Jev `choice` over the candidates plus three gate nouls, then a
//! deterministic selection rule (cumulative probability, `k` bounds, flat-distribution fallback).

use std::collections::BTreeMap;
use std::time::Instant;

use crate::Entry;
use crate::bm25::Bm25;
use crate::jev::{Choice, Jev, Noul, Question};

const NONE_OPTION: &str = "none";
/// In flat mode, candidates below this probability are not returned (never fewer than one).
const MIN_FLAT_P: f64 = 0.001;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("no upstream tools to route over")]
    NoCandidates,
    #[error(transparent)]
    Jev(#[from] crate::jev::Error),
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RouterSettings {
    /// Default number of tools returned by `find_tools` when the caller passes no `k`.
    pub k_default: usize,
    /// Hard cap on `k`.
    pub k_max: usize,
    /// Tools are added in probability order until their cumulative probability reaches this.
    pub cumulative: f64,
    /// Below this max probability the distribution counts as flat and top-k is returned anyway.
    pub flat_threshold: f64,
    /// BM25 shortlist size sent to Jev; `0` sends every tool.
    pub shortlist: usize,
    /// Requests longer than this are truncated before routing (characters).
    pub max_request_chars: usize,
    /// Tool descriptions are truncated to this many characters in the routing prompt.
    pub max_description_chars: usize,
    /// Gateway tool names that are also listed directly, so clients can call them without a
    /// `find_tools` round trip. `mcpi-cli stats` recommends candidates.
    pub expose_direct: Vec<String>,
}

impl Default for RouterSettings {
    fn default() -> Self {
        Self {
            k_default: 5,
            k_max: 10,
            cumulative: 0.5,
            flat_threshold: 0.3,
            shortlist: 0,
            max_request_chars: 8000,
            max_description_chars: 200,
            expose_direct: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Scored {
    pub key: String,
    pub p: f64,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Routing {
    /// Every candidate sent to Jev, sorted by descending probability.
    pub candidates: Vec<Scored>,
    /// Indices into `candidates` that `find_tools` returns.
    pub selected: Vec<usize>,
    /// Max tool probability was below the flat threshold (or `none` won): top-k returned anyway.
    pub flat: bool,
    pub p_none: f64,
    /// Mean of the "does this request need a tool at all" nouls.
    pub gate: f64,
    pub input_tokens: u64,
    pub latency_ms: u64,
}

pub struct Router {
    jev: Jev,
    settings: RouterSettings,
    bm25: Option<Bm25>,
    onelines: Vec<String>,
}

impl Router {
    pub fn new(jev: Jev, settings: RouterSettings, entries: &[Entry]) -> Self {
        let onelines: Vec<String> = entries
            .iter()
            .map(|e| e.oneline(settings.max_description_chars))
            .collect();
        let bm25 = (settings.shortlist > 0 && entries.len() > settings.shortlist)
            .then(|| Bm25::new(onelines.iter().map(String::as_str)));
        Self {
            jev,
            settings,
            bm25,
            onelines,
        }
    }

    pub fn settings(&self) -> &RouterSettings {
        &self.settings
    }

    /// Rank `entries` (the same slice `new` was built from) for `request`.
    pub async fn route(
        &self,
        entries: &[Entry],
        request: &str,
        k: Option<usize>,
    ) -> Result<Routing, Error> {
        let k = k
            .unwrap_or(self.settings.k_default)
            .clamp(1, self.settings.k_max);
        let request = truncate_chars(request, self.settings.max_request_chars);
        let candidates: Vec<usize> = match &self.bm25 {
            Some(index) => index.top(&request, self.settings.shortlist),
            None => (0..entries.len()).collect(),
        };
        if candidates.is_empty() {
            return Err(Error::NoCandidates);
        }

        let mut criteria: BTreeMap<String, String> = candidates
            .iter()
            .map(|&i| (entries[i].key.clone(), self.onelines[i].clone()))
            .collect();
        criteria.insert(
            NONE_OPTION.into(),
            "No tool is needed; the request can be answered directly.".into(),
        );
        let questions: BTreeMap<String, Question> = [
            (
                "pick",
                Question::Choice(Choice {
                    instructions: "Which of these tools, if any, is the right one to call to fulfil the user's request in `request`?".into(),
                    criteria,
                }),
            ),
            (
                "acts",
                Question::Noul(Noul {
                    instructions: "Does `request` ask the assistant to act on the user's systems, accounts or data, rather than merely explain something?".into(),
                }),
            ),
            (
                "lookup",
                Question::Noul(Noul {
                    instructions: "Would fulfilling `request` require looking up live data or calling an external service, rather than relying on general knowledge?".into(),
                }),
            ),
            (
                "prose",
                Question::Noul(Noul {
                    instructions: "Can `request` be answered with prose alone, with no tool call and no external data?".into(),
                }),
            ),
        ]
        .into_iter()
        .map(|(k, q)| (k.to_string(), q))
        .collect();

        let started = Instant::now();
        let answers = self
            .jev
            .system_one(&serde_json::json!({ "request": request }), &questions)
            .await?;
        let latency_ms = started.elapsed().as_millis() as u64;

        let pick = answers
            .choices
            .get("pick")
            .ok_or_else(|| crate::jev::Error::MissingAnswer("pick".into()))?;
        let noul = |name: &str| answers.nouls.get(name).copied().unwrap_or(0.5);
        let gate = (noul("acts") + noul("lookup") + (1.0 - noul("prose"))) / 3.0;
        let p_none = pick.probabilities.get(NONE_OPTION).copied().unwrap_or(0.0);

        let mut scored: Vec<Scored> = candidates
            .iter()
            .map(|&i| Scored {
                key: entries[i].key.clone(),
                p: pick
                    .probabilities
                    .get(&entries[i].key)
                    .copied()
                    .unwrap_or(0.0),
            })
            .collect();
        scored.sort_by(|a, b| b.p.total_cmp(&a.p).then_with(|| a.key.cmp(&b.key)));

        let (selected, flat) = select(&scored, p_none, k, &self.settings);
        Ok(Routing {
            candidates: scored,
            selected,
            flat,
            p_none,
            gate,
            input_tokens: answers.input_tokens,
            latency_ms,
        })
    }
}

/// Deterministic selection over a descending-sorted distribution.
/// Adds tools until cumulative probability reaches `cumulative`, capped at `k`, never empty.
/// If the top tool is below `flat_threshold` or `none` beats it, the distribution is flat and
/// the top `k` (minus zero-mass padding) are returned instead.
fn select(sorted: &[Scored], p_none: f64, k: usize, s: &RouterSettings) -> (Vec<usize>, bool) {
    let top = sorted.first().map_or(0.0, |x| x.p);
    let flat = top < s.flat_threshold || p_none >= top;
    if flat {
        let n = sorted
            .iter()
            .take(k)
            .filter(|x| x.p >= MIN_FLAT_P)
            .count()
            .max(1)
            .min(sorted.len());
        return ((0..n).collect(), true);
    }
    let mut out = Vec::new();
    let mut cum = 0.0;
    for (i, x) in sorted.iter().enumerate().take(k) {
        out.push(i);
        cum += x.p;
        if cum >= s.cumulative {
            break;
        }
    }
    (out, false)
}

fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        s.chars().take(max).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(keys: &[(&str, f64)]) -> Vec<Scored> {
        keys.iter()
            .map(|(k, p)| Scored {
                key: k.to_string(),
                p: *p,
            })
            .collect()
    }

    #[test]
    fn cumulative_rule_stops_at_half() {
        let st = RouterSettings::default();
        let sorted = s(&[("a", 0.4), ("b", 0.2), ("c", 0.1)]);
        assert_eq!(select(&sorted, 0.05, 5, &st), (vec![0, 1], false));
    }

    #[test]
    fn confident_top_returns_one() {
        let st = RouterSettings::default();
        assert_eq!(
            select(&s(&[("a", 0.9), ("b", 0.05)]), 0.01, 5, &st),
            (vec![0], false)
        );
    }

    #[test]
    fn k_caps_selection() {
        let st = RouterSettings::default();
        let sorted = s(&[("a", 0.31), ("b", 0.1), ("c", 0.05), ("d", 0.04)]);
        assert_eq!(select(&sorted, 0.0, 2, &st), (vec![0, 1], false));
    }

    #[test]
    fn flat_returns_top_k_without_zero_padding() {
        let st = RouterSettings::default();
        let sorted = s(&[("a", 0.2), ("b", 0.15), ("c", 0.1), ("d", 0.05)]);
        assert_eq!(select(&sorted, 0.1, 3, &st), (vec![0, 1, 2], true));
        assert_eq!(
            select(&s(&[("a", 0.4), ("b", 0.1)]), 0.45, 5, &st),
            (vec![0, 1], true)
        );
        assert_eq!(
            select(&s(&[("a", 0.25), ("b", 0.0), ("c", 0.0)]), 0.7, 5, &st),
            (vec![0], true)
        );
    }
}
