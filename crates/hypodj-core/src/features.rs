//! MEASURED per-track audio features: the on-disk schema the offline `hypodj-mir`
//! batch writes, the pure mappings from extractor output to it, and the
//! [`FileFeatureStore`] the daemon reads it back through
//! (docs/design/audio-features/design.md).
//!
//! The split mirrors the design's rule "the daemon only READS this directory, it
//! never runs a model": everything here is parsing, validity and lookup. The
//! extractors themselves (Essentia's classic `streaming_extractor_music`, and the
//! optional TensorFlow heads helper) run in a separate process driven by the
//! `hypodj-mir` bin, so neither AGPL code nor non-commercial weights are ever
//! linked into the daemon.
//!
//! Layout: one file per song, `<dir>/<song_id>.json`, written atomically. `<dir>`
//! is a SIBLING of the offline store's root, never inside it: the store reconciler
//! deletes every file it does not recognise.
//!
//! Validity key = (song id, source fingerprint, extractor version, model set). Any
//! mismatch means "analyse again"; a match is a no-op, so a killed run resumes at
//! the first song it had not yet written.

use crate::intelligence::{Coverage, FeatureStore, MetadataStore, TrackFeatures};
use crate::model::Song;
use crate::store::{Fingerprint, IndexEntry};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::ffi::OsString;
use std::io::Read;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, SystemTime};

/// On-disk schema version. A file with another value is ignored (re-analysed).
pub const FEATURES_SCHEMA: u32 = 1;
/// The extractor name recorded in every file.
pub const EXTRACTOR_NAME: &str = "hypodj-mir";
/// Bump whenever the MAPPING from raw extractor output to [`Scalars`] changes, so
/// every stored file reads stale and the next batch run recomputes it.
pub const EXTRACTOR_VERSION: &str = "1";
/// The "no voices" threshold (design decision 5): below this a track counts as
/// instrumental. 0.35..0.65 is the uncertain band.
pub const VOICE_INSTRUMENTAL_BELOW: f32 = 0.35;

/// The identity of the analysed bytes: the same `(size, suffix, created)` triple
/// the offline store keys its sidecars on, so staleness never needs a hash.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct SourceKey {
    pub size: u64,
    pub suffix: String,
    #[serde(default)]
    pub created: Option<String>,
}

impl From<&Fingerprint> for SourceKey {
    fn from(f: &Fingerprint) -> Self {
        SourceKey { size: f.size, suffix: f.suffix.clone(), created: f.created.clone() }
    }
}

/// Which extractor build and which model files produced a file.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct ExtractorInfo {
    pub name: String,
    pub version: String,
    /// Model names (file stems), sorted. Empty = classic DSP only.
    #[serde(default)]
    pub models: Vec<String>,
}

/// The per-song scalars. Every field is optional: a classic-only run has no
/// `voice`/`arousal`, a run with models has everything.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
pub struct Scalars {
    // Classic DSP (Essentia streaming_extractor_music).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bpm: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scale: Option<String>,
    /// ReplayGain-style gain in dB (positive = quiet track). Essentia 2.1_beta2's
    /// classic extractor has no EBU R128 integrated loudness, so this is the
    /// loudness measurement it offers; it is NOT LUFS and is not named so.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replay_gain_db: Option<f32>,
    /// Detrended-fluctuation danceability, about 0..3.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub danceability_dfa: Option<f32>,
    /// Onsets per second.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub onset_rate: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dynamic_complexity: Option<f32>,
    // Learned heads (Essentia TF models, optional).
    /// P(voice) from voice_instrumental-discogs-effnet.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub voice: Option<f32>,
    /// P(relaxed) from mood_relaxed-discogs-effnet.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relaxed: Option<f32>,
    /// P(danceable) from danceability-discogs-effnet.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub danceable: Option<f32>,
    /// DEAM arousal / valence, 1..9 scale (deam-msd-musicnn).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arousal: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub valence: Option<f32>,
}

/// Time-domain data for the transition planner. Not on [`TrackFeatures`] (the hot
/// path stays small).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
pub struct Timeline {
    #[serde(default)]
    pub beats: Vec<f32>,
}

/// One `<song_id>.json`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct FeatureFile {
    pub schema: u32,
    pub song_id: String,
    pub source: SourceKey,
    pub extractor: ExtractorInfo,
    pub scalars: Scalars,
    /// Mean-pooled discogs-effnet embedding (1280-d) when the models ran.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub embedding: Option<Vec<f32>>,
    #[serde(default)]
    pub timeline: Timeline,
}

impl FeatureFile {
    /// Is this file the current answer for `(song_id, source)` under `models`?
    /// The single validity rule the batch and the tests share. A file made with
    /// MORE models than asked for still counts (a classic-only run must never
    /// overwrite a learned-heads file with less); one missing a model does not.
    pub fn is_valid_for(&self, song_id: &str, source: &SourceKey, models: &[String]) -> bool {
        self.schema == FEATURES_SCHEMA
            && self.song_id == song_id
            && &self.source == source
            && self.extractor.name == EXTRACTOR_NAME
            && self.extractor.version == EXTRACTOR_VERSION
            && models.iter().all(|m| self.extractor.models.contains(m))
    }

