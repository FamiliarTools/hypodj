//! `hypodj-mir`: the offline audio-feature batch (docs/mir.md,
//! docs/design/audio-features/design.md).
//!
//! Mirror-first: it walks the offline store's sidecars READ-ONLY, analyses each
//! audio file whose feature file is missing or stale, and writes
//! `<out>/<song_id>.json` atomically. Killed mid-run = that song is simply not
//! written; the next run resumes there. A run over an up-to-date directory is a
//! no-op.
//!
//! The extractors run as CHILD processes (Essentia's classic
//! `streaming_extractor_music`, and optionally `tools/mir/mir_tf.py` for the
//! learned heads), so nothing AGPL or non-commercial is linked here. Concurrency
//! follows measured headroom ([`hypodj_core::features::workers_for`]) before
//! every track, and the whole process runs at nice 19.
//!
//! usage:
//!   hypodj-mir --store DIR --out DIR [--extractor PATH]
//!              [--models DIR --tf-python PATH --tf-script PATH]
//!              [--jobs N] [--limit N] [--allow-battery]

use hypodj_core::features::{
    self, ExtractorInfo, FeatureFile, Scalars, SourceKey, EXTRACTOR_NAME, EXTRACTOR_VERSION,
    FEATURES_SCHEMA,
};
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

const GIB: u64 = 1 << 30;
/// RAM left for everything else on the machine before a worker may start.
const RESERVE: u64 = 3 * GIB;
/// Measured peak RSS per worker: classic extractor 160-300 MB; with the TF heads
/// 2.1 GB on a 170 s track (bubble-gum, 2026-11-25), rounded up.
const PER_WORKER_CLASSIC: u64 = GIB / 2;
const PER_WORKER_TF: u64 = 3 * GIB;
/// Cores handed to one TF worker (it barely uses more, design measurement).
const TF_CPUS: usize = 2;
const YIELD_SLEEP: Duration = Duration::from_secs(15);

struct Args {
    store: PathBuf,
    out: PathBuf,
    extractor: String,
    tf: Option<(PathBuf, PathBuf, PathBuf)>, // (models, python, script)
    jobs: usize,
    limit: Option<usize>,
    allow_battery: bool,
}

fn usage() -> ! {
    eprintln!(
        "usage: hypodj-mir --store DIR --out DIR [--extractor PATH] [--models DIR --tf-python PATH --tf-script PATH] [--jobs N] [--limit N] [--allow-battery]"
    );
    std::process::exit(2)
}

fn parse_args() -> Args {
    let mut it = std::env::args().skip(1);
    let (mut store, mut out, mut models, mut py, mut script) = (None, None, None, None, None);
    let mut extractor = "streaming_extractor_music".to_string();
    let (mut jobs, mut limit, mut allow_battery) = (2usize, None, false);
    while let Some(a) = it.next() {
        let mut val = || it.next().unwrap_or_else(|| usage());
        match a.as_str() {
            "--store" => store = Some(PathBuf::from(val())),
            "--out" => out = Some(PathBuf::from(val())),
            "--extractor" => extractor = val(),
            "--models" => models = Some(PathBuf::from(val())),
            "--tf-python" => py = Some(PathBuf::from(val())),
            "--tf-script" => script = Some(PathBuf::from(val())),
            "--jobs" => jobs = val().parse().unwrap_or_else(|_| usage()),
            "--limit" => limit = Some(val().parse().unwrap_or_else(|_| usage())),
            "--allow-battery" => allow_battery = true,
            _ => usage(),
        }
    }
    let tf = match (models, py, script) {
        (Some(m), Some(p), Some(s)) => Some((m, p, s)),
        (None, None, None) => None,
        _ => {
            eprintln!("--models, --tf-python and --tf-script go together");
            usage()
        }
    };
    Args {
        store: store.unwrap_or_else(|| usage()),
        out: out.unwrap_or_else(|| usage()),
        extractor,
        tf,
        jobs: jobs.max(1),
        limit,
        allow_battery,
    }
}

