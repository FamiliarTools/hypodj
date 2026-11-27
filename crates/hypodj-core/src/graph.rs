//! The node graph's first slice (docs/design/sensing-and-graph/design.md, Part 2):
//! one pairwise EDGE COST between two tracks and a greedy WALK that orders an
//! enqueue batch by it. No graph is stored; the graph is this function.
//!
//! ```text
//! edge(a, b) = W_TEMPO   * tempo_mismatch(a, b)   // bpm ratio, octave-folded
//!            + W_KEY     * key_distance(a, b)     // Camelot wheel steps
//!            + W_AROUSAL * |energy(a) - energy(b)|// measured energy only
//!            + W_TIMBRE  * (1 - cos(emb a, emb b)) / 2
//! ```
//!
//! Every term is in `0..=1`. A term whose feature is missing on either side reads
//! [`MISSING_TERM`], the middle of its range, rather than 0: a 0 would make an
//! unanalysed track look like the PERFECT neighbour and pull it to the front of a
//! half-analysed batch. With no features anywhere every edge is the same constant,
//! so the walk keeps the incoming order exactly (and [`walk`] short-circuits before
//! computing anything) - the degrade-to-today identity.
//!
//! The genre-lexicon energy of [`TrackFeatures::heuristic`] is NOT a measurement and
//! never enters the arousal term; only `measured` energy does.
//!
//! Everything here is PURE: no clock, no I/O, no lock.

use crate::intelligence::TrackFeatures;

/// A key on the Camelot wheel: `number` 1..=12 (a step is a fifth), `minor` is the
/// A ring, major the B ring. Relative major/minor share a number.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Camelot {
    pub number: u8,
    pub minor: bool,
}

impl Camelot {
    /// Parse Essentia's `key_key` ("C", "F#", "Bb", ...) and `key_scale`
    /// ("major" / "minor"). Anything else is `None`.
    pub fn parse(key: &str, scale: &str) -> Option<Camelot> {
        let mut chars = key.trim().chars();
        let base = match chars.next()?.to_ascii_uppercase() {
            'C' => 0,
            'D' => 2,
            'E' => 4,
            'F' => 5,
            'G' => 7,
            'A' => 9,
            'B' => 11,
            _ => return None,
        };
        let pc: i32 = match chars.as_str() {
            "" => base,
            "#" => base + 1,
            "b" => base - 1,
            _ => return None,
        };
        let minor = match scale.trim().to_ascii_lowercase().as_str() {
            "major" => false,
            "minor" => true,
            _ => return None,
        };
        // A minor key sits at the number of its relative major (three semitones up).
        let major_pc = (pc + if minor { 3 } else { 0 }).rem_euclid(12);
        // C major is 8B; each fifth (7 semitones) is one step clockwise.
        let number = ((major_pc * 7).rem_euclid(12) + 7) % 12 + 1;
        Some(Camelot { number: number as u8, minor })
    }

    /// Wheel steps between two keys: the circular number distance (0..=6) plus one
    /// for changing ring. 0 = same key; 1 = the mixing-compatible neighbours
    /// (a fifth either way, or the relative major/minor).
    pub fn steps(self, other: Camelot) -> u8 {
        let d = self.number.abs_diff(other.number);
        d.min(12 - d) + u8::from(self.minor != other.minor)
    }
}

/// What a missing term reads: the middle of its `0..=1` range.
pub const MISSING_TERM: f32 = 0.5;
/// Tempo distance (log2 of the ratio, octave-folded) at which the tempo term
/// saturates: a 12% tempo change already reads as a full mismatch.
pub const TEMPO_SATURATE_LOG2: f32 = 0.1635; // log2(1.12)
/// Camelot steps at which the key term saturates (the far side of the wheel).
pub const KEY_SATURATE_STEPS: f32 = 7.0;
/// Term weights. Equal on purpose: there is no data to fit them against yet, and
/// the kill criterion (`kill_criterion`) is what decides whether the sum earns
/// its keep at all.
pub const W_TEMPO: f32 = 1.0;
pub const W_KEY: f32 = 1.0;
pub const W_AROUSAL: f32 = 1.0;
pub const W_TIMBRE: f32 = 1.0;
/// How much one unit of edge cost weighs against the batch's incoming rank (which
/// spans `0..=1` from last to first) in the walk.
pub const EDGE_WEIGHT: f32 = 2.0;

