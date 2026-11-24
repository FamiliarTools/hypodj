//! LISTEN: an objective, machine-judgeable stand-in for "does this transition sound
//! right", over the REAL mixed output of the two decks (captured from a private
//! PipeWire null sink by the transition witness, see `scripts/transition-listen`).
//!
//! "It sounds right" is decomposed into measurable properties, each with a threshold
//! grounded in a published source:
//!
//! - LEVEL CONTINUITY. The 50 ms RMS level relative to the steady level just before the
//!   trigger. The level JND for a steady sound is about 1 dB (Jesteadt, Wier & Green
//!   1977, "Intensity discrimination as a function of frequency and sensation level",
//!   JASA 61; Zwicker & Fastl, Psychoacoustics, ch. 7), so within +-1 dB is PASS: no
//!   listener can tell. +-3 dB is the dip of an equal-GAIN crossfade of uncorrelated
//!   signals (each leg at 0.5 amplitude sums to 0.5 power), the textbook audible
//!   failure that equal-POWER crossfading exists to remove; beyond it is FAIL, between
//!   the two is WARN.
//! - NO GAP. Gap detection for broadband sound is 2 to 3 ms (Plomp 1964, "Rate of decay
//!   of auditory sensation", JASA 36; Moore, An Introduction to the Psychology of
//!   Hearing, ch. 5), finer than the 10 ms analysis window, so ANY 10 ms window more
//!   than 40 dB under the reference inside a continuing transition is a gap: FAIL.
//! - NO CLICK. The fixtures are pure tones, whose second difference is bounded
//!   analytically: `A * (2 pi f / fs)^2`. A sample whose second difference exceeds 1.5x
//!   that bound for the loudest possible content (two full decks at the highest
//!   fixture frequency) carries energy the stimulus cannot contain - a step
//!   discontinuity, i.e. a click. Any click is FAIL. (No standard gives an audibility
//!   floor for an arbitrary step; this one flags steps from about 22 dB under the tone
//!   peak, all of which are plainly audible on a pure tone.)
//! - LOUDNESS CONTINUITY. Momentary loudness (400 ms, K-weighted, ITU-R BS.1770-5,
//!   EBU Tech 3341) before the trigger vs at the end of the window. EBU R128 accepts
//!   +-1 LU around a target where exact normalisation is impractical, so a step beyond
//!   1 LU between the old track and the new is FAIL.
//! - FADE-OUT (stop, pause, clear). Monotonic: no re-brightening by more than the 1 dB
//!   JND. Slewed: at least 250 ms from leaving -1 dB to reaching -40 dB, the startle
//!   floor of the hypodj fade spec (acoustic startle needs a rise faster than about
//!   140 to 220 ms; Memoria m:01kxhjqg). Silent after: nothing above -60 dB from 200 ms
//!   after the fade lands to the end of the window.
//!
//! This module is pure analysis (WAV in, rows and curves out) plus a tiny PNG writer
//! so a plot can be read back by an agent; the live capture lives in the witness.

use std::fmt::Write as _;

pub(crate) const FS: u32 = 48_000;
/// Analysis grid and windows.
const HOP_MS: f64 = 50.0;
const RMS_MS: f64 = 50.0;
const GAP_WIN_MS: f64 = 10.0;
const MOMENTARY_MS: f64 = 400.0;

pub(crate) const JND_DB: f64 = 1.0;
pub(crate) const EQUAL_GAIN_DIP_DB: f64 = 3.0;
pub(crate) const GAP_FLOOR_DB: f64 = -40.0;
pub(crate) const LOUDNESS_TOL_LU: f64 = 1.0;
pub(crate) const STARTLE_FLOOR_MS: f64 = 250.0;
pub(crate) const TAIL_FLOOR_DB: f64 = -60.0;
/// The reference must be a real signal, or the run is not a measurement.
const REF_MIN_DBFS: f64 = -70.0;

/// Interleaved-or-not, a capture reduced to its first two channels.
pub(crate) struct Wav {
    pub l: Vec<f32>,
    pub r: Vec<f32>,
}

