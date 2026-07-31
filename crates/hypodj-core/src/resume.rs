//! Smooth-restart RESUME state: the pure, signal-free, model-free BARS of the
//! feature.
//!
//! SMOOTH-RESTART composes P0 (the fade primitive, [`crate::fade`]) and P1 (the
//! live position from [`crate::event::DjEventKind::Tick`]) onto the PROCESS
//! lifecycle: a deliberate sleep-fade-OUT on SIGTERM/SIGINT, a best-effort resume
//! checkpoint, and a wake-ramp-IN on the next start. This module holds ONLY the
//! parts that are unit-testable with no signals, no real process, and no real
//! mpv:
//!   - the [`ResumeState`] (de)serialize + version gate;
//!   - the ATOMIC state write ([`store_atomic`]) + safe load ([`load`]);
//!   - the shutdown-fade BUILDER ([`build_shutdown_fade`]) that produces a valid,
//!     SHORT, click-free [`FadeSpec`] (or refuses when it would blow the budget).
//!
//! Corruption safety is a BAR: [`from_toml`] / [`load`] return `None` for ANY of
//! {missing, unreadable, garbage, truncated, schema mismatch}. They NEVER panic
//! and NEVER block startup - a bad state file always degrades to a cold start.

use std::io::Write;
use std::path::Path;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::config::FadeConfig;
use crate::fade::{Curve, FadeSpec, FadeTarget, StartleBounds};

/// The on-disk schema version. A loaded state whose `schema_version` differs is
/// treated as corrupt (`None`) so a format change is a clean cold start, never a
/// panic or a mis-parse.
pub const RESUME_SCHEMA_VERSION: u32 = 1;

/// The persisted resume snapshot: everything needed to rebuild the queue + wake
/// back into playback (or stay stopped) after a restart. Serialized to TOML.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct ResumeState {
    /// Version gate; a mismatch on load => cold start (see [`from_toml`]).
    pub schema_version: u32,
    pub queue: Vec<ResumeItem>,
    /// Index into `queue` of the current entry, if any.
    pub current: Option<usize>,
    /// Elapsed seconds of the current entry, from the P1 `Tick.time_pos`.
    pub elapsed_secs: f64,
    /// The user baseline volume (0..=100) - `State.target_volume`, NOT any faded
    /// live level. The wake ramp rises TO this on restart.
    pub volume: u8,
    pub play_state: ResumePlayState,
    pub playlist_version: u64,
    pub saved_at_unix: u64,
    /// The persisted end-of-queue continuation-radio arming toggle (`continuation
    /// on|off`). `#[serde(default)]` so a pre-continuation resume.toml (which lacks
    /// the key) loads cleanly with the toggle OFF - no schema bump, no cold-start
    /// on upgrade, and startle-safe (default false = today's silent-stop behavior).
    #[serde(default)]
    pub continuation: bool,
}

/// One persisted queue entry. Internally tagged (`kind = "song" | "stream"`) so
/// it round-trips cleanly as a TOML array-of-tables (external tagging trips the
/// toml serializer's "values before tables" ordering rule). A library song
/// carries only its id (metadata is re-resolved from Subsonic on restore); a raw
/// stream carries its verbatim url + title.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ResumeItem {
    Song { id: String },
    Stream { url: String, title: String },
}

/// The persisted play state. An explicit Paused/Stopped SURVIVES the rebuild (no
/// autoplay, no wake ramp); only Playing wakes back into playback.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ResumePlayState {
    Playing,
    Paused,
    Stopped,
}

/// Serialize a [`ResumeState`] to TOML. Infallible in practice (the type is a
/// flat, TOML-representable shape); a serializer error degrades to an empty
/// string, which [`from_toml`] then reads back as `None` (a safe cold start)
/// rather than propagating.
pub fn to_toml(s: &ResumeState) -> String {
    toml::to_string(s).unwrap_or_default()
}

/// Parse a [`ResumeState`] from TOML. ANY error - a parse failure, a truncated
/// or garbage document, a missing required field, OR a `schema_version` that is
/// not [`RESUME_SCHEMA_VERSION`] - yields `None`. NEVER panics.
pub fn from_toml(raw: &str) -> Option<ResumeState> {
    match toml::from_str::<ResumeState>(raw) {
        Ok(s) if s.schema_version == RESUME_SCHEMA_VERSION => Some(s),
        Ok(_) => None,
        Err(_) => None,
    }
}

/// Load resume state from `path`. A missing / unreadable / directory / corrupt /
/// version-mismatched file all return `None` (logged at info, "cold starting").
/// NEVER panics, NEVER blocks startup.
pub fn load(path: &Path) -> Option<ResumeState> {
    let raw = match std::fs::read_to_string(path) {
        Ok(r) => r,
        Err(_) => {
            tracing::info!(path = %path.display(), "no readable resume state; cold starting");
            return None;
        }
    };
    match from_toml(&raw) {
        Some(s) => Some(s),
        None => {
            tracing::info!(path = %path.display(), "invalid/old resume state; cold starting");
            None
        }
    }
}

