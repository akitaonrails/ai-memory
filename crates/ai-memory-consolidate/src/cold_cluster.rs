//! Pure, zero-LLM cold-cluster algorithm for A3 (docs/design-memory-aging.md
//! §A3): DBSCAN over embeddings with an adaptive k-distance eps.
//!
//! This module is the *algorithm* only — pure functions over vectors, no store,
//! no wiki, no provider (invariant #13, zero-LLM). The forget sweep is the
//! orchestrator: it materialises the bounded cold-episodic set, loads its
//! embeddings, calls [`adaptive_eps`] and [`dbscan`] here, and collapses each
//! returned cluster via supersession. Keeping the math here makes it unit
//! testable and keeps the O(N²) pairwise distance confined to the bounded cold
//! set the sweep already holds.
//!
//! Distance is **cosine distance** (`1 - cosine_similarity`). Stored embeddings
//! are unit-normalised, so cosine similarity is the dot product; the code
//! normalises defensively anyway so a caller passing raw vectors still gets a
//! correct distance.

/// Cosine distance between two vectors: `1 - cos(θ)`, clamped to `[0, 2]`.
///
/// Returns `1.0` (maximally distant, the neutral default) for a
/// zero-magnitude vector or a length mismatch, so a degenerate row can never
/// pull unrelated pages into a cluster.
#[must_use]
pub fn cosine_distance(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() {
        return 1.0;
    }
    let mut dot = 0.0f32;
    let mut na = 0.0f32;
    let mut nb = 0.0f32;
    for (x, y) in a.iter().zip(b.iter()) {
        dot += x * y;
        na += x * x;
        nb += y * y;
    }
    if na <= 0.0 || nb <= 0.0 {
        return 1.0;
    }
    let sim = dot / (na.sqrt() * nb.sqrt());
    (1.0 - sim).clamp(0.0, 2.0)
}

/// Pick a conservative eps from the k-distance elbow (the field-standard DBSCAN
/// heuristic).
///
/// For each point, take the distance to its `k`-th nearest neighbour; sort those
/// k-distances ascending; the "elbow" — the point of maximum curvature — is the
/// eps at which density drops off. It is located by the maximum perpendicular
/// distance from the chord joining the first and last sorted k-distances (the
/// kneedle construction). The result is then clamped to `max_eps` so an operator
/// keeps a hard conservative ceiling: A3 errs toward NOT merging.
///
/// Returns `None` when there are too few points to form a `k`-distance
/// (`points.len() <= k`), which the caller treats as "nothing to cluster".
#[must_use]
pub fn adaptive_eps(points: &[Vec<f32>], k: usize, max_eps: f32) -> Option<f32> {
    let n = points.len();
    if k == 0 || n <= k {
        return None;
    }
    // k-th nearest-neighbour distance for every point.
    let mut k_distances: Vec<f32> = Vec::with_capacity(n);
    for (i, p) in points.iter().enumerate() {
        let mut dists: Vec<f32> = points
            .iter()
            .enumerate()
            .filter(|(j, _)| *j != i)
            .map(|(_, q)| cosine_distance(p, q))
            .collect();
        dists.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        // `dists` excludes self, so the k-th nearest neighbour is index k-1.
        k_distances.push(dists[k - 1]);
    }
    k_distances.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));

    let elbow = knee_value(&k_distances);
    Some(elbow.clamp(0.0, max_eps))
}

/// The elbow (knee) of an ascending curve, by maximum perpendicular distance
/// from the chord between its first and last points. For a flat or two-point
/// curve it returns the last value; the caller's `max_eps` clamp bounds it.
fn knee_value(sorted: &[f32]) -> f32 {
    let n = sorted.len();
    if n == 0 {
        return 0.0;
    }
    if n <= 2 {
        return sorted[n - 1];
    }
    let x0 = 0.0f32;
    let y0 = sorted[0];
    #[allow(clippy::cast_precision_loss)]
    let x1 = (n - 1) as f32;
    let y1 = sorted[n - 1];
    let dx = x1 - x0;
    let dy = y1 - y0;
    let denom = (dx * dx + dy * dy).sqrt();
    if denom <= 0.0 {
        return sorted[n - 1];
    }
    let mut best_idx = n - 1;
    let mut best_dist = -1.0f32;
    for (i, &y) in sorted.iter().enumerate() {
        #[allow(clippy::cast_precision_loss)]
        let x = i as f32;
        // Perpendicular distance from (x, y) to the chord (x0,y0)-(x1,y1).
        let perp = ((dy * (x - x0)) - (dx * (y - y0))).abs() / denom;
        if perp > best_dist {
            best_dist = perp;
            best_idx = i;
        }
    }
    sorted[best_idx]
}

