//! The pure map/reduce state machine of chunked processing.
//!
//! `next_action` is a function of the planned chunks and the persisted node
//! records only, so a restart, a second worker or a retry reaches the same
//! decision: complete nodes are never recomputed, a recoverable node is
//! expanded into its smaller parts, and a hierarchy that cannot shrink stops
//! with an error instead of recursing.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::chunk_digest::ChunkDigest;
use super::chunk_plan::{PlannedChunk, Span, hex, map_key};

/// Prompt cost charged per child for its group header line.
pub(super) const GROUP_HEADER_COST: usize = 40;
const MAX_SPLIT_DEPTH: usize = 64;

/// What a persisted node says.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub(super) enum NodeRecord {
    /// A completed map call.
    Map {
        digest: ChunkDigest,
        spans: Vec<Span>,
        cost: usize,
        dropped: u32,
    },
    /// A completed reduce call.
    Reduce {
        digest: ChunkDigest,
        children: Vec<String>,
        lo: u32,
        hi: u32,
        cost: usize,
        dropped: u32,
    },
    /// A map call whose input was cut into smaller calls.
    SplitMap { parts: Vec<Vec<Span>> },
    /// A reduce call whose children were regrouped.
    SplitReduce { parts: Vec<Vec<String>> },
}

/// One digest ready to feed a parent.
#[derive(Debug, Clone)]
pub(super) struct Output {
    pub key: String,
    pub cost: usize,
    pub lo: u32,
    pub hi: u32,
    pub digest: ChunkDigest,
}

/// What the driver must do next.
#[derive(Debug)]
pub(super) enum Action {
    Map {
        key: String,
        spans: Vec<Span>,
    },
    Reduce {
        key: String,
        level: u32,
        children: Vec<Output>,
    },
    /// Everything is reduced far enough for the final call.
    Final {
        outputs: Vec<Output>,
        /// Every primary span the completed map nodes covered.
        leaves: Vec<Span>,
    },
}

/// Why no further progress is possible.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum PipelineError {
    /// A reduce level cannot shrink (recoverable with a larger budget).
    Stalled(String),
    /// Persisted nodes contradict each other.
    Corrupt(String),
}

fn span_range(spans: &[Span]) -> (u32, u32) {
    let lo = spans.iter().map(|s| s.ordinal).min().unwrap_or(0);
    let hi = spans.iter().map(|s| s.ordinal).max().unwrap_or(0);
    (lo, hi)
}

fn resolve_map(
    spans: &[Span],
    nodes: &HashMap<String, NodeRecord>,
    out: &mut Vec<Output>,
    leaves: &mut Vec<Span>,
    depth: usize,
) -> Result<Option<Action>, PipelineError> {
    if depth > MAX_SPLIT_DEPTH {
        return Err(PipelineError::Corrupt("map split chain too deep".into()));
    }
    let key = map_key(spans);
    match nodes.get(&key) {
        Some(NodeRecord::Map {
            digest,
            spans: covered,
            cost,
            ..
        }) => {
            let (lo, hi) = span_range(covered);
            out.push(Output {
                key,
                cost: *cost,
                lo,
                hi,
                digest: digest.clone(),
            });
            leaves.extend(covered.iter().cloned());
            Ok(None)
        }
        Some(NodeRecord::SplitMap { parts }) => {
            for part in parts {
                if let Some(action) = resolve_map(part, nodes, out, leaves, depth + 1)? {
                    return Ok(Some(action));
                }
            }
            Ok(None)
        }
        _ => Ok(Some(Action::Map {
            key,
            spans: spans.to_vec(),
        })),
    }
}

fn reduce_key(level: u32, group: &[Output]) -> String {
    let mut hasher = Sha256::new();
    for child in group {
        hasher.update(child.key.as_bytes());
        hasher.update(b"|");
    }
    format!("r:{level}:{}", &hex(&hasher.finalize())[..24])
}

