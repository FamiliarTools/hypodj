//! Head-y from a face box. rustface (SeetaFace funnel cascade) on the gray frame;
//! once a face is found, the next frame searches only a region around it, which
//! is what keeps the cost inside the budget. A blank frame (privacy switch: the
//! controller serves flat frames) is never searched and yields no face.

use anyhow::{Context, Result};
use rustface::{Detector, ImageData};

/// The model: `DJ_SENSE_MODEL` (the Nix package sets it to the fetched
/// seeta_fd_frontal_v1.0.bin).
pub fn model_path() -> Option<String> {
    std::env::var("DJ_SENSE_MODEL").ok().filter(|p| std::path::Path::new(p).is_file())
}

#[derive(Debug, Clone, Copy)]
pub struct FaceBox {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

impl FaceBox {
    pub fn centre_y(&self) -> f64 {
        self.y + self.h / 2.0
    }
}

/// Luma standard deviation under which a frame counts as blank.
const BLANK_STD: f64 = 3.0;

pub struct Tracker {
    det: Box<dyn Detector>,
    last: Option<FaceBox>,
    roi: Vec<u8>,
    /// Frames resolved by the tracked region, and full-frame scans.
    pub tracked: usize,
    pub full: usize,
}

impl Tracker {
    pub fn new(model: &str) -> Result<Self> {
        let mut det = rustface::create_detector(model).with_context(|| format!("loading face model {model}"))?;
        det.set_min_face_size(40);
        det.set_score_thresh(2.0);
        det.set_pyramid_scale_factor(0.8);
        det.set_slide_window_step(4, 4);
        Ok(Tracker { det, last: None, roi: Vec::new(), tracked: 0, full: 0 })
    }

    pub fn is_blank(frame: &[u8]) -> bool {
        let n = frame.len().max(1) as f64;
        let m = frame.iter().map(|&v| v as f64).sum::<f64>() / n;
        let var = frame.iter().map(|&v| (v as f64 - m).powi(2)).sum::<f64>() / n;
        var.sqrt() < BLANK_STD
    }

    /// The face in this frame, if any. Largest box wins (the person at the laptop).
    pub fn find(&mut self, frame: &[u8], w: usize, h: usize) -> Option<FaceBox> {
        if Self::is_blank(frame) {
            self.last = None;
            return None;
        }
        if let Some(prev) = self.last {
            if let Some(b) = self.find_near(frame, w, h, prev) {
                self.last = Some(b);
                self.tracked += 1;
                return Some(b);
            }
        }
        self.full += 1;
        let b = best(self.det.detect(&ImageData::new(frame, w as u32, h as u32)));
        self.last = b;
        b
    }
}

impl Tracker {
    /// Search only a region around the last face, resampled so that face is about
    /// `TRACK_FACE_PX` tall: the cascade then scans a handful of small pyramid
    /// levels instead of the whole frame from 40 px faces up.
    fn find_near(&mut self, frame: &[u8], w: usize, h: usize, prev: FaceBox) -> Option<FaceBox> {
        let pad = 0.4 * prev.w.max(prev.h);
        let x0 = (prev.x - pad).max(0.0);
        let y0 = (prev.y - pad).max(0.0);
        let x1 = (prev.x + prev.w + pad).min(w as f64);
        let y1 = (prev.y + prev.h + pad).min(h as f64);
        let s = (TRACK_FACE_PX / prev.h).min(1.0);
        let (rw, rh) = (((x1 - x0) * s) as usize, ((y1 - y0) * s) as usize);
        if rw < 44 || rh < 44 {
            return None;
        }
        // Bilinear resample of the region.
        self.roi.clear();
        for ry in 0..rh {
            let fy = (y0 + (ry as f64 + 0.5) / s - 0.5).clamp(0.0, (h - 1) as f64);
            let (iy, ty) = (fy as usize, fy.fract());
            let iy1 = (iy + 1).min(h - 1);
            for rx in 0..rw {
                let fx = (x0 + (rx as f64 + 0.5) / s - 0.5).clamp(0.0, (w - 1) as f64);
                let (ix, tx) = (fx as usize, fx.fract());
                let ix1 = (ix + 1).min(w - 1);
                let p = |x: usize, y: usize| frame[y * w + x] as f64;
                let v = (p(ix, iy) * (1.0 - tx) + p(ix1, iy) * tx) * (1.0 - ty)
                    + (p(ix, iy1) * (1.0 - tx) + p(ix1, iy1) * tx) * ty;
                self.roi.push(v as u8);
            }
        }
        // Only the pyramid levels near the tracked size: faces up to 1.4x it.
        self.det.set_max_face_size((TRACK_FACE_PX * 1.4) as u32);
        let found = best(self.det.detect(&ImageData::new(&self.roi, rw as u32, rh as u32)));
        self.det.set_max_face_size(0);
        found.map(|b| FaceBox { x: x0 + b.x / s, y: y0 + b.y / s, w: b.w / s, h: b.h / s })
    }
}

/// Face height the tracking region is resampled to (the cascade window is 40).
const TRACK_FACE_PX: f64 = 48.0;

fn best(faces: Vec<rustface::FaceInfo>) -> Option<FaceBox> {
    faces
        .iter()
        .map(|f| {
            let r = f.bbox();
            FaceBox { x: r.x() as f64, y: r.y() as f64, w: r.width() as f64, h: r.height() as f64 }
        })
        .max_by(|a, b| (a.w * a.h).total_cmp(&(b.w * b.h)))
}

/// Head-y trace in face heights: box centre y divided by the median box height,
/// so the scale does not depend on distance to the camera.
pub fn trace(boxes: &[Option<FaceBox>]) -> Vec<Option<f64>> {
    let mut hs: Vec<f64> = boxes.iter().flatten().map(|b| b.h).collect();
    if hs.is_empty() {
        return vec![None; boxes.len()];
    }
    hs.sort_by(f64::total_cmp);
    let med = hs[hs.len() / 2].max(1.0);
    boxes.iter().map(|b| b.map(|b| b.centre_y() / med)).collect()
}
