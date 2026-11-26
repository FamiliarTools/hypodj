//! dj-sense FILE --bpm N: tempo-lock of the head in a video file. Prints one line
//! per 10 s window, a lock score, and a suggestion line. Reads files only.

use anyhow::{bail, Context, Result};
use hypodj_sense::{estimator, face, head_trace, score_trace, HOP_S, WINDOW_S};
use std::path::PathBuf;

const USAGE: &str = "usage: dj-sense VIDEO --bpm N [--model PATH]\n\
  Reads a video FILE (never a device), finds the face per frame, and scores\n\
  whether head-y moves at N/60 Hz (or 0.5x, 2x) against a wrong-tempo control.";

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let (mut path, mut bpm, mut model) = (None, None, face::model_path());
    while let Some(a) = args.next() {
        match a.as_str() {
            "--bpm" => bpm = Some(args.next().context("--bpm needs a value")?.parse::<f64>().context("--bpm")?),
            "--model" => model = Some(args.next().context("--model needs a path")?),
            "-h" | "--help" => {
                println!("{USAGE}");
                return Ok(());
            }
            _ if path.is_none() => path = Some(PathBuf::from(a)),
            _ => bail!("unexpected argument {a}\n{USAGE}"),
        }
    }
    let (Some(path), Some(bpm)) = (path, bpm) else { bail!("{USAGE}") };
    if !(bpm.is_finite() && bpm > 0.0) {
        bail!("--bpm must be positive");
    }
    let model = model.context("no face model: set DJ_SENSE_MODEL or pass --model")?;

    let trace = head_trace(&path, &model)?;
    let r = score_trace(&trace, bpm);
    println!(
        "dj-sense {}: {} frames, face in {}, bpm {bpm} (control {:.0}), {WINDOW_S} s windows every {HOP_S} s",
        path.display(),
        r.frames,
        r.faces,
        bpm * estimator::CONTROL_FACTOR
    );
    for (i, s) in r.windows.iter().enumerate() {
        println!(
            "  window {:>2} t={:>4.0}s  {:<7}  lock {:.2} at {}x  control {:.2}  peak {:.2} Hz  rms {:.3}  face {:.0}%",
            i,
            i as f64 * HOP_S,
            s.verdict.as_str(),
            s.lock,
            s.ratio,
            s.control,
            s.peak_hz,
            s.rms,
            s.coverage * 100.0
        );
    }
    let locked = r.count(estimator::Verdict::Lock);
    let best = r.windows.iter().map(|s| if s.verdict == estimator::Verdict::Lock { s.lock } else { 0.0 }).fold(0.0, f64::max);
    println!("lock: {locked}/{} windows, score {best:.2}", r.windows.len());
    if let Some((own, dec)) = cpu_seconds() {
        let secs = r.frames as f64 / hypodj_sense::video::FPS;
        println!(
            "cpu: {own:.2} s analysis + {dec:.2} s decoder for {secs:.0} s of video = {:.1}% + {:.1}% of one core",
            100.0 * own / secs,
            100.0 * dec / secs
        );
    }
    println!("{}", estimator::suggestion(&r.verdicts()));
    Ok(())
}

/// (own, reaped children) CPU seconds from /proc/self/stat: utime+stime and
/// cutime+cstime. The ffmpeg decoder is a child, reaped by the time this runs.
fn cpu_seconds() -> Option<(f64, f64)> {
    let stat = std::fs::read_to_string("/proc/self/stat").ok()?;
    // Fields after the parenthesised comm; utime is field 14 overall.
    let rest = &stat[stat.rfind(')')? + 2..];
    let f: Vec<f64> = rest.split_whitespace().skip(11).take(4).filter_map(|v| v.parse().ok()).collect();
    if f.len() < 4 {
        return None;
    }
    let hz = 100.0; // USER_HZ on Linux
    Some(((f[0] + f[1]) / hz, (f[2] + f[3]) / hz))
}