/// Parse a RIFF/WAVE file (pw-record writes 32-bit float, possibly extensible, and may
/// leave the data size unfinalised): f32 or s16, any channel count, 48 kHz only.
pub(crate) fn read_wav(b: &[u8]) -> Result<Wav, String> {
    if b.len() < 12 || &b[0..4] != b"RIFF" || &b[8..12] != b"WAVE" {
        return Err("not a RIFF/WAVE file".into());
    }
    let u16_at = |i: usize| u16::from_le_bytes([b[i], b[i + 1]]);
    let u32_at = |i: usize| u32::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]]);
    let mut i = 12;
    let mut fmt: Option<(u16, u16, u32, u16)> = None;
    while i + 8 <= b.len() {
        let id = &b[i..i + 4];
        let size = u32_at(i + 4) as usize;
        let body = i + 8;
        if id == b"fmt " && body + 16 <= b.len() {
            fmt = Some((u16_at(body), u16_at(body + 2), u32_at(body + 4), u16_at(body + 14)));
        } else if id == b"data" {
            let (tag, ch, fs, bits) = fmt.ok_or("data before fmt")?;
            if fs != FS {
                return Err(format!("sample rate {fs}, analysis needs {FS}"));
            }
            let end = if size == 0 || size == u32::MAX as usize || body + size > b.len() {
                b.len()
            } else {
                body + size
            };
            let data = &b[body..end];
            let ch = ch.max(1) as usize;
            let float = bits == 32 && (tag == 3 || tag == 0xFFFE);
            let width = if float { 4 } else if bits == 16 { 2 } else {
                return Err(format!("unsupported sample format tag {tag} bits {bits}"));
            };
            let frames = data.len() / (width * ch);
            let mut l = Vec::with_capacity(frames);
            let mut r = Vec::with_capacity(frames);
            let at = |f: usize, c: usize| -> f32 {
                let o = (f * ch + c) * width;
                if float {
                    f32::from_le_bytes([data[o], data[o + 1], data[o + 2], data[o + 3]])
                } else {
                    i16::from_le_bytes([data[o], data[o + 1]]) as f32 / 32768.0
                }
            };
            for f in 0..frames {
                l.push(at(f, 0));
                r.push(at(f, if ch > 1 { 1 } else { 0 }));
            }
            return Ok(Wav { l, r });
        }
        i = body + size + (size & 1);
    }
    Err("no data chunk".into())
}

fn db(p: f64) -> f64 {
    10.0 * p.max(1e-20).log10()
}

fn ms_to_n(ms: f64) -> usize {
    (ms * FS as f64 / 1000.0).round() as usize
}

/// Mean square of `x[a..b]` (clamped).
fn mean_sq(x: &[f32], a: usize, b: usize) -> f64 {
    let b = b.min(x.len());
    if a >= b {
        return 0.0;
    }
    x[a..b].iter().map(|v| (*v as f64) * (*v as f64)).sum::<f64>() / (b - a) as f64
}

/// One biquad, direct form I.
fn biquad(x: &[f64], b: [f64; 3], a: [f64; 3]) -> Vec<f64> {
    let (mut x1, mut x2, mut y1, mut y2) = (0.0, 0.0, 0.0, 0.0);
    x.iter()
        .map(|&x0| {
            let y0 = b[0] * x0 + b[1] * x1 + b[2] * x2 - a[1] * y1 - a[2] * y2;
            x2 = x1;
            x1 = x0;
            y2 = y1;
            y1 = y0;
            y0
        })
        .collect()
}

/// The BS.1770 K-weighting filter (pre-filter shelf + RLB high-pass), with the
/// standard's published 48 kHz coefficients.
pub(crate) fn k_weight(x: &[f32]) -> Vec<f64> {
    let x: Vec<f64> = x.iter().map(|v| *v as f64).collect();
    let s1 = biquad(
        &x,
        [1.53512485958697, -2.69169618940638, 1.19839281085285],
        [1.0, -1.69065929318241, 0.73248077421585],
    );
    biquad(&s1, [1.0, -2.0, 1.0], [1.0, -1.99004745483398, 0.99007225036621])
}