/// Tempo mismatch in `0..=1`: half and double time count as the same tempo.
pub fn tempo_mismatch(a: f32, b: f32) -> f32 {
    if !(a.is_finite() && b.is_finite() && a > 0.0 && b > 0.0) {
        return MISSING_TERM;
    }
    let x = (b / a).log2();
    let folded = (x - x.round()).abs();
    (folded / TEMPO_SATURATE_LOG2).min(1.0)
}

/// Key distance in `0..=1` from Camelot steps.
pub fn key_distance(a: Camelot, b: Camelot) -> f32 {
    (f32::from(a.steps(b)) / KEY_SATURATE_STEPS).min(1.0)
}

/// Timbre distance in `0..=1` from embedding cosine; a length mismatch or a zero
/// vector is a missing term.
pub fn timbre_distance(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return MISSING_TERM;
    }
    let (mut dot, mut na, mut nb) = (0.0f64, 0.0f64, 0.0f64);
    for (x, y) in a.iter().zip(b) {
        let (x, y) = (f64::from(*x), f64::from(*y));
        dot += x * y;
        na += x * x;
        nb += y * y;
    }
    if na == 0.0 || nb == 0.0 || !dot.is_finite() {
        return MISSING_TERM;
    }
    let cos = (dot / (na.sqrt() * nb.sqrt())).clamp(-1.0, 1.0);
    ((1.0 - cos) / 2.0) as f32
}

/// The edge cost between two tracks, `0..=W_TEMPO+W_KEY+W_AROUSAL+W_TIMBRE`. Pure
/// and total: symmetric, finite for any input, and constant when either side has
/// no measured features.
pub fn edge(a: &TrackFeatures, b: &TrackFeatures) -> f32 {
    let tempo = match (a.bpm, b.bpm) {
        (Some(x), Some(y)) => tempo_mismatch(x, y),
        _ => MISSING_TERM,
    };
    let key = match (a.camelot, b.camelot) {
        (Some(x), Some(y)) => key_distance(x, y),
        _ => MISSING_TERM,
    };
    let arousal = if a.measured && b.measured && a.energy.is_finite() && b.energy.is_finite() {
        (a.energy - b.energy).abs().min(1.0)
    } else {
        MISSING_TERM
    };
    let timbre = match (&a.embedding, &b.embedding) {
        (Some(x), Some(y)) => timbre_distance(x, y),
        _ => MISSING_TERM,
    };
    W_TEMPO * tempo + W_KEY * key + W_AROUSAL * arousal + W_TIMBRE * timbre
}

/// Does `f` carry anything [`edge`] can read? The heuristic genre energy does not.
pub fn edge_bearing(f: &TrackFeatures) -> bool {
    f.measured || f.bpm.is_some() || f.camelot.is_some() || f.embedding.is_some()
}

/// Order a resolved batch as a greedy walk from `prev` (the queue tail it will be
/// appended after, `None` when there is none). Each step takes the candidate with
/// the best `rank - EDGE_WEIGHT * edge(prev, cand)`, where `rank` is its incoming
/// position mapped to `1..=0` (the incoming order already carries the selector's
/// own ranking: similarity, behaviour, the pull). Ties go to the earlier position.
///
/// Only PERMUTES: the output is exactly the input's elements. Returns the input
/// untouched when fewer than two candidates have edge-bearing features (then no
/// pair can differ in cost from any other and the walk would be the identity).
pub fn walk<T>(prev: Option<&TrackFeatures>, candidates: Vec<T>, feats: &[Option<TrackFeatures>]) -> Vec<T> {
    let n = candidates.len();
    debug_assert_eq!(n, feats.len());
    if n <= 1 || feats.len() != n {
        return candidates;
    }
    let bearing = feats.iter().filter(|f| f.as_ref().is_some_and(edge_bearing)).count();
    if bearing < 2 {
        return candidates;
    }
    let none = TrackFeatures::heuristic(0.5, 0.5);
    let feat = |i: usize| feats[i].as_ref().unwrap_or(&none);
    let last = (n - 1) as f32;
    let mut left: Vec<usize> = (0..n).collect();
    let mut order = Vec::with_capacity(n);
    let mut at: Option<usize> = None;
    while !left.is_empty() {
        let from = match at {
            Some(i) => Some(feat(i)),
            None => prev,
        };
        let mut best = (0usize, f32::NEG_INFINITY);
        for (slot, &i) in left.iter().enumerate() {
            let rank = (last - i as f32) / last;
            let cost = from.map_or(0.0, |p| edge(p, feat(i)));
            let score = rank - EDGE_WEIGHT * cost;
            // Strictly greater keeps the earlier index on a tie (left stays sorted).
            if score > best.1 {
                best = (slot, score);
            }
        }
        let i = left.remove(best.0);
        order.push(i);
        at = Some(i);
    }
    let mut src: Vec<Option<T>> = candidates.into_iter().map(Some).collect();
    order.into_iter().map(|i| src[i].take().expect("each index once")).collect()
}

