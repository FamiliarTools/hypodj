//! Daemon configuration, loaded from TOML.
//!
//! FOUNDATION: real, used by the vertical slice.

use serde::Deserialize;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    pub server: ServerConfig,
    #[serde(default)]
    pub mpd: MpdConfig,
    #[serde(default)]
    pub mpris: MprisConfig,
    /// Per-user fade/envelope tunables. The whole `[fade]` section is optional
    /// and every knob defaults from the evidence-based fade research, so a config
    /// with no `[fade]` block still yields a fully startle-safe primitive.
    #[serde(default)]
    pub fade: FadeConfig,
    /// Smooth-restart (resume) tunables. Optional; when the state dir cannot be
    /// resolved (neither this section's `state_dir` nor `$STATE_DIRECTORY` set),
    /// resume is disabled and the daemon simply cold-starts.
    #[serde(default)]
    pub restart: RestartConfig,
    /// End-of-queue continuation-radio config. Optional; with no `[continuation]`
    /// section (or no `station`) the feature is entirely off and the deck ends
    /// stopped at the end of the queue exactly as it does today.
    #[serde(default)]
    pub continuation: ContinuationConfig,
    /// Auto-identify (songrec) config for no-ICY streams. Optional; with no
    /// `[recognize]` section the defaults apply (auto ON, interval 300s).
    #[serde(default)]
    pub recognize: RecognizeConfig,
    /// Offline audio store config. Optional; with no `[store]` section the store
    /// is ON with an 8 GiB budget, because the offline path is meant to be the
    /// EVERYDAY path (an untested branch is how offline support rots). The safety
    /// bound is `max_bytes`, not the flag.
    #[serde(default)]
    pub store: StoreConfig,
    /// The HEARD LEDGER: the append-only, session-scoped record of what the radio
    /// played and what was marked. Optional; with no `[heard]` section the ledger is
    /// ON (it costs one append per title change and nothing else) and lives under the
    /// state dir. With no state dir it simply never opens.
    #[serde(default)]
    pub heard: HeardConfig,
    /// THE TAPE: retroactive audio capture off mpv's own demuxer cache ([`crate::tape`]),
    /// taken by the `mark` gesture. Optional; with no `[tape]` section it is ON with a
    /// 2 GiB rolling budget and lives beside `heard/` under the state dir. With no state
    /// dir it simply never opens and `mark` behaves exactly as it did without it.
    #[serde(default)]
    pub tape: TapeConfig,
}

/// `[tape]` config for THE TAPE ([`crate::tape`]): the rolling cache of retroactively
/// captured stream audio a `mark` press keeps when audio is the only thing that can still
/// help.
///
/// A ROLLING CACHE, NOT AN ARCHIVE, and that framing is what makes the budget honest: he
/// acts on a segment the next morning or he does not, and if he has not acted in ten
/// weeks the ledger row is still there and the sound is gone. A row outliving its audio is
/// designed for, not a defect.
#[derive(Debug, Clone, Deserialize)]
pub struct TapeConfig {
    /// Master switch. Default TRUE: this is substrate, and an untested branch is how a
    /// feature rots. The safety bound is `max_bytes`, not the flag.
    #[serde(default = "d_tape_enable")]
    pub enable: bool,
    /// Where segments live. `None` resolves to `<state_dir>/tape`, the same shape
    /// `[heard].dir` uses.
    ///
    /// It must be a DEDICATED directory, and the tape enforces that rather than trusting
    /// it: `tape::sweep` deletes every `.mkv` without a matching `.toml` and every `.toml`
    /// without a matching `.mkv`, because that is what an interrupted commit looks like.
    /// Pointed at, say, a videos folder it would take the lot on the first press. So the
    /// daemon claims the root with a `.hypodj-tape` marker at startup and REFUSES a
    /// non-empty directory that carries none, running without the tape instead - the same
    /// guard `[store].dir` has.
    ///
    /// It MUST NOT be inside the offline store's directory, for the reason `[heard].dir`
    /// records: the store owns its root EXCLUSIVELY and its reconciler deletes everything
    /// in it that is not a valid cached song. Its `scan_dir` is also non-recursive and
    /// skips subdirectories, so a nested tape directory would be invisible to the index
    /// while sitting in a root that deletes strangers. A SIBLING, always.
    ///
    /// It must additionally contain no `"`, `\` or newline: mpv's flat command syntax
    /// C-unescapes inside double quotes, so such a path is mangled rather than merely
    /// risky. `main.rs` validates that once at resolution and disables the tape rather
    /// than handing mpv an ambiguous string.
    #[serde(default)]
    pub dir: Option<PathBuf>,
    /// Hard byte budget for captured AUDIO. Oldest-first eviction by press time runs
    /// BEFORE each dump as well as after each commit, so the budget is enforced by making
    /// room rather than by observing the disk. Pinned segments (`heard keep <n>`) still
    /// COUNT against it - a budget that excludes pins lies - but are never deleted; if
    /// pins alone exceed it, the sweep warns and the next press is refused honestly.
    ///
    /// The default arithmetic, stated so it can be checked rather than trusted: at NTS
    /// mp3's 32.2 KB/s a 300 s window is 9.66 MB, so 2 GiB is about 222 segments. At five
    /// presses an evening with roughly two discarded on a successful star, that is about
    /// seventy evenings - call it ten weeks, NOT four months. It is also 2.3% of the
    /// ~85 GiB free on a 91%-full disk the offline store still intends to eat 5.77 GiB of.
    #[serde(default = "d_tape_max_bytes")]
    pub max_bytes: u64,
    /// How many seconds of PAST a press asks for. Generous on purpose (Rule 0: dump wide,
    /// refine on disk); the actual span is clamped to what mpv's cache really holds.
    #[serde(default = "d_tape_back_secs")]
    pub back_secs: u64,
    /// The hard span cap per dump, seconds. Bounds BOTH disk per press and the actor's
    /// blocking time per dump: 1200 s of NTS mp3 is about 47 MB, roughly 135 ms of
    /// non-pumped event loop at the measured ~350 MB/s. Applied ON the actor after the
    /// cache-state read, so neither a config typo nor a caller can widen it.
    #[serde(default = "d_tape_max_secs")]
    pub max_secs: u64,
}

pub const DEFAULT_TAPE_ENABLE: bool = true;
/// 2 GiB - about 222 segments at NTS mp3's 300 s window, roughly ten weeks of use.
pub const DEFAULT_TAPE_MAX_BYTES: u64 = 2 * 1024 * 1024 * 1024;
pub const DEFAULT_TAPE_BACK_SECS: u64 = 300;
pub const DEFAULT_TAPE_MAX_SECS: u64 = 1200;

/// Hard floor on `tape.max_bytes`, 64 MiB (mirroring [`STORE_MIN_MAX_BYTES`]). Below one
/// segment plus slack the sweep would evict every press immediately after taking it, so
/// the feature could not function at all. Clamp up, never reject.
pub const TAPE_MIN_MAX_BYTES: u64 = 64 * 1024 * 1024;

/// Hard floor on `tape.back_secs`, 30 s. Shorter than this and a press would routinely
/// resolve below `tape::TAPE_MIN_SECS` and be refused for a reason the human did not
/// choose.
pub const TAPE_MIN_BACK_SECS: u64 = 30;

/// Hard floor on `tape.max_secs`, 60 s. It must never sit BELOW `back_secs`' own floor,
/// or the cap would silently void every window the back-ask produced.
pub const TAPE_MIN_MAX_SECS: u64 = 60;

fn d_tape_enable() -> bool {
    DEFAULT_TAPE_ENABLE
}
fn d_tape_max_bytes() -> u64 {
    DEFAULT_TAPE_MAX_BYTES
}
fn d_tape_back_secs() -> u64 {
    DEFAULT_TAPE_BACK_SECS
}
fn d_tape_max_secs() -> u64 {
    DEFAULT_TAPE_MAX_SECS
}

/// MANUAL (not derived), the [`StoreConfig`] / [`HeardConfig`] rule: a derived `Default`
/// would give `enable = false, max_bytes = 0, back_secs = 0` to everyone who never wrote
/// the section - the tape silently off and, worse, silently zero-budget. The parity test
/// below is what keeps this true.
impl Default for TapeConfig {
    fn default() -> Self {
        Self {
            enable: DEFAULT_TAPE_ENABLE,
            dir: None,
            max_bytes: DEFAULT_TAPE_MAX_BYTES,
            back_secs: DEFAULT_TAPE_BACK_SECS,
            max_secs: DEFAULT_TAPE_MAX_SECS,
        }
    }
}

impl TapeConfig {
    /// Floor-clamp at LOAD time, logging every correction. TOTAL - there is no input for
    /// which this can fail or panic, including `0` and `u64::MAX` on every knob.
    pub fn normalize(&mut self) {
        if self.max_bytes < TAPE_MIN_MAX_BYTES {
            tracing::warn!(
                configured = self.max_bytes,
                floor = TAPE_MIN_MAX_BYTES,
                "tape.max_bytes below the 64 MiB floor; clamping up (below one segment the sweep would evict every press)"
            );
            self.max_bytes = TAPE_MIN_MAX_BYTES;
        }
        if self.back_secs < TAPE_MIN_BACK_SECS {
            tracing::warn!(
                configured = self.back_secs,
                floor = TAPE_MIN_BACK_SECS,
                "tape.back_secs below the floor; clamping up"
            );
            self.back_secs = TAPE_MIN_BACK_SECS;
        }
        if self.max_secs < TAPE_MIN_MAX_SECS {
            tracing::warn!(
                configured = self.max_secs,
                floor = TAPE_MIN_MAX_SECS,
                "tape.max_secs below the floor; clamping up (a cap under the back-ask would void every window)"
            );
            self.max_secs = TAPE_MIN_MAX_SECS;
        }
    }
}