fn resolve_reduce(
    level: u32,
    group: &[Output],
    nodes: &HashMap<String, NodeRecord>,
    next: &mut Vec<Output>,
    depth: usize,
) -> Result<Option<Action>, PipelineError> {
    if depth > MAX_SPLIT_DEPTH {
        return Err(PipelineError::Corrupt("reduce split chain too deep".into()));
    }
    if group.len() == 1 {
        next.push(group[0].clone());
        return Ok(None);
    }
    let key = reduce_key(level, group);
    match nodes.get(&key) {
        Some(NodeRecord::Reduce {
            digest,
            cost,
            lo,
            hi,
            ..
        }) => {
            next.push(Output {
                key,
                cost: *cost,
                lo: *lo,
                hi: *hi,
                digest: digest.clone(),
            });
            Ok(None)
        }
        Some(NodeRecord::SplitReduce { parts }) => {
            for part in parts {
                let sub: Vec<Output> = part
                    .iter()
                    .filter_map(|k| group.iter().find(|o| &o.key == k).cloned())
                    .collect();
                if sub.len() != part.len() || sub.is_empty() {
                    return Err(PipelineError::Corrupt(
                        "reduce split names an unknown child".into(),
                    ));
                }
                if let Some(action) = resolve_reduce(level, &sub, nodes, next, depth + 1)? {
                    return Ok(Some(action));
                }
            }
            Ok(None)
        }
        _ => Ok(Some(Action::Reduce {
            key,
            level,
            children: group.to_vec(),
        })),
    }
}

/// Greedy chronological packing: every group holds as many consecutive
/// digests as fit `cap`, and at least one.
fn pack(current: &[Output], cap: usize) -> Vec<Vec<Output>> {
    let mut groups: Vec<Vec<Output>> = Vec::new();
    let mut used = 0usize;
    for out in current {
        let cost = out.cost + GROUP_HEADER_COST;
        match groups.last_mut() {
            Some(group) if used + cost <= cap => {
                group.push(out.clone());
                used += cost;
            }
            _ => {
                groups.push(vec![out.clone()]);
                used = cost;
            }
        }
    }
    groups
}

/// Decide the next step from persisted state alone.
///
/// # Errors
/// [`PipelineError::Stalled`] when a level cannot shrink.
pub(super) fn next_action(
    chunks: &[PlannedChunk],
    nodes: &HashMap<String, NodeRecord>,
    reduce_cap: usize,
    final_cap: usize,
) -> Result<Action, PipelineError> {
    let mut current: Vec<Output> = Vec::new();
    let mut leaves: Vec<Span> = Vec::new();
    for chunk in chunks {
        if let Some(action) = resolve_map(&chunk.spans, nodes, &mut current, &mut leaves, 0)? {
            return Ok(action);
        }
    }
    let mut level = 1u32;
    loop {
        let total: usize = current.iter().map(|o| o.cost + GROUP_HEADER_COST).sum();
        if current.len() <= 1 || total <= final_cap {
            return Ok(Action::Final {
                outputs: current,
                leaves,
            });
        }
        let mut next: Vec<Output> = Vec::new();
        for group in pack(&current, reduce_cap) {
            if let Some(action) = resolve_reduce(level, &group, nodes, &mut next, 0)? {
                return Ok(action);
            }
        }
        if next.len() >= current.len() {
            return Err(PipelineError::Stalled(format!(
                "reduce level {level} cannot shrink {} digests below the final budget",
                current.len()
            )));
        }
        current = next;
        level += 1;
    }
}