/// Mean adjacent edge cost of a sequence starting after `prev`.
pub fn mean_adjacent_edge(prev: Option<&TrackFeatures>, seq: &[&TrackFeatures]) -> f32 {
    let mut pairs: Vec<(&TrackFeatures, &TrackFeatures)> = seq.windows(2).map(|w| (w[0], w[1])).collect();
    if let (Some(p), Some(first)) = (prev, seq.first()) {
        pairs.insert(0, (p, first));
    }
    if pairs.is_empty() {
        return 0.0;
    }
    pairs.iter().map(|(a, b)| edge(a, b)).sum::<f32>() / pairs.len() as f32
}

/// Largest adjacent energy step and the net energy change of a sequence after
/// `prev`.
pub fn energy_steps(prev: Option<&TrackFeatures>, seq: &[&TrackFeatures]) -> (f32, f32) {
    let mut path: Vec<f32> = prev.map(|p| p.energy).into_iter().collect();
    path.extend(seq.iter().map(|f| f.energy));
    let max = path.windows(2).map(|w| (w[1] - w[0]).abs()).fold(0.0, f32::max);
    let net = match (path.first(), path.last()) {
        (Some(a), Some(b)) => b - a,
        _ => 0.0,
    };
    (max, net)
}

/// One sampled seed for the kill-criterion evaluation: the seed (the queue tail)
/// and its candidates in the wire (server) order. The "calmer" leg derives the
/// pull-only order from the same pool the way the `Calmer` selector does.
pub struct EvalCase {
    pub seed: TrackFeatures,
    pub wire: Vec<TrackFeatures>,
}

/// The kill-criterion verdict (design Part 2, "Acceptance checks").
#[derive(Debug, Clone, PartialEq)]
pub struct KillVerdict {
    pub cases: usize,
    /// Seeds where the walk's mean adjacent edge is strictly lower than the wire
    /// order's.
    pub edge_wins: usize,
    /// Seeds where, under "calmer", the walk's largest energy step is strictly
    /// smaller than pull-only ordering's AND its net energy change is still
    /// negative.
    pub calmer_wins: usize,
}

impl KillVerdict {
    /// Both criteria hold in at least 80% of seeds.
    pub fn passes(&self) -> bool {
        self.cases > 0 && self.edge_wins * 5 >= self.cases * 4 && self.calmer_wins * 5 >= self.cases * 4
    }
}