    /// The `0..=1` energy this file measures, or `None` when it carries nothing
    /// energy-bearing. Precedence: DEAM arousal (a learned regression, the design's
    /// primary) over the classic proxy.
    ///
    /// The classic proxy is a DOCUMENTED combination of three measured signals,
    /// each normalised to 0..1 and averaged over those present:
    /// - loudness: `(6 - replay_gain_db) / 18` (a +6 dB-gain quiet ambient track
    ///   reads 0, a -12 dB-gain loud master reads 1);
    /// - danceability: `danceability_dfa / 3`;
    /// - density: `onset_rate / 6` (six onsets a second reads as fully busy).
    pub fn energy(&self) -> Option<f32> {
        let s = &self.scalars;
        if let Some(a) = s.arousal {
            return Some(unit((a - 1.0) / 8.0));
        }
        let parts: Vec<f32> = [
            s.replay_gain_db.map(|g| unit((6.0 - g) / 18.0)),
            s.danceability_dfa.map(|d| unit(d / 3.0)),
            s.onset_rate.map(|o| unit(o / 6.0)),
        ]
        .into_iter()
        .flatten()
        .collect();
        if parts.is_empty() {
            None
        } else {
            Some(parts.iter().sum::<f32>() / parts.len() as f32)
        }
    }

    /// DEAM valence mapped to `0..=1`, when the models ran.
    pub fn valence01(&self) -> Option<f32> {
        self.scalars.valence.map(|v| unit((v - 1.0) / 8.0))
    }
}

fn unit(x: f32) -> f32 {
    if x.is_finite() {
        x.clamp(0.0, 1.0)
    } else {
        0.5
    }
}

fn f(v: &serde_json::Value) -> Option<f32> {
    v.as_f64().map(|x| x as f32).filter(|x| x.is_finite())
}

/// Map Essentia's classic `streaming_extractor_music` JSON (2.1_beta2 layout) to
/// [`Scalars`] + beats. Missing fields stay `None`; a non-object is `None`.
pub fn parse_classic(raw: &str) -> Option<(Scalars, Timeline)> {
    let v: serde_json::Value = serde_json::from_str(raw).ok()?;
    let rhythm = v.get("rhythm")?;
    let tonal = v.get("tonal");
    let low = v.get("lowlevel");
    let props = v.get("metadata").and_then(|m| m.get("audio_properties"));
    let s = Scalars {
        bpm: rhythm.get("bpm").and_then(f).filter(|b| *b > 0.0),
        key: tonal.and_then(|t| t.get("key_key")).and_then(|k| k.as_str()).map(str::to_string),
        scale: tonal.and_then(|t| t.get("key_scale")).and_then(|k| k.as_str()).map(str::to_string),
        replay_gain_db: props.and_then(|p| p.get("replay_gain")).and_then(f),
        danceability_dfa: rhythm.get("danceability").and_then(f),
        onset_rate: rhythm.get("onset_rate").and_then(f),
        dynamic_complexity: low.and_then(|l| l.get("dynamic_complexity")).and_then(f),
        ..Scalars::default()
    };
    let beats = rhythm
        .get("beats_position")
        .and_then(|b| b.as_array())
        .map(|a| a.iter().filter_map(f).map(|x| (x * 1000.0).round() / 1000.0).collect())
        .unwrap_or_default();
    Some((s, Timeline { beats }))
}

/// The TF helper's one-line JSON (`tools/mir/mir_tf.py`).
#[derive(Deserialize, Debug, Default)]
pub struct TfOutput {
    #[serde(default)]
    pub voice: Option<f32>,
    #[serde(default)]
    pub relaxed: Option<f32>,
    #[serde(default)]
    pub danceable: Option<f32>,
    #[serde(default)]
    pub arousal: Option<f32>,
    #[serde(default)]
    pub valence: Option<f32>,
    #[serde(default)]
    pub embedding: Option<Vec<f32>>,
    #[serde(default)]
    pub models: Vec<String>,
}

/// The range rules, shared by the batch (which DROPS a bad value from the TF
/// helper) and the daemon (which REJECTS a file carrying one): the batch never
/// writes what the daemon would refuse, and a hand-edited or foreign file cannot
/// smuggle in an `inf` that `unit` would quietly read as energy 0.5.
fn prob_ok(v: f32) -> bool {
    v.is_finite() && (0.0..=1.0).contains(&v)
}
fn deam_ok(v: f32) -> bool {
    v.is_finite() && (0.0..=10.0).contains(&v)
}
fn embedding_ok(e: &[f32]) -> bool {
    !e.is_empty() && e.iter().all(|x| x.is_finite())
}