/// The model stems the TF helper will use, derived from what is ON DISK (the
/// helper uses exactly the known files that exist), sorted - part of the
/// validity key, so adding a model re-analyses.
fn tf_models(dir: &Path) -> Vec<String> {
    const KNOWN: &[&str] = &[
        "danceability-discogs-effnet-1",
        "deam-msd-musicnn-2",
        "discogs-effnet-bs64-1",
        "mood_relaxed-discogs-effnet-1",
        "msd-musicnn-1",
        "voice_instrumental-discogs-effnet-1",
    ];
    let has = |s: &str| dir.join(format!("{s}.pb")).is_file();
    let mut v: Vec<String> = KNOWN
        .iter()
        .filter(|s| {
            let s = **s;
            if s.ends_with("discogs-effnet-1") {
                has(s) && has("discogs-effnet-bs64-1")
            } else if s == "msd-musicnn-1" || s == "deam-msd-musicnn-2" {
                has("msd-musicnn-1") && has("deam-msd-musicnn-2")
            } else {
                has(s)
            }
        })
        .map(|s| s.to_string())
        .collect();
    v.sort();
    v
}

struct Job {
    id: String,
    label: String,
    audio: PathBuf,
    source: SourceKey,
}

fn mem_available() -> u64 {
    std::fs::read_to_string("/proc/meminfo")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("MemAvailable:"))
                .and_then(|l| l.split_whitespace().nth(1))
                .and_then(|kb| kb.parse::<u64>().ok())
        })
        .map(|kb| kb * 1024)
        .unwrap_or(0)
}

fn load1() -> f64 {
    std::fs::read_to_string("/proc/loadavg")
        .ok()
        .and_then(|s| s.split_whitespace().next().and_then(|x| x.parse().ok()))
        .unwrap_or(f64::MAX)
}

/// True when a mains supply exists and reports offline.
fn on_battery() -> bool {
    let Ok(rd) = std::fs::read_dir("/sys/class/power_supply") else { return false };
    rd.flatten().any(|e| {
        let p = e.path();
        let read = |f: &str| std::fs::read_to_string(p.join(f)).unwrap_or_default();
        read("type").trim() == "Mains" && read("online").trim() == "0"
    })
}

fn collect_jobs(a: &Args, models: &[String]) -> (Vec<Job>, usize) {
    let mut jobs = Vec::new();
    let mut fresh = 0;
    let Ok(rd) = std::fs::read_dir(&a.store) else {
        eprintln!("cannot read store dir {}", a.store.display());
        std::process::exit(1)
    };
    let mut paths: Vec<PathBuf> = rd.flatten().map(|e| e.path()).collect();
    paths.sort();
    for p in paths {
        if p.extension().and_then(|x| x.to_str()) != Some("toml") {
            continue;
        }
        let Some(sc) = std::fs::read_to_string(&p)
            .ok()
            .and_then(|raw| hypodj_core::store::sidecar_from_toml(&raw))
        else {
            continue;
        };
        let id = sc.song.id.0.clone();
        let audio = a.store.join(format!("{id}.{}", sc.fingerprint.suffix));
        // The sidecar is the commit; an audio file of another length is mid-write
        // or damaged, never analysed.
        if std::fs::metadata(&audio).map(|m| m.len()).ok() != Some(sc.fingerprint.size) {
            continue;
        }
        let source = SourceKey::from(&sc.fingerprint);
        if features::read_file(&features::file_path(&a.out, &id))
            .is_some_and(|f| f.is_valid_for(&id, &source, models))
        {
            fresh += 1;
            continue;
        }
        let label = format!(
            "{} - {}",
            sc.song.artist.as_deref().unwrap_or("?"),
            sc.song.title
        );
        jobs.push(Job { id, label, audio, source });
    }
    (jobs, fresh)
}