/// Evaluate the kill criterion over `cases`. Pure: the caller supplies the
/// features (synthetic in the test, the real feature store plus live
/// `getSimilarSongs2` in the `graph-eval` bin).
pub fn kill_criterion(cases: &[EvalCase]) -> KillVerdict {
    let mut v = KillVerdict { cases: cases.len(), edge_wins: 0, calmer_wins: 0 };
    for c in cases {
        let feats: Vec<Option<TrackFeatures>> = c.wire.iter().cloned().map(Some).collect();
        let idx: Vec<usize> = (0..c.wire.len()).collect();
        let walked: Vec<&TrackFeatures> =
            walk(Some(&c.seed), idx, &feats).into_iter().map(|i| &c.wire[i]).collect();
        let wire: Vec<&TrackFeatures> = c.wire.iter().collect();
        if mean_adjacent_edge(Some(&c.seed), &walked) < mean_adjacent_edge(Some(&c.seed), &wire) {
            v.edge_wins += 1;
        }
        // Pull-only "calmer", as the `Calmer` selector emits it: the wire pool is
        // its 2x over-fetch, it keeps the calmest half, ascending, ties by wire
        // position.
        let mut calm: Vec<usize> = (0..c.wire.len()).collect();
        calm.sort_by(|&a, &b| c.wire[a].energy.total_cmp(&c.wire[b].energy).then(a.cmp(&b)));
        calm.truncate(c.wire.len().div_ceil(2));
        let calm_feats: Vec<Option<TrackFeatures>> = calm.iter().map(|&i| Some(c.wire[i].clone())).collect();
        let pull_only: Vec<&TrackFeatures> = calm.iter().map(|&i| &c.wire[i]).collect();
        let calm_walk: Vec<&TrackFeatures> =
            walk(Some(&c.seed), calm.clone(), &calm_feats).into_iter().map(|i| &c.wire[i]).collect();
        let (pull_max, _) = energy_steps(Some(&c.seed), &pull_only);
        let (walk_max, walk_net) = energy_steps(Some(&c.seed), &calm_walk);
        if walk_max < pull_max && walk_net < 0.0 {
            v.calmer_wins += 1;
        }
    }
    v
}

