//! dj-sense: does the room move with the music? First slice - a video FILE and a
//! known bpm in, a tempo-lock score and a suggestion line out. No camera, no
//! daemon, no action. See docs/design/sensing-and-graph/design.md Part 1.

pub mod estimator;
pub mod face;
pub mod video;

use anyhow::{Context, Result};
use estimator::{Params, Score, Verdict};
use std::path::Path;

/// Analysis window and hop, seconds. 10 s windows (the design's duty cycle) every
/// 2 s give 11 windows over a 30 s clip.
pub const WINDOW_S: f64 = 10.0;
pub const HOP_S: f64 = 2.0;

pub struct Report {
    pub frames: usize,
    pub faces: usize,
    pub windows: Vec<Score>,
}

impl Report {
    pub fn verdicts(&self) -> Vec<Verdict> {
        self.windows.iter().map(|s| s.verdict).collect()
    }
    pub fn count(&self, v: Verdict) -> usize {
        self.windows.iter().filter(|s| s.verdict == v).count()
    }
}

/// Decode `path`, find a face per frame, return the head-y trace (face heights).
pub fn head_trace(path: &Path, model: &str) -> Result<Vec<Option<f64>>> {
    let mut frames = video::Frames::open(path)?;
    let mut tracker = face::Tracker::new(model)?;
    let mut boxes = Vec::new();
    while let Some(f) = frames.next_frame().context("decoding")? {
        boxes.push(tracker.find(f, video::WIDTH, video::HEIGHT));
    }
    if std::env::var_os("DJ_SENSE_STATS").is_some() {
        eprintln!("dj-sense: {} frames tracked, {} full-frame scans", tracker.tracked, tracker.full);
    }
    Ok(face::trace(&boxes))
}

/// Score a trace against `bpm`, window by window.
pub fn score_trace(trace: &[Option<f64>], bpm: f64) -> Report {
    let fs = video::FPS;
    let win = (WINDOW_S * fs) as usize;
    let hop = (HOP_S * fs) as usize;
    let p = Params::default();
    let mut ws: Vec<Score> = estimator::windows(trace, win, hop).iter().map(|w| estimator::score(w, fs, bpm, &p)).collect();
    if ws.is_empty() && !trace.is_empty() {
        // A clip shorter than one window is scored whole.
        ws.push(estimator::score(trace, fs, bpm, &p));
    }
    Report { frames: trace.len(), faces: trace.iter().flatten().count(), windows: ws }
}