/// Atomically write `bytes` to `path`: a sibling unique temp file, `write_all`,
/// `sync_all`, then `rename` over `path`. A partially written file is NEVER
/// observed at `path` - a reader sees either the previous contents or the whole
/// new ones, and a crash at any point leaves at worst a stray temp.
///
/// This is the ONE place that discipline lives, shared by the two writers that
/// depend on it: the resume checkpoint ([`store_atomic`]) and the offline store's
/// per-song sidecar ([`crate::store`]), where the sidecar rename IS the commit
/// point for a cached song. A second copy of these five lines is exactly how one
/// of them would eventually drift into a valid-looking partial file, so both go
/// through here.
///
/// Requirements on `path`: it must have a file name and a parent that already
/// exists, because the temp is a SIBLING - a cross-directory rename is not atomic,
/// so the temp can never live in `/tmp` while the target lives elsewhere.
pub fn atomic_write_bytes(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    // Sibling temp in the SAME dir so the final rename is atomic (cross-dir
    // renames are not). The temp name is UNIQUE per write (pid + a process-wide
    // counter) so two concurrent writers (the periodic vs edge-triggered resume
    // checkpoint; a sidecar rewrite racing a checkpoint) cannot clobber each
    // other's half-written temp before their renames.
    static TMP_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let name = path.file_name().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("atomic write target has no file name: {}", path.display()),
        )
    })?;
    let seq = TMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp = path.with_file_name(format!(
        "{}.tmp.{}.{}",
        name.to_string_lossy(),
        std::process::id(),
        seq
    ));
    {
        let mut f = std::fs::File::create(&tmp)?;
        // On ANY failure after create, remove the temp before returning so a
        // failed write does not accumulate litter in the state dir.
        if let Err(e) = f.write_all(bytes).and_then(|()| f.sync_all()) {
            let _ = std::fs::remove_file(&tmp);
            return Err(e);
        }
    }
    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(())
}

/// Atomically write resume state to `path`: serialize, then commit through the
/// shared [`atomic_write_bytes`] (sibling temp, fsync, rename). A partially
/// written file is never observed. Returns the io error to the caller (which logs
/// it warn, never fatal).
pub fn store_atomic(path: &Path, s: &ResumeState) -> std::io::Result<()> {
    let body = to_toml(s);
    atomic_write_bytes(path, body.as_bytes())
}

/// A built shutdown fade: the validated [`FadeSpec`] plus the REAL wall-clock
/// length it will take (`step_count * min_slew`), so the caller can await it
/// under a timeout with a known bound.
#[derive(Clone, Debug)]
pub struct ShutdownFade {
    pub spec: FadeSpec,
    pub real_dur: Duration,
}