/// Synthetic evaluation cases: `n` seeds over a deterministic pseudo-random
/// library (an xorshift, no dependency) whose features have real structure - two
/// timbre clusters, cluster-typical tempo and key, energy spread - and a "wire"
/// order that is the server's rough similarity: same-cluster first, shuffled.
/// Used to exercise the harness before real features exist; NOT evidence about
/// the real library.
pub fn synthetic_cases(n: usize, pool: usize, mut seed: u64) -> Vec<EvalCase> {
    let mut rnd = move || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        (seed >> 11) as f32 / (1u64 << 53) as f32
    };
    let track = |cluster: usize, rnd: &mut dyn FnMut() -> f32| {
        let dim = 8;
        let mut emb = vec![0.0f32; dim];
        emb[cluster] = 1.0;
        for x in emb.iter_mut() {
            *x += 0.35 * (rnd() - 0.5);
        }
        let bpm = if cluster == 0 { 90.0 } else { 128.0 } * (1.0 + 0.1 * (rnd() - 0.5));
        let number = (if cluster == 0 { 5.0 } else { 10.0 } + 3.0 * (rnd() - 0.5)).round() as u8;
        TrackFeatures {
            energy: rnd(),
            valence: 0.5,
            embedding: Some(emb),
            voice: None,
            bpm: Some(bpm),
            camelot: Some(Camelot { number: number.clamp(1, 12), minor: rnd() < 0.5 }),
            measured: true,
        }
    };
    let mut cases = Vec::with_capacity(n);
    for _ in 0..n {
        let cluster = usize::from(rnd() < 0.5);
        let mut s = track(cluster, &mut rnd);
        // Seeds that ask for "calmer" are the energetic ones.
        s.energy = 0.7 + 0.3 * s.energy;
        let mut wire: Vec<(f32, TrackFeatures)> = (0..pool)
            .map(|_| {
                let c = if rnd() < 0.75 { cluster } else { 1 - cluster };
                let t = track(c, &mut rnd);
                // Server similarity: same cluster ranks higher, noisily.
                let score = if c == cluster { 1.0 } else { 0.0 } + rnd();
                (score, t)
            })
            .collect();
        wire.sort_by(|a, b| b.0.total_cmp(&a.0));
        cases.push(EvalCase { seed: s, wire: wire.into_iter().map(|(_, t)| t).collect() });
    }
    cases
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feat(energy: f32) -> TrackFeatures {
        TrackFeatures { measured: true, ..TrackFeatures::heuristic(energy, 0.5) }
    }

    #[test]
    fn camelot_table() {
        let c = |k: &str, s: &str| Camelot::parse(k, s).map(|c| (c.number, c.minor));
        assert_eq!(c("C", "major"), Some((8, false)));
        assert_eq!(c("A", "minor"), Some((8, true)));
        assert_eq!(c("G", "major"), Some((9, false)));
        assert_eq!(c("F", "major"), Some((7, false)));
        assert_eq!(c("B", "major"), Some((1, false)));
        assert_eq!(c("F#", "major"), Some((2, false)));
        assert_eq!(c("Gb", "major"), Some((2, false)));
        assert_eq!(c("Db", "major"), Some((3, false)));
        assert_eq!(c("C#", "minor"), Some((12, true)));
        assert_eq!(c("G#", "minor"), Some((1, true)));
        assert_eq!(c("Ab", "minor"), Some((1, true)));
        assert_eq!(c("Eb", "minor"), Some((2, true)));
        assert_eq!(c("D", "minor"), Some((7, true)));
        assert_eq!(c("H", "major"), None);
        assert_eq!(c("C", "dorian"), None);
        assert_eq!(c("", "major"), None);
    }

    #[test]
    fn camelot_steps_table() {
        let k = |n, m| Camelot { number: n, minor: m };
        assert_eq!(k(8, false).steps(k(8, false)), 0);
        assert_eq!(k(8, false).steps(k(8, true)), 1, "relative minor");
        assert_eq!(k(8, false).steps(k(9, false)), 1, "a fifth up");
        assert_eq!(k(12, false).steps(k(1, false)), 1, "wraps");
        assert_eq!(k(1, false).steps(k(7, true)), 7, "the far side");
    }

    #[test]
    fn tempo_table() {
        assert_eq!(tempo_mismatch(120.0, 120.0), 0.0);
        assert!(tempo_mismatch(120.0, 60.0) < 1e-6, "half time is the same tempo");
        assert!(tempo_mismatch(70.0, 140.0) < 1e-6, "double time is the same tempo");
        let near = tempo_mismatch(120.0, 123.0);
        assert!(near > 0.0 && near < 0.3, "{near}");
        assert_eq!(tempo_mismatch(100.0, 140.0), 1.0, "40% apart saturates");
        assert_eq!(tempo_mismatch(0.0, 120.0), MISSING_TERM);
        assert_eq!(tempo_mismatch(f32::NAN, 120.0), MISSING_TERM);
        assert_eq!(tempo_mismatch(120.0, 100.0), tempo_mismatch(100.0, 120.0));
    }

    #[test]
    fn edge_is_constant_without_features_and_symmetric_with() {
        let h = |e| TrackFeatures::heuristic(e, 0.5);
        let k = edge(&h(0.1), &h(0.9));
        assert_eq!(k, edge(&h(0.5), &h(0.5)), "genre energy never enters the edge");
        assert_eq!(k, 4.0 * MISSING_TERM);
        let mut a = feat(0.2);
        a.bpm = Some(100.0);
        a.camelot = Camelot::parse("C", "major");
        a.embedding = Some(vec![1.0, 0.0]);
        let mut b = feat(0.6);
        b.bpm = Some(200.0);
        b.camelot = Camelot::parse("A", "minor");
        b.embedding = Some(vec![0.0, 1.0]);
        let e = edge(&a, &b);
        assert_eq!(e, edge(&b, &a));
        assert!((e - (0.0 + 1.0 / 7.0 + 0.4 + 0.5)).abs() < 1e-5, "{e}");
        assert_eq!(edge(&a, &a), 0.0);
        // A NaN embedding is a missing term, never a NaN edge.
        let mut c = a.clone();
        c.embedding = Some(vec![f32::NAN, 0.0]);
        assert!(edge(&a, &c).is_finite());
    }

    #[test]
    fn walk_is_identity_without_edge_features() {
        let feats: Vec<Option<TrackFeatures>> =
            [0.9, 0.1, 0.5, 0.3].iter().map(|&e| Some(TrackFeatures::heuristic(e, 0.5))).collect();
        let prev = TrackFeatures::heuristic(0.9, 0.5);
        assert_eq!(walk(Some(&prev), vec![0, 1, 2, 3], &feats), vec![0, 1, 2, 3]);
        assert_eq!(walk(None, vec![0, 1, 2, 3], &[None, None, None, None]), vec![0, 1, 2, 3]);
        // One analysed track alone cannot be ordered against anything.
        let mut one = feats.clone();
        one[2] = Some(feat(0.5));
        assert_eq!(walk(Some(&prev), vec![0, 1, 2, 3], &one), vec![0, 1, 2, 3]);
    }

    #[test]
    fn walk_is_a_permutation() {
        for case in synthetic_cases(20, 15, 7) {
            let feats: Vec<_> = case.wire.iter().cloned().map(Some).collect();
            let mut out = walk(Some(&case.seed), (0..15).collect::<Vec<_>>(), &feats);
            out.sort();
            assert_eq!(out, (0..15).collect::<Vec<_>>());
        }
    }

    /// Emergent behaviour 1: with "calmer", pull-only ordering jumps straight to
    /// the calmest track; the walk descends, every step small, net still down.
    #[test]
    fn calmer_pull_path_descends_gradually() {
        let tail = feat(0.95);
        // Pull-only order: ascending energy (what Calmer emits).
        let energies = [0.05, 0.2, 0.35, 0.5, 0.65, 0.8];
        let feats: Vec<Option<TrackFeatures>> = energies.iter().map(|&e| Some(feat(e))).collect();
        let out = walk(Some(&tail), (0..energies.len()).collect(), &feats);
        let walked: Vec<&TrackFeatures> = out.iter().map(|&i| feats[i].as_ref().unwrap()).collect();
        let pull_only: Vec<&TrackFeatures> = feats.iter().map(|f| f.as_ref().unwrap()).collect();
        let (walk_max, walk_net) = energy_steps(Some(&tail), &walked);
        let (pull_max, _) = energy_steps(Some(&tail), &pull_only);
        assert!(walk_max < pull_max, "walk {walk_max} vs pull-only {pull_max}: {out:?}");
        assert!(walk_max <= 0.15 + 1e-6, "every step small: {walk_max} {out:?}");
        assert!(walk_net < 0.0);
        assert_eq!(out, vec![5, 4, 3, 2, 1, 0], "a monotone descent");
    }

    /// Emergent behaviour 2: tail in cluster A, a batch interleaving A and B. The
    /// walk exhausts A, crosses at the cheapest A-B pair, then continues in B. No
    /// genre is read; only the embedding differs.
    #[test]
    fn two_clusters_bridge_at_the_cheapest_pair() {
        // A sits near the x axis at 90 bpm and drifts toward B; B sits near the y
        // axis and drifts from a bridge track (close to A in tempo and timbre) out
        // to 128 bpm. Several axes agree on where the bridge is, as on a real pair.
        let at = |deg: f32, bpm: f32| {
            let r = deg.to_radians();
            TrackFeatures { embedding: Some(vec![r.cos(), r.sin()]), bpm: Some(bpm), ..feat(0.5) }
        };
        let tail = at(0.0, 90.0);
        // Wire order interleaves the clusters, the far end of B ranked first.
        let a = [at(5.0, 90.0), at(12.0, 91.0), at(20.0, 92.0)];
        let b = [at(55.0, 94.0), at(80.0, 110.0), at(90.0, 128.0)];
        let batch = vec![a[0].clone(), b[2].clone(), a[1].clone(), b[1].clone(), a[2].clone(), b[0].clone()];
        let is_a = [true, false, true, false, true, false];
        let feats: Vec<Option<TrackFeatures>> = batch.iter().cloned().map(Some).collect();
        let out = walk(Some(&tail), (0..batch.len()).collect(), &feats);
        let clusters: Vec<bool> = out.iter().map(|&i| is_a[i]).collect();
        assert_eq!(clusters, vec![true, true, true, false, false, false], "{out:?}");
        // The crossing is the cheapest A-B pair in the batch.
        let cross = edge(&batch[out[2]], &batch[out[3]]);
        let cheapest = a
            .iter()
            .flat_map(|x| b.iter().map(move |y| edge(x, y)))
            .fold(f32::INFINITY, f32::min);
        assert!((cross - cheapest).abs() < 1e-6, "crossed at {cross}, cheapest {cheapest}");
    }

    /// The harness runs end to end on synthetic data. It is NOT the real verdict
    /// (that needs the real feature store, see docs/design/sensing-and-graph), and
    /// it is not tuned to pass: the synthetic library draws energy independently
    /// of tempo, key and timbre, the worst case for the calmer leg, and there the
    /// walk wins 39 of 50 (78%), just under the bar. Pinned so a change to the
    /// edge or the walk shows up here as a moved number, not a silent drift.
    #[test]
    fn kill_criterion_on_synthetic_library() {
        let v = kill_criterion(&synthetic_cases(50, 12, 0x5eed));
        assert_eq!(v, KillVerdict { cases: 50, edge_wins: 50, calmer_wins: 39 });
        assert!(!v.passes());
    }
}