/// Momentary loudness (LUFS) of the 400 ms window ENDING at sample `end`.
fn momentary(kl: &[f64], kr: &[f64], end: usize) -> f64 {
    let n = ms_to_n(MOMENTARY_MS);
    let a = end.saturating_sub(n);
    let b = end.min(kl.len());
    if b <= a {
        return f64::NEG_INFINITY;
    }
    let ms = |k: &[f64]| k[a..b].iter().map(|v| v * v).sum::<f64>() / (b - a) as f64;
    -0.691 + db(ms(kl) + ms(kr))
}

/// What a cause is SUPPOSED to sound like, decided by the cause, never by the capture.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum Expect {
    /// Playback goes on (possibly to another source): continuous level, no gap.
    Continue,
    /// The audible source must not change at all.
    Steady,
    /// A deliberate stop: slewed, monotonic fade to silence, then nothing.
    FadeOut,
}

impl Expect {
    fn name(self) -> &'static str {
        match self {
            Expect::Continue => "continue",
            Expect::Steady => "steady",
            Expect::FadeOut => "fade-out",
        }
    }
}

/// One transition window to analyse: where the trigger sits in the capture.
pub(crate) struct Window {
    pub trigger: usize,
    pub pre_ms: f64,
    pub post_ms: f64,
    /// Highest tone frequency the fixture can play (click bound).
    pub f_max: f64,
}

#[derive(Debug, Clone)]
pub(crate) struct Curve {
    /// ms relative to the trigger.
    pub t: Vec<f64>,
    /// 50 ms RMS (both channels), dB relative to the pre-trigger reference.
    pub rms_rel: Vec<f64>,
    /// Momentary loudness, LU relative to the pre-trigger momentary loudness.
    pub m_rel: Vec<f64>,
    /// 10 ms RMS relative to the reference, at a 10 ms hop (for gaps).
    pub fine_t: Vec<f64>,
    pub fine_rel: Vec<f64>,
    /// ms (rel. trigger) of each detected click.
    pub clicks: Vec<f64>,
}

#[derive(Debug, Clone)]
pub(crate) struct AudioRow {
    pub cause: String,
    pub expect: Expect,
    pub ref_dbfs: f64,
    pub max_dip_db: f64,
    pub max_over_db: f64,
    pub gap_ms: f64,
    pub clicks: usize,
    pub pre_clicks: usize,
    pub m_pre_lufs: f64,
    pub m_post_lufs: f64,
    pub loud_step_lu: f64,
    pub m_min_rel_lu: f64,
    pub fade_ms: f64,
    pub rebright_db: f64,
    pub tail_db: f64,
    pub verdict: &'static str,
    pub reasons: Vec<String>,
}

pub(crate) const AUDIO_HEADER: &str = "cause\texpect\tverdict\tref_dbfs\tmax_dip_db\tmax_over_db\tgap_ms\tclicks\tpre_clicks\tm_pre_lufs\tm_post_lufs\tloud_step_lu\tm_min_rel_lu\tfade_ms\trebright_db\ttail_db\treasons";

fn f1(v: f64) -> String {
    if v.is_finite() { format!("{v:.1}") } else { "-".into() }
}

impl AudioRow {
    fn fail(&mut self, why: String) {
        self.verdict = "FAIL";
        self.reasons.push(why);
    }

    pub(crate) fn tsv(&self) -> String {
        format!(
            "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
            self.cause,
            self.expect.name(),
            self.verdict,
            f1(self.ref_dbfs),
            f1(self.max_dip_db),
            f1(self.max_over_db),
            f1(self.gap_ms),
            self.clicks,
            self.pre_clicks,
            f1(self.m_pre_lufs),
            f1(self.m_post_lufs),
            f1(self.loud_step_lu),
            f1(self.m_min_rel_lu),
            f1(self.fade_ms),
            f1(self.rebright_db),
            f1(self.tail_db),
            if self.reasons.is_empty() { "-".to_string() } else { self.reasons.join("; ") }
        )
    }
}