/// Parse the TF helper output; a probability outside `0..=1` or a non-finite
/// value is dropped rather than trusted.
pub fn parse_tf(raw: &str) -> Option<TfOutput> {
    let mut o: TfOutput = serde_json::from_str(raw.trim()).ok()?;
    o.voice = o.voice.filter(|v| prob_ok(*v));
    o.relaxed = o.relaxed.filter(|v| prob_ok(*v));
    o.danceable = o.danceable.filter(|v| prob_ok(*v));
    o.arousal = o.arousal.filter(|v| deam_ok(*v));
    o.valence = o.valence.filter(|v| deam_ok(*v));
    if o.embedding.as_ref().is_some_and(|e| !embedding_ok(e)) {
        o.embedding = None;
    }
    Some(o)
}

impl Scalars {
    /// Does every present value pass the same rules [`parse_tf`] and
    /// [`parse_classic`] apply on the way in? (JSON cannot spell `inf`, but a
    /// `1e39` deserialises to an `f32` infinity.)
    pub fn is_sane(&self) -> bool {
        let fin = |x: Option<f32>| x.is_none_or(f32::is_finite);
        fin(self.bpm)
            && self.bpm.is_none_or(|b| b > 0.0)
            && fin(self.replay_gain_db)
            && fin(self.danceability_dfa)
            && fin(self.onset_rate)
            && fin(self.dynamic_complexity)
            && [self.voice, self.relaxed, self.danceable].iter().all(|x| x.is_none_or(prob_ok))
            && [self.arousal, self.valence].iter().all(|x| x.is_none_or(deam_ok))
    }
}

/// The largest feature file the daemon will read. Measured, not picked: the
/// largest legitimate file is a 1280-d embedding plus a beat grid, and
/// `largest_legitimate_file_fits_the_cap` builds a deliberately oversized one (a
/// three-hour track at 300 bpm, every float at full printed width) and asserts
/// it stays under a quarter of this (about 0.6 MB). Anything bigger is not something `hypodj-mir` wrote.
pub const MAX_FEATURE_FILE: u64 = 4 << 20;

/// Read at most [`MAX_FEATURE_FILE`] bytes of a REGULAR file, in bounded time.
/// `O_NOFOLLOW` refuses a symlink, `O_NONBLOCK` makes opening a FIFO return at
/// once instead of waiting for a writer forever, and the `fstat` of the OPENED
/// descriptor (not the name, which can be swapped in between) rejects anything
/// that is not a regular file before a single byte is read.
fn read_bounded(path: &Path) -> Option<Vec<u8>> {
    let f = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .ok()?;
    let md = f.metadata().ok()?;
    if !md.is_file() || md.len() > MAX_FEATURE_FILE {
        return None;
    }
    let mut buf = Vec::with_capacity(md.len() as usize);
    f.take(MAX_FEATURE_FILE + 1).read_to_end(&mut buf).ok()?;
    (buf.len() as u64 <= MAX_FEATURE_FILE).then_some(buf)
}

/// Read one feature file; `None` for anything unreadable, unparseable, not a
/// regular file, oversized, of another schema, or carrying an out-of-range value.
pub fn read_file(path: &Path) -> Option<FeatureFile> {
    let raw = read_bounded(path)?;
    let ff: FeatureFile = serde_json::from_slice(&raw).ok()?;
    let sane = ff.scalars.is_sane() && ff.embedding.as_deref().is_none_or(embedding_ok);
    (ff.schema == FEATURES_SCHEMA && sane).then_some(ff)
}

/// The path of `song_id`'s file under `dir`.
pub fn file_path(dir: &Path, song_id: &str) -> PathBuf {
    dir.join(format!("{song_id}.json"))
}

/// How many analysis workers the measured headroom allows right now (the design's
/// "match force to headroom": a level computed from data, never a constant).
///
/// - CPU: cores not already busy, `cores - ceil(load1)`, each worker needing
///   `cpus_per_worker` of them;
/// - RAM: `mem_available` minus a `reserve` left for everything else, divided by
///   the measured per-worker peak;
/// - never more than `cap`.
///
/// `0` means "yield": the caller sleeps and measures again. Saturating
/// throughout, so no input can wrap.
pub fn workers_for(
    cores: usize,
    load1: f64,
    cpus_per_worker: usize,
    mem_available: u64,
    reserve: u64,
    per_worker_mem: u64,
    cap: usize,
) -> usize {
    let busy = if load1.is_finite() && load1 > 0.0 { load1.ceil() as usize } else { 0 };
    let by_cpu = cores.saturating_sub(busy) / cpus_per_worker.max(1);
    let by_mem = (mem_available.saturating_sub(reserve) / per_worker_mem.max(1)) as usize;
    by_cpu.min(by_mem).min(cap)
}