/// `[heard]` config for the HEARD LEDGER ([`crate::heard`]): the append-only record of
/// ICY titles, recognition outcomes and MARK presses, plus its `heard` read-back.
///
/// The write path is an unbounded mpsc to a dedicated `O_APPEND` task, so the ledger
/// can never touch the director spine and there is no sync/fsync knob to expose here.
#[derive(Debug, Clone, Deserialize)]
pub struct HeardConfig {
    /// Master switch. Default TRUE: this is substrate, and an untested branch is how
    /// a feature rots.
    #[serde(default = "d_heard_enable")]
    pub enable: bool,
    /// Where session files live. `None` resolves to `<state_dir>/heard`, mirroring how
    /// `store.dir` is resolved.
    ///
    /// It MUST NOT be inside the offline store's directory: the store owns its root
    /// EXCLUSIVELY and its reconciler deletes everything in it that is not a valid
    /// cached song, so a ledger file there would be swept as an orphan. The default
    /// sits beside `resume.toml` at the state-dir root for exactly that reason.
    #[serde(default)]
    pub dir: Option<PathBuf>,
    /// How many past session files to keep. The oldest are removed by the ledger task
    /// on its first blocking hop, before the first write, so the record cannot grow
    /// without bound in the user's home. Floored to [`HEARD_MIN_KEEP_SESSIONS`] so a
    /// `0` can never empty the directory.
    #[serde(default = "d_heard_keep_sessions")]
    pub keep_sessions: u32,
    /// The window over which the RENDER collapses repeated titles, seconds. The FILE
    /// logs every occurrence faithfully; only the view curates. A window rather than
    /// adjacency because real duplicates are interleaved A,B,A,B, which
    /// consecutive-dedupe suppresses none of.
    #[serde(default = "d_heard_dedupe_window_secs")]
    pub dedupe_window_secs: u64,
}

pub const DEFAULT_HEARD_ENABLE: bool = true;
pub const DEFAULT_HEARD_KEEP_SESSIONS: u32 = 30;
pub const DEFAULT_HEARD_DEDUPE_WINDOW_SECS: u64 = 1800;

/// Hard floor on `heard.keep_sessions`. Zero would mean the sweep deletes the session
/// it just opened, so the ledger would silently record nothing - clamp up, never
/// reject (the [`FadeConfig::normalize`] posture).
pub const HEARD_MIN_KEEP_SESSIONS: u32 = 1;

fn d_heard_enable() -> bool {
    DEFAULT_HEARD_ENABLE
}
fn d_heard_keep_sessions() -> u32 {
    DEFAULT_HEARD_KEEP_SESSIONS
}
fn d_heard_dedupe_window_secs() -> u64 {
    DEFAULT_HEARD_DEDUPE_WINDOW_SECS
}

/// MANUAL (not derived), the [`StoreConfig`] rule: a derived `Default` would give
/// `enable = false, keep_sessions = 0` to everyone who never wrote the section, which
/// is the exact opposite of the intent. The parity test below keeps this true.
impl Default for HeardConfig {
    fn default() -> Self {
        Self {
            enable: DEFAULT_HEARD_ENABLE,
            dir: None,
            keep_sessions: DEFAULT_HEARD_KEEP_SESSIONS,
            dedupe_window_secs: DEFAULT_HEARD_DEDUPE_WINDOW_SECS,
        }
    }
}

impl HeardConfig {
    /// Floor-clamp at LOAD time, logging the correction. TOTAL - there is no input for
    /// which this can fail or panic.
    pub fn normalize(&mut self) {
        if self.keep_sessions < HEARD_MIN_KEEP_SESSIONS {
            tracing::warn!(
                configured = self.keep_sessions,
                floor = HEARD_MIN_KEEP_SESSIONS,
                "heard.keep_sessions below the floor; clamping up (0 would delete the session it just opened)"
            );
            self.keep_sessions = HEARD_MIN_KEEP_SESSIONS;
        }
    }
}

/// `[store]` config for the OFFLINE AUDIO STORE ([`crate::store`]): the on-disk
/// mirror that lets starred songs and the upcoming queue window play from local
/// bytes when the server is slow, flaky, or gone.
///
/// Only `/rest/download` ORIGINALS are ever stored, which is why there is no
/// format/bitrate knob here: the store mirrors exactly what the server holds, so
/// a server-side transcoding change cannot invalidate a cached file. The
/// metadata-client timeouts that bound "transient failure" are hardcoded consts
/// in `subsonic.rs`, not config.
#[derive(Debug, Clone, Deserialize)]
pub struct StoreConfig {
    /// Master switch. Additionally disabled when no state dir resolves (neither
    /// `store.dir` nor `restart.state_dir` nor `$STATE_DIRECTORY`) - resume's
    /// posture: warn and run without it, never fail to start.
    #[serde(default = "d_store_enable")]
    pub enable: bool,
    /// Where the store lives. `None` resolves to `<state_dir>/store`, mirroring
    /// how `restart.state_dir` is resolved. An override MUST sit on the same
    /// filesystem as its own temp files (they are siblings), because the atomic
    /// rename that commits a download is not atomic across filesystems.
    ///
    /// THE DIRECTORY IS OWNED EXCLUSIVELY: the store converges it by DELETING
    /// everything in it that is not a valid cached song, so it must be a dedicated
    /// directory and never the state dir, a music library, or a home directory.
    /// That is enforced, not merely documented - the store adopts only an EMPTY
    /// directory (marking it) or one it already marked, and refuses any other,
    /// running without a store rather than deleting a byte of it.
    #[serde(default)]
    pub dir: Option<PathBuf>,
    /// SOFT byte budget for stored AUDIO (originals, so FLACs count full) - the
    /// absolute cap, never the whole rule.
    ///
    /// The EFFECTIVE budget each pass is
    /// `min(max_bytes, (free + own) - reserve)`, measured from the store
    /// filesystem itself (see [`crate::store::derive_budget`]). So this knob can
    /// only ever LOWER what the store uses: when the disk fills, the effective
    /// budget falls below it and the store hands space back. That is what makes
    /// "hypodj cannot fill the disk" a property rather than a hope, and it is why
    /// the number here could be raised without a new knob.
    ///
    /// Over budget, the store evicts in ONE order: opportunistic entries by oldest
    /// `last_played`, then whole pin groups from the tail of the pin frontier
    /// backwards. Nothing is exempt any more - a pin set that alone exceeds the
    /// budget is DEFERRED by name (the `store` verb lists which albums), rather
    /// than silently halting forever.
    ///
    /// Floored to [`STORE_MIN_MAX_BYTES`] at load: below that the budget cannot
    /// hold even a couple of FLAC originals, so eviction would thrash against
    /// every download instead of bounding anything. The DERIVED budget is
    /// deliberately allowed below that floor, including to zero - "the disk is
    /// nearly full" must beat "the store would like at least 64 MiB".
    #[serde(default = "d_store_max_bytes")]
    pub max_bytes: u64,
    /// How many UPCOMING queue Song entries beyond the current one join the
    /// desired set, so an ordinary end-of-track advance is a disk open rather than
    /// a network fetch. `0` disables queue-ahead downloads (pins still apply).
    #[serde(default = "d_store_queue_ahead")]
    pub queue_ahead: u32,
    /// Cadence of the FULL reconcile pass (directory scan + one `getStarred2` +
    /// fingerprint verdicts), seconds. Floored to
    /// [`STORE_MIN_SYNC_INTERVAL_SECS`] so no config typo can turn the reconciler
    /// into a busy loop against the server. Star flips kick a full pass; window
    /// changes, suspect marks, and skip pins kick a LIGHT pass immediately, so
    /// this cadence is not the latency of anything the user is waiting for.
    #[serde(default = "d_store_sync_interval_secs")]
    pub sync_interval_secs: u64,
    /// Whether the `getStarred2` song set is the authoritative PIN set - the
    /// hearted-songs-work-offline promise. With this false the store still mirrors
    /// the queue window opportunistically, but nothing is protected from eviction
    /// beyond the window itself.
    #[serde(default = "d_store_pin_starred")]
    pub pin_starred: bool,
}

pub const DEFAULT_STORE_ENABLE: bool = true;
/// 16 GiB, and the sizing is MEASURED rather than guessed.
///
/// The union of one real user's three starred kinds (47 songs, 36 albums, 1
/// artist) is 347 unique tracks. Per stored original the live store gives a
/// median of 31 MiB and a mean of 37 MiB - not the ~22 MB this constant used to
/// claim - so that union is about 12.3 GiB. 16 GiB covers it plus roughly 25 %
/// for stars added before anyone looks at this number again.
///
/// It is a CAP, not an allocation: nothing is written until it is wanted, and the
/// free-space clamp above ([`StoreConfig::max_bytes`]) can pull the effective
/// budget below it at any moment.
pub const DEFAULT_STORE_MAX_BYTES: u64 = 17_179_869_184;
pub const DEFAULT_STORE_QUEUE_AHEAD: u32 = 3;
pub const DEFAULT_STORE_SYNC_INTERVAL_SECS: u64 = 900;
pub const DEFAULT_STORE_PIN_STARRED: bool = true;

/// Hard floor on `store.max_bytes`, 64 MiB. A budget smaller than a couple of
/// originals makes every download immediately over-cap, so the store would
/// download-then-evict forever instead of bounding disk use. Clamp up, never
/// reject (the [`FadeConfig::normalize`] posture).
pub const STORE_MIN_MAX_BYTES: u64 = 64 * 1024 * 1024;

/// Hard floor on `store.sync_interval_secs`, 60s. The full pass costs a directory
/// scan plus a `getStarred2` round trip; below a minute a typo (`= 1`) would hammer
/// the server and starve the runtime for no gain, since every latency-sensitive
/// trigger is already a kick, not a tick.
pub const STORE_MIN_SYNC_INTERVAL_SECS: u64 = 60;

