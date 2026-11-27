//! The tempo-lock estimator: pure, no clock, no I/O.
//!
//! Input is one window of uniformly sampled head-y (in face heights, `None` where
//! no face was found) plus the track's tempo. The question is narrow: does the
//! trace carry a spectral peak at the beat rate or a metrical neighbour (0.5x,
//! 2x)? The same trace scored against a wrong tempo is the built-in null - if it
//! locks there too, the detector measures motion, not music.

/// Metrical neighbours of the beat a head may follow.
pub const RATIOS: [f64; 3] = [0.5, 1.0, 2.0];
/// The wrong-tempo control: the same ratios at this multiple of the bpm. 0.8 keeps
/// every control band clear of every true band (closest pair: 1.6 vs 2.0 Hz at
/// 120 bpm, four bandwidths apart).
pub const CONTROL_FACTOR: f64 = 0.8;

/// Bounds of the motion band analysed (the beat band is 0.5-4 Hz).
const BAND_LO_HZ: f64 = 0.4;
const BAND_HI_HZ: f64 = 4.5;
/// Spectral grid step and the half-width credited to a target frequency.
const GRID_HZ: f64 = 0.02;
const TARGET_HALF_WIDTH_HZ: f64 = 0.12;

/// Decision thresholds, all data.
#[derive(Debug, Clone, Copy)]
pub struct Params {
    /// Minimum share of in-band power at the matched target to call it a lock.
    pub min_share: f64,
    /// The matched share must exceed the control's best share by this factor.
    pub min_control_ratio: f64,
    /// Detrended RMS (face heights) below which the trace is "still": detector
    /// jitter on a motionless head is not a rhythm (measured: about 0.014 on
    /// the synthetic still clip, a 1 px wobble of a 60 px box; a visible bob is
    /// near 0.09).
    pub min_rms: f64,
    /// Fraction of frames that must carry a face, else the window is blind.
    pub min_coverage: f64,
}

