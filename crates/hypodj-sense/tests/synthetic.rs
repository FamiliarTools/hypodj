//! End to end on synthetic VIDEO FILES: render frames in-process, encode them with
//! ffmpeg, then run the real decode + face detector + estimator on the file.
//!
//! Face fixture: tests/fixtures/face.pgm, a 160x200 grayscale downscale of
//! "Official Portrait of President Reagan 1981" (Wikimedia Commons,
//! https://commons.wikimedia.org/wiki/File:Official_Portrait_of_President_Reagan_1981.jpg),
//! a work of the US federal government and so in the PUBLIC DOMAIN. Everything
//! else in the clips (background, motion, noise) is generated here.
//!
//! Needs ffmpeg and the face model (DJ_SENSE_MODEL); skips cleanly without them,
//! as in a sandbox that provides neither.

use hypodj_sense::estimator::Verdict;
use hypodj_sense::{face, head_trace, score_trace, video};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const W: usize = 640;
const H: usize = 480;
const FPS: f64 = 15.0;
const SECS: f64 = 30.0;

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> f64 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        ((self.0 >> 11) as f64) / ((1u64 << 53) as f64)
    }
}

fn load_face() -> (Vec<u8>, usize, usize) {
    let raw = std::fs::read(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/face.pgm")).unwrap();
    // Binary PGM: "P5\n<w> <h>\n<max>\n" then w*h bytes (no comments in ours).
    let mut fields = Vec::new();
    let mut i = 0;
    while fields.len() < 4 {
        while raw[i].is_ascii_whitespace() {
            i += 1;
        }
        let s = i;
        while !raw[i].is_ascii_whitespace() {
            i += 1;
        }
        fields.push(String::from_utf8_lossy(&raw[s..i]).into_owned());
    }
    assert_eq!(fields[0], "P5");
    let (w, h): (usize, usize) = (fields[1].parse().unwrap(), fields[2].parse().unwrap());
    let px = raw[i + 1..i + 1 + w * h].to_vec();
    // Upscale 2x so the face is about 60 px tall after the 320x240 analysis scale.
    let mut up = vec![0u8; 4 * w * h];
    for y in 0..2 * h {
        for x in 0..2 * w {
            up[y * 2 * w + x] = px[(y / 2) * w + x / 2];
        }
    }
    (up, 2 * w, 2 * h)
}

/// Render a clip where the face patch's top-left is at `pos(t)` (None: blank frame).
fn render(name: &str, pos: impl Fn(f64) -> Option<(f64, f64)>) -> PathBuf {
    let (face, fw, fh) = load_face();
    let path = std::env::temp_dir().join(format!("dj-sense-{}-{name}.mkv", std::process::id()));
    let mut child = Command::new(video::ffmpeg())
        .args(["-nostdin", "-v", "error", "-y", "-f", "rawvideo", "-pix_fmt", "gray", "-s"])
        .arg(format!("{W}x{H}"))
        .args(["-r", "15", "-i", "pipe:0", "-c:v", "ffv1"])
        .arg(&path)
        .stdin(Stdio::piped())
        .spawn()
        .expect("ffmpeg");
    let mut stdin = child.stdin.take().unwrap();
    let mut rng = Rng(name.len() as u64 * 7919);
    let mut frame = vec![0u8; W * H];
    for i in 0..(SECS * FPS) as usize {
        let t = i as f64 / FPS;
        match pos(t) {
            None => frame.fill(0),
            Some((px, py)) => {
                let (ox, oy) = (px.round() as isize, py.round() as isize);
                for y in 0..H {
                    for x in 0..W {
                        // A soft room gradient plus sensor noise.
                        let mut v = 60.0 + 60.0 * x as f64 / W as f64 + 20.0 * y as f64 / H as f64;
                        let (fx, fy) = (x as isize - ox, y as isize - oy);
                        if fx >= 0 && fy >= 0 && (fx as usize) < fw && (fy as usize) < fh {
                            v = face[fy as usize * fw + fx as usize] as f64;
                        }
                        v += 8.0 * (rng.next() - 0.5);
                        frame[y * W + x] = v.clamp(0.0, 255.0) as u8;
                    }
                }
            }
        }
        stdin.write_all(&frame).unwrap();
    }
    drop(stdin);
    assert!(child.wait().unwrap().success());
    path
}

fn ready() -> Option<String> {
    let ff = Command::new(video::ffmpeg()).arg("-version").stdout(Stdio::null()).status().is_ok_and(|s| s.success());
    match (ff, face::model_path()) {
        (true, Some(m)) => Some(m),
        _ => {
            eprintln!("skipping: needs ffmpeg and DJ_SENSE_MODEL");
            None
        }
    }
}

const X0: f64 = 160.0;
const Y0: f64 = 40.0;

/// Typing at a laptop: mostly still, glances down at the keys and back at
/// irregular intervals, small shifts in between. Precomputed so it is a pure fn.
fn typing_path() -> Vec<(f64, f64)> {
    let mut r = Rng(42);
    let n = (SECS * FPS) as usize;
    let mut out = Vec::with_capacity(n);
    let (mut dy, mut dx, mut hold) = (0.0, 0.0, 0usize);
    for _ in 0..n {
        if hold == 0 {
            if dy == 0.0 && r.next() < 0.5 {
                dy = 10.0 + 20.0 * r.next(); // look down at the keys
            } else {
                dy = 0.0;
            }
            dx = 12.0 * (r.next() - 0.5);
            hold = 4 + (r.next() * 25.0) as usize; // 0.3 to 2 s
        }
        hold -= 1;
        out.push((X0 + dx + 2.0 * (r.next() - 0.5), Y0 + dy + 2.0 * (r.next() - 0.5)));
    }
    out
}

#[test]
fn synthetic_clips() {
    let Some(model) = ready() else { return };
    let amp = 16.0; // px at 640x480: about 0.13 face heights

    let bob = render("bob", |t| Some((X0, Y0 + amp * (2.0 * std::f64::consts::PI * 2.0 * t).sin())));
    let typing_pts = typing_path();
    let typing = render("typing", |t| Some(typing_pts[((t * FPS).round() as usize).min(typing_pts.len() - 1)]));
    let still = render("still", |_| Some((X0, Y0)));
    let blank = render("blank", |_| None);

    let run = |p: &Path| head_trace(p, &model).unwrap();
    let tb = run(&bob);
    let faces = tb.iter().flatten().count();
    assert!(faces * 10 >= tb.len() * 9, "face found in only {faces}/{} bob frames", tb.len());

    let r = score_trace(&tb, 120.0);
    let n = r.windows.len();
    assert!(r.count(Verdict::Lock) >= n - 1, "bob at 120 bpm: {:?}", r.verdicts());
    for s in &r.windows {
        assert!((s.peak_hz - 2.0).abs() <= 0.1, "recovered {} Hz", s.peak_hz);
    }
    let c = score_trace(&tb, 96.0);
    assert!(c.count(Verdict::Lock) <= 1, "bob vs wrong tempo: {:?}", c.verdicts());

    let ty = score_trace(&run(&typing), 120.0);
    assert!(ty.count(Verdict::Lock) <= 1, "typing: {:?}", ty.verdicts());
    let st = score_trace(&run(&still), 120.0);
    assert!(st.count(Verdict::Lock) <= 1, "still: {:?}", st.verdicts());
    assert!(st.count(Verdict::Still) >= n - 1, "still: {:?}", st.verdicts());
    let bl = score_trace(&run(&blank), 120.0);
    assert_eq!(bl.count(Verdict::Blind), bl.windows.len(), "blank: {:?}", bl.verdicts());
    assert!(hypodj_sense::estimator::suggestion(&bl.verdicts()).starts_with("blind"));

    eprintln!(
        "bob120 {}/{n} lock, bob96 {}/{n}, typing {}/{n}, still {}/{n}, blank blind {}/{n}",
        r.count(Verdict::Lock),
        c.count(Verdict::Lock),
        ty.count(Verdict::Lock),
        st.count(Verdict::Lock),
        bl.count(Verdict::Blind)
    );
    for p in [bob, typing, still, blank] {
        let _ = std::fs::remove_file(p);
    }
}