fn d_store_enable() -> bool {
    DEFAULT_STORE_ENABLE
}
fn d_store_max_bytes() -> u64 {
    DEFAULT_STORE_MAX_BYTES
}
fn d_store_queue_ahead() -> u32 {
    DEFAULT_STORE_QUEUE_AHEAD
}
fn d_store_sync_interval_secs() -> u64 {
    DEFAULT_STORE_SYNC_INTERVAL_SECS
}
fn d_store_pin_starred() -> bool {
    DEFAULT_STORE_PIN_STARRED
}

/// MANUAL (not derived) so a MISSING `[store]` section - which the top-level
/// `#[serde(default)]` fills from `Default` - is byte-identical to an EMPTY
/// `[store]` section. A derived `Default` would give `enable = false`,
/// `max_bytes = 0`, `queue_ahead = 0`: the store silently off for everyone who
/// never wrote the section, which is the exact opposite of the intent. This is the
/// [`ContinuationConfig`] rule; the parity test below is what keeps it true.
impl Default for StoreConfig {
    fn default() -> Self {
        Self {
            enable: DEFAULT_STORE_ENABLE,
            dir: None,
            max_bytes: DEFAULT_STORE_MAX_BYTES,
            queue_ahead: DEFAULT_STORE_QUEUE_AHEAD,
            sync_interval_secs: DEFAULT_STORE_SYNC_INTERVAL_SECS,
            pin_starred: DEFAULT_STORE_PIN_STARRED,
        }
    }
}

impl StoreConfig {
    /// Floor-clamp the two knobs whose out-of-range values are pathological rather
    /// than merely odd, at LOAD time, logging any correction (the
    /// [`FadeConfig::normalize`] posture: clamp up, never reject, never panic).
    /// TOTAL - there is no input for which this can fail.
    pub fn normalize(&mut self) {
        if self.max_bytes < STORE_MIN_MAX_BYTES {
            tracing::warn!(
                configured = self.max_bytes,
                floor = STORE_MIN_MAX_BYTES,
                "store.max_bytes below the 64 MiB floor; clamping up"
            );
            self.max_bytes = STORE_MIN_MAX_BYTES;
        }
        if self.sync_interval_secs < STORE_MIN_SYNC_INTERVAL_SECS {
            tracing::warn!(
                configured = self.sync_interval_secs,
                floor = STORE_MIN_SYNC_INTERVAL_SECS,
                "store.sync_interval_secs below the 60s floor; clamping up"
            );
            self.sync_interval_secs = STORE_MIN_SYNC_INTERVAL_SECS;
        }
    }
}

/// `[recognize]` config for AUTO now-playing recognition of no-ICY streams (task
/// bspk8v5). When a raw stream becomes current and no usable ICY metadata arrives
/// within a short grace window, the daemon auto-fires the same songrec identify path
/// the manual `identify` verb uses, so a Shazam-matchable stream names itself in the
/// now-playing surfaces without a manual verb. ICY still WINS when present; this is
/// the no-ICY fallback. Rate-limited (min interval + no-match backoff + single-flight)
/// so it never hammers the Shazam endpoint.
#[derive(Debug, Clone, Deserialize)]
pub struct RecognizeConfig {
    /// Master on/off for auto-identify. Default TRUE (the feature exists to fix the
    /// bare-URL now-playing complaint; the backoff + interval floor + single-flight
    /// make the default safe). Set `false` to disable auto-identify entirely (the
    /// manual `identify` verb is unaffected).
    #[serde(default = "d_recognize_auto")]
    pub auto: bool,
    /// The re-identify cadence, seconds: on a long-running stream the airing track
    /// changes with no boundary signal, so a hit schedules the next attempt this far
    /// out. Floor-clamped to `RECOGNIZE_MIN_INTERVAL_SECS` at load so no config typo
    /// can produce a tight loop. A miss backs off over this, and the two miss KINDS
    /// back off differently: a CONTENT miss (Shazam reached, no match) caps at ONE
    /// doubling (interval * 2 flat), while a TRANSPORT failure keeps the full
    /// exponential up to interval * 8. The split exists because an all-miss mixtape
    /// evening on the fused curve produced 40 minutes of deafness, and a content miss
    /// says nothing about whether the NEXT track is recognizable.
    #[serde(default = "d_recognize_interval_secs")]
    pub interval_secs: u64,
}

pub const DEFAULT_RECOGNIZE_AUTO: bool = true;
pub const DEFAULT_RECOGNIZE_INTERVAL_SECS: u64 = 300;

/// The hard floor on the auto-identify interval, seconds. Comfortably above the 40s
/// per-attempt ceiling (`recognize::RECOGNIZE_TIMEOUT`) so even a config typo of
/// `interval_secs = 1` cannot make the cadence collide with an in-flight attempt.
pub const RECOGNIZE_MIN_INTERVAL_SECS: u64 = 60;

fn d_recognize_auto() -> bool {
    DEFAULT_RECOGNIZE_AUTO
}
fn d_recognize_interval_secs() -> u64 {
    DEFAULT_RECOGNIZE_INTERVAL_SECS
}

impl Default for RecognizeConfig {
    fn default() -> Self {
        Self {
            auto: DEFAULT_RECOGNIZE_AUTO,
            interval_secs: DEFAULT_RECOGNIZE_INTERVAL_SECS,
        }
    }
}

impl RecognizeConfig {
    /// Floor-clamp the interval at LOAD time so an out-of-range TOML value can never
    /// produce a Shazam-hammering cadence downstream (mirrors [`FadeConfig::normalize`]).
    pub fn normalize(&mut self) {
        if self.interval_secs < RECOGNIZE_MIN_INTERVAL_SECS {
            self.interval_secs = RECOGNIZE_MIN_INTERVAL_SECS;
        }
    }
}

/// The end-of-queue CONTINUATION behavior when the play queue drains and the
/// runtime `continuation on|off` toggle is armed. Two disjoint mechanisms share
/// the ONE arming toggle; this config field selects which one fires.
///
/// - `Radio` (the default, back-compat): flow into a configured online radio
///   station (a raw stream) - the original slice-1/slice-2 behavior.
/// - `Autofill`: append real LIBRARY tracks similar to the recency seed so the
///   music keeps going from the user's own library (they scrobble, seek, carry
///   full metadata, and re-autofill on each subsequent true-drain).
///
/// `#[serde(rename_all = "lowercase")]` so the TOML value is `mode = "radio"` /
/// `mode = "autofill"`. Every existing config (station-only or no `[continuation]`
/// section at all) deserializes to `Radio`, so the radio path is byte-identical to
/// today; `mode = "autofill"` is the only way to switch.
#[derive(Debug, Clone, Copy, PartialEq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ContinuationMode {
    /// Flow into the configured radio station (the back-compat default).
    #[default]
    Radio,
    /// Append similar LIBRARY songs from the user's own library.
    Autofill,
}

/// `[continuation]` config for the end-of-queue continuation feature: when the
/// play queue drains, either flow into a configured online radio station (`mode =
/// "radio"`, the default) OR append similar library tracks (`mode = "autofill"`)
/// instead of stopping silent. This holds the station identity and the mode
/// selector; the runtime on/off arming is a persisted toggle (`continuation
/// on|off`), default OFF, so the feature is never surprising - it does nothing
/// until it is explicitly armed.
#[derive(Debug, Clone, Deserialize)]
pub struct ContinuationConfig {
    /// The continuation station: either a saved Navidrome internet-radio station
    /// NAME (resolved to its stream URL via `getInternetRadioStations`) or an
    /// absolute `http(s)://` stream URL used directly. `None` (unset) means the
    /// radio path is off - the deck ends stopped at end-of-queue as it does today.
    /// Ignored when `mode = "autofill"` (autofill seeds from the library, not a
    /// station).
    #[serde(default)]
    pub station: Option<String>,
    /// Which continuation mechanism fires at the drain edge (radio | autofill).
    /// Defaults to `Radio` so every pre-existing config keeps its exact behavior.
    #[serde(default)]
    pub mode: ContinuationMode,
    /// How many similar library tracks an `autofill` refill appends per true-drain
    /// (the target count after dedup shrinkage; the fetch over-fetches 2x). Unused
    /// in `radio` mode. Default 20.
    #[serde(default = "d_autofill_count")]
    pub autofill_count: u32,
    /// How many tracks to keep AHEAD of the play position, so an armed walk is visible
    /// in the up-next list instead of materialising one track at a time at the drain
    /// edge. `0` restores the old drain-edge-only behavior exactly. Unused in `radio`
    /// mode. Default 5 - enough to see where the walk is going without the queue
    /// running far ahead of what you would actually sit through.
    #[serde(default = "d_lookahead")]
    pub lookahead: u32,
}

/// Manual (not derived) so a MISSING `[continuation]` section - which the top-level
/// `#[serde(default)]` fills from `Default` - matches the per-field serde defaults: a
/// derived `Default` would give `autofill_count = 0` (a zero-length refill), so spell
/// it out to keep the no-section case byte-identical to an empty `[continuation]`.
impl Default for ContinuationConfig {
    fn default() -> Self {
        Self {
            station: None,
            mode: ContinuationMode::Radio,
            autofill_count: d_autofill_count(),
            lookahead: d_lookahead(),
        }
    }
}

pub const DEFAULT_AUTOFILL_COUNT: u32 = 20;

fn d_autofill_count() -> u32 {
    DEFAULT_AUTOFILL_COUNT
}

pub const DEFAULT_LOOKAHEAD: u32 = 5;

fn d_lookahead() -> u32 {
    DEFAULT_LOOKAHEAD
}

