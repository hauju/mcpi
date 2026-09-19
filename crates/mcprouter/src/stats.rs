//! Offline analysis of the JSONL log: per-tool usage, router hit rate, definition cost, and a
//! recommendation whether the tool should be exposed directly or stay behind `find_tools`.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use serde::Deserialize;

#[derive(Debug, Clone, Copy)]
pub struct Thresholds {
    /// Recommend `direct` when the tool is called in at least this share of tool-using turns.
    pub direct_share: f64,
    /// Recommend `direct` when the router failed to return the tool before at least this share of its calls.
    pub direct_miss: f64,
    /// Ignore tools with fewer calls than this when recommending.
    pub min_calls: usize,
}

impl Default for Thresholds {
    fn default() -> Self {
        Self {
            direct_share: 0.3,
            direct_miss: 0.1,
            min_calls: 5,
        }
    }
}

#[derive(Debug, Default, Clone)]
pub struct ToolStats {
    pub server: String,
    pub calls: usize,
    pub direct_calls: usize,
    /// Calls where the preceding `find_tools` did not return the tool.
    pub misses: usize,
    /// Calls with a preceding `find_tools` in the same session.
    pub linked: usize,
    pub p_sum: f64,
    pub rank_sum: usize,
    pub def_chars: usize,
    pub errors: usize,
}

#[derive(Debug, Default)]
pub struct Stats {
    pub find_tools: usize,
    pub flat: usize,
    pub jev_tokens: u64,
    pub latency_sum_ms: u64,
    pub tools: BTreeMap<String, ToolStats>,
    pub expose_direct: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Recommendation {
    Direct,
    Route,
    /// Too few calls to say.
    Unknown,
}

#[derive(Deserialize)]
struct Line {
    event: String,
    #[serde(default)]
    tools: Vec<CatalogLine>,
    #[serde(default)]
    expose_direct: Vec<String>,
    #[serde(default)]
    name: String,
    #[serde(default)]
    server: Option<String>,
    #[serde(default)]
    direct: bool,
    #[serde(default)]
    find_id: Option<u64>,
    #[serde(default)]
    rank: Option<usize>,
    #[serde(default)]
    p: Option<f64>,
    #[serde(default)]
    in_returned: Option<bool>,
    #[serde(default)]
    is_error: bool,
    #[serde(default)]
    flat: bool,
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    latency_ms: u64,
}

#[derive(Deserialize)]
struct CatalogLine {
    key: String,
    server: String,
    def_chars: usize,
}

impl Stats {
    pub fn from_jsonl(text: &str) -> Self {
        let mut s = Stats::default();
        for line in text.lines().filter(|l| !l.trim().is_empty()) {
            let Ok(l) = serde_json::from_str::<Line>(line) else {
                continue;
            };
            match l.event.as_str() {
                "catalog" => {
                    for t in l.tools {
                        let e = s.tools.entry(t.key).or_default();
                        e.server = t.server;
                        e.def_chars = t.def_chars;
                    }
                    s.expose_direct = l.expose_direct;
                }
                "find_tools" => {
                    s.find_tools += 1;
                    s.flat += usize::from(l.flat);
                    s.jev_tokens += l.input_tokens;
                    s.latency_sum_ms += l.latency_ms;
                }
                "call_tool" => {
                    let e = s.tools.entry(l.name).or_default();
                    if let Some(sv) = l.server {
                        e.server = sv;
                    }
                    e.calls += 1;
                    e.direct_calls += usize::from(l.direct);
                    e.errors += usize::from(l.is_error);
                    if l.find_id.is_some() {
                        e.linked += 1;
                        if l.in_returned != Some(true) {
                            e.misses += 1;
                        }
                        e.p_sum += l.p.unwrap_or(0.0);
                        e.rank_sum += l.rank.unwrap_or(usize::MAX / 4).min(1000);
                    }
                }
                _ => {}
            }
        }
        s
    }

    /// Share of tool-using turns (`find_tools` events) in which the tool was called.
    pub fn share(&self, t: &ToolStats) -> f64 {
        t.calls as f64 / self.find_tools.max(1) as f64
    }

    pub fn recommend(&self, t: &ToolStats, th: Thresholds) -> Recommendation {
        if t.calls < th.min_calls {
            return Recommendation::Unknown;
        }
        let miss = if t.linked > 0 {
            t.misses as f64 / t.linked as f64
        } else {
            0.0
        };
        if self.share(t) >= th.direct_share || miss >= th.direct_miss {
            Recommendation::Direct
        } else {
            Recommendation::Route
        }
    }