/// `(chunks_total, chunks_done)` for progress reporting.
pub(super) fn progress(chunks: &[PlannedChunk], nodes: &HashMap<String, NodeRecord>) -> (u32, u32) {
    let done = nodes
        .values()
        .filter(|n| matches!(n, NodeRecord::Map { .. }))
        .count();
    let total = chunks.len().max(done);
    (
        u32::try_from(total).unwrap_or(u32::MAX),
        u32::try_from(done).unwrap_or(u32::MAX),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn span(ordinal: u32) -> Span {
        Span {
            ordinal,
            start: 0,
            end: 10,
            total: 10,
        }
    }

    fn chunks(n: u32) -> Vec<PlannedChunk> {
        (0..n)
            .map(|i| PlannedChunk {
                spans: vec![span(i)],
            })
            .collect()
    }

    fn done_map(spans: &[Span], cost: usize) -> NodeRecord {
        NodeRecord::Map {
            digest: ChunkDigest::default(),
            spans: spans.to_vec(),
            cost,
            dropped: 0,
        }
    }

    /// Drive the machine with a fake provider whose reduce output costs
    /// `reduce_out`; returns (calls, leaves covered).
    fn drive(
        chunks: &[PlannedChunk],
        map_cost: usize,
        reduce_out: usize,
        reduce_cap: usize,
        final_cap: usize,
    ) -> Result<(usize, usize), PipelineError> {
        let mut nodes: HashMap<String, NodeRecord> = HashMap::new();
        let mut calls = 0;
        loop {
            match next_action(chunks, &nodes, reduce_cap, final_cap)? {
                Action::Map { key, spans } => {
                    calls += 1;
                    nodes.insert(key, done_map(&spans, map_cost));
                }
                Action::Reduce {
                    key,
                    level,
                    children,
                } => {
                    calls += 1;
                    assert!(level >= 1);
                    let lo = children.iter().map(|c| c.lo).min().unwrap();
                    let hi = children.iter().map(|c| c.hi).max().unwrap();
                    nodes.insert(
                        key,
                        NodeRecord::Reduce {
                            digest: ChunkDigest::default(),
                            children: children.iter().map(|c| c.key.clone()).collect(),
                            lo,
                            hi,
                            cost: reduce_out,
                            dropped: 0,
                        },
                    );
                }
                Action::Final { outputs, leaves } => {
                    let total: usize = outputs.iter().map(|o| o.cost + GROUP_HEADER_COST).sum();
                    assert!(outputs.len() == 1 || total <= final_cap);
                    return Ok((calls, leaves.len()));
                }
            }
            assert!(calls < 10_000, "must terminate");
        }
    }

    #[test]
    fn a_long_session_reduces_hierarchically_and_covers_every_leaf() {
        let cs = chunks(200);
        let (calls, leaves) = drive(&cs, 800, 800, 6_000, 3_000).unwrap();
        assert_eq!(leaves, 200, "every map leaf is accounted for");
        assert!(calls > 200 && calls < 420, "bounded hierarchy: {calls}");
    }

    #[test]
    fn a_single_chunk_goes_straight_to_final_after_one_map_call() {
        assert_eq!(drive(&chunks(1), 500, 500, 6_000, 3_000).unwrap(), (1, 1));
    }

    #[test]
    fn a_reduce_that_cannot_shrink_stops_with_stalled() {
        // every digest alone costs more than half the reduce cap: nothing packs
        let r = drive(&chunks(8), 4_000, 4_000, 6_000, 3_000);
        assert!(matches!(r, Err(PipelineError::Stalled(_))));
    }

    #[test]
    fn completed_nodes_are_never_asked_for_again() {
        let cs = chunks(3);
        let mut nodes = HashMap::new();
        for c in &cs {
            nodes.insert(map_key(&c.spans), done_map(&c.spans, 100));
        }
        assert!(matches!(
            next_action(&cs, &nodes, 6_000, 3_000).unwrap(),
            Action::Final { .. }
        ));
        nodes.remove(&map_key(&cs[1].spans));
        match next_action(&cs, &nodes, 6_000, 3_000).unwrap() {
            Action::Map { spans, .. } => assert_eq!(spans, cs[1].spans),
            other => panic!("expected the one missing map node, got {other:?}"),
        }
    }

    #[test]
    fn a_split_map_node_expands_into_its_parts_in_order() {
        let whole = vec![span(0), span(1)];
        let cs = vec![PlannedChunk {
            spans: whole.clone(),
        }];
        let mut nodes = HashMap::new();
        nodes.insert(
            map_key(&whole),
            NodeRecord::SplitMap {
                parts: vec![vec![span(0)], vec![span(1)]],
            },
        );
        match next_action(&cs, &nodes, 6_000, 3_000).unwrap() {
            Action::Map { spans, .. } => assert_eq!(spans, vec![span(0)]),
            other => panic!("{other:?}"),
        }
        nodes.insert(map_key(&[span(0)]), done_map(&[span(0)], 50));
        match next_action(&cs, &nodes, 6_000, 3_000).unwrap() {
            Action::Map { spans, .. } => assert_eq!(spans, vec![span(1)]),
            other => panic!("{other:?}"),
        }
        nodes.insert(map_key(&[span(1)]), done_map(&[span(1)], 50));
        match next_action(&cs, &nodes, 6_000, 3_000).unwrap() {
            Action::Final { outputs, leaves } => {
                assert_eq!(outputs.len(), 2);
                assert_eq!(leaves.len(), 2);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn progress_counts_completed_map_nodes_only() {
        let cs = chunks(4);
        let mut nodes = HashMap::new();
        nodes.insert(map_key(&cs[0].spans), done_map(&cs[0].spans, 10));
        nodes.insert("r:1:x".into(), NodeRecord::SplitReduce { parts: vec![] });
        assert_eq!(progress(&cs, &nodes), (4, 1));
    }
}