/// One directory entry as last seen: the identity of the bytes (inode, length,
/// mtime) and what they parsed to. An unchanged identity is never re-read; an
/// atomic rename always changes the inode, so a rewrite can never hide behind an
/// equal size and a coarse timestamp.
struct Seen {
    ino: u64,
    len: u64,
    mtime: Option<SystemTime>,
    /// `None` = read and rejected (kept so a bad file is not re-parsed forever).
    ff: Option<Arc<FeatureFile>>,
}

#[derive(Default)]
struct ScanState {
    /// The directory mtime of the last COMPLETE scan; `None` forces a rescan.
    dir_mtime: Option<SystemTime>,
    seen: HashMap<OsString, Seen>,
}

/// Directory mtimes this close to now are not trusted to mean "nothing changed
/// since": timestamps are kernel-tick coarse, so a rename landing in the same tick
/// as the scan's stat would otherwise leave the same mtime and be missed forever.
const MTIME_SETTLE: Duration = Duration::from_secs(2);

struct Inner {
    dir: PathBuf,
    /// The served snapshot: song id -> its valid, energy-bearing file. Readers
    /// clone the `Arc` out under a short lock; a reload swaps it whole.
    snap: RwLock<Arc<HashMap<String, Arc<FeatureFile>>>>,
    /// Serialises scans; held only by the (blocking) reloader, never by readers.
    scan: Mutex<ScanState>,
    /// A background reload is queued or running (so refreshes never pile up).
    inflight: AtomicBool,
    /// Files opened and parsed so far (the incremental scan's own receipt).
    reads: AtomicUsize,
}

impl Inner {
    fn reload(&self) {
        let mut st = self.scan.lock().unwrap();
        let dir_mtime = std::fs::metadata(&self.dir).and_then(|m| m.modified()).ok();
        if dir_mtime.is_some() && st.dir_mtime == dir_mtime {
            return;
        }
        let mut names: Vec<OsString> = match std::fs::read_dir(&self.dir) {
            Ok(rd) => rd.flatten().map(|e| e.file_name()).collect(),
            Err(_) => Vec::new(),
        };
        // `readdir` racing a rename-over is allowed to return NEITHER the old nor
        // the new entry (observed on btrfs: the replacing name lands at a new
        // index past the stream's end while the old one is deleted). So a name the
        // last scan held is never dropped on readdir's word alone: it is looked up
        // by path, and only a failed `lstat` below means it is really gone.
        let listed: std::collections::HashSet<OsString> = names.iter().cloned().collect();
        names.extend(st.seen.keys().filter(|n| !listed.contains(*n)).cloned());
        let mut seen = HashMap::new();
        for name in names {
            let Some(stem) = name.to_str().and_then(|n| n.strip_suffix(".json")) else {
                continue;
            };
            let path = self.dir.join(&name);
            // The name, NOT followed: a symlink, FIFO, socket or directory is
            // never opened (read_bounded re-checks the opened descriptor).
            let Ok(md) = std::fs::symlink_metadata(&path) else { continue };
            if !md.file_type().is_file() {
                continue;
            }
            let (ino, len, mtime) = (md.ino(), md.len(), md.modified().ok());
            let ff = match st.seen.remove(&name) {
                Some(old) if old.ino == ino && old.len == len && old.mtime == mtime => old.ff,
                _ => {
                    self.reads.fetch_add(1, Ordering::Relaxed);
                    read_file(&path)
                    // Keyed by NAME, like hypodj-mir's own lookup: a file whose
                    // stem is not its song id is not the answer for either id.
                    .filter(|ff| ff.song_id == stem)
                    .filter(|ff| ff.extractor.version == EXTRACTOR_VERSION)
                    // A file with no energy-bearing value measures nothing the
                    // daemon serves; it is not counted as analysed.
                    .filter(|ff| ff.energy().is_some())
                    .map(Arc::new)
                }
            };
            seen.insert(name, Seen { ino, len, mtime, ff });
        }
        let map: HashMap<String, Arc<FeatureFile>> = seen
            .values()
            .filter_map(|s| s.ff.as_ref().map(|ff| (ff.song_id.clone(), ff.clone())))
            .collect();
        *self.snap.write().unwrap() = Arc::new(map);
        st.seen = seen;
        let settled = dir_mtime
            .and_then(|m| SystemTime::now().duration_since(m).ok())
            .is_some_and(|age| age >= MTIME_SETTLE);
        st.dir_mtime = if settled { dir_mtime } else { None };
    }
}

/// The daemon's READ-ONLY view of the features directory. Implements
/// [`FeatureStore`]: a song with a valid measured file gets MEASURED energy (and
/// voice/bpm/embedding); every other song falls through to [`MetadataStore`], so
/// callers keep working on a library that is only partly analysed.
///
/// Reads never touch the disk; [`FeatureStore::refresh`] hands the scan to a
/// blocking-pool thread and returns at once, so a large or hostile directory can
/// never stall a tokio worker. The scan itself is incremental: one `stat` per
/// entry, and only entries whose identity moved are read and parsed.
pub struct FileFeatureStore {
    inner: Arc<Inner>,
}