/// `[restart]` config for the smooth-restart (sleep-fade-out on SIGTERM + resume
/// state + wake-ramp-in) feature. All fields optional.
#[derive(Debug, Clone, Deserialize)]
pub struct RestartConfig {
    /// Where the resume state file (`resume.toml`) lives. When `None` the daemon
    /// reads `$STATE_DIRECTORY` (set by systemd `StateDirectory=`) at startup;
    /// if neither is present, resume is disabled (safe cold start). This is NEVER
    /// the RuntimeDirectory (/run tmpfs is wiped on stop, defeating SIGKILL
    /// resume) - it must be a persistent location.
    #[serde(default)]
    pub state_dir: Option<PathBuf>,
    /// Coarse periodic checkpoint cadence, seconds (refreshes only elapsed while a
    /// track is live). Edge events (track/state/queue changes) checkpoint
    /// immediately regardless.
    #[serde(default = "d_checkpoint_secs")]
    pub checkpoint_secs: u64,
}

pub const DEFAULT_CHECKPOINT_SECS: u64 = 12;

fn d_checkpoint_secs() -> u64 {
    DEFAULT_CHECKPOINT_SECS
}

impl Default for RestartConfig {
    fn default() -> Self {
        Self {
            state_dir: None,
            checkpoint_secs: DEFAULT_CHECKPOINT_SECS,
        }
    }
}

/// Tunable knobs for the volume-envelope (fade) primitive. Defaults are the
/// research-backed constants below; both [`crate::fade::FadeSpec::new`] (via the
/// handler) and the `fade` DSL parser read from this ONE struct, so wiring a
/// per-user TOML override is a one-line change. See the fade-design-spec /
/// config-knobs memories for the rationale behind each default.
#[derive(Debug, Clone, Deserialize)]
pub struct FadeConfig {
    /// Hard floor on any step interval, ms. Startle re-emerges below ~200 ms; the
    /// design floor is 250 ms and it applies to EVERY transition incl user nudges.
    #[serde(default = "d_min_slew_ms")]
    pub min_slew_ms: u64,
    /// Max per-step dB delta for a sub-JND fade (sleep-safe, imperceptible).
    #[serde(default = "d_step_db")]
    pub step_size_db: f64,
    /// The non-zero low floor for a wind-down fade (does NOT reach silence).
    /// Reachable from the DSL as `fade to floor` (see the handler): a deliberate
    /// wind-down to this level that leaves playback running, distinct from
    /// `fade out` which ramps all the way to silence + stops. Normalized into
    /// `(synth_floor_db, wake_ceiling_db)` by [`FadeConfig::normalize`].
    #[serde(default = "d_floor_db")]
    pub floor_level_db: f64,
    /// The perceptual synth floor: at/below it the signal is treated as silence
    /// (mpv volume 0). Keeps the dB domain finite (never -inf).
    #[serde(default = "d_synth_floor")]
    pub synth_floor_db: f64,
    /// The comfort ceiling a wake ramp-in must never overshoot (0 dB == vol 100).
    #[serde(default = "d_ceiling_db")]
    pub wake_ceiling_db: f64,
    /// Tick == step interval, ms (clamped up to `min_slew_ms`).
    #[serde(default = "d_tick_ms")]
    pub tick_ms: u64,
    /// Default duration of a sleep-stop fade to silence, seconds. RESERVED and
    /// consumed by the P1 sleep-timer executor (the scheduled `fade out` a sleep
    /// timer fires); the immediate DSL `fade out` uses `winddown_fade_secs`. It
    /// is not silently ignored: [`FadeConfig::normalize`] reads and clamps it
    /// into `[min_slew, max_dur]` so a bad value is caught at load, not at P1.
    #[serde(default = "d_sleep_fade_s")]
    pub sleep_fade_secs: u64,
    /// Default duration of a wind-down `fade out` / `fade to`, seconds.
    #[serde(default = "d_winddown_s")]
    pub winddown_fade_secs: u64,
    /// Default duration of a wake `fade in` ramp, seconds.
    #[serde(default = "d_wake_s")]
    pub wake_ramp_secs: u64,
    /// Absolute ceiling on any fade duration, seconds (clamps a runaway request).
    #[serde(default = "d_max_dur_s")]
    pub max_dur_secs: u64,
    /// Duration of the DELIBERATE sleep-fade-out run on SIGTERM/SIGINT before the
    /// daemon exits, seconds. This is a deliberate (not sub-JND) fade at the 3
    /// dB/step cap, kept SHORT so it never slows a nixos-rebuild / service
    /// restart; the smooth-restart ramp-IN on the next start uses
    /// `restart_fade_secs` (NOT the long alarm `wake_ramp_secs`). Normalized into
    /// `[min_slew_s, max_dur_secs]`.
    #[serde(default = "d_shutdown_fade_s")]
    pub shutdown_fade_secs: u64,
    /// Duration of the smooth-restart ramp-IN, seconds: the counterpart to
    /// `shutdown_fade_secs`, run when the daemon RESTORES playback after a restart.
    /// A restart resume must come back QUICKLY, so this is short and DISTINCT from
    /// the gentle alarm `wake_ramp_secs` (8 min) - reusing that would leave the
    /// music barely audible for minutes after a rebuild.
    #[serde(default = "d_restart_fade_s")]
    pub restart_fade_secs: u64,
    /// Duration of the startle-safe transport pause/resume fade, SECONDS (a float,
    /// unlike the coarse `*_secs` knobs, so a sub-second nominal is expressible).
    /// On PAUSE the transport runs a short sub-JND fade to silence THEN pauses mpv
    /// (silent at the freeze, no click); on RESUME it unpauses from silence THEN
    /// ramps back to the prior level. Kept SHORT so pause feels responsive; the
    /// fade primitive still extends it as far as sub-JND startle safety requires.
    /// Normalized into `[min_slew_s, max_dur_secs]`.
    #[serde(default = "d_pause_fade_s")]
    pub pause_fade_secs: f64,
    /// Duration of the startle-safe USER-skip dip fade, SECONDS (a float, like
    /// `pause_fade_secs`). On a USER Next/Previous while playing, the transport
    /// runs a short DELIBERATE dip to silence, loads the target from silence, then
    /// ramps back to the baseline. Kept SHORT (default ~0.35s) so a skip feels
    /// responsive; the fade primitive still extends it as far as deliberate
    /// startle safety requires. Normalized into `[min_slew_s, max_dur_secs]`.
    #[serde(default = "d_skip_fade_s")]
    pub skip_fade_secs: f64,
    /// Duration of the graduated absolute-volume GLIDE fade, SECONDS (a float,
    /// like `pause_fade_secs`). A manual `setvol` / MPRIS Volume set GLIDES to the
    /// target over this span (never snaps), like a hand moving a knob. Distinct
    /// from `pause_fade_secs` so the setvol feel is tunable independently of the
    /// pause ramp. Kept SHORT (default ~0.4s); a large span (e.g. 0 -> 100) still
    /// extends past it via the deliberate 3 dB/step clamp so it stays startle-safe.
    /// Normalized into `[min_slew_s, max_dur_secs]`.
    #[serde(default = "d_glide_fade_s")]
    pub glide_fade_secs: f64,
}

// Research-backed defaults (memory 01kxhjqr). Exposed as `pub const` so the fade
// DSL parser can reference the SAME source of truth as the serde defaults.
pub const DEFAULT_MIN_SLEW_MS: u64 = 250;
pub const DEFAULT_STEP_SIZE_DB: f64 = 0.75;
pub const DEFAULT_FLOOR_LEVEL_DB: f64 = -45.0;
pub const DEFAULT_SYNTH_FLOOR_DB: f64 = -60.0;
pub const DEFAULT_WAKE_CEILING_DB: f64 = 0.0;
pub const DEFAULT_TICK_MS: u64 = 250;
pub const DEFAULT_SLEEP_FADE_SECS: u64 = 480;
pub const DEFAULT_WINDDOWN_FADE_SECS: u64 = 300;
pub const DEFAULT_WAKE_RAMP_SECS: u64 = 480;
pub const DEFAULT_MAX_DUR_SECS: u64 = 1800;
pub const DEFAULT_SHUTDOWN_FADE_SECS: u64 = 6;
pub const DEFAULT_RESTART_FADE_SECS: u64 = 5;
pub const DEFAULT_PAUSE_FADE_SECS: f64 = 0.5;
pub const DEFAULT_SKIP_FADE_SECS: f64 = 0.35;
pub const DEFAULT_GLIDE_FADE_SECS: f64 = 0.4;

/// Positive minimum for `step_size_db`. A `0` (or negative) step would divide by
/// zero in [`crate::fade::FadeSpec::new`]'s sub-JND path (`range / step_size` ->
/// +inf -> `u64::MAX` steps -> `Vec::with_capacity` / `Duration` overflow panic).
/// Floored to this so the divide is always well defined. Small enough to never
/// coarsen a real (>= default) configured step.
pub const MIN_STEP_SIZE_DB: f64 = 0.05;

/// Minimum headroom, in dB, the wake ceiling must sit above the synth floor. The
/// wind-down floor clamp uses `lo = synth_floor + 1` and `hi = ceiling - 1`; with
/// this margin (>= 2) `hi >= lo` always holds, so no `clamp(lo, hi)` can ever be
/// called with `lo > hi` (which `f64::clamp` panics on).
const CEILING_MIN_MARGIN_DB: f64 = 2.0;