fn analyse(a: &Args, models: &[String], tmp: &Path, worker: usize, job: &Job) -> Result<FeatureFile, String> {
    let raw_path = tmp.join(format!("{}.classic.json", job.id));
    let st = Command::new(&a.extractor)
        .arg(&job.audio)
        .arg(&raw_path)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|e| format!("cannot run {}: {e}", a.extractor))?;
    if !st.success() {
        return Err(format!("extractor exited {st}"));
    }
    let raw = std::fs::read_to_string(&raw_path).map_err(|e| e.to_string())?;
    let _ = std::fs::remove_file(&raw_path);
    let (mut scalars, timeline) =
        features::parse_classic(&raw).ok_or("unparseable extractor output")?;
    let mut embedding = None;
    if let Some((mdir, py, script)) = &a.tf {
        let out = Command::new(py)
            .arg("-I")
            .arg(script)
            .arg(mdir)
            .arg(&job.audio)
            .env("HYPODJ_MIR_CPUS", TF_CPUS.to_string())
            .env("HYPODJ_MIR_CPU_OFFSET", (worker * TF_CPUS).to_string())
            .stderr(Stdio::null())
            .output()
            .map_err(|e| format!("cannot run TF helper: {e}"))?;
        if !out.status.success() {
            return Err(format!("TF helper exited {}", out.status));
        }
        let tf = features::parse_tf(&String::from_utf8_lossy(&out.stdout))
            .ok_or("unparseable TF helper output")?;
        if tf.models != models {
            return Err(format!("TF helper used {:?}, expected {:?}", tf.models, models));
        }
        scalars = Scalars {
            voice: tf.voice,
            relaxed: tf.relaxed,
            danceable: tf.danceable,
            arousal: tf.arousal,
            valence: tf.valence,
            ..scalars
        };
        embedding = tf.embedding;
    }
    Ok(FeatureFile {
        schema: FEATURES_SCHEMA,
        song_id: job.id.clone(),
        source: job.source.clone(),
        extractor: ExtractorInfo {
            name: EXTRACTOR_NAME.into(),
            version: EXTRACTOR_VERSION.into(),
            models: models.to_vec(),
        },
        scalars,
        embedding,
        timeline,
    })
}

fn summary(ff: &FeatureFile) -> String {
    let s = &ff.scalars;
    let mut parts = Vec::new();
    if let Some(b) = s.bpm {
        parts.push(format!("bpm {b:.1}"));
    }
    if let (Some(k), Some(sc)) = (&s.key, &s.scale) {
        parts.push(format!("key {k} {sc}"));
    }
    if let Some(g) = s.replay_gain_db {
        parts.push(format!("rg {g:+.1} dB"));
    }
    if let Some(e) = ff.energy() {
        parts.push(format!("energy {e:.2}"));
    }
    if let Some(v) = s.voice {
        parts.push(format!("voice {v:.2}"));
    }
    if let Some(r) = s.relaxed {
        parts.push(format!("relaxed {r:.2}"));
    }
    if let (Some(a), Some(v)) = (s.arousal, s.valence) {
        parts.push(format!("arousal {a:.2} valence {v:.2}"));
    }
    parts.join(", ")
}

fn rusage_children() -> (f64, u64) {
    // SAFETY: getrusage writes into the zeroed struct we own.
    let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
    unsafe { libc::getrusage(libc::RUSAGE_CHILDREN, &mut ru) };
    let t = |tv: libc::timeval| tv.tv_sec as f64 + tv.tv_usec as f64 / 1e6;
    (t(ru.ru_utime) + t(ru.ru_stime), ru.ru_maxrss as u64 * 1024)
}