impl FileFeatureStore {
    /// Open (and synchronously read) `dir`. A missing directory is an empty store,
    /// not an error: the batch may simply not have run yet. Blocking: call it off
    /// the runtime (the daemon uses `spawn_blocking`).
    pub fn open(dir: PathBuf) -> Self {
        let s = FileFeatureStore {
            inner: Arc::new(Inner {
                dir,
                snap: RwLock::new(Arc::new(HashMap::new())),
                scan: Mutex::new(ScanState::default()),
                inflight: AtomicBool::new(false),
                reads: AtomicUsize::new(0),
            }),
        };
        s.reload_now();
        s
    }

    pub fn dir(&self) -> &Path {
        &self.inner.dir
    }

    /// Scan now, on the calling thread (blocking). For startup and tests.
    pub fn reload_now(&self) {
        self.inner.reload();
    }

    /// Queue a scan on tokio's blocking pool and return immediately; without a
    /// runtime (a plain thread, a sync test) scan inline. At most one is queued.
    pub fn reload_in_background(&self) {
        let Ok(rt) = tokio::runtime::Handle::try_current() else {
            self.reload_now();
            return;
        };
        if self.inner.inflight.swap(true, Ordering::AcqRel) {
            return;
        }
        let inner = self.inner.clone();
        rt.spawn_blocking(move || {
            // Cleared BEFORE the scan: a change landing mid-scan queues another.
            inner.inflight.store(false, Ordering::Release);
            inner.reload();
        });
    }

    /// How many files the scans have opened and parsed in total.
    pub fn files_read(&self) -> usize {
        self.inner.reads.load(Ordering::Relaxed)
    }

    fn snapshot(&self) -> Arc<HashMap<String, Arc<FeatureFile>>> {
        self.inner.snap.read().unwrap().clone()
    }

    /// How many songs carry a valid, energy-bearing measured file right now
    /// (regardless of whether they are still in any mirror).
    pub fn count(&self) -> usize {
        self.snapshot().len()
    }

    /// The raw file for `id`, if loaded.
    pub fn get(&self, id: &str) -> Option<FeatureFile> {
        self.snapshot().get(id).map(|ff| (**ff).clone())
    }

    /// The measured features for `song`, or `None` when it has no file or the
    /// file describes different bytes (the song's size/suffix moved since).
    fn measured(&self, song: &Song) -> Option<TrackFeatures> {
        let snap = self.snapshot();
        let ff = snap.get(&song.id.0)?;
        if !describes(ff, song.size, song.suffix.as_deref()) {
            return None;
        }
        let energy = ff.energy()?;
        let valence = ff.valence01().unwrap_or_else(|| crate::intelligence::valence_score(song));
        Some(TrackFeatures {
            energy,
            valence,
            embedding: ff.embedding.clone(),
            voice: ff.scalars.voice,
            bpm: ff.scalars.bpm,
            measured: true,
        })
    }
}

/// Does `ff` describe these bytes? An unknown size/suffix does not disqualify it.
fn describes(ff: &FeatureFile, size: Option<u64>, suffix: Option<&str>) -> bool {
    size.is_none_or(|s| s == ff.source.size)
        && suffix.is_none_or(|x| x.eq_ignore_ascii_case(&ff.source.suffix))
}

impl FeatureStore for FileFeatureStore {
    fn features(&self, song: &Song) -> Option<TrackFeatures> {
        self.measured(song).or_else(|| MetadataStore.features(song))
    }

    fn refresh(&self) {
        self.reload_in_background();
    }

    fn measured_voices(&self) -> Vec<(String, f32)> {
        self.snapshot()
            .iter()
            .filter_map(|(id, ff)| ff.scalars.voice.map(|v| (id.clone(), v)))
            .collect()
    }

    fn measured_coverage(&self, mirror: Option<&[IndexEntry]>) -> Option<Coverage> {
        let snap = self.snapshot();
        let Some(entries) = mirror else {
            return Some(Coverage { measured: snap.len(), stale: 0 });
        };
        let mut c = Coverage::default();
        for e in entries {
            if let Some(ff) = snap.get(&e.id.0) {
                if describes(ff, Some(e.size), Some(e.suffix.as_str())) {
                    c.measured += 1;
                } else {
                    c.stale += 1;
                }
            }
        }
        Some(c)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mk(id: &str) -> Song {
        toml::from_str(&format!("id = \"{id}\"\ntitle = \"t\"")).unwrap()
    }

    const CLASSIC: &str = r#"{
      "lowlevel": {"average_loudness": 0.003, "dynamic_complexity": 11.48},
      "metadata": {"audio_properties": {"replay_gain": -6.13, "length": 170.3}},
      "rhythm": {"bpm": 123.7365, "danceability": 0.8677, "onset_rate": 2.44,
                 "beats_position": [0.4643, 0.9404, 1.42199]},
      "tonal": {"key_key": "C", "key_scale": "minor"}
    }"#;