fn d_min_slew_ms() -> u64 { DEFAULT_MIN_SLEW_MS }
fn d_step_db() -> f64 { DEFAULT_STEP_SIZE_DB }
fn d_floor_db() -> f64 { DEFAULT_FLOOR_LEVEL_DB }
fn d_synth_floor() -> f64 { DEFAULT_SYNTH_FLOOR_DB }
fn d_ceiling_db() -> f64 { DEFAULT_WAKE_CEILING_DB }
fn d_tick_ms() -> u64 { DEFAULT_TICK_MS }
fn d_sleep_fade_s() -> u64 { DEFAULT_SLEEP_FADE_SECS }
fn d_winddown_s() -> u64 { DEFAULT_WINDDOWN_FADE_SECS }
fn d_wake_s() -> u64 { DEFAULT_WAKE_RAMP_SECS }
fn d_max_dur_s() -> u64 { DEFAULT_MAX_DUR_SECS }
fn d_shutdown_fade_s() -> u64 { DEFAULT_SHUTDOWN_FADE_SECS }
fn d_restart_fade_s() -> u64 { DEFAULT_RESTART_FADE_SECS }
fn d_pause_fade_s() -> f64 { DEFAULT_PAUSE_FADE_SECS }
fn d_skip_fade_s() -> f64 { DEFAULT_SKIP_FADE_SECS }
fn d_glide_fade_s() -> f64 { DEFAULT_GLIDE_FADE_SECS }

impl FadeConfig {
    /// Clamp every knob into its safe range at LOAD time, logging any correction,
    /// so an out-of-range TOML value can never silently produce a startle-unsafe
    /// or degenerate fade downstream. This is the ONE place the invariants are
    /// enforced across the config surface; the handler and [`crate::fade`] then
    /// trust the normalized values.
    ///
    /// Enforced here:
    ///   - `min_slew_ms >= STARTLE_HARD_MIN_SLEW_MS` (200 ms): below it startle
    ///     re-emerges and, historically, `FadeSpec::new` rejected EVERY fade
    ///     (silent no-op). Clamp up, don't reject.
    ///   - `tick_ms >= min_slew_ms` (the tick is the step interval).
    ///   - `synth_floor_db` is pinned to the player's cubic-softvol seam value
    ///     ([`crate::player::SYNTH_FLOOR_DB`]) - it is the SINGLE source of truth
    ///     shared by `db_to_mpv_volume`'s mute threshold and the `FadeSpec`
    ///     Silence ramp, so the final step into silence is always reached by the
    ///     slewed ramp, never a jump. Not independently tunable; the field exists
    ///     for visibility and is normalized to the seam.
    ///   - `floor_level_db` kept strictly inside `(synth_floor_db, wake_ceiling_db)`
    ///     so `fade to floor` is a real, non-degenerate wind-down.
    ///   - the default durations (`sleep_fade_secs`, `winddown_fade_secs`,
    ///     `wake_ramp_secs`) clamped into `[min_slew, max_dur]`.
    pub fn normalize(&mut self) {
        use crate::fade::STARTLE_HARD_MIN_SLEW_MS;
        use crate::player::SYNTH_FLOOR_DB;

        // TOTAL and provably panic-free: every knob is coerced into a range such
        // that NO `f64::clamp` below can ever be called with `min > max` or a NaN
        // bound (either of which panics), and no downstream divide-by-zero /
        // overflow can arise. Sanitize non-finite floats FIRST (TOML permits
        // `nan` / `inf`), so a poisoned value can never reach a comparison.
        if !self.step_size_db.is_finite() {
            self.step_size_db = DEFAULT_STEP_SIZE_DB;
        }
        if !self.floor_level_db.is_finite() {
            self.floor_level_db = DEFAULT_FLOOR_LEVEL_DB;
        }
        if !self.synth_floor_db.is_finite() {
            self.synth_floor_db = DEFAULT_SYNTH_FLOOR_DB;
        }
        if !self.wake_ceiling_db.is_finite() {
            self.wake_ceiling_db = DEFAULT_WAKE_CEILING_DB;
        }

        if self.min_slew_ms < STARTLE_HARD_MIN_SLEW_MS {
            tracing::warn!(
                configured = self.min_slew_ms,
                floor = STARTLE_HARD_MIN_SLEW_MS,
                "min_slew_ms below the 200ms startle hard floor; clamping up"
            );
            self.min_slew_ms = STARTLE_HARD_MIN_SLEW_MS;
        }
        // A tick is at least one startle-slew long. It is NOT upper-bounded here
        // (a large min_slew implies a large tick); FadeSpec::new uses saturating
        // Duration arithmetic so even a degenerate huge tick cannot overflow/panic.
        if self.tick_ms < self.min_slew_ms {
            self.tick_ms = self.min_slew_ms;
        }
        // Floor step_size_db to a positive minimum: a 0 (or negative) step would
        // divide by zero in FadeSpec::new's sub-JND path -> u64::MAX steps ->
        // capacity/Duration overflow panic. This is the config-side guarantee;
        // FadeSpec::new guards the divide too (belt and suspenders).
        if self.step_size_db < MIN_STEP_SIZE_DB {
            tracing::warn!(
                configured = self.step_size_db,
                floor = MIN_STEP_SIZE_DB,
                "step_size_db not positive enough; flooring to the minimum"
            );
            self.step_size_db = MIN_STEP_SIZE_DB;
        }
        // The synth floor is defined by the player's cubic softvol seam. Pin it
        // so the mute threshold and the Silence ramp agree exactly.
        if (self.synth_floor_db - SYNTH_FLOOR_DB).abs() > f64::EPSILON {
            tracing::warn!(
                configured = self.synth_floor_db,
                seam = SYNTH_FLOOR_DB,
                "synth_floor_db is fixed by the cubic softvol seam; pinning to it"
            );
            self.synth_floor_db = SYNTH_FLOOR_DB;
        }
        // Keep the wake ceiling a sane margin above the synth floor so the
        // wind-down-floor clamp bounds (lo = synth_floor + 1, hi = ceiling - 1)
        // always satisfy lo <= hi - this is what makes the clamp below panic-free.
        let min_ceiling = self.synth_floor_db + CEILING_MIN_MARGIN_DB;
        if self.wake_ceiling_db < min_ceiling {
            tracing::warn!(
                configured = self.wake_ceiling_db,
                floor = min_ceiling,
                "wake_ceiling_db too close to (or below) the synth floor; raising"
            );
            self.wake_ceiling_db = min_ceiling;
        }
        // Keep the wind-down floor a real level strictly between silence and the
        // ceiling (read floor_level_db so it is never a dead knob). With the
        // ceiling margin enforced above, lo <= hi always; the max() is a
        // belt-and-suspenders so no clamp can ever see min > max.
        let lo = self.synth_floor_db + 1.0;
        let hi = (self.wake_ceiling_db - 1.0).max(lo);
        if self.floor_level_db <= lo || self.floor_level_db >= hi {
            let clamped = self.floor_level_db.clamp(lo, hi);
            tracing::warn!(
                configured = self.floor_level_db,
                clamped,
                "floor_level_db out of (synth_floor, ceiling); clamping"
            );
            self.floor_level_db = clamped;
        }
        // Clamp every default duration into [min_slew, max_dur]. max_dur must
        // itself be >= the per-second min derived from min_slew, otherwise the
        // clamp(min_s, max_dur) would have min > max and panic.
        let min_s = ((self.min_slew_ms as f64 / 1000.0).ceil() as u64).max(1);
        if self.max_dur_secs < min_s {
            tracing::warn!(
                configured = self.max_dur_secs,
                floor = min_s,
                "max_dur_secs below the per-second min_slew floor; raising"
            );
            self.max_dur_secs = min_s;
        }
        for d in [
            &mut self.sleep_fade_secs,
            &mut self.winddown_fade_secs,
            &mut self.wake_ramp_secs,
            &mut self.shutdown_fade_secs,
            &mut self.restart_fade_secs,
        ] {
            *d = (*d).clamp(min_s, self.max_dur_secs);
        }
        // The pause fade is a FLOAT-second knob; a sub-second nominal is allowed
        // down to the per-millisecond min_slew. Sanitize non-finite first (TOML
        // permits nan/inf), then clamp into [min_slew_s, max_dur_secs]. The lower
        // bound uses the exact min_slew in seconds (not the ceil'd `min_s`) so a
        // 0.25s min_slew stays a 0.25s floor, keeping the pause genuinely short.
        if !self.pause_fade_secs.is_finite() {
            self.pause_fade_secs = DEFAULT_PAUSE_FADE_SECS;
        }
        let pause_lo = self.min_slew_ms as f64 / 1000.0;
        self.pause_fade_secs = self.pause_fade_secs.clamp(pause_lo, self.max_dur_secs as f64);
        // The skip-dip fade is the same FLOAT-second shape as the pause fade.
        if !self.skip_fade_secs.is_finite() {
            self.skip_fade_secs = DEFAULT_SKIP_FADE_SECS;
        }
        self.skip_fade_secs = self.skip_fade_secs.clamp(pause_lo, self.max_dur_secs as f64);
        // The glide fade is the same FLOAT-second shape as the pause fade.
        if !self.glide_fade_secs.is_finite() {
            self.glide_fade_secs = DEFAULT_GLIDE_FADE_SECS;
        }
        self.glide_fade_secs = self.glide_fade_secs.clamp(pause_lo, self.max_dur_secs as f64);
    }
}

