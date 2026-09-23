//! Tool selection: one Jev `choice` over the candidates plus three gate nouls, then a
//! deterministic selection rule (cumulative probability, `k` bounds, flat-distribution fallback).

use std::collections::BTreeMap;
use std::time::Instant;

use crate::Entry;
use crate::bm25::Bm25;
use crate::jev::{Choice, Jev, Noul, Question};

const NONE_OPTION: &str = "none";
/// Below this mean the three gate nouls say the request wants no tool at all.
/// The nouls are a mean of three 0-1 judgments, so 0.5 is the neutral point.
const GATE_FLOOR: f64 = 0.5;
/// A runner-up scoring at least this fraction of the winner makes the two
/// comparable, which is what "scored alike" has to mean: a ratio against the
/// field, never an absolute floor. A winner ten times its nearest rival is a
/// clear pick however small its probability, because a crowded catalog divides
/// the mass among everything it contains.
const UNCERTAIN_MARGIN: f64 = 0.5;
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
    /// Below this max probability the distribution counts as flat and top-k is returned anyway —
    /// unless the top tool clearly leads its runner-up, which a crowded catalog makes common.
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

/// What the router concluded beyond the ranking itself.
///
/// Deliberately missing: "nothing in the catalog fits". A choice distribution
/// is relative — the probabilities sum to one, so with `n` candidates something
/// always scores at least `1/n`, and a low top means the field is crowded or
/// the tools tie, never that none of them work. Reading a weak spread as "no
/// match" would claim what the answer cannot say: four equally apt tools at
/// 0.24 with `none` at 0.04 is the model insisting a tool *is* wanted. That
/// verdict needs a question of its own — one noul asking whether any listed
/// tool serves the request — which is a fixed extra cost, not one per tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    /// The request can be answered without calling anything — what the gate
    /// nouls measure, and what `none` winning the choice means.
    NoToolNeeded,
    /// Nothing leads: either the field scored alike or none of it scored high.
    /// The list is a set of candidates, not a ranking to trust. Which of the two
    /// produced it is not worth asserting — `flat` and the per-tool `p` are in
    /// the same payload for a caller that wants to look.
    Uncertain,
    /// The ranking means what it says.
    Ranked,
}

impl Verdict {
    /// One line for the model reading the result. `None` when the ranking stands
    /// on its own and needs no explaining.
    pub fn note(self) -> Option<&'static str> {
        match self {
            Verdict::NoToolNeeded => Some(
                "This request looks answerable without a tool call; the tools below are only the \
                 closest matches.",
            ),
            Verdict::Uncertain => Some(
                "No tool stands out here — read the schemas and pick on the merits, or ask again \
                 more specifically. This does not mean none of them fit.",
            ),
            Verdict::Ranked => None,
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
    /// No tool clearly led (or `none` won): top-k returned anyway.
    pub flat: bool,
    pub p_none: f64,
    /// Mean of the "does this request need a tool at all" nouls.
    pub gate: f64,
    /// What the gate and the distribution together say about the list.
    pub verdict: Verdict,
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
        // `onelines` and the BM25 index are positional over the construction slice; a
        // different slice would route over the wrong descriptions without failing.
        assert_eq!(
            entries.len(),
            self.onelines.len(),
            "Router::route called with a different catalog than Router::new"
        );
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
        // An absent key reads as 0, so a response keyed differently than the criteria
        // would silently collapse the ranking to flat. Say so when most are missing.
        let missing = candidates
            .iter()
            .filter(|&&i| !pick.probabilities.contains_key(&entries[i].key))
            .count();
        if missing * 2 > candidates.len() {
            tracing::warn!(
                missing,
                candidates = candidates.len(),
                "Jev returned no probability for most candidates"
            );
        }
        scored.sort_by(|a, b| b.p.total_cmp(&a.p).then_with(|| a.key.cmp(&b.key)));

        let (selected, flat) = select(&scored, p_none, k, &self.settings);
        let top = scored.first().map_or(0.0, |x| x.p);
        let runner_up = scored.get(1).map_or(0.0, |x| x.p);
        let verdict = verdict(flat, top, runner_up, p_none, gate);
        Ok(Routing {
            candidates: scored,
            selected,
            flat,
            p_none,
            gate,
            verdict,
            input_tokens: answers.input_tokens,
            latency_ms,
        })
    }
}

/// Read the gate and the distribution together.
///
/// Takes `select`'s own `flat` rather than recomputing a variant of it, so the
/// two fields in one payload cannot contradict each other: a flat ranking is
/// never `Ranked`, and `Ranked` always means a clear winner.
///
/// The gate answers "does this need a tool at all". It decides only where the
/// choice has no strong opinion of its own — three generic nouls must not
/// overrule a distribution built over the actual catalog, which would steer a
/// model away from a tool the router rated 0.85. See [`Verdict`] for the
/// verdict this deliberately cannot reach.
fn verdict(flat: bool, top: f64, runner_up: f64, p_none: f64, gate: f64) -> Verdict {
    // Strictly greater: a tie is the model having no opinion, which `flat`
    // already carries, not a positive claim that no tool is wanted.
    if p_none > top || (gate < GATE_FLOOR && flat) {
        return Verdict::NoToolNeeded;
    }
    if flat || runner_up >= top * UNCERTAIN_MARGIN {
        return Verdict::Uncertain;
    }
    Verdict::Ranked
}