    fn file(scalars: Scalars) -> FeatureFile {
        FeatureFile {
            schema: FEATURES_SCHEMA,
            song_id: "s1".into(),
            source: SourceKey { size: 10, suffix: "flac".into(), created: Some("t".into()) },
            extractor: ExtractorInfo {
                name: EXTRACTOR_NAME.into(),
                version: EXTRACTOR_VERSION.into(),
                models: vec![],
            },
            scalars,
            embedding: None,
            timeline: Timeline::default(),
        }
    }

    #[test]
    fn classic_maps_the_fields_the_design_names() {
        let (s, t) = parse_classic(CLASSIC).unwrap();
        assert!((s.bpm.unwrap() - 123.7365).abs() < 1e-3);
        assert_eq!(s.key.as_deref(), Some("C"));
        assert_eq!(s.scale.as_deref(), Some("minor"));
        assert!((s.replay_gain_db.unwrap() + 6.13).abs() < 1e-4);
        assert!((s.danceability_dfa.unwrap() - 0.8677).abs() < 1e-4);
        assert!((s.onset_rate.unwrap() - 2.44).abs() < 1e-4);
        assert_eq!(t.beats, vec![0.464, 0.94, 1.422], "beats rounded to the millisecond");
        assert!(s.voice.is_none() && s.arousal.is_none(), "classic never invents learned heads");
    }