impl Default for FadeConfig {
    fn default() -> Self {
        Self {
            min_slew_ms: DEFAULT_MIN_SLEW_MS,
            step_size_db: DEFAULT_STEP_SIZE_DB,
            floor_level_db: DEFAULT_FLOOR_LEVEL_DB,
            synth_floor_db: DEFAULT_SYNTH_FLOOR_DB,
            wake_ceiling_db: DEFAULT_WAKE_CEILING_DB,
            tick_ms: DEFAULT_TICK_MS,
            sleep_fade_secs: DEFAULT_SLEEP_FADE_SECS,
            winddown_fade_secs: DEFAULT_WINDDOWN_FADE_SECS,
            wake_ramp_secs: DEFAULT_WAKE_RAMP_SECS,
            max_dur_secs: DEFAULT_MAX_DUR_SECS,
            shutdown_fade_secs: DEFAULT_SHUTDOWN_FADE_SECS,
            restart_fade_secs: DEFAULT_RESTART_FADE_SECS,
            pause_fade_secs: DEFAULT_PAUSE_FADE_SECS,
            skip_fade_secs: DEFAULT_SKIP_FADE_SECS,
            glide_fade_secs: DEFAULT_GLIDE_FADE_SECS,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct ServerConfig {
    /// Base URL of the OpenSubsonic server, e.g. https://music.example.com
    pub url: String,
    pub username: String,
    pub password: String,
    /// Client name reported to the server (OpenSubsonic `c` param).
    #[serde(default = "default_client_name")]
    pub client_name: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct MpdConfig {
    /// Address the MPD-protocol listener binds to.
    ///
    /// Default is 6601 ON PURPOSE: the real mopidy daemon owns 6600 and must
    /// not be disturbed. Production parity flips this to 6600 once mopidy is
    /// retired.
    #[serde(default = "default_mpd_bind")]
    pub bind: String,
}

impl Default for MpdConfig {
    fn default() -> Self {
        Self { bind: default_mpd_bind() }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct MprisConfig {
    /// Expose the MPRIS (org.mpris.MediaPlayer2.hypodj) D-Bus server on the
    /// session bus so desktops show now-playing + controls. Default true; set
    /// false to disable. Registered under the `.hypodj` bus name (NOT `.mopidy`),
    /// so it never conflicts with a running mopidy MPRIS server.
    #[serde(default = "default_mpris_enable")]
    pub enable: bool,
    /// Command run by the MPRIS root `Raise()` method when a desktop media widget
    /// is clicked - typically a terminal running the user's music client
    /// (e.g. `["kitty", "ncmpcpp"]`). The first element is the program, the rest
    /// are args. Absent = None = `CanRaise` reports false and `Raise()` is a no-op.
    #[serde(default)]
    pub raise_command: Option<Vec<String>>,
}

impl Default for MprisConfig {
    fn default() -> Self {
        Self { enable: default_mpris_enable(), raise_command: None }
    }
}

fn default_mpris_enable() -> bool {
    true
}

fn default_client_name() -> String {
    "hypodj".to_string()
}

fn default_mpd_bind() -> String {
    "127.0.0.1:6601".to_string()
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("reading config {0}: {1}")]
    Io(String, #[source] std::io::Error),
    #[error("parsing config: {0}")]
    Parse(#[from] toml::de::Error),
}

impl Config {
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let raw = std::fs::read_to_string(path)
            .map_err(|e| ConfigError::Io(path.display().to_string(), e))?;
        let mut cfg: Config = toml::from_str(&raw)?;
        cfg.fade.normalize();
        cfg.recognize.normalize();
        cfg.store.normalize();
        cfg.heard.normalize();
        cfg.tape.normalize();
        Ok(cfg)
    }

    /// Parse from a TOML string (test/embedded use). Kept as an inherent method
    /// with this name for ergonomics; it is not the `FromStr` trait.
    ///
    /// Normalizes the SAME sections as [`Config::load`] - a section normalized in
    /// only one of the two is a latent bug where a test-parsed config and a
    /// file-loaded config disagree.
    #[allow(clippy::should_implement_trait)]
    pub fn from_str(raw: &str) -> Result<Self, ConfigError> {
        let mut cfg: Config = toml::from_str(raw)?;
        cfg.fade.normalize();
        cfg.recognize.normalize();
        cfg.store.normalize();
        cfg.heard.normalize();
        cfg.tape.normalize();
        Ok(cfg)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_defaults_bind_to_6601_not_6600() {
        // No [mpd] section -> the default must be 6601, honoring the hard
        // constraint that mopidy owns 6600.
        let cfg = Config::from_str(
            r#"
            [server]
            url = "https://music.example.com"
            username = "alice"
            password = "s3cr3t"
        "#,
        )
        .expect("valid config");
        assert_eq!(cfg.server.url, "https://music.example.com");
        assert_eq!(cfg.server.username, "alice");
        assert_eq!(cfg.server.client_name, "hypodj");
        assert_eq!(cfg.mpd.bind, "127.0.0.1:6601");
    }

    #[test]
    fn fade_section_defaults_and_overrides() {
        // No [fade] section -> every knob defaults from the research constants.
        let cfg = Config::from_str(
            r#"
            [server]
            url = "https://m"
            username = "a"
            password = "b"
        "#,
        )
        .unwrap();
        assert_eq!(cfg.fade.min_slew_ms, 250);
        assert_eq!(cfg.fade.step_size_db, 0.75);
        assert_eq!(cfg.fade.synth_floor_db, -60.0);
        assert_eq!(cfg.fade.max_dur_secs, 1800);
        assert_eq!(cfg.fade.pause_fade_secs, 0.5, "pause fade defaults to a short 0.5s");

        // A partial [fade] section overrides only the named knobs.
        let cfg = Config::from_str(
            r#"
            [server]
            url = "https://m"
            username = "a"
            password = "b"
            [fade]
            winddown_fade_secs = 120
            step_size_db = 0.5
        "#,
        )
        .unwrap();
        assert_eq!(cfg.fade.winddown_fade_secs, 120);
        assert_eq!(cfg.fade.step_size_db, 0.5);
        // Untouched knobs still default.
        assert_eq!(cfg.fade.min_slew_ms, 250);
    }

    #[test]
    fn normalize_clamps_bad_min_slew_and_pins_synth_floor() {
        // A sub-200ms min_slew (which historically no-op'd every fade) is clamped
        // UP to the startle hard floor, and an off-seam synth_floor is pinned.
        let cfg = Config::from_str(
            r#"
            [server]
            url = "https://m"
            username = "a"
            password = "b"
            [fade]
            min_slew_ms = 50
            synth_floor_db = -45.0
            tick_ms = 60
        "#,
        )
        .unwrap();
        assert_eq!(cfg.fade.min_slew_ms, 200, "min_slew clamped to startle floor");
        assert!(cfg.fade.tick_ms >= cfg.fade.min_slew_ms, "tick >= min_slew");
        assert_eq!(cfg.fade.synth_floor_db, -60.0, "synth floor pinned to the seam");
    }

    #[test]
    fn normalize_clamps_durations_and_floor() {
        let cfg = Config::from_str(
            r#"
            [server]
            url = "https://m"
            username = "a"
            password = "b"
            [fade]
            max_dur_secs = 100
            winddown_fade_secs = 999999
            sleep_fade_secs = 999999
            floor_level_db = 5.0
        "#,
        )
        .unwrap();
        assert_eq!(cfg.fade.winddown_fade_secs, 100, "clamped to max_dur");
        assert_eq!(cfg.fade.sleep_fade_secs, 100, "sleep_fade read + clamped to max_dur");
        // floor pushed back inside (synth_floor, ceiling) = (-60, 0).
        assert!(cfg.fade.floor_level_db > -60.0 && cfg.fade.floor_level_db < 0.0);
    }

    // C1: normalize is TOTAL - degenerate/extreme values that would make an
    // internal clamp have min > max (or divide by zero downstream) must be
    // coerced to safe bounds, NEVER panic.
    #[test]
    fn normalize_never_panics_on_degenerate_values() {
        // max_dur_secs = 0: the duration clamp(min_s, max_dur) would have
        // min_s (>= 1) > 0 and panic without the guard.
        let cfg = Config::from_str(
            r#"
            [server]
            url = "https://m"
            username = "a"
            password = "b"
            [fade]
            max_dur_secs = 0
        "#,
        )
        .unwrap();
        assert!(cfg.fade.max_dur_secs >= 1, "max_dur raised to per-second min");
        assert!(cfg.fade.winddown_fade_secs <= cfg.fade.max_dur_secs);

        // wake_ceiling_db = -60: hi = ceiling - 1 = -61 < lo = synth_floor + 1 =
        // -59, so the floor clamp would be clamp(-59, -61) and panic.
        let cfg = Config::from_str(
            r#"
            [server]
            url = "https://m"
            username = "a"
            password = "b"
            [fade]
            wake_ceiling_db = -60.0
        "#,
        )
        .unwrap();
        assert!(
            cfg.fade.wake_ceiling_db >= cfg.fade.synth_floor_db + CEILING_MIN_MARGIN_DB,
            "ceiling raised to a sane margin above the synth floor"
        );
        assert!(cfg.fade.floor_level_db > cfg.fade.synth_floor_db);
        assert!(cfg.fade.floor_level_db < cfg.fade.wake_ceiling_db);

        // step_size_db = 0: a 0 step divides by zero downstream. Floored positive.
        let cfg = Config::from_str(
            r#"
            [server]
            url = "https://m"
            username = "a"
            password = "b"
            [fade]
            step_size_db = 0.0
        "#,
        )
        .unwrap();
        assert!(cfg.fade.step_size_db >= MIN_STEP_SIZE_DB);
        // And FadeSpec::new over this normalized config must not panic (belt).
        use crate::fade::{Curve, FadeSpec, FadeTarget, StartleBounds};
        let bounds = StartleBounds {
            min_slew: std::time::Duration::from_millis(cfg.fade.min_slew_ms),
            step_size_db: cfg.fade.step_size_db,
            synth_floor_db: cfg.fade.synth_floor_db,
            sub_jnd: true,
        };
        let _ = FadeSpec::new(
            0.0,
            FadeTarget::Db(-45.0),
            std::time::Duration::from_secs(60),
            std::time::Duration::from_millis(cfg.fade.tick_ms),
            Curve::DbLinear,
            bounds,
        );
    }

    // C1: a battery of extreme / non-finite values must all normalize to sane,
    // ordered bounds without panicking.
    #[test]
    fn normalize_extreme_battery_stays_sane() {
        let cases = [
            "min_slew_ms = 0\ntick_ms = 0\nmax_dur_secs = 0\nstep_size_db = 0.0",
            "step_size_db = -5.0\nwake_ceiling_db = -100.0\nfloor_level_db = 999.0",
            "wake_ceiling_db = nan\nstep_size_db = inf\nfloor_level_db = -inf",
            "max_dur_secs = 1\nmin_slew_ms = 999999\ntick_ms = 1",
            "synth_floor_db = 40.0\nwake_ceiling_db = 41.0\nfloor_level_db = 0.0",
        ];
        for extra in cases {
            let raw = format!(
                "[server]\nurl = \"https://m\"\nusername = \"a\"\npassword = \"b\"\n[fade]\n{extra}\n"
            );
            let cfg = Config::from_str(&raw).unwrap_or_else(|e| panic!("parse {extra:?}: {e}"));
            let f = &cfg.fade;
            // All invariants hold: no clamp could have had min > max.
            assert!(f.min_slew_ms >= 200);
            assert!(f.tick_ms >= f.min_slew_ms);
            assert!(f.step_size_db >= MIN_STEP_SIZE_DB);
            assert!(f.step_size_db.is_finite());
            assert_eq!(f.synth_floor_db, -60.0);
            assert!(f.wake_ceiling_db >= f.synth_floor_db + CEILING_MIN_MARGIN_DB);
            assert!(f.wake_ceiling_db.is_finite());
            assert!(f.floor_level_db > f.synth_floor_db && f.floor_level_db < f.wake_ceiling_db);
            let min_s = ((f.min_slew_ms as f64 / 1000.0).ceil() as u64).max(1);
            assert!(f.max_dur_secs >= min_s);
            for d in [f.sleep_fade_secs, f.winddown_fade_secs, f.wake_ramp_secs] {
                assert!(d >= min_s && d <= f.max_dur_secs, "duration {d} out of range in {extra:?}");
            }
        }
    }

    #[test]
    fn continuation_section_defaults_off_and_parses_station() {
        // No [continuation] section -> the feature is entirely off (station None).
        let cfg = Config::from_str(
            r#"
            [server]
            url = "https://m"
            username = "a"
            password = "b"
        "#,
        )
        .unwrap();
        assert_eq!(cfg.continuation.station, None, "no section => feature off");

        // A [continuation] station (name or URL) parses through.
        let cfg = Config::from_str(
            r#"
            [server]
            url = "https://m"
            username = "a"
            password = "b"
            [continuation]
            station = "NTS 1"
        "#,
        )
        .unwrap();
        assert_eq!(cfg.continuation.station.as_deref(), Some("NTS 1"));
        // Back-compat: mode defaults to Radio and autofill_count to 20 whenever the
        // key is absent (a station-only config, or no section at all).
        assert_eq!(cfg.continuation.mode, ContinuationMode::Radio, "mode defaults to radio");
        assert_eq!(cfg.continuation.autofill_count, DEFAULT_AUTOFILL_COUNT);
        let cfg = Config::from_str(
            r#"
            [server]
            url = "https://m"
            username = "a"
            password = "b"
        "#,
        )
        .unwrap();
        assert_eq!(cfg.continuation.mode, ContinuationMode::Radio, "no section => radio");
        assert_eq!(cfg.continuation.autofill_count, DEFAULT_AUTOFILL_COUNT);

        // mode = "autofill" parses (lowercase serde rename) and a custom count sticks.
        let cfg = Config::from_str(
            r#"
            [server]
            url = "https://m"
            username = "a"
            password = "b"
            [continuation]
            mode = "autofill"
            autofill_count = 12
        "#,
        )
        .unwrap();
        assert_eq!(cfg.continuation.mode, ContinuationMode::Autofill);
        assert_eq!(cfg.continuation.autofill_count, 12);
    }

    #[test]
    fn recognize_section_defaults_on_and_clamps_interval() {
        // No [recognize] section -> defaults: auto ON, interval 300s.
        let cfg = Config::from_str(
            r#"
            [server]
            url = "https://m"
            username = "a"
            password = "b"
        "#,
        )
        .unwrap();
        assert!(cfg.recognize.auto, "auto-identify defaults ON");
        assert_eq!(cfg.recognize.interval_secs, DEFAULT_RECOGNIZE_INTERVAL_SECS);

        // auto can be turned off, and a sub-floor interval clamps to the 60s floor at
        // load so no config typo can hammer Shazam.
        let cfg = Config::from_str(
            r#"
            [server]
            url = "https://m"
            username = "a"
            password = "b"
            [recognize]
            auto = false
            interval_secs = 1
        "#,
        )
        .unwrap();
        assert!(!cfg.recognize.auto, "auto = false disables the feature");
        assert_eq!(
            cfg.recognize.interval_secs, RECOGNIZE_MIN_INTERVAL_SECS,
            "interval_secs floor-clamped to 60"
        );
    }

    /// The minimal valid config: a `[server]` section and nothing else. Every
    /// section-default test starts from this so "no section at all" is what is
    /// actually being exercised.
    const BARE: &str = r#"
        [server]
        url = "https://m"
        username = "a"
        password = "b"
    "#;

    #[test]
    fn store_section_defaults_on_with_the_documented_budget() {
        // No [store] section at all -> the store is ON by default (the offline path
        // is meant to be the everyday path) with the documented 16 GiB budget.
        let cfg = Config::from_str(BARE).unwrap();
        assert!(cfg.store.enable, "the store defaults ON");
        assert_eq!(cfg.store.dir, None, "dir defaults to <state_dir>/store");
        assert_eq!(cfg.store.max_bytes, 17_179_869_184, "16 GiB");
        assert_eq!(cfg.store.queue_ahead, 3);
        assert_eq!(cfg.store.sync_interval_secs, 900);
        assert!(cfg.store.pin_starred, "starred is the pin set by default");
    }

    #[test]
    fn store_section_partial_overrides_only_named_keys() {
        let cfg = Config::from_str(
            r#"
            [server]
            url = "https://m"
            username = "a"
            password = "b"
            [store]
            dir = "/srv/hypodj-audio"
            max_bytes = 1073741824
            queue_ahead = 0
        "#,
        )
        .unwrap();
        assert_eq!(cfg.store.dir.as_deref(), Some(Path::new("/srv/hypodj-audio")));
        assert_eq!(cfg.store.max_bytes, 1_073_741_824);
        assert_eq!(cfg.store.queue_ahead, 0, "0 disables queue-ahead downloads");
        // Untouched keys still default.
        assert!(cfg.store.enable);
        assert_eq!(cfg.store.sync_interval_secs, DEFAULT_STORE_SYNC_INTERVAL_SECS);
        assert!(cfg.store.pin_starred);

        // The master switch and the pin set are both individually flippable.
        let cfg = Config::from_str(
            r#"
            [server]
            url = "https://m"
            username = "a"
            password = "b"
            [store]
            enable = false
            pin_starred = false
        "#,
        )
        .unwrap();
        assert!(!cfg.store.enable);
        assert!(!cfg.store.pin_starred);
    }

    #[test]
    fn heard_normalize_floors_keep_sessions_and_is_total() {
        // `keep_sessions = 0` would make the retention sweep delete the session it just
        // opened, so the ledger would silently record nothing. Clamp UP, never reject.
        let cfg = Config::from_str(
            r#"
            [server]
            url = "https://m"
            username = "a"
            password = "b"
            [heard]
            keep_sessions = 0
        "#,
        )
        .unwrap();
        assert_eq!(cfg.heard.keep_sessions, HEARD_MIN_KEEP_SESSIONS);
        assert!(cfg.heard.enable, "the section existing must not turn the feature off");

        // An in-range value is untouched, and the dir override is carried verbatim.
        let cfg = Config::from_str(
            r#"
            [server]
            url = "https://m"
            username = "a"
            password = "b"
            [heard]
            enable = false
            dir = "/tmp/ledger"
            keep_sessions = 7
            dedupe_window_secs = 60
        "#,
        )
        .unwrap();
        assert!(!cfg.heard.enable);
        assert_eq!(cfg.heard.dir, Some(PathBuf::from("/tmp/ledger")));
        assert_eq!(cfg.heard.keep_sessions, 7);
        assert_eq!(cfg.heard.dedupe_window_secs, 60);

        // TOTAL: no parseable value can panic, including the extremes.
        for raw in ["keep_sessions = 4294967295", "dedupe_window_secs = 0", "keep_sessions = 1"] {
            let mut c = HeardConfig::default();
            let _ = raw;
            c.keep_sessions = 0;
            c.dedupe_window_secs = 0;
            c.normalize();
            assert_eq!(c.keep_sessions, HEARD_MIN_KEEP_SESSIONS);
            let mut c = HeardConfig { keep_sessions: u32::MAX, ..HeardConfig::default() };
            c.normalize();
            assert_eq!(c.keep_sessions, u32::MAX, "a large value is not clamped down");
        }
    }

    #[test]
    fn store_normalize_floors_max_bytes_and_interval() {
        // A pathologically small budget would make every download instantly
        // over-cap (download-evict thrash), and a 1s cadence would hammer the
        // server. Both clamp UP at load, never reject.
        let cfg = Config::from_str(
            r#"
            [server]
            url = "https://m"
            username = "a"
            password = "b"
            [store]
            max_bytes = 1
            sync_interval_secs = 1
        "#,
        )
        .unwrap();
        assert_eq!(cfg.store.max_bytes, STORE_MIN_MAX_BYTES, "floored to 64 MiB");
        assert_eq!(
            cfg.store.sync_interval_secs, STORE_MIN_SYNC_INTERVAL_SECS,
            "floored to 60s"
        );

        // Zero is the same story (and must not underflow anything).
        let cfg = Config::from_str(
            r#"
            [server]
            url = "https://m"
            username = "a"
            password = "b"
            [store]
            max_bytes = 0
            sync_interval_secs = 0
        "#,
        )
        .unwrap();
        assert_eq!(cfg.store.max_bytes, STORE_MIN_MAX_BYTES);
        assert_eq!(cfg.store.sync_interval_secs, STORE_MIN_SYNC_INTERVAL_SECS);

        // An in-range value is untouched by normalize (it is a FLOOR, not a pin).
        let cfg = Config::from_str(
            r#"
            [server]
            url = "https://m"
            username = "a"
            password = "b"
            [store]
            max_bytes = 209715200
            sync_interval_secs = 120
        "#,
        )
        .unwrap();
        assert_eq!(cfg.store.max_bytes, 209_715_200);
        assert_eq!(cfg.store.sync_interval_secs, 120);
    }

    // The manual `impl Default for StoreConfig` MUST match the per-field serde
    // defaults, or a config with no [store] section behaves differently from one
    // with an empty [store] section. A derived Default would give enable = false
    // and max_bytes = 0 - the store silently off for every existing config.
    #[test]
    fn store_manual_default_matches_serde_defaults() {
        // (a) an EMPTY [store] section: every field comes from its `d_*` fn.
        let from_serde = Config::from_str(
            r#"
            [server]
            url = "https://m"
            username = "a"
            password = "b"
            [store]
        "#,
        )
        .unwrap()
        .store;
        // (b) NO [store] section: the field's #[serde(default)] uses `Default`.
        let from_default = Config::from_str(BARE).unwrap().store;

        assert_eq!(from_default.enable, from_serde.enable);
        assert_eq!(from_default.dir, from_serde.dir);
        assert_eq!(from_default.max_bytes, from_serde.max_bytes);
        assert_eq!(from_default.queue_ahead, from_serde.queue_ahead);
        assert_eq!(from_default.sync_interval_secs, from_serde.sync_interval_secs);
        assert_eq!(from_default.pin_starred, from_serde.pin_starred);

        // And the bare `Default` impl itself (used by main.rs / tests that build a
        // Config in code) agrees with both, field by field.
        let d = StoreConfig::default();
        assert_eq!(d.enable, from_serde.enable);
        assert_eq!(d.dir, from_serde.dir);
        assert_eq!(d.max_bytes, from_serde.max_bytes);
        assert_eq!(d.queue_ahead, from_serde.queue_ahead);
        assert_eq!(d.sync_interval_secs, from_serde.sync_interval_secs);
        assert_eq!(d.pin_starred, from_serde.pin_starred);
    }

    // The same parity bar for the OTHER manually-defaulted sections, so the rule
    // is enforced across the config surface rather than only where it was last
    // remembered.
    #[test]
    fn manual_defaults_match_serde_defaults_for_every_section() {
        let bare = Config::from_str(BARE).unwrap();
        let empty = Config::from_str(
            r#"
            [server]
            url = "https://m"
            username = "a"
            password = "b"
            [mpd]
            [mpris]
            [fade]
            [restart]
            [continuation]
            [recognize]
            [store]
            [heard]
            [tape]
        "#,
        )
        .unwrap();
        assert_eq!(bare.mpd.bind, empty.mpd.bind);
        assert_eq!(bare.mpris.enable, empty.mpris.enable);
        assert_eq!(bare.mpris.raise_command, empty.mpris.raise_command);
        assert_eq!(bare.restart.state_dir, empty.restart.state_dir);
        assert_eq!(bare.restart.checkpoint_secs, empty.restart.checkpoint_secs);
        assert_eq!(bare.continuation.station, empty.continuation.station);
        assert_eq!(bare.continuation.mode, empty.continuation.mode);
        assert_eq!(bare.continuation.autofill_count, empty.continuation.autofill_count);
        assert_eq!(bare.recognize.auto, empty.recognize.auto);
        assert_eq!(bare.recognize.interval_secs, empty.recognize.interval_secs);
        assert_eq!(bare.store.enable, empty.store.enable);
        assert_eq!(bare.store.max_bytes, empty.store.max_bytes);
        assert_eq!(bare.store.queue_ahead, empty.store.queue_ahead);
        assert_eq!(bare.store.sync_interval_secs, empty.store.sync_interval_secs);
        assert_eq!(bare.store.pin_starred, empty.store.pin_starred);
        assert_eq!(bare.heard.enable, empty.heard.enable);
        assert_eq!(bare.heard.dir, empty.heard.dir);
        assert_eq!(bare.heard.keep_sessions, empty.heard.keep_sessions);
        assert_eq!(bare.heard.dedupe_window_secs, empty.heard.dedupe_window_secs);
        assert_eq!(bare.tape.enable, empty.tape.enable);
        assert_eq!(bare.tape.dir, empty.tape.dir);
        assert_eq!(bare.tape.max_bytes, empty.tape.max_bytes);
        assert_eq!(bare.tape.back_secs, empty.tape.back_secs);
        assert_eq!(bare.tape.max_secs, empty.tape.max_secs);
        // The float/int fade knobs too, since [fade] is the largest section.
        assert_eq!(bare.fade.min_slew_ms, empty.fade.min_slew_ms);
        assert_eq!(bare.fade.max_dur_secs, empty.fade.max_dur_secs);
        assert_eq!(bare.fade.pause_fade_secs, empty.fade.pause_fade_secs);
        assert_eq!(bare.fade.skip_fade_secs, empty.fade.skip_fade_secs);
        assert_eq!(bare.fade.glide_fade_secs, empty.fade.glide_fade_secs);
    }

    // Config::load and Config::from_str must normalize the SAME sections: a
    // section normalized in only one is a latent divergence between the daemon's
    // real load path and every test.
    #[test]
    fn load_normalizes_the_same_sections_as_from_str() {
        let raw = r#"
            [server]
            url = "https://m"
            username = "a"
            password = "b"
            [fade]
            min_slew_ms = 10
            [recognize]
            interval_secs = 2
            [store]
            max_bytes = 3
            sync_interval_secs = 4
            [heard]
            keep_sessions = 0
            [tape]
            max_bytes = 5
            back_secs = 1
            max_secs = 2
        "#;
        let dir = std::env::temp_dir().join(format!("hypodj-config-load-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("config.toml");
        std::fs::write(&path, raw).expect("write config");
        let loaded = Config::load(&path).expect("load");
        let parsed = Config::from_str(raw).expect("from_str");
        assert_eq!(loaded.fade.min_slew_ms, parsed.fade.min_slew_ms);
        assert_eq!(loaded.recognize.interval_secs, parsed.recognize.interval_secs);
        assert_eq!(loaded.store.max_bytes, parsed.store.max_bytes);
        assert_eq!(loaded.store.sync_interval_secs, parsed.store.sync_interval_secs);
        assert_eq!(loaded.heard.keep_sessions, parsed.heard.keep_sessions);
        assert_eq!(loaded.tape.max_bytes, parsed.tape.max_bytes);
        assert_eq!(loaded.tape.back_secs, parsed.tape.back_secs);
        assert_eq!(loaded.tape.max_secs, parsed.tape.max_secs);
        // And the values really are the normalized ones, not the raw TOML.
        assert_eq!(loaded.store.max_bytes, STORE_MIN_MAX_BYTES);
        assert_eq!(loaded.store.sync_interval_secs, STORE_MIN_SYNC_INTERVAL_SECS);
        assert_eq!(loaded.heard.keep_sessions, HEARD_MIN_KEEP_SESSIONS);
        assert_eq!(loaded.tape.max_bytes, TAPE_MIN_MAX_BYTES);
        assert_eq!(loaded.tape.back_secs, TAPE_MIN_BACK_SECS);
        assert_eq!(loaded.tape.max_secs, TAPE_MIN_MAX_SECS);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn tape_normalize_floors_its_three_knobs_and_never_panics() {
        // Clamp UP from a parsed TOML, in-range untouched, dir carried verbatim, and both
        // extremes total - the `heard_normalize_floors_keep_sessions` shape.
        let low = Config::from_str(
            r#"
            [server]
            url = "https://m"
            username = "a"
            password = "b"
            [tape]
            dir = "/var/tmp/mine"
            max_bytes = 1
            back_secs = 0
            max_secs = 0
        "#,
        )
        .unwrap();
        assert_eq!(low.tape.max_bytes, TAPE_MIN_MAX_BYTES);
        assert_eq!(low.tape.back_secs, TAPE_MIN_BACK_SECS);
        assert_eq!(low.tape.max_secs, TAPE_MIN_MAX_SECS);
        assert_eq!(low.tape.dir, Some(PathBuf::from("/var/tmp/mine")));
        assert!(low.tape.enable);

        let in_range = Config::from_str(
            r#"
            [server]
            url = "https://m"
            username = "a"
            password = "b"
            [tape]
            enable = false
            max_bytes = 1073741824
            back_secs = 600
            max_secs = 900
        "#,
        )
        .unwrap();
        assert_eq!(in_range.tape.max_bytes, 1_073_741_824);
        assert_eq!(in_range.tape.back_secs, 600);
        assert_eq!(in_range.tape.max_secs, 900);
        assert!(!in_range.tape.enable);

        // TOTAL at the extremes: nothing overflows, nothing panics.
        let mut huge = TapeConfig {
            max_bytes: u64::MAX,
            back_secs: u64::MAX,
            max_secs: u64::MAX,
            ..TapeConfig::default()
        };
        huge.normalize();
        assert_eq!(huge.max_bytes, u64::MAX);
        let mut zero = TapeConfig {
            max_bytes: 0,
            back_secs: 0,
            max_secs: 0,
            ..TapeConfig::default()
        };
        zero.normalize();
        assert_eq!(zero.max_bytes, TAPE_MIN_MAX_BYTES);
    }

    #[test]
    fn explicit_bind_overrides_default() {
        let cfg = Config::from_str(
            r#"
            [server]
            url = "https://m.example.com"
            username = "a"
            password = "b"
            [mpd]
            bind = "127.0.0.1:7000"
        "#,
        )
        .unwrap();
        assert_eq!(cfg.mpd.bind, "127.0.0.1:7000");
    }
}
