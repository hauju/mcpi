//! Minimal BM25 over tool name + description, used as an optional lexical shortlist before Jev.

use std::collections::HashMap;

const K1: f64 = 1.2;
const B: f64 = 0.75;

pub struct Bm25 {
    docs: Vec<HashMap<String, usize>>,
    doc_len: Vec<usize>,
    avg_len: f64,
    df: HashMap<String, usize>,
}

/// Lowercase alphanumeric terms; splits snake_case, kebab-case and camelCase identifiers.
pub fn tokenize(text: &str) -> Vec<String> {
    let mut terms = Vec::new();
    let mut cur = String::new();
    let mut prev_lower = false;
    for c in text.chars() {
        if c.is_alphanumeric() {
            if c.is_uppercase() && prev_lower && !cur.is_empty() {
                terms.push(std::mem::take(&mut cur));
            }
            cur.extend(c.to_lowercase());
            prev_lower = c.is_lowercase() || c.is_numeric();
        } else {
            if !cur.is_empty() {
                terms.push(std::mem::take(&mut cur));
            }
            prev_lower = false;
        }
    }
    if !cur.is_empty() {
        terms.push(cur);
    }
    terms
}

impl Bm25 {
    pub fn new<'a>(docs: impl IntoIterator<Item = &'a str>) -> Self {
        let mut df: HashMap<String, usize> = HashMap::new();
        let mut tf_docs = Vec::new();
        let mut doc_len = Vec::new();
        for d in docs {
            let terms = tokenize(d);
            doc_len.push(terms.len());
            let mut tf: HashMap<String, usize> = HashMap::new();
            for t in terms {
                *tf.entry(t).or_default() += 1;
            }
            for t in tf.keys() {
                *df.entry(t.clone()).or_default() += 1;
            }
            tf_docs.push(tf);
        }
        let avg_len = doc_len.iter().sum::<usize>() as f64 / doc_len.len().max(1) as f64;
        Self {
            docs: tf_docs,
            doc_len,
            avg_len,
            df,
        }
    }

    /// Document indices ordered by descending score, at most `n`.
    pub fn top(&self, query: &str, n: usize) -> Vec<usize> {
        let n_docs = self.docs.len() as f64;
        let q = tokenize(query);
        let mut scores: Vec<(usize, f64)> = self
            .docs
            .iter()
            .enumerate()
            .map(|(i, tf)| {
                let len_norm = 1.0 - B + B * self.doc_len[i] as f64 / self.avg_len.max(1e-9);
                let s: f64 = q
                    .iter()
                    .filter_map(|t| {
                        let f = *tf.get(t)? as f64;
                        let df = *self.df.get(t)? as f64;
                        let idf = ((n_docs - df + 0.5) / (df + 0.5) + 1.0).ln();
                        Some(idf * f * (K1 + 1.0) / (f + K1 * len_norm))
                    })
                    .sum();
                (i, s)
            })
            .collect();
        scores.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
        scores.into_iter().take(n).map(|(i, _)| i).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_identifiers() {
        assert_eq!(
            tokenize("list_open_issues getStats v2"),
            ["list", "open", "issues", "get", "stats", "v2"]
        );
    }

    #[test]
    fn ranks_lexical_overlap_first() {
        let idx = Bm25::new([
            "git_log: show recent commits",
            "send_email: send an email via gmail",
            "weather: forecast",
        ]);
        assert_eq!(idx.top("show me the last commits", 2)[0], 0);
        assert_eq!(idx.top("email my boss", 1), [1]);
    }
}