/// Deterministic selection over a descending-sorted distribution.
/// Adds tools until cumulative probability reaches `cumulative`, capped at `k`, never empty.
/// If `none` beats the top tool, or the top tool is below `flat_threshold` without clearly
/// leading its runner-up (see [`UNCERTAIN_MARGIN`]), the distribution is flat and the top `k`
/// (minus zero-mass padding) are returned instead.
fn select(sorted: &[Scored], p_none: f64, k: usize, s: &RouterSettings) -> (Vec<usize>, bool) {
    let top = sorted.first().map_or(0.0, |x| x.p);
    let runner_up = sorted.get(1).map_or(0.0, |x| x.p);
    let flat = p_none >= top || (top < s.flat_threshold && runner_up >= top * UNCERTAIN_MARGIN);
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

    /// A flat ranking is read against the gate: the nouls decide only where the
    /// choice itself has no opinion.
    #[test]
    fn a_weak_spread_is_read_against_the_gate() {
        assert_eq!(verdict(true, 0.2, 0.15, 0.05, 0.9), Verdict::Uncertain);
        assert_eq!(verdict(true, 0.2, 0.15, 0.05, 0.2), Verdict::NoToolNeeded);
    }

    /// Four equally apt tools split the vote; `none` at 0.04 says a tool *is*
    /// wanted. The verdict may not read that as a catalog that cannot serve the
    /// request, and the note may not imply one.
    #[test]
    fn tools_that_tie_are_uncertain_never_a_verdict_on_the_catalog() {
        assert_eq!(verdict(true, 0.24, 0.24, 0.04, 0.9), Verdict::Uncertain);
        assert!(
            Verdict::Uncertain
                .note()
                .is_some_and(|n| n.contains("does not mean none of them fit")),
            "the note must not imply a missing capability"
        );
    }

    /// A crowded catalog divides the probability mass, so the winner's absolute
    /// value says nothing on its own. Ten times the runner-up is a clear pick at
    /// 0.29 exactly as it would be at 0.9.
    #[test]
    fn a_clear_winner_stays_clear_however_crowded_the_field() {
        assert_eq!(verdict(false, 0.29, 0.03, 0.02, 0.9), Verdict::Ranked);
        // Halve the margin and the two become comparable.
        assert_eq!(verdict(false, 0.29, 0.15, 0.02, 0.9), Verdict::Uncertain);
    }

    /// The gate is generic; the choice is built over the real catalog. A
    /// confident pick must survive nouls that read the request as conversational.
    #[test]
    fn the_gate_does_not_overrule_a_confident_choice() {
        assert_eq!(verdict(false, 0.85, 0.05, 0.01, 0.433), Verdict::Ranked);
    }

    /// `flat` and `verdict` come out of the same numbers and must never
    /// disagree: an unreliable ranking cannot also be one that means what it says.
    #[test]
    fn a_flat_ranking_is_never_ranked() {
        for (top, runner_up, p_none, gate) in [
            (0.40, 0.05, 0.40, 0.9),
            (0.29, 0.03, 0.02, 0.9),
            (0.20, 0.19, 0.01, 0.9),
        ] {
            assert_ne!(
                verdict(true, top, runner_up, p_none, gate),
                Verdict::Ranked,
                "flat ranking reported as trustworthy: {top} / {runner_up}"
            );
        }
    }

    #[test]
    fn none_winning_the_choice_reads_as_no_tool_needed() {
        // Even a confident top tool loses to a `none` the model rates higher.
        assert_eq!(verdict(false, 0.4, 0.1, 0.45, 0.9), Verdict::NoToolNeeded);
        assert_eq!(verdict(false, 0.4, 0.1, 0.05, 0.9), Verdict::Ranked);
    }

    /// Every verdict but `Ranked` has to say something, or the field tells the
    /// model nothing it can act on.
    #[test]
    fn only_a_plain_ranking_goes_unexplained() {
        assert!(Verdict::Ranked.note().is_none());
        assert!(Verdict::Uncertain.note().is_some());
        assert!(Verdict::NoToolNeeded.note().is_some());
    }

    /// `select` and `verdict` together: a winner ten times its runner-up in a
    /// crowded catalog is not flat, so it reaches `Ranked` as the margin promises.
    #[test]
    fn a_crowded_clear_winner_is_ranked_end_to_end() {
        let st = RouterSettings::default();
        let sorted = s(&[("a", 0.25), ("b", 0.025), ("c", 0.02)]);
        let (selected, flat) = select(&sorted, 0.01, 5, &st);
        assert!(!flat, "a clear lead below flat_threshold is not flat");
        assert_eq!(selected[0], 0);
        assert_eq!(verdict(flat, 0.25, 0.025, 0.01, 0.9), Verdict::Ranked);
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