/// Analyse one window of a capture against what the cause is supposed to sound like.
pub(crate) fn analyse(cause: &str, expect: Expect, w: &Wav, win: &Window) -> (AudioRow, Curve) {
    let n_pre = ms_to_n(win.pre_ms);
    let start = win.trigger.saturating_sub(n_pre);
    let end = (win.trigger + ms_to_n(win.post_ms)).min(w.l.len());
    let t_of = |i: usize| (i as f64 - win.trigger as f64) * 1000.0 / FS as f64;
    let power = |a: usize, b: usize| (mean_sq(&w.l, a, b) + mean_sq(&w.r, a, b)) / 2.0;

    // Reference: the steady level over the pre window (skipping the last 50 ms, so a
    // trigger landing a hair early cannot pull it down).
    let ref_end = win.trigger.saturating_sub(ms_to_n(50.0)).max(start + 1);
    let ref_p = power(start, ref_end);
    let ref_dbfs = db(ref_p);

    let kl = k_weight(&w.l);
    let kr = k_weight(&w.r);
    let m_pre = momentary(&kl, &kr, win.trigger);

    let mut c = Curve { t: vec![], rms_rel: vec![], m_rel: vec![], fine_t: vec![], fine_rel: vec![], clicks: vec![] };
    let hop = ms_to_n(HOP_MS);
    let rms_n = ms_to_n(RMS_MS);
    let mut i = start;
    while i + rms_n <= end {
        c.t.push(t_of(i + rms_n));
        c.rms_rel.push(db(power(i, i + rms_n)) - ref_dbfs);
        c.m_rel.push(momentary(&kl, &kr, i + rms_n) - m_pre);
        i += hop;
    }
    let fine = ms_to_n(GAP_WIN_MS);
    let mut i = start;
    while i + fine <= end {
        c.fine_t.push(t_of(i + fine));
        c.fine_rel.push(db(power(i, i + fine)) - ref_dbfs);
        i += fine;
    }

    // Clicks: second difference beyond what two full decks of the highest tone can do.
    let peak = 2.0 * (2.0 * ref_p).sqrt();
    let thr = 1.5 * peak * (2.0 * std::f64::consts::PI * win.f_max / FS as f64).powi(2);
    let merge = ms_to_n(10.0);
    let mut last: Option<usize> = None;
    for ch in [&w.l, &w.r] {
        for j in (start + 2).max(2)..end {
            let d2 = (ch[j] as f64 - 2.0 * ch[j - 1] as f64 + ch[j - 2] as f64).abs();
            if d2 > thr && last.is_none_or(|l| j > l + merge) {
                c.clicks.push(t_of(j));
                last = Some(j);
            } else if d2 > thr {
                last = Some(j);
            }
        }
        last = None;
    }
    c.clicks.sort_by(f64::total_cmp);
    c.clicks.dedup_by(|a, b| (*a - *b).abs() < 10.0);

    let after = |t: &[f64], v: &[f64], from: f64| -> Vec<f64> {
        t.iter().zip(v).filter(|(t, _)| **t > from).map(|(_, v)| *v).collect()
    };
    let rel_after = after(&c.t, &c.rms_rel, 0.0);
    let m_post = if end > 0 { momentary(&kl, &kr, end) } else { f64::NEG_INFINITY };
    let mut row = AudioRow {
        cause: cause.to_string(),
        expect,
        ref_dbfs,
        max_dip_db: rel_after.iter().copied().fold(0.0, f64::min),
        max_over_db: rel_after.iter().copied().fold(0.0, f64::max),
        gap_ms: 0.0,
        clicks: c.clicks.iter().filter(|t| **t >= 0.0).count(),
        pre_clicks: c.clicks.iter().filter(|t| **t < 0.0).count(),
        m_pre_lufs: m_pre,
        m_post_lufs: m_post,
        loud_step_lu: m_post - m_pre,
        m_min_rel_lu: after(&c.t, &c.m_rel, MOMENTARY_MS).into_iter().fold(0.0, f64::min),
        fade_ms: f64::NAN,
        rebright_db: f64::NAN,
        tail_db: f64::NAN,
        verdict: "PASS",
        reasons: vec![],
    };
    if ref_dbfs < REF_MIN_DBFS {
        row.verdict = "INVALID";
        row.reasons.push(format!("nothing audible before the trigger ({ref_dbfs:.1} dBFS)"));
        return (row, c);
    }
    if row.clicks > 0 {
        row.fail(format!("{} click(s) at {:?} ms", row.clicks, c.clicks.iter().filter(|t| **t >= 0.0).map(|t| t.round() as i64).collect::<Vec<_>>()));
    }
    if row.pre_clicks > 0 {
        row.reasons.push(format!("{} click(s) BEFORE the trigger (control window)", row.pre_clicks));
    }
    match expect {
        Expect::Continue | Expect::Steady => {
            row.gap_ms = after(&c.fine_t, &c.fine_rel, 0.0)
                .iter()
                .filter(|v| **v < GAP_FLOOR_DB)
                .count() as f64
                * GAP_WIN_MS;
            if row.gap_ms > 0.0 {
                row.fail(format!("gap {:.0} ms below {GAP_FLOOR_DB} dB", row.gap_ms));
            }
            let dev = (-row.max_dip_db).max(row.max_over_db);
            if dev > EQUAL_GAIN_DIP_DB {
                row.fail(format!("level deviates {dev:.1} dB (> {EQUAL_GAIN_DIP_DB} dB equal-gain dip)"));
            } else if dev > JND_DB && row.verdict == "PASS" {
                row.verdict = "WARN";
                row.reasons.push(format!("level deviates {dev:.1} dB (> {JND_DB} dB JND)"));
            }
            if row.loud_step_lu.abs() > LOUDNESS_TOL_LU || !row.loud_step_lu.is_finite() {
                row.fail(format!("loudness step {:.1} LU (> {LOUDNESS_TOL_LU} LU)", row.loud_step_lu));
            }
        }
        Expect::FadeOut => {
            // The 10 ms grid: a cut to silence must read as ~0 ms, not one 50 ms window.
            let pts: Vec<(f64, f64)> =
                c.fine_t.iter().zip(&c.fine_rel).filter(|(t, _)| **t > 0.0).map(|(t, v)| (*t, *v)).collect();
            // Landed: the first point under the gap floor after which nothing rises above it.
            let land = (0..pts.len()).find(|&k| pts[k..].iter().all(|(_, v)| *v < GAP_FLOOR_DB));
            match land {
                None => row.fail(format!("never fell below {GAP_FLOOR_DB} dB")),
                Some(k) => {
                    let t_land = pts[k].0;
                    // Leaving and re-brightening read the 50 ms curve: a 10 ms window
                    // holds only ~2 cycles of the lowest tone and ripples near the JND.
                    let coarse: Vec<(f64, f64)> = c.t.iter().zip(&c.rms_rel)
                        .filter(|(t, _)| **t > 0.0 && **t <= t_land + RMS_MS)
                        .map(|(t, v)| (*t, *v)).collect();
                    let t_leave = coarse.iter().find(|(_, v)| *v < -JND_DB).map(|p| p.0 - RMS_MS).unwrap_or(t_land).min(t_land);
                    // Credit back one window so a genuine fade is not under-read.
                    row.fade_ms = t_land - t_leave + GAP_WIN_MS;
                    let mut run_min = f64::INFINITY;
                    let mut rb: f64 = 0.0;
                    for (_, v) in &coarse {
                        rb = rb.max(v - run_min);
                        run_min = run_min.min(*v);
                    }
                    row.rebright_db = rb;
                    row.tail_db = pts
                        .iter()
                        .filter(|(t, _)| *t > t_land + 200.0)
                        .map(|p| p.1)
                        .fold(f64::NEG_INFINITY, f64::max);
                    if row.fade_ms < STARTLE_FLOOR_MS {
                        row.fail(format!("fade {:.0} ms (< {STARTLE_FLOOR_MS} ms startle floor)", row.fade_ms));
                    }
                    if rb > JND_DB {
                        row.fail(format!("re-brightens {rb:.1} dB during the fade"));
                    }
                    if row.tail_db > TAIL_FLOOR_DB {
                        row.fail(format!("audio after the fade ({:.1} dB)", row.tail_db));
                    }
                }
            }
        }
    }
    (row, c)
}