/// DBSCAN over `points` with cosine distance. Returns the discovered clusters as
/// lists of indices into `points`; noise points are omitted (not returned as
/// singletons). Deterministic: points are visited in index order and each
/// cluster's members are in the order they were reached.
///
/// `min_pts` is the density floor (a core point has at least `min_pts` points,
/// itself included, within `eps`). A3 keeps `min_pts` small (2): a pair of
/// near-duplicate cold pages is exactly what it wants to collapse, and the
/// conservative `eps` is the guard against over-merging.
#[must_use]
pub fn dbscan(points: &[Vec<f32>], eps: f32, min_pts: usize) -> Vec<Vec<usize>> {
    let n = points.len();
    let mut labels: Vec<Label> = vec![Label::Unvisited; n];
    let mut clusters: Vec<Vec<usize>> = Vec::new();

    for i in 0..n {
        if labels[i] != Label::Unvisited {
            continue;
        }
        let neighbors = region_query(points, i, eps);
        if neighbors.len() < min_pts {
            labels[i] = Label::Noise;
            continue;
        }
        let cluster_id = clusters.len();
        let mut members: Vec<usize> = Vec::new();
        labels[i] = Label::Clustered(cluster_id);
        members.push(i);

        // Expand the cluster over a growing frontier (iterative, not recursive,
        // so a large dense set cannot blow the stack).
        let mut queue = std::collections::VecDeque::from(neighbors);
        while let Some(q) = queue.pop_front() {
            match labels[q] {
                Label::Noise => {
                    // A border point: attach it, but do not expand from it.
                    labels[q] = Label::Clustered(cluster_id);
                    members.push(q);
                }
                Label::Unvisited => {
                    labels[q] = Label::Clustered(cluster_id);
                    members.push(q);
                    let q_neighbors = region_query(points, q, eps);
                    if q_neighbors.len() >= min_pts {
                        for nb in q_neighbors {
                            if labels[nb] == Label::Unvisited || labels[nb] == Label::Noise {
                                queue.push_back(nb);
                            }
                        }
                    }
                }
                Label::Clustered(_) => {}
            }
        }
        clusters.push(members);
    }
    clusters
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Label {
    Unvisited,
    Noise,
    Clustered(usize),
}

/// Indices of every point within `eps` of `idx`, including `idx` itself.
fn region_query(points: &[Vec<f32>], idx: usize, eps: f32) -> Vec<usize> {
    let p = &points[idx];
    (0..points.len())
        .filter(|&j| cosine_distance(p, &points[j]) <= eps)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Three tight points near one axis, one far point on another: DBSCAN must
    /// find exactly one cluster of the three, and drop the far point as noise.
    #[test]
    fn three_tight_points_and_one_far_point() {
        let points = vec![
            vec![1.0, 0.0, 0.0],
            vec![0.98, 0.02, 0.0],
            vec![0.99, 0.0, 0.01],
            vec![0.0, 1.0, 0.0], // far
        ];
        let clusters = dbscan(&points, 0.1, 2);
        assert_eq!(clusters.len(), 1, "one cluster only: {clusters:?}");
        let mut members = clusters[0].clone();
        members.sort_unstable();
        assert_eq!(members, vec![0, 1, 2], "the three tight points cluster");
        assert!(
            !clusters[0].contains(&3),
            "the far point is noise, not clustered"
        );
    }

    #[test]
    fn deterministic_across_runs() {
        let points = vec![
            vec![1.0, 0.0],
            vec![0.99, 0.01],
            vec![0.0, 1.0],
            vec![0.01, 0.99],
        ];
        let a = dbscan(&points, 0.1, 2);
        let b = dbscan(&points, 0.1, 2);
        assert_eq!(a, b, "same input yields identical clustering");
        assert_eq!(a.len(), 2, "two well-separated pairs: {a:?}");
    }

    #[test]
    fn adaptive_eps_picks_a_sane_threshold() {
        // Two tight pairs, far apart. The k-distance elbow should sit above the
        // within-pair distance (so pairs cluster) and below the across-pair
        // distance (so the two pairs stay separate).
        let points = vec![
            vec![1.0, 0.0],
            vec![0.999, 0.001],
            vec![0.0, 1.0],
            vec![0.001, 0.999],
        ];
        let eps = adaptive_eps(&points, 2, 0.5).expect("eps");
        assert!(eps > 0.0, "a positive threshold: {eps}");
        assert!(eps <= 0.5, "clamped to the conservative ceiling: {eps}");
        let clusters = dbscan(&points, eps, 2);
        assert_eq!(
            clusters.len(),
            2,
            "adaptive eps keeps the two tight pairs separate: {clusters:?}"
        );
    }

    #[test]
    fn adaptive_eps_respects_the_conservative_ceiling() {
        // A spread-out set whose natural elbow is large; the ceiling clamps it.
        let points = vec![
            vec![1.0, 0.0],
            vec![0.0, 1.0],
            vec![-1.0, 0.0],
            vec![0.0, -1.0],
        ];
        let eps = adaptive_eps(&points, 2, 0.05).expect("eps");
        assert!(eps <= 0.05, "ceiling honoured: {eps}");
    }

    #[test]
    fn too_few_points_yields_no_eps() {
        assert_eq!(adaptive_eps(&[vec![1.0, 0.0]], 2, 0.5), None);
        assert_eq!(adaptive_eps(&[], 2, 0.5), None);
    }

    #[test]
    fn no_cluster_when_everything_is_far() {
        let points = vec![
            vec![1.0, 0.0, 0.0],
            vec![0.0, 1.0, 0.0],
            vec![0.0, 0.0, 1.0],
        ];
        let clusters = dbscan(&points, 0.1, 2);
        assert!(clusters.is_empty(), "no near-duplicates, no clusters");
    }

    #[test]
    fn cosine_distance_basics() {
        assert!(
            cosine_distance(&[1.0, 0.0], &[1.0, 0.0]) < 1e-6,
            "identical = 0"
        );
        assert!(
            (cosine_distance(&[1.0, 0.0], &[0.0, 1.0]) - 1.0).abs() < 1e-6,
            "orthogonal = 1"
        );
        assert_eq!(
            cosine_distance(&[0.0, 0.0], &[1.0, 0.0]),
            1.0,
            "zero vector = neutral 1"
        );
        assert_eq!(
            cosine_distance(&[1.0], &[1.0, 0.0]),
            1.0,
            "mismatch = neutral 1"
        );
    }
}
