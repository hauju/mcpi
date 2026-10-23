//! Hybrid retrieval for the shortlist: BM25, embeddings, and routing memory, fused by
//! reciprocal rank fusion.
//!
//! The shortlist is the one lossy step before Jev: a tool it drops is never ranked. BM25 alone
//! drops a tool whose description uses other words than the request, so a dense ranking and the
//! user's own past requests vote beside it. Nothing here decides what `find_tools` returns —
//! the fused list only chooses which candidates Jev sees.
//!
//! This crate ships no embedding model: [`Embedder`] is the seam, and the caller brings one.

use crate::BoxFuture;

/// Reciprocal rank fusion constant: a document at rank `r` (1-based) scores `1 / (K + r)`.
pub const RRF_K: f64 = 60.0;

/// How many of the nearest past requests vote for their tool.
pub const MEMORY_NEIGHBORS: usize = 10;

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct EmbedError(pub String);

/// Turns text into vectors. Dyn-compatible so a gateway can hold whatever model it runs.
pub trait Embedder: Send + Sync + 'static {
    /// One vector per input text, all the same dimension. Errors are the caller's to log;
    /// retrieval degrades to BM25 when this fails.
    fn embed<'a>(&'a self, texts: &'a [String])
    -> BoxFuture<'a, Result<Vec<Vec<f32>>, EmbedError>>;
}

/// A previous `find_tools` request that led to a successful call of `tool_key`.
#[derive(Debug, Clone, PartialEq)]
pub struct PastRequest {
    pub tool_key: String,
    pub embedding: Vec<f32>,
}

/// Which sources shaped one shortlist. Present only when an embedder is configured and the
/// catalog was large enough to shortlist.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RetrievalStats {
    /// Shortlisted candidates BM25 scored above zero.
    pub bm25: usize,
    /// Shortlisted candidates the embedding ranking contributed (0 when it was unavailable).
    pub embedding: usize,
    /// Shortlisted candidates a past request voted for.
    pub memory: usize,
    /// Shortlisted candidates BM25's own top-N would have dropped.
    pub beyond_bm25: usize,
    /// Time to embed the request; `None` when embedding failed.
    pub embed_ms: Option<u64>,
}

/// Cosine similarity; `None` for mismatched dimensions or a zero vector.
pub fn cosine(a: &[f32], b: &[f32]) -> Option<f32> {
    if a.len() != b.len() || a.is_empty() {
        return None;
    }
    let (mut dot, mut na, mut nb) = (0.0f32, 0.0f32, 0.0f32);
    for (x, y) in a.iter().zip(b) {
        dot += x * y;
        na += x * x;
        nb += y * y;
    }
    (na > 0.0 && nb > 0.0).then(|| dot / (na.sqrt() * nb.sqrt()))
}

/// Indices of `docs` by descending cosine to `query`, ties by index. A document with no
/// positive similarity is left out, as is one that cannot be compared.
pub fn rank_by_cosine(query: &[f32], docs: &[Vec<f32>]) -> Vec<usize> {
    let mut scored: Vec<(usize, f32)> = docs
        .iter()
        .enumerate()
        .filter_map(|(i, d)| Some((i, cosine(query, d)?)))
        .filter(|(_, c)| *c > 0.0)
        .collect();
    scored.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
    scored.into_iter().map(|(i, _)| i).collect()
}

/// Tools voted for by the [`MEMORY_NEIGHBORS`] past requests nearest `query`, each at the rank
/// of its nearest request; a request with no positive similarity does not vote. `memory` pairs a catalog index with a request embedding; ties keep
/// `memory`'s order.
pub fn rank_by_memory(query: &[f32], memory: &[(usize, Vec<f32>)]) -> Vec<usize> {
    let mut near: Vec<(usize, f32)> = memory
        .iter()
        .filter_map(|(tool, e)| Some((*tool, cosine(query, e)?)))
        .filter(|(_, c)| *c > 0.0)
        .collect();
    near.sort_by(|a, b| b.1.total_cmp(&a.1));
    let mut out: Vec<usize> = Vec::new();
    for (tool, _) in near.into_iter().take(MEMORY_NEIGHBORS) {
        if !out.contains(&tool) {
            out.push(tool);
        }
    }
    out
}

/// Reciprocal rank fusion of ranked index lists: descending fused score, ties by index.
pub fn rrf(lists: &[&[usize]]) -> Vec<usize> {
    let mut score: std::collections::HashMap<usize, f64> = std::collections::HashMap::new();
    for list in lists {
        for (rank, &i) in list.iter().enumerate() {
            *score.entry(i).or_default() += 1.0 / (RRF_K + rank as f64 + 1.0);
        }
    }
    let mut fused: Vec<(usize, f64)> = score.into_iter().collect();
    fused.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
    fused.into_iter().map(|(i, _)| i).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rrf_rewards_agreement_and_breaks_ties_by_index() {
        // 2 is second in both lists, 0 and 1 are first in one each.
        assert_eq!(rrf(&[&[0, 2, 3], &[1, 2]]), [2, 0, 1, 3]);
        // Identical rank patterns tie; the lower index wins, every time.
        assert_eq!(rrf(&[&[5, 4], &[4, 5]]), [4, 5]);
        assert_eq!(rrf(&[&[7, 3]]), [7, 3], "one list keeps its order");
        assert!(rrf(&[]).is_empty());
    }

    #[test]
    fn cosine_refuses_what_it_cannot_compare() {
        assert_eq!(cosine(&[1.0, 0.0], &[1.0, 0.0]), Some(1.0));
        assert_eq!(cosine(&[1.0, 0.0], &[0.0, 1.0]), Some(0.0));
        assert_eq!(cosine(&[1.0], &[1.0, 0.0]), None);
        assert_eq!(cosine(&[0.0, 0.0], &[1.0, 0.0]), None);
    }

    #[test]
    fn memory_votes_once_per_tool_at_its_nearest_request() {
        let memory = vec![
            (3, vec![0.0, 1.0]),
            (1, vec![1.0, 0.1]),
            (3, vec![1.0, 0.0]),
            (2, vec![0.5, 0.5]),
        ];
        assert_eq!(rank_by_memory(&[1.0, 0.0], &memory), [3, 1, 2]);
    }
}