    #[test]
    fn classic_garbage_and_missing_rhythm_are_none() {
        assert!(parse_classic("not json").is_none());
        assert!(parse_classic(r#"{"tonal":{}}"#).is_none());
        // A zero bpm is "no tempo found", never a tempo of 0.
        let (s, _) = parse_classic(r#"{"rhythm":{"bpm":0}}"#).unwrap();
        assert!(s.bpm.is_none());
    }

    #[test]
    fn tf_output_drops_out_of_range_values() {
        let o = parse_tf(r#"{"voice":0.72,"relaxed":1.7,"arousal":5.0,"valence":-3,"embedding":[],"models":["a"]}"#)
            .unwrap();
        assert_eq!(o.voice, Some(0.72));
        assert_eq!(o.relaxed, None, "a probability above 1 is not trusted");
        assert_eq!(o.arousal, Some(5.0));
        assert_eq!(o.valence, None);
        assert!(o.embedding.is_none(), "an empty embedding is no embedding");
        assert!(parse_tf("garbage").is_none());
    }

    #[test]
    fn energy_prefers_arousal_then_the_documented_classic_proxy() {
        let a = file(Scalars { arousal: Some(9.0), replay_gain_db: Some(20.0), ..Default::default() });
        assert_eq!(a.energy(), Some(1.0), "arousal wins over the classic proxy");
        let quiet = file(Scalars {
            replay_gain_db: Some(6.0),
            danceability_dfa: Some(0.0),
            onset_rate: Some(0.0),
            ..Default::default()
        });
        let loud = file(Scalars {
            replay_gain_db: Some(-12.0),
            danceability_dfa: Some(3.0),
            onset_rate: Some(6.0),
            ..Default::default()
        });
        assert_eq!(quiet.energy(), Some(0.0));
        assert_eq!(loud.energy(), Some(1.0));
        // Only the present parts are averaged.
        let one = file(Scalars { danceability_dfa: Some(1.5), ..Default::default() });
        assert_eq!(one.energy(), Some(0.5));
        assert_eq!(file(Scalars::default()).energy(), None);
    }

    #[test]
    fn validity_key_is_id_source_version_and_models() {
        let ff = file(Scalars::default());
        let src = ff.source.clone();
        assert!(ff.is_valid_for("s1", &src, &[]));
        assert!(!ff.is_valid_for("s2", &src, &[]), "another song");
        let moved = SourceKey { size: 11, ..src.clone() };
        assert!(!ff.is_valid_for("s1", &moved, &[]), "the bytes changed");
        assert!(!ff.is_valid_for("s1", &src, &["voice".into()]), "models became available");
        let mut rich = ff.clone();
        rich.extractor.models = vec!["voice".into()];
        assert!(rich.is_valid_for("s1", &src, &[]), "a classic-only run keeps a richer file");
        let mut old = ff.clone();
        old.extractor.version = "0".into();
        assert!(!old.is_valid_for("s1", &src, &[]), "an older mapping is recomputed");
    }

    #[test]
    fn store_serves_measured_and_falls_back_to_metadata() {
        let dir = std::env::temp_dir().join(format!("hypodj-feat-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let ff = file(Scalars { arousal: Some(3.0), voice: Some(0.1), bpm: Some(90.0), ..Default::default() });
        crate::resume::atomic_write_bytes(&file_path(&dir, "s1"), &serde_json::to_vec(&ff).unwrap())
            .unwrap();
        std::fs::write(dir.join("junk.json"), "{").unwrap();
        let store = FileFeatureStore::open(dir.clone());
        assert_eq!(store.count(), 1, "the corrupt file is skipped");

        let mut song = mk("s1");
        song.size = Some(10);
        let f = store.features(&song).unwrap();
        assert!(f.measured);
        assert_eq!(f.energy, 0.25);
        assert_eq!(f.voice, Some(0.1));
        assert_eq!(f.bpm, Some(90.0));

        // Different bytes on the server = the file is stale, metadata answers.
        song.size = Some(99);
        assert!(!store.features(&song).unwrap().measured);
        // An unanalysed song gets the metadata heuristic, never None.
        let other = mk("nope");
        assert!(!store.features(&other).unwrap().measured);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn workers_follow_measured_headroom() {
        const G: u64 = 1 << 30;
        // Idle 8-core box, 38 GiB free: the cap binds.
        assert_eq!(workers_for(8, 0.3, 2, 38 * G, 3 * G, 3 * G, 2), 2);
        // Busy CPU: 8 cores, load 6.2 -> 1 free core, 2 needed per worker -> yield.
        assert_eq!(workers_for(8, 6.2, 2, 38 * G, 3 * G, 3 * G, 2), 0);
        // Tight RAM: 5 GiB free, 3 reserved, 3 per worker -> yield.
        assert_eq!(workers_for(8, 0.0, 1, 5 * G, 3 * G, 3 * G, 4), 0);
        assert_eq!(workers_for(8, 0.0, 1, 7 * G, 1 * G, 3 * G, 4), 2);
        // Degenerate inputs never panic or wrap.
        assert_eq!(workers_for(0, f64::NAN, 0, 0, u64::MAX, 0, 4), 0);
    }

    #[test]
    fn missing_dir_is_an_empty_store() {
        let store = FileFeatureStore::open(PathBuf::from("/nonexistent/hypodj-features"));
        assert_eq!(store.count(), 0);
        store.reload_now();
        assert_eq!(store.count(), 0);
    }

    fn tmp(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("hypodj-feat-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn put(dir: &Path, name: &str, ff: &FeatureFile) {
        crate::resume::atomic_write_bytes(&dir.join(name), &serde_json::to_vec(ff).unwrap()).unwrap();
    }

    fn with_id(id: &str, arousal: f32) -> FeatureFile {
        let mut ff = file(Scalars { arousal: Some(arousal), ..Default::default() });
        ff.song_id = id.into();
        ff
    }

    /// Open on a worker thread; panics if it does not return within `secs`.
    fn open_bounded(dir: PathBuf, secs: u64) -> FileFeatureStore {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(FileFeatureStore::open(dir));
        });
        rx.recv_timeout(Duration::from_secs(secs)).expect("open() hung on the features dir")
    }

    #[test]
    fn fifo_symlink_dir_and_oversized_entries_are_skipped_in_bounded_time() {
        let dir = tmp("hostile");
        put(&dir, "s1.json", &with_id("s1", 5.0));
        // A FIFO with no writer: open(O_RDONLY) without O_NONBLOCK blocks forever.
        let fifo = std::ffi::CString::new(dir.join("s2.json").into_os_string().into_encoded_bytes())
            .unwrap();
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
        // A symlink to a VALID file elsewhere is still not followed.
        let outside = tmp("hostile-outside");
        put(&outside, "s3.json", &with_id("s3", 5.0));
        std::os::unix::fs::symlink(outside.join("s3.json"), dir.join("s3.json")).unwrap();
        std::fs::create_dir(dir.join("s4.json")).unwrap();
        // Valid JSON, padded past the cap.
        let mut big = serde_json::to_vec(&with_id("s5", 5.0)).unwrap();
        big.pop();
        big.extend(std::iter::repeat_n(b' ', MAX_FEATURE_FILE as usize));
        big.push(b'}');
        std::fs::write(dir.join("s5.json"), &big).unwrap();

        let store = open_bounded(dir.clone(), 10);
        assert_eq!(store.count(), 1, "only the regular, in-cap file loads");
        assert!(store.get("s1").is_some());
        store.reload_now();
        assert_eq!(store.count(), 1);
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&outside);
    }

    #[test]
    fn largest_legitimate_file_fits_the_cap() {
        // Worst case on purpose: a 1280-d embedding and a three-hour beat grid at
        // 300 bpm, every float at the widest width serde prints for an f32.
        let mut ff = file(Scalars {
            bpm: Some(123.45678),
            arousal: Some(1.2345678),
            ..Default::default()
        });
        ff.embedding = Some(vec![-1.2345678e-30; 1280]);
        ff.timeline.beats = vec![-12345.678; 3 * 60 * 300];
        let n = serde_json::to_vec(&ff).unwrap().len() as u64;
        assert!(n * 4 < MAX_FEATURE_FILE, "{n} bytes must sit far below the cap");
    }

    #[test]
    fn reload_is_incremental_and_rereads_only_what_changed() {
        let dir = tmp("incr");
        for i in 0..50 {
            put(&dir, &format!("t{i}.json"), &with_id(&format!("t{i}"), 5.0));
        }
        let store = FileFeatureStore::open(dir.clone());
        assert_eq!((store.count(), store.files_read()), (50, 50));
        // A fresh dir mtime is never trusted, so this rescans - and re-reads none.
        store.reload_now();
        assert_eq!(store.files_read(), 50, "unchanged files are not re-parsed");
        put(&dir, "t7.json", &with_id("t7", 9.0));
        store.reload_now();
        assert_eq!(store.files_read(), 51, "exactly the rewritten file is re-read");
        assert_eq!(store.get("t7").unwrap().scalars.arousal, Some(9.0));
        std::fs::remove_file(dir.join("t8.json")).unwrap();
        store.reload_now();
        assert_eq!((store.count(), store.files_read()), (49, 51), "a removal reads nothing");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn refresh_on_the_runtime_never_scans_inline() {
        let dir = tmp("bg");
        let store = FileFeatureStore::open(dir.clone());
        put(&dir, "s1.json", &with_id("s1", 5.0));
        {
            // A scan inline would deadlock on this lock; refresh must just return.
            let _held = store.inner.scan.lock().unwrap();
            store.refresh();
            store.refresh();
            assert_eq!(store.count(), 0);
        }
        for _ in 0..500 {
            if store.count() == 1 {
                break;
            }
            tokio::task::yield_now().await;
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(store.count(), 1, "the background scan picked the file up");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_file_is_only_the_answer_for_the_id_it_is_named_after() {
        let dir = tmp("stem");
        put(&dir, "a.json", &with_id("b", 5.0));
        put(&dir, "c.json", &with_id("c", 5.0));
        let store = FileFeatureStore::open(dir.clone());
        assert!(store.get("b").is_none() && store.get("a").is_none(), "a.json claiming b is rejected");
        assert_eq!(store.count(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn out_of_range_and_energy_less_files_are_rejected() {
        let dir = tmp("range");
        let mut v = serde_json::to_value(with_id("inf", 5.0)).unwrap();
        v["scalars"]["arousal"] = serde_json::json!(1e39); // reads as f32 inf
        std::fs::write(dir.join("inf.json"), v.to_string()).unwrap();
        put(&dir, "hi.json", &with_id("hi", 42.0));
        let mut vp = with_id("vp", 5.0);
        vp.scalars.voice = Some(1.5);
        put(&dir, "vp.json", &vp);
        let mut none = file(Scalars { voice: Some(0.1), ..Default::default() });
        none.song_id = "none".into();
        put(&dir, "none.json", &none);
        put(&dir, "ok.json", &with_id("ok", 5.0));
        let store = FileFeatureStore::open(dir.clone());
        assert_eq!(store.count(), 1, "only ok.json is a valid measurement");
        let mut s = mk("inf");
        s.size = Some(10);
        assert!(!store.features(&s).unwrap().measured, "inf arousal is not energy 0.5");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn coverage_counts_valid_mirror_entries_and_reports_stale_apart() {
        let dir = tmp("cov");
        for id in ["m1", "m2", "gone"] {
            put(&dir, &format!("{id}.json"), &with_id(id, 5.0));
        }
        let store = FileFeatureStore::open(dir.clone());
        let entry = |id: &str, size: u64| IndexEntry {
            id: crate::model::SongId(id.into()),
            suffix: "flac".into(),
            size,
            created: None,
            indexed: None,
            pinned: false,
            stale: false,
            suspect: false,
            last_played_unix: 0,
            recency_dirty: false,
        };
        // m1 matches, m2's bytes moved (stale), m3 unanalysed, `gone` left the mirror.
        let mirror = [entry("m1", 10), entry("m2", 11), entry("m3", 10)];
        assert_eq!(store.measured_coverage(Some(&mirror)), Some(Coverage { measured: 1, stale: 1 }));
        assert_eq!(store.measured_coverage(None), Some(Coverage { measured: 3, stale: 0 }));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn atomic_replace_during_reloads_never_tears() {
        let dir = tmp("swap");
        put(&dir, "s1.json", &with_id("s1", 2.0));
        let store = FileFeatureStore::open(dir.clone());
        let stop = Arc::new(AtomicBool::new(false));
        let w = {
            let (dir, stop) = (dir.clone(), stop.clone());
            std::thread::spawn(move || {
                let mut i = 0u32;
                while !stop.load(Ordering::Relaxed) {
                    // Different lengths too, so a torn read would not parse.
                    let mut ff = with_id("s1", if i % 2 == 0 { 8.0 } else { 2.0 });
                    ff.timeline.beats = vec![1.0; (i % 7) as usize];
                    put(&dir, "s1.json", &ff);
                    i += 1;
                }
            })
        };
        for _ in 0..300 {
            store.reload_now();
            let a = store.get("s1").expect("s1 never vanishes mid-replace").scalars.arousal;
            assert!(a == Some(2.0) || a == Some(8.0), "{a:?}");
        }
        stop.store(true, Ordering::Relaxed);
        w.join().unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