pub(crate) fn curve_tsv(c: &Curve) -> String {
    let mut s = String::from("t_ms\trms50_rel_db\tmomentary_rel_lu\n");
    for i in 0..c.t.len() {
        let _ = writeln!(s, "{:.0}\t{}\t{}", c.t[i], f1(c.rms_rel[i]), f1(c.m_rel[i]));
    }
    s
}

// ── a dependency-free PNG plot ──────────────────────────────────────────────

const PX_PER_MS: f64 = 0.2;
const DB_TOP: f64 = 6.0;
const DB_BOT: f64 = -66.0;
const PX_PER_DB: f64 = 4.0;

/// Plot one window: grey = 10 ms RMS, black = 50 ms RMS, green = momentary loudness
/// (all relative to the pre-trigger reference); blue vertical = trigger; light grid
/// every 6 dB; red band = +-1 dB (JND), orange lines = +-3 dB; red ticks on top =
/// clicks.
pub(crate) fn plot_png(c: &Curve, pre_ms: f64, post_ms: f64) -> Vec<u8> {
    let w = ((pre_ms + post_ms) * PX_PER_MS) as usize + 1;
    let h = ((DB_TOP - DB_BOT) * PX_PER_DB) as usize + 1;
    let mut img = vec![255u8; w * h * 3];
    let put = |img: &mut Vec<u8>, x: i64, y: i64, rgb: [u8; 3]| {
        if x >= 0 && y >= 0 && (x as usize) < w && (y as usize) < h {
            let o = (y as usize * w + x as usize) * 3;
            img[o..o + 3].copy_from_slice(&rgb);
        }
    };
    let xof = |t: f64| ((t + pre_ms) * PX_PER_MS).round() as i64;
    let yof = |v: f64| ((DB_TOP - v.clamp(DB_BOT, DB_TOP)) * PX_PER_DB).round() as i64;
    for x in 0..w as i64 {
        for y in yof(JND_DB)..=yof(-JND_DB) {
            put(&mut img, x, y, [255, 225, 225]);
        }
        let mut g = DB_TOP;
        while g >= DB_BOT {
            put(&mut img, x, yof(g), [215, 215, 215]);
            g -= 6.0;
        }
        put(&mut img, x, yof(0.0), [150, 150, 150]);
        put(&mut img, x, yof(EQUAL_GAIN_DIP_DB), [255, 160, 0]);
        put(&mut img, x, yof(-EQUAL_GAIN_DIP_DB), [255, 160, 0]);
        put(&mut img, x, yof(GAP_FLOOR_DB), [255, 120, 120]);
    }
    for y in 0..h as i64 {
        put(&mut img, xof(0.0), y, [60, 90, 255]);
    }
    let line = |img: &mut Vec<u8>, ts: &[f64], vs: &[f64], rgb: [u8; 3], thick: i64| {
        for k in 1..ts.len() {
            let (x0, y0, x1, y1) = (xof(ts[k - 1]), yof(vs[k - 1]), xof(ts[k]), yof(vs[k]));
            let steps = (x1 - x0).abs().max((y1 - y0).abs()).max(1);
            for s in 0..=steps {
                let x = x0 + (x1 - x0) * s / steps;
                let y = y0 + (y1 - y0) * s / steps;
                for d in 0..thick {
                    put(img, x, y + d, rgb);
                }
            }
        }
    };
    line(&mut img, &c.fine_t, &c.fine_rel, [175, 175, 175], 1);
    line(&mut img, &c.t, &c.m_rel, [0, 160, 60], 2);
    line(&mut img, &c.t, &c.rms_rel, [0, 0, 0], 2);
    for t in &c.clicks {
        for y in 0..12 {
            put(&mut img, xof(*t), y, [230, 0, 0]);
            put(&mut img, xof(*t) + 1, y, [230, 0, 0]);
        }
    }
    encode_png(w, h, &img)
}