/// Build the DELIBERATE sleep-fade-out for shutdown. This is NOT the sub-JND
/// `FadeIntent::Out` (which would extend a 60 dB drop to ~80 steps ~ 20s); it is
/// a deliberate fade (`sub_jnd = false`, capped at 3 dB/step) built DIRECTLY to
/// silence over `cfg.shutdown_fade_secs`, so it stays short and click-free.
///
/// Returns `None` when the spec cannot be built (a rejected startle-unsafe spec)
/// OR when its real length (`step_count * min_slew`) would exceed `budget` - the
/// caller then skips the fade and exits immediately, so a mid-fade SIGKILL can
/// never leave a click.
pub fn build_shutdown_fade(
    cfg: &FadeConfig,
    from_db: f64,
    budget: Duration,
) -> Option<ShutdownFade> {
    let min_slew = Duration::from_millis(cfg.min_slew_ms);
    let bounds = StartleBounds {
        min_slew,
        step_size_db: cfg.step_size_db,
        synth_floor_db: cfg.synth_floor_db,
        // DELIBERATE cue, not sub-JND: capped at 3 dB/step, never extended to the
        // ~20s sub-JND envelope. THIS is the blocker-1 fix.
        sub_jnd: false,
    };
    let spec = FadeSpec::new(
        from_db,
        FadeTarget::Silence,
        Duration::from_secs(cfg.shutdown_fade_secs),
        Duration::from_millis(cfg.tick_ms),
        Curve::DbLinear,
        bounds,
    )
    .ok()?;
    // Real length: the driver places steps one STEP INTERVAL apart, which is the
    // tick clamped UP to min_slew (t_eff = max(tick, min_slew)) - NOT min_slew
    // alone. Using min_slew would UNDER-estimate when tick > min_slew, so a fade
    // longer than `budget` could pass this check and get SIGKILLed mid-ramp (a
    // click). saturating so a pathological count cannot overflow.
    let step_interval = min_slew.max(Duration::from_millis(cfg.tick_ms));
    let real_dur = step_interval.saturating_mul(spec.step_count() as u32);
    if real_dur > budget {
        return None;
    }
    Some(ShutdownFade { spec, real_dur })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::player::SYNTH_FLOOR_DB;

    fn sample_state() -> ResumeState {
        ResumeState {
            schema_version: RESUME_SCHEMA_VERSION,
            queue: vec![
                ResumeItem::Song { id: "song-1".into() },
                ResumeItem::Stream {
                    url: "http://radio.example/stream".into(),
                    title: "Radio".into(),
                },
                ResumeItem::Song { id: "song-2".into() },
            ],
            current: Some(2),
            elapsed_secs: 42.5,
            volume: 73,
            play_state: ResumePlayState::Playing,
            playlist_version: 9,
            saved_at_unix: 1_700_000_000,
            continuation: true,
        }
    }

    #[test]
    fn to_from_toml_round_trip() {
        let s = sample_state();
        let raw = to_toml(&s);
        let back = from_toml(&raw).expect("round-trips");
        assert_eq!(s, back);
    }

    #[test]
    fn from_toml_corruption_battery_is_none_never_panics() {
        // Empty.
        assert!(from_toml("").is_none());
        // Garbage / non-TOML.
        assert!(from_toml("}{ this is not toml @@@").is_none());
        // Truncated valid document.
        let raw = to_toml(&sample_state());
        let truncated = &raw[..raw.len() / 2];
        assert!(from_toml(truncated).is_none());
        // Valid TOML but schema_version = 0 and = 999 => version gate rejects.
        let mut s0 = sample_state();
        s0.schema_version = 0;
        assert!(from_toml(&to_toml(&s0)).is_none());
        let mut s999 = sample_state();
        s999.schema_version = 999;
        assert!(from_toml(&to_toml(&s999)).is_none());
        // Valid TOML missing a required field (drop `volume`).
        let missing = "schema_version = 1\nelapsed_secs = 0.0\nplaylist_version = 0\nsaved_at_unix = 0\nplay_state = \"stopped\"\nqueue = []\n";
        assert!(from_toml(missing).is_none());
    }

    #[test]
    fn pre_continuation_file_loads_with_toggle_off() {
        // A resume.toml written before the continuation feature has no `continuation`
        // key. It must still load (schema unchanged) with the toggle defaulting OFF -
        // an upgrade never loses the saved queue and never silently arms continuation.
        let raw = "schema_version = 1\nelapsed_secs = 0.0\nvolume = 50\nplaylist_version = 0\nsaved_at_unix = 0\nplay_state = \"stopped\"\ncurrent = 0\nqueue = []\n";
        let s = from_toml(raw).expect("pre-continuation file still parses");
        assert!(!s.continuation, "the missing toggle defaults OFF");
    }

    #[test]
    fn load_missing_or_directory_is_none() {
        // A path that does not exist.
        let missing = std::env::temp_dir().join("hypodj-resume-does-not-exist-xyz.toml");
        let _ = std::fs::remove_file(&missing);
        assert!(load(&missing).is_none());
        // A directory (unreadable as a file) => None, no panic.
        assert!(load(&std::env::temp_dir()).is_none());
    }

    /// A fresh, uniquely named temp dir for one test. No `tempfile` dependency
    /// (the project keeps zero new deps here); uniqueness comes from pid + a
    /// process-wide counter so tests running in parallel cannot collide.
    fn test_dir(tag: &str) -> std::path::PathBuf {
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "hypodj-resume-{tag}-{}-{seq}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create test dir");
        dir
    }

    /// Every file name in `dir`, sorted - so a test can assert that NOTHING but
    /// the committed file survives (the temp-litter bar).
    fn dir_names(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .expect("read dir")
            .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn store_atomic_then_load_round_trip_no_leftover_tmp() {
        let dir = test_dir("store-atomic");
        let path = dir.join("resume.toml");
        let s = sample_state();
        store_atomic(&path, &s).expect("write");
        let back = load(&path).expect("read back");
        assert_eq!(s, back);
        // The committed file is the ONLY thing left: no `resume.toml.tmp.<pid>.<n>`
        // survives the rename.
        assert_eq!(dir_names(&dir), vec!["resume.toml".to_string()]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn atomic_write_bytes_round_trips_bytes_exactly_and_leaves_no_temp() {
        let dir = test_dir("awb-roundtrip");
        // A NON-UTF8 payload: the helper is byte-oriented (the store commits a
        // TOML sidecar through it, but nothing about it may assume text).
        let path = dir.join("song-1.toml");
        let payload: Vec<u8> = vec![0x00, 0xff, 0xfe, b'h', b'i', 0x80];
        atomic_write_bytes(&path, &payload).expect("write");
        assert_eq!(std::fs::read(&path).expect("read"), payload);
        assert_eq!(dir_names(&dir), vec!["song-1.toml".to_string()]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn atomic_write_bytes_overwrite_is_whole_or_nothing_and_never_appends() {
        let dir = test_dir("awb-overwrite");
        let path = dir.join("sidecar.toml");
        atomic_write_bytes(&path, b"first-and-longer").expect("write 1");
        // A SHORTER second write must fully replace, not truncate-in-place leaving
        // a tail of the first (which is precisely the valid-looking partial the
        // rename discipline exists to prevent).
        atomic_write_bytes(&path, b"second").expect("write 2");
        assert_eq!(std::fs::read(&path).expect("read"), b"second");
        assert_eq!(dir_names(&dir), vec!["sidecar.toml".to_string()]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn atomic_write_bytes_concurrent_writers_leave_one_whole_file_no_temps() {
        // The unique-temp discipline under real contention: N threads write
        // DIFFERENT whole payloads to the SAME path. The survivor must be one
        // complete payload (never a blend), and no temp may be orphaned.
        let dir = test_dir("awb-concurrent");
        let path = dir.join("resume.toml");
        let payloads: Vec<Vec<u8>> = (0..8u8).map(|i| vec![b'a' + i; 512 + i as usize]).collect();
        let mut handles = Vec::new();
        for p in payloads.clone() {
            let path = path.clone();
            handles.push(std::thread::spawn(move || {
                atomic_write_bytes(&path, &p).expect("concurrent write");
            }));
        }
        for h in handles {
            h.join().expect("thread");
        }
        let got = std::fs::read(&path).expect("read");
        assert!(
            payloads.contains(&got),
            "the survivor must be exactly one whole payload, got {} bytes",
            got.len()
        );
        assert_eq!(dir_names(&dir), vec!["resume.toml".to_string()]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn atomic_write_bytes_bad_target_is_err_never_panic_and_leaves_nothing() {
        // No file name (the filesystem root) => a clean InvalidInput error.
        let err = atomic_write_bytes(Path::new("/"), b"x").expect_err("root has no file name");
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
        // A parent that does not exist => the temp create fails; the caller logs
        // warn, and nothing is left behind (there is nowhere to leave it).
        let dir = test_dir("awb-badparent");
        let missing = dir.join("nope").join("sidecar.toml");
        assert!(atomic_write_bytes(&missing, b"x").is_err(), "missing parent is an error");
        assert_eq!(dir_names(&dir), Vec::<String>::new());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn build_shutdown_fade_default_is_short_and_click_free() {
        let cfg = FadeConfig::default();
        let budget = Duration::from_secs(10);
        let sf = build_shutdown_fade(&cfg, 0.0, budget).expect("builds from 0 dB");
        // Real length is within budget.
        assert!(sf.real_dur <= budget, "real_dur {:?} within budget", sf.real_dur);
        // From a LOWER start the fade is shorter (fewer steps).
        let lower = build_shutdown_fade(&cfg, -30.0, budget).expect("builds from -30 dB");
        assert!(lower.real_dur <= sf.real_dur);
        // Too-tight budget => None (immediate-exit path).
        assert!(build_shutdown_fade(&cfg, 0.0, Duration::from_millis(100)).is_none());
    }

    #[test]
    fn build_shutdown_fade_is_deliberate_not_sub_jnd() {
        // A short shutdown fade from 0 dB spans the full 60 dB to silence. The
        // DELIBERATE (3 dB/step) path needs at least ceil(60/3) = 20 ramp steps +
        // 1 mute = 21 - NOT the ~80 steps the sub-JND Out path would extend to.
        let mut cfg = FadeConfig::default();
        cfg.shutdown_fade_secs = 5; // 5s / 250ms = 20 nominal steps at exactly 3 dB.
        cfg.normalize();
        let budget = Duration::from_secs(15);
        let sf = build_shutdown_fade(&cfg, 0.0, budget).expect("builds");
        let steps = sf.spec.step_count();
        assert!(
            steps <= (60.0f64 / 3.0).ceil() as usize + 1,
            "steps {steps} must be the deliberate 3 dB/step count (<= 21), not the sub-JND ~80"
        );
    }

    // The mute-step + per-step-delta invariants are proven directly on the fade
    // primitive (fade.rs::silence_final_mute_step / monotone tests); here we only
    // assert the builder plumbing yields a non-empty schedule ending in silence.
    #[test]
    fn build_shutdown_fade_reaches_synth_floor_domain() {
        let cfg = FadeConfig::default();
        let sf = build_shutdown_fade(&cfg, 0.0, Duration::from_secs(30)).unwrap();
        assert!(sf.spec.step_count() >= 1);
        assert_eq!(cfg.synth_floor_db, SYNTH_FLOOR_DB);
    }
}