    pub fn render(&self, th: Thresholds) -> String {
        let mut out = String::new();
        let _ = writeln!(
            out,
            "find_tools calls: {}   flat: {}   mean latency: {} ms   Jev input tokens: {}   (≈ ${:.4} at $42/1B)",
            self.find_tools,
            self.flat,
            self.latency_sum_ms / self.find_tools.max(1) as u64,
            self.jev_tokens,
            self.jev_tokens as f64 * 42.0 / 1e9
        );
        let h = [
            "tool",
            "server",
            "calls",
            "share",
            "miss%",
            "p",
            "direct",
            "def tok",
            "recommend",
        ];
        let _ = writeln!(
            out,
            "{:<40} {:<12} {:>5} {:>6} {:>6} {:>5} {:>6} {:>8}  {}",
            h[0], h[1], h[2], h[3], h[4], h[5], h[6], h[7], h[8]
        );
        let mut rows: Vec<(&String, &ToolStats)> =
            self.tools.iter().filter(|(_, t)| t.calls > 0).collect();
        rows.sort_by(|a, b| b.1.calls.cmp(&a.1.calls).then(a.0.cmp(b.0)));
        for (key, t) in rows {
            let miss = if t.linked > 0 {
                format!("{:.0}%", 100.0 * t.misses as f64 / t.linked as f64)
            } else {
                "-".into()
            };
            let p = if t.linked > 0 {
                format!("{:.2}", t.p_sum / t.linked as f64)
            } else {
                "-".into()
            };
            let rec = match self.recommend(t, th) {
                Recommendation::Direct => "direct",
                Recommendation::Route => "route",
                Recommendation::Unknown => "?",
            };
            let already = if self.expose_direct.contains(key) {
                " (is direct)"
            } else {
                ""
            };
            let _ = writeln!(
                out,
                "{:<40} {:<12} {:>5} {:>5.0}% {:>6} {:>5} {:>6} {:>8}  {rec}{already}",
                key.chars().take(40).collect::<String>(),
                t.server.chars().take(12).collect::<String>(),
                t.calls,
                100.0 * self.share(t),
                miss,
                p,
                t.direct_calls,
                t.def_chars / 4
            );
        }
        let never: usize = self.tools.values().filter(|t| t.calls == 0).count();
        let never_tok: usize = self
            .tools
            .values()
            .filter(|t| t.calls == 0)
            .map(|t| t.def_chars / 4)
            .sum();
        let _ = writeln!(
            out,
            "{never} catalog tools never called (≈{never_tok} definition tokens kept out of context per turn)"
        );
        let _ = writeln!(
            out,
            "rule: direct if share ≥ {:.0}% of tool-using turns or router miss ≥ {:.0}% (min {} calls); route otherwise",
            th.direct_share * 100.0,
            th.direct_miss * 100.0,
            th.min_calls
        );
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LOG: &str = r#"{"event":"catalog","ts":"1","tools":[{"key":"a","server":"s","def_chars":400},{"key":"b","server":"s","def_chars":800},{"key":"c","server":"s","def_chars":100}],"expose_direct":[]}
{"event":"find_tools","ts":"1","session":"x","find_id":1,"request":"r","k":5,"candidates":[],"selected":[],"flat":false,"p_none":0.0,"gate":0.9,"input_tokens":100,"latency_ms":300,"returned":["a"]}
{"event":"call_tool","ts":"1","session":"x","name":"a","server":"s","direct":false,"find_id":1,"rank":0,"p":0.9,"in_returned":true,"ok":true,"is_error":false,"latency_ms":3}
{"event":"find_tools","ts":"1","session":"x","find_id":2,"request":"r","k":5,"candidates":[],"selected":[],"flat":true,"p_none":0.5,"gate":0.2,"input_tokens":100,"latency_ms":300,"returned":["c"]}
{"event":"call_tool","ts":"1","session":"x","name":"b","server":"s","direct":false,"find_id":2,"rank":7,"p":0.01,"in_returned":false,"ok":true,"is_error":false,"latency_ms":3}
"#;

    #[test]
    fn aggregates_and_recommends() {
        let s = Stats::from_jsonl(LOG);
        assert_eq!(s.find_tools, 2);
        assert_eq!(s.flat, 1);
        assert_eq!(s.jev_tokens, 200);
        let a = &s.tools["a"];
        assert_eq!((a.calls, a.misses, a.def_chars), (1, 0, 400));
        let b = &s.tools["b"];
        assert_eq!((b.calls, b.misses, b.linked), (1, 1, 1));
        let th = Thresholds {
            min_calls: 1,
            ..Default::default()
        };
        assert_eq!(s.recommend(a, th), Recommendation::Direct); // share 50% ≥ 30%
        assert_eq!(s.recommend(b, th), Recommendation::Direct); // router missed it
        assert_eq!(s.recommend(&s.tools["c"], th), Recommendation::Unknown);
        assert!(s.render(th).contains("1 catalog tools never called"));
    }
}