fn crc32(data: &[u8]) -> u32 {
    let mut c = 0xFFFF_FFFFu32;
    for &b in data {
        c ^= b as u32;
        for _ in 0..8 {
            c = if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 };
        }
    }
    !c
}

/// RGB8 PNG with a zlib stream of stored (uncompressed) deflate blocks.
fn encode_png(w: usize, h: usize, rgb: &[u8]) -> Vec<u8> {
    let mut raw = Vec::with_capacity(h * (w * 3 + 1));
    for y in 0..h {
        raw.push(0);
        raw.extend_from_slice(&rgb[y * w * 3..(y + 1) * w * 3]);
    }
    let mut z = vec![0x78, 0x01];
    let chunks: Vec<&[u8]> = raw.chunks(65_535).collect();
    for (i, ch) in chunks.iter().enumerate() {
        z.push(u8::from(i + 1 == chunks.len()));
        z.extend_from_slice(&(ch.len() as u16).to_le_bytes());
        z.extend_from_slice(&(!(ch.len() as u16)).to_le_bytes());
        z.extend_from_slice(ch);
    }
    let (mut a, mut b) = (1u32, 0u32);
    for &x in &raw {
        a = (a + x as u32) % 65_521;
        b = (b + a) % 65_521;
    }
    z.extend_from_slice(&((b << 16) | a).to_be_bytes());
    let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
    let chunk = |out: &mut Vec<u8>, kind: &[u8], body: &[u8]| {
        out.extend_from_slice(&(body.len() as u32).to_be_bytes());
        let mut c = kind.to_vec();
        c.extend_from_slice(body);
        out.extend_from_slice(&c);
        out.extend_from_slice(&crc32(&c).to_be_bytes());
    };
    let mut ihdr = Vec::new();
    ihdr.extend_from_slice(&(w as u32).to_be_bytes());
    ihdr.extend_from_slice(&(h as u32).to_be_bytes());
    ihdr.extend_from_slice(&[8, 2, 0, 0, 0]);
    chunk(&mut out, b"IHDR", &ihdr);
    chunk(&mut out, b"IDAT", &z);
    chunk(&mut out, b"IEND", &[]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const AMP: f64 = 0.4;

    /// A synthetic capture: `gain_a(t)` on a 330 Hz tone plus `gain_b(t)` on 550 Hz,
    /// both channels, trigger at 1 s, 4 s long.
    fn synth(ga: impl Fn(f64) -> f64, gb: impl Fn(f64) -> f64) -> Wav {
        let n = FS as usize * 4;
        let mut l = Vec::with_capacity(n);
        for i in 0..n {
            let t = i as f64 / FS as f64;
            let tt = t - 1.0;
            let v = ga(tt) * AMP * (2.0 * std::f64::consts::PI * 330.0 * t).sin()
                + gb(tt) * AMP * (2.0 * std::f64::consts::PI * 550.0 * t).sin();
            l.push(v as f32);
        }
        Wav { r: l.clone(), l }
    }

    fn win() -> Window {
        Window { trigger: FS as usize, pre_ms: 400.0, post_ms: 2900.0, f_max: 1210.0 }
    }

    /// Crossfade over [0.2, 1.7] s with a given leg law.
    fn xf(law: fn(f64) -> (f64, f64)) -> Wav {
        let x = |t: f64| ((t - 0.2) / 1.5).clamp(0.0, 1.0);
        synth(move |t| law(x(t)).0, move |t| law(x(t)).1)
    }

    #[test]
    fn an_equal_power_crossfade_passes() {
        let w = xf(|x| ((x * std::f64::consts::FRAC_PI_2).cos(), (x * std::f64::consts::FRAC_PI_2).sin()));
        let (r, _) = analyse("xf", Expect::Continue, &w, &win());
        assert_eq!(r.verdict, "PASS", "{r:?}");
        assert!(r.max_dip_db > -0.5, "{r:?}");
        assert_eq!(r.clicks, 0);
    }

    #[test]
    fn an_equal_gain_crossfade_dips_3_db_and_is_flagged() {
        let w = xf(|x| (1.0 - x, x));
        let (r, _) = analyse("xf", Expect::Continue, &w, &win());
        assert!((r.max_dip_db + 3.0).abs() < 0.4, "{r:?}");
        assert_ne!(r.verdict, "PASS", "{r:?}");
    }

    #[test]
    fn a_gap_and_its_hard_edges_fail() {
        // Cut a quarter cycle past a zero crossing, i.e. at the tone's peak.
        let w = synth(|t| if t < 0.5 + 0.25 / 330.0 { 1.0 } else { 0.0 }, |t| if t >= 0.6 { 1.0 } else { 0.0 });
        let (r, _) = analyse("gap", Expect::Continue, &w, &win());
        assert_eq!(r.verdict, "FAIL");
        assert!(r.gap_ms >= 90.0, "{r:?}");
        assert!(r.clicks >= 1, "a hard cut mid-cycle is a click: {r:?}");
    }

    #[test]
    fn a_steady_tone_has_no_clicks_and_no_deviation() {
        let w = synth(|_| 1.0, |_| 0.0);
        let (r, _) = analyse("steady", Expect::Steady, &w, &win());
        assert_eq!(r.verdict, "PASS", "{r:?}");
        assert_eq!(r.clicks + r.pre_clicks, 0);
        assert!(r.loud_step_lu.abs() < 0.05);
    }

    #[test]
    fn a_slewed_monotonic_fade_passes_and_a_snap_or_rebound_fails() {
        // Linear-in-dB fade 0 -> -60 dB over 500 ms, then true silence.
        let fade = |t: f64| if t < 0.0 { 1.0 } else if t < 0.5 { 10f64.powf(-3.0 * t / 0.5) } else { 0.0 };
        let (r, _) = analyse("fade", Expect::FadeOut, &synth(fade, |_| 0.0), &win());
        assert_eq!(r.verdict, "PASS", "{r:?}");
        assert!(r.fade_ms >= 250.0 && r.fade_ms < 700.0, "{r:?}");
        // A 50 ms snap to silence is under the startle floor.
        let snap = |t: f64| if t < 0.0 { 1.0 } else if t < 0.05 { 1.0 - t / 0.05 } else { 0.0 };
        let (r, _) = analyse("snap", Expect::FadeOut, &synth(snap, |_| 0.0), &win());
        assert_eq!(r.verdict, "FAIL", "{r:?}");
        // Sound coming back after the fade landed.
        let back = move |t: f64| if t > 1.5 && t < 2.0 { 0.5 } else { fade(t) };
        let (r, _) = analyse("back", Expect::FadeOut, &synth(back, |_| 0.0), &win());
        assert_eq!(r.verdict, "FAIL", "{r:?}");
    }

    #[test]
    fn k_weighting_reads_a_997_hz_full_scale_sine_at_minus_3_lufs() {
        // BS.1770 calibration: a 0 dBFS 997 Hz sine in ONE channel reads -3.01 LKFS.
        let n = FS as usize;
        let l: Vec<f32> = (0..n).map(|i| (2.0 * std::f64::consts::PI * 997.0 * i as f64 / FS as f64).sin() as f32).collect();
        let kl = k_weight(&l);
        let kr = vec![0.0; n];
        let m = momentary(&kl, &kr, n);
        assert!((m + 3.01).abs() < 0.05, "{m}");
    }

    #[test]
    fn the_wav_reader_round_trips_a_float_file_and_the_png_is_well_formed() {
        let mut b = b"RIFF\0\0\0\0WAVEfmt ".to_vec();
        b.extend_from_slice(&16u32.to_le_bytes());
        b.extend_from_slice(&3u16.to_le_bytes());
        b.extend_from_slice(&2u16.to_le_bytes());
        b.extend_from_slice(&FS.to_le_bytes());
        b.extend_from_slice(&(FS * 8).to_le_bytes());
        b.extend_from_slice(&8u16.to_le_bytes());
        b.extend_from_slice(&32u16.to_le_bytes());
        b.extend_from_slice(b"data");
        b.extend_from_slice(&0u32.to_le_bytes());
        for v in [0.5f32, -0.5, 0.25, -0.25] {
            b.extend_from_slice(&v.to_le_bytes());
        }
        let w = read_wav(&b).unwrap();
        assert_eq!(w.l, vec![0.5, 0.25]);
        assert_eq!(w.r, vec![-0.5, -0.25]);
        let png = plot_png(&Curve { t: vec![-100.0, 0.0], rms_rel: vec![0.0, -3.0], m_rel: vec![0.0, 0.0], fine_t: vec![], fine_rel: vec![], clicks: vec![5.0] }, 400.0, 600.0);
        assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
        assert_eq!(crc32(b"IEND"), 0xAE42_6082);
    }
}