impl Default for Params {
    fn default() -> Self {
        Params { min_share: 0.35, min_control_ratio: 3.0, min_rms: 0.025, min_coverage: 0.7 }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Head motion locked to the beat.
    Lock,
    /// Moving, but not with this tempo.
    NoLock,
    /// A face, but no motion worth scoring.
    Still,
    /// No face in most frames (blank camera, privacy switch, nobody there).
    /// Never "nobody moving".
    Blind,
}

impl Verdict {
    pub fn as_str(self) -> &'static str {
        match self {
            Verdict::Lock => "lock",
            Verdict::NoLock => "no-lock",
            Verdict::Still => "still",
            Verdict::Blind => "blind",
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Score {
    pub verdict: Verdict,
    /// Share of in-band power at the best matching beat ratio, in [0, 1]. This is
    /// the lock score.
    pub lock: f64,
    /// Which ratio matched (0.5, 1 or 2).
    pub ratio: f64,
    /// Best share at the wrong-tempo control.
    pub control: f64,
    /// Strongest in-band frequency of the trace, Hz.
    pub peak_hz: f64,
    /// Detrended RMS of the trace, face heights.
    pub rms: f64,
    /// Fraction of frames with a face.
    pub coverage: f64,
}

/// Score one window. `fs` is the sample rate in Hz.
pub fn score(samples: &[Option<f64>], fs: f64, bpm: f64, p: &Params) -> Score {
    let n = samples.len();
    let present = samples.iter().filter(|s| s.is_some()).count();
    let coverage = if n == 0 { 0.0 } else { present as f64 / n as f64 };
    let blind = Score {
        verdict: Verdict::Blind,
        lock: 0.0,
        ratio: 1.0,
        control: 0.0,
        peak_hz: 0.0,
        rms: 0.0,
        coverage,
    };
    if n < 8 || coverage < p.min_coverage || !(fs > 0.0) || !(bpm > 0.0) {
        return blind;
    }
    let y = detrend(&fill_gaps(samples));
    let rms = (y.iter().map(|v| v * v).sum::<f64>() / n as f64).sqrt();

    // Hann-windowed power on a fine grid over the motion band.
    let w: Vec<f64> = (0..n)
        .map(|i| {
            let h = 0.5 - 0.5 * (2.0 * std::f64::consts::PI * i as f64 / (n - 1) as f64).cos();
            y[i] * h
        })
        .collect();
    let hi = BAND_HI_HZ.min(fs / 2.0);
    let mut grid = Vec::new();
    let mut f = BAND_LO_HZ;
    while f <= hi + 1e-9 {
        grid.push((f, power(&w, fs, f)));
        f += GRID_HZ;
    }
    let total: f64 = grid.iter().map(|(_, pw)| pw).sum();
    let peak_hz = grid
        .iter()
        .copied()
        .fold((0.0, f64::MIN), |a, b| if b.1 > a.1 { b } else { a })
        .0;
    let share = |f0: f64| -> f64 {
        if total <= 0.0 {
            return 0.0;
        }
        grid.iter()
            .filter(|(f, _)| (f - f0).abs() <= TARGET_HALF_WIDTH_HZ)
            .map(|(_, pw)| pw)
            .sum::<f64>()
            / total
    };
    let beat = bpm / 60.0;
    let (lock, ratio) = RATIOS
        .iter()
        .map(|r| (share(beat * r), *r))
        .fold((0.0, 1.0), |a, b| if b.0 > a.0 { b } else { a });
    let control = RATIOS
        .iter()
        .map(|r| share(beat * CONTROL_FACTOR * r))
        .fold(0.0, f64::max);

    let verdict = if rms < p.min_rms {
        Verdict::Still
    } else if lock >= p.min_share && lock >= p.min_control_ratio * control {
        Verdict::Lock
    } else {
        Verdict::NoLock
    };
    Score { verdict, lock, ratio, control, peak_hz, rms, coverage }
}

fn power(x: &[f64], fs: f64, f: f64) -> f64 {
    let step = 2.0 * std::f64::consts::PI * f / fs;
    let (mut re, mut im) = (0.0, 0.0);
    for (i, v) in x.iter().enumerate() {
        let a = step * i as f64;
        re += v * a.cos();
        im -= v * a.sin();
    }
    re * re + im * im
}

/// Linear interpolation over missing samples; edges hold the nearest value.
fn fill_gaps(s: &[Option<f64>]) -> Vec<f64> {
    let known: Vec<(usize, f64)> = s.iter().enumerate().filter_map(|(i, v)| v.map(|v| (i, v))).collect();
    let mut out = vec![0.0; s.len()];
    if known.is_empty() {
        return out;
    }
    let mut k = 0;
    for (i, o) in out.iter_mut().enumerate() {
        while k + 1 < known.len() && known[k + 1].0 <= i {
            k += 1;
        }
        let (i0, v0) = known[k];
        *o = if i <= i0 || k + 1 == known.len() {
            v0
        } else {
            let (i1, v1) = known[k + 1];
            v0 + (v1 - v0) * (i - i0) as f64 / (i1 - i0) as f64
        };
    }
    out
}

/// Remove the least-squares line (posture drift is not rhythm).
fn detrend(y: &[f64]) -> Vec<f64> {
    let n = y.len() as f64;
    let mx = (n - 1.0) / 2.0;
    let my = y.iter().sum::<f64>() / n;
    let (mut sxy, mut sxx) = (0.0, 0.0);
    for (i, v) in y.iter().enumerate() {
        let dx = i as f64 - mx;
        sxy += dx * (v - my);
        sxx += dx * dx;
    }
    let b = if sxx > 0.0 { sxy / sxx } else { 0.0 };
    y.iter().enumerate().map(|(i, v)| v - my - b * (i as f64 - mx)).collect()
}

/// Split a trace into windows of `win` samples every `hop` samples.
pub fn windows(trace: &[Option<f64>], win: usize, hop: usize) -> Vec<&[Option<f64>]> {
    let mut out = Vec::new();
    let mut s = 0;
    while win > 0 && hop > 0 && s + win <= trace.len() {
        out.push(&trace[s..s + win]);
        s += hop;
    }
    out
}

/// The suggestion line. Reason stage of the design: sustained lock (two
/// consecutive windows) is the only thing that suggests anything; absence of lock
/// never suggests turning down (stillness looks like deep focus).
pub fn suggestion(verdicts: &[Verdict]) -> &'static str {
    let sustained = verdicts.windows(2).any(|w| w[0] == Verdict::Lock && w[1] == Verdict::Lock);
    let blind = verdicts.iter().filter(|v| **v == Verdict::Blind).count();
    if sustained {
        "suggest: moving with the beat - louder?"
    } else if !verdicts.is_empty() && blind * 2 > verdicts.len() {
        "blind: the camera shows no face - no suggestion"
    } else {
        "no suggestion: not moving with the beat"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FS: f64 = 15.0;
    const N: usize = 150; // a 10 s window

    /// Deterministic LCG so the table is reproducible.
    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> f64 {
            self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            ((self.0 >> 11) as f64) / ((1u64 << 53) as f64)
        }
        fn gauss(&mut self) -> f64 {
            (0..12).map(|_| self.next()).sum::<f64>() - 6.0
        }
    }

    fn sine(f: f64, amp: f64, noise: f64, seed: u64) -> Vec<Option<f64>> {
        let mut r = Rng(seed);
        (0..N)
            .map(|i| Some(amp * (2.0 * std::f64::consts::PI * f * i as f64 / FS).sin() + noise * r.gauss()))
            .collect()
    }

    fn v(s: &[Option<f64>], bpm: f64) -> Score {
        score(s, FS, bpm, &Params::default())
    }

    #[test]
    fn beat_sine_locks_and_recovers_frequency() {
        for seed in 1..=10 {
            let s = v(&sine(2.0, 0.08, 0.02, seed), 120.0);
            assert_eq!(s.verdict, Verdict::Lock, "{s:?}");
            assert!((s.peak_hz - 2.0).abs() <= 0.1, "{s:?}");
            assert_eq!(s.ratio, 1.0);
        }
    }

    #[test]
    fn metrical_neighbours_lock() {
        assert_eq!(v(&sine(1.0, 0.08, 0.02, 3), 120.0).verdict, Verdict::Lock);
        assert_eq!(v(&sine(4.0, 0.05, 0.01, 3), 120.0).verdict, Verdict::Lock);
    }

    #[test]
    fn wrong_tempo_control_does_not_lock() {
        // A 2 Hz bob scored against 96 bpm (0.8x).
        for seed in 1..=10 {
            assert_ne!(v(&sine(2.0, 0.08, 0.02, seed), 96.0).verdict, Verdict::Lock);
        }
        // A bob at 0.8x the track's tempo.
        for seed in 1..=10 {
            assert_ne!(v(&sine(1.6, 0.08, 0.02, seed), 120.0).verdict, Verdict::Lock);
        }
    }

    #[test]
    fn random_walk_does_not_lock() {
        for seed in 1..=10 {
            let mut r = Rng(seed);
            let mut y = 0.0;
            let s: Vec<_> = (0..N).map(|_| { y += 0.02 * r.gauss(); Some(y) }).collect();
            assert_ne!(v(&s, 120.0).verdict, Verdict::Lock);
        }
    }

    #[test]
    fn typing_bursts_do_not_lock() {
        for seed in 1..=10 {
            let mut r = Rng(seed);
            let mut base = 0.0;
            let s: Vec<_> = (0..N)
                .map(|_| {
                    if r.next() < 0.04 {
                        base = 0.1 * (r.next() - 0.5);
                    }
                    Some(base + 0.01 * r.gauss())
                })
                .collect();
            assert_ne!(v(&s, 120.0).verdict, Verdict::Lock);
        }
    }

    #[test]
    fn flat_line_is_still() {
        let s = v(&sine(2.0, 0.0, 0.003, 1), 120.0);
        assert_eq!(s.verdict, Verdict::Still);
    }

    #[test]
    fn blank_is_blind() {
        assert_eq!(v(&vec![None; N], 120.0).verdict, Verdict::Blind);
        assert_eq!(v(&[], 120.0).verdict, Verdict::Blind);
        // A face in only half the frames is still blind.
        let half: Vec<_> = sine(2.0, 0.08, 0.0, 1).into_iter().enumerate().map(|(i, s)| if i % 2 == 0 { s } else { None }).collect();
        assert_eq!(v(&half, 120.0).verdict, Verdict::Blind);
    }

    #[test]
    fn sparse_gaps_are_interpolated() {
        let gappy: Vec<_> = sine(2.0, 0.08, 0.01, 2).into_iter().enumerate().map(|(i, s)| if i % 7 == 0 { None } else { s }).collect();
        assert_eq!(v(&gappy, 120.0).verdict, Verdict::Lock);
    }

    #[test]
    fn suggestion_needs_sustained_lock() {
        use Verdict::*;
        assert!(suggestion(&[Lock, Lock]).starts_with("suggest"));
        assert!(suggestion(&[Lock, NoLock, Lock]).starts_with("no suggestion"));
        assert!(suggestion(&[Blind, Blind, Still]).starts_with("blind"));
        assert!(suggestion(&[]).starts_with("no suggestion"));
    }
}