fn main() {
    let a = parse_args();
    // Background work: the whole tree (children inherit) at the lowest priority.
    // SAFETY: plain syscall on our own process.
    unsafe { libc::setpriority(libc::PRIO_PROCESS, 0, 19) };
    if !a.allow_battery && on_battery() {
        println!("hypodj-mir: on battery, deferring (pass --allow-battery to override)");
        return;
    }
    if let Err(e) = std::fs::create_dir_all(&a.out) {
        eprintln!("cannot create {}: {e}", a.out.display());
        std::process::exit(1);
    }
    let models = a.tf.as_ref().map(|(m, _, _)| tf_models(m)).unwrap_or_default();
    let (mut jobs, fresh) = collect_jobs(&a, &models);
    if let Some(n) = a.limit {
        jobs.truncate(n);
    }
    let total = jobs.len();
    println!(
        "hypodj-mir: {fresh} up to date, {total} to analyse, models [{}]",
        models.join(", ")
    );
    if total == 0 {
        return;
    }
    let tmp = std::env::temp_dir().join(format!("hypodj-mir-{}", std::process::id()));
    std::fs::create_dir_all(&tmp).expect("temp dir");
    let (cpus, per_mem) = if a.tf.is_some() { (TF_CPUS, PER_WORKER_TF) } else { (1, PER_WORKER_CLASSIC) };
    let cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1);
    let queue = Mutex::new(jobs.into_iter().collect::<VecDeque<_>>());
    let done = Mutex::new((0usize, 0usize, Duration::ZERO)); // (written, failed, wall sum)
    let started = Instant::now();
    std::thread::scope(|s| {
        for w in 0..a.jobs {
            let (a, models, tmp, queue, done) = (&a, &models, &tmp, &queue, &done);
            s.spawn(move || loop {
                // Measure BEFORE taking a track: worker w runs only while the
                // headroom allows more than w workers.
                loop {
                    let allowed = features::workers_for(
                        cores, load1(), cpus, mem_available(), RESERVE, per_mem, a.jobs,
                    );
                    if w < allowed {
                        break;
                    }
                    if queue.lock().unwrap().is_empty() {
                        return;
                    }
                    println!(
                        "hypodj-mir: worker {w} yielding (load {:.1}, {:.1} GiB free)",
                        load1(),
                        mem_available() as f64 / GIB as f64
                    );
                    std::thread::sleep(YIELD_SLEEP);
                }
                let Some(job) = queue.lock().unwrap().pop_front() else { return };
                let t0 = Instant::now();
                let res = analyse(a, models, tmp, w, &job).and_then(|ff| {
                    let bytes = serde_json::to_vec(&ff).map_err(|e| e.to_string())?;
                    hypodj_core::resume::atomic_write_bytes(
                        &features::file_path(&a.out, &job.id),
                        &bytes,
                    )
                    .map_err(|e| e.to_string())?;
                    Ok(ff)
                });
                let dt = t0.elapsed();
                let mut d = done.lock().unwrap();
                match res {
                    Ok(ff) => {
                        d.0 += 1;
                        d.2 += dt;
                        println!(
                            "[{}/{}] {} ({}) {:.1}s: {}",
                            d.0 + d.1,
                            total,
                            job.label,
                            job.id,
                            dt.as_secs_f64(),
                            summary(&ff)
                        );
                    }
                    Err(e) => {
                        d.1 += 1;
                        println!("[{}/{}] {} ({}) FAILED: {e}", d.0 + d.1, total, job.label, job.id);
                    }
                }
            });
        }
    });
    let _ = std::fs::remove_dir_all(&tmp);
    let (written, failed, wall) = *done.lock().unwrap();
    let (cpu, maxrss) = rusage_children();
    println!(
        "hypodj-mir: wrote {written}, failed {failed}; wall {:.1}s total, {:.1}s/track; child CPU {:.1}s ({:.1}s/track); peak child RSS {} MiB",
        started.elapsed().as_secs_f64(),
        wall.as_secs_f64() / written.max(1) as f64,
        cpu,
        cpu / (written + failed).max(1) as f64,
        maxrss >> 20
    );
    if failed > 0 {
        std::process::exit(1);
    }
}
