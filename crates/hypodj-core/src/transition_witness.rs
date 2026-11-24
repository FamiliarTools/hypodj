//! TRANSITION WITNESS: a live-libmpv recorder of what every head change SOUNDS like.
//!
//! This is the verification harness for the one-transition-engine redesign. For each
//! cause that can move the head (skip, previous, natural EOF, a jump, a delete of the
//! playing row, a full queue swap, a plan action, radio, undo...) it drives the REAL
//! handler over a REAL two-deck mpv pair (`ao=null`, so the room stays silent) wired to
//! the real director spine, and samples every deck's actual mpv state (`path`,
//! `core-idle`, `volume`) every few milliseconds through the test-only
//! [`PlayerHandle::witness_probe`]. The samples are then classified into the audible
//! SHAPE of the transition:
//!
//! - `CROSSFADE`: the old and new sources were audible at the same instant (two decks).
//! - `DIP`: one source at a time, but the old one ducked and/or the new one rose.
//! - `CUT`: the switch happened at full level with no measurable gap.
//! - `SILENCE-START`: a gap of nothing audible between the old source and the new.
//! - `NONE`: the audible source did not change (e.g. moving the playing row).
//! - `STOP`: the old source went away and nothing new became audible.
//!
//! It RECORDS, it asserts nothing about the desired shape. `transition_witness_record`
//! prints the table and writes it to `$HYPODJ_WITNESS_OUT` (default: a temp file);
//! `HYPODJ_WITNESS_BLESS=1` writes the committed baseline
//! `tests/fixtures/transition-baseline.tsv` instead. `transition_witness_diff` compares
//! a recorded run against that baseline, cause by cause.
//!
//! Self-contained: the Subsonic `stream` endpoint is a tiny loopback HTTP server serving
//! generated tone WAVs, so nothing touches the user's daemon, port 6600, Navidrome, the
//! offline store, or a speaker.

use super::*;
use crate::player::{DeckProbe, MpvPlayer};
use crate::plan::{Action, QueueSelector};
use std::collections::HashMap as Map;
use std::io::{BufRead, BufReader, Write};

const SAMPLE_MS: u64 = 10;
const PRE_MS: u64 = 400;
const POST_MS: u64 = 3500;
/// A source counts as AUDIBLE above this mpv softvol (cubic: 2 is about -102 dB, i.e.
/// anything the fade driver did not deliberately park at zero).
const AUDIBLE_VOL: f64 = 0.5;
/// A gap shorter than this is a CUT: the sampler cannot resolve it, and neither can an ear.
const GAP_MS: u64 = 60;

// ── fixtures ────────────────────────────────────────────────────────────────

fn tone_wav(freq: f64, secs: f64) -> Vec<u8> {
    let sr = 22050u32;
    let n = (sr as f64 * secs) as usize;
    let mut pcm: Vec<u8> = Vec::with_capacity(n * 2);
    for i in 0..n {
        let t = i as f64 / sr as f64;
        let s = (2.0 * std::f64::consts::PI * freq * t).sin() * 0.4;
        pcm.extend_from_slice(&((s * i16::MAX as f64) as i16).to_le_bytes());
    }
    let data_len = pcm.len() as u32;
    let mut wav = Vec::with_capacity(44 + pcm.len());
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(36 + data_len).to_le_bytes());
    wav.extend_from_slice(b"WAVEfmt ");
    wav.extend_from_slice(&16u32.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes());
    wav.extend_from_slice(&sr.to_le_bytes());
    wav.extend_from_slice(&(sr * 2).to_le_bytes());
    wav.extend_from_slice(&2u16.to_le_bytes());
    wav.extend_from_slice(&16u16.to_le_bytes());
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&data_len.to_le_bytes());
    wav.extend_from_slice(&pcm);
    wav
}

/// The witness library: id -> (album id, track, seconds).
fn library() -> Vec<(&'static str, &'static str, u32, u32)> {
    vec![
        ("A", "alb-1", 1, 8),
        ("B", "alb-2", 1, 8),
        ("C", "alb-3", 1, 8),
        ("D", "alb-4", 1, 8),
        ("E", "alb-5", 1, 8),
        ("R", "alb-6", 1, 8),
        // Short pair for the natural-EOF causes.
        ("X", "alb-7", 1, 3),
        ("Y", "alb-8", 1, 8),
        ("P", "alb-9", 1, 3),
        ("Q", "alb-9", 2, 8),
    ]
}

fn song(id: &str) -> Song {
    let (_, alb, track, secs) =
        library().into_iter().find(|(i, ..)| *i == id).expect("witness song");
    serde_json::from_value(serde_json::json!({
        "id": id,
        "title": format!("Tone {id}"),
        "album": alb,
        "album_id": alb,
        "artist": "Witness",
        "track": track,
        "duration_secs": secs,
    }))
    .expect("witness song json")
}

/// A loopback Subsonic `stream` endpoint: `GET /rest/stream?id=X` answers X's tone WAV
/// (honouring a `Range: bytes=N-` so mpv may seek); everything else is a 404, which
/// the scrobbler and the autofill simply log.
fn serve_tones() -> u16 {
    let lib: Arc<Map<String, Vec<u8>>> = Arc::new(
        library()
            .into_iter()
            .enumerate()
            .map(|(i, (id, _, _, secs))| (id.to_string(), tone_wav(220.0 + 110.0 * i as f64, secs as f64)))
            .collect(),
    );
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind tone server");
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for conn in listener.incoming().flatten() {
            let lib = lib.clone();
            std::thread::spawn(move || {
                let mut rd = BufReader::new(conn.try_clone().expect("clone"));
                let mut line = String::new();
                if rd.read_line(&mut line).is_err() {
                    return;
                }
                let mut range_from = 0usize;
                loop {
                    let mut h = String::new();
                    if rd.read_line(&mut h).unwrap_or(0) == 0 || h == "\r\n" {
                        break;
                    }
                    let lower = h.to_ascii_lowercase();
                    if let Some(r) = lower.strip_prefix("range: bytes=") {
                        range_from = r.split('-').next().and_then(|s| s.trim().parse().ok()).unwrap_or(0);
                    }
                }
                let path = line.split_whitespace().nth(1).unwrap_or("");
                let id = path
                    .split(['?', '&'])
                    .find_map(|kv| kv.strip_prefix("id="))
                    .unwrap_or("");
                let mut c = conn;
                match lib.get(id).filter(|_| path.contains("/rest/stream")) {
                    Some(body) => {
                        let from = range_from.min(body.len());
                        let status = if range_from > 0 { "206 Partial Content" } else { "200 OK" };
                        let _ = write!(
                            c,
                            "HTTP/1.1 {status}\r\nContent-Type: audio/wav\r\nContent-Length: {}\r\n\
                             Accept-Ranges: bytes\r\nContent-Range: bytes {from}-{}/{}\r\nConnection: close\r\n\r\n",
                            body.len() - from,
                            body.len().saturating_sub(1),
                            body.len()
                        );
                        let _ = c.write_all(&body[from..]);
                    }
                    None => {
                        let _ = write!(c, "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
                    }
                }
            });
        }
    });
    port
}

// ── sampling + classification ───────────────────────────────────────────────

/// One deck at one instant: the source it is playing (song id) if AUDIBLE, and its volume.
#[derive(Debug, Clone)]
pub(crate) struct DeckSample {
    pub src: Option<String>,
    pub vol: f64,
}

#[derive(Debug, Clone)]
pub(crate) struct Sample {
    /// Milliseconds relative to the trigger (negative = before).
    pub t: i64,
    pub decks: Vec<DeckSample>,
    pub head: Option<String>,
}

fn src_of(p: &DeckProbe) -> Option<String> {
    if p.core_idle || p.vol_silent() {
        return None;
    }
    let path = p.path.as_deref()?;
    path.split(['?', '&']).find_map(|kv| kv.strip_prefix("id=")).map(str::to_string)
}

impl DeckProbe {
    fn vol_silent(&self) -> bool {
        self.volume <= AUDIBLE_VOL
    }
}

/// The classified shape of one transition, one TSV row.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Row {
    pub cause: String,
    pub shape: String,
    pub overlap_ms: i64,
    pub gap_ms: i64,
    /// Lowest volume the OLD source reached while still audible after the trigger.
    pub old_min_vol: f64,
    /// The NEW source's volume the first time it was audible.
    pub new_first_vol: f64,
    /// Trigger to the new source first audible.
    pub switch_ms: i64,
    /// Max deviation of summed POWER from the baseline during the overlap, percent.
    pub power_dev_pct: f64,
    pub src_before: String,
    pub src_after: String,
    pub head_before: String,
    pub head_after: String,
}

pub(crate) const HEADER: &str = "cause\tshape\toverlap_ms\tgap_ms\told_min_vol\tnew_first_vol\tswitch_ms\tpower_dev_pct\tsrc_before\tsrc_after\thead_before\thead_after";

impl Row {
    pub(crate) fn tsv(&self) -> String {
        format!(
            "{}\t{}\t{}\t{}\t{:.1}\t{:.1}\t{}\t{:.0}\t{}\t{}\t{}\t{}",
            self.cause,
            self.shape,
            self.overlap_ms,
            self.gap_ms,
            self.old_min_vol,
            self.new_first_vol,
            self.switch_ms,
            self.power_dev_pct,
            self.src_before,
            self.src_after,
            self.head_before,
            self.head_after
        )
    }
}

fn audible(s: &Sample, src: &str) -> Option<f64> {
    s.decks
        .iter()
        .filter(|d| d.src.as_deref() == Some(src))
        .map(|d| d.vol)
        .fold(None, |a: Option<f64>, v| Some(a.map_or(v, |a| a.max(v))))
}

/// The loudest audible source in a sample.
fn dominant(s: &Sample) -> Option<String> {
    s.decks
        .iter()
        .filter(|d| d.src.is_some())
        .max_by(|a, b| a.vol.total_cmp(&b.vol))
        .and_then(|d| d.src.clone())
}

/// `HYPODJ_WITNESS_DUMP=1` prints every sample (t, per-deck src@vol, head), for reading
/// a classification back against the raw timeline.
fn dump(cause: &str, samples: &[Sample]) {
    if !std::env::var("HYPODJ_WITNESS_DUMP").is_ok_and(|v| v == "1") {
        return;
    }
    for s in samples {
        let decks: Vec<String> = s
            .decks
            .iter()
            .map(|d| format!("{}@{:.1}", d.src.as_deref().unwrap_or("."), d.vol))
            .collect();
        eprintln!("dump {cause}\t{}\t{}\t{}", s.t, decks.join(" "), s.head.as_deref().unwrap_or("-"));
    }
}

pub(crate) fn classify(cause: &str, samples: &[Sample]) -> Row {
    let dash = || "-".to_string();
    let before: Vec<&Sample> = samples.iter().filter(|s| s.t < 0).collect();
    let after: Vec<&Sample> = samples.iter().filter(|s| s.t >= 0).collect();
    let old = before.iter().rev().find_map(|s| dominant(s));
    let base_vol = before
        .iter()
        .rev()
        .find_map(|s| old.as_deref().and_then(|o| audible(s, o)))
        .unwrap_or(100.0);
    // The new source is what is dominant at the END of the window.
    let new = after.iter().rev().take(20).find_map(|s| dominant(s));
    let head_before = before.last().and_then(|s| s.head.clone()).unwrap_or_else(dash);
    let head_after = after.last().and_then(|s| s.head.clone()).unwrap_or_else(dash);
    let mut row = Row {
        cause: cause.to_string(),
        shape: String::new(),
        overlap_ms: 0,
        gap_ms: 0,
        old_min_vol: base_vol,
        new_first_vol: 0.0,
        switch_ms: -1,
        power_dev_pct: 0.0,
        src_before: old.clone().unwrap_or_else(dash),
        src_after: new.clone().unwrap_or_else(dash),
        head_before,
        head_after,
    };
    let Some(n) = new.clone().filter(|n| Some(n) != old.as_ref()) else {
        row.shape = if new.is_none() { "STOP" } else { "NONE" }.to_string();
        if let Some(o) = &old {
            row.old_min_vol =
                after.iter().filter_map(|s| audible(s, o)).fold(base_vol, f64::min);
        }
        return row;
    };
    // Each sample stands for the time until the NEXT one, measured, never the nominal
    // SAMPLE_MS: a probe is a round trip to every deck actor, so the real cadence is
    // slower and varies. Counting samples times SAMPLE_MS under-read a 1.5 s overlap
    // as ~300 ms.
    let span = |i: usize| -> i64 {
        after.get(i + 1).map(|n| n.t - after[i].t).unwrap_or(SAMPLE_MS as i64).max(0)
    };
    let g = |v: f64| (v / 100.0).powi(3);
    let first_new = after.iter().position(|s| audible(s, &n).is_some());
    if let Some(i) = first_new {
        row.switch_ms = after[i].t;
        row.new_first_vol = audible(after[i], &n).unwrap_or(0.0);
    }
    let last_old = old
        .as_deref()
        .and_then(|o| after.iter().rposition(|s| audible(s, o).is_some()));
    if let Some(o) = old.as_deref() {
        let upto = last_old.map(|i| i + 1).unwrap_or(0);
        row.old_min_vol =
            after[..upto].iter().filter_map(|s| audible(s, o)).fold(base_vol, f64::min);
        let mut dev: f64 = 0.0;
        for (i, s) in after.iter().enumerate() {
            if let (Some(vo), Some(vn)) = (audible(s, o), audible(s, &n)) {
                row.overlap_ms += span(i);
                let p = g(vo).powi(2) + g(vn).powi(2);
                let pb = g(base_vol).powi(2).max(1e-9);
                dev = dev.max(((p - pb) / pb).abs() * 100.0);
            }
        }
        row.power_dev_pct = dev;
    }
    // Gap: samples between the old source's last audible one and the new one's first
    // with nothing audible at all.
    let gap_from = last_old.map(|i| i + 1).unwrap_or(0);
    let gap_to = first_new.unwrap_or(after.len());
    if gap_to > gap_from {
        row.gap_ms = (gap_from..gap_to)
            .filter(|&i| after[i].decks.iter().all(|d| d.src.is_none()))
            .map(span)
            .sum();
    }
    let ducked = row.old_min_vol < 0.8 * base_vol || row.new_first_vol < 0.8 * base_vol;
    row.shape = if row.overlap_ms > 0 {
        "CROSSFADE"
    } else if ducked {
        "DIP"
    } else if row.gap_ms >= GAP_MS as i64 {
        "SILENCE-START"
    } else {
        "CUT"
    }
    .to_string();
    row
}

// ── the rig ─────────────────────────────────────────────────────────────────

struct Rig {
    h: Arc<HypodjHandler>,
    _runtime: crate::director::DjRuntime,
}

fn head_of(h: &HypodjHandler) -> Option<String> {
    let st = h.state.lock().unwrap();
    st.current().and_then(|i| st.queue.get(i)).map(|it| match &it.entry {
        QueueEntry::Song(s) => s.id.0.clone(),
        QueueEntry::Stream { url, .. } => url.clone(),
    })
}

fn qid_at(h: &HypodjHandler, pos: usize) -> u64 {
    h.state.lock().unwrap().queue[pos].id
}

impl Rig {
    async fn new(port: u16, songs: &[&str]) -> Rig {
        let cfg = crate::config::ServerConfig {
            url: format!("http://127.0.0.1:{port}"),
            username: "witness".to_string(),
            password: "witness".to_string(),
            client_name: "hypodj-witness".to_string(),
        };
        let client = Arc::new(SubsonicClient::connect(&cfg).expect("subsonic client"));
        let (player, events) = MpvPlayer::spawn_pair(crate::player::AudioOut::Null);
        assert!(player.can_crossfade(), "the witness needs the real two-deck pair");
        let h = Arc::new(HypodjHandler::with_fade_config(
            client.clone(),
            player,
            FadeConfig::default(),
        ));
        h.set_self_ref(Arc::downgrade(&h));
        let scrobbler = Arc::new(crate::scrobble::Scrobbler::new(client.clone()));
        let runtime =
            crate::director::run(TokioClock, h.clone(), scrobbler, client, events);
        for s in songs {
            h.enqueue_song_for_test(song(s)).await;
        }
        Rig { h, _runtime: runtime }
    }

    async fn sample(&self, t0: tokio::time::Instant) -> Sample {
        let (probes, _active) = self.h.player.witness_probe().await;
        let t = t0.elapsed().as_millis() as i64 - PRE_MS as i64;
        Sample {
            t,
            decks: probes.iter().map(|p| DeckSample { src: src_of(p), vol: p.volume }).collect(),
            head: head_of(&self.h),
        }
    }

    /// Sample PRE_MS, run `trigger`, sample POST_MS after it returns-or-not (the trigger
    /// is raced against the sampler so a long-running gesture is observed DURING itself).
    async fn record<F>(&self, trigger: F) -> Vec<Sample>
    where
        F: std::future::Future<Output = ()>,
    {
        let t0 = tokio::time::Instant::now();
        let mut out = Vec::new();
        while t0.elapsed() < Duration::from_millis(PRE_MS) {
            out.push(self.sample(t0).await);
            tokio::time::sleep(Duration::from_millis(SAMPLE_MS)).await;
        }
        let end = t0 + Duration::from_millis(PRE_MS + POST_MS);
        tokio::pin!(trigger);
        let mut done = false;
        while tokio::time::Instant::now() < end {
            if !done {
                tokio::select! {
                    _ = &mut trigger => { done = true; }
                    _ = tokio::time::sleep(Duration::from_millis(SAMPLE_MS)) => {}
                }
            } else {
                tokio::time::sleep(Duration::from_millis(SAMPLE_MS)).await;
            }
            out.push(self.sample(t0).await);
        }
        out
    }

    /// For a cause with no gesture (natural EOF): sample until the HEAD moves, then
    /// re-base time so the head change is t = 0.
    async fn record_until_head_moves(&self, limit: Duration) -> Vec<Sample> {
        let t0 = tokio::time::Instant::now();
        let start_head = head_of(&self.h);
        let mut out = Vec::new();
        let mut moved_at: Option<i64> = None;
        loop {
            let s = self.sample(t0).await;
            if moved_at.is_none() && s.head != start_head {
                moved_at = Some(s.t);
            }
            let stop = match moved_at {
                Some(m) => s.t >= m + POST_MS as i64,
                None => t0.elapsed() >= limit,
            };
            out.push(s);
            if stop {
                break;
            }
            tokio::time::sleep(Duration::from_millis(SAMPLE_MS)).await;
        }
        let m = moved_at.unwrap_or(i64::MAX / 2);
        for s in &mut out {
            s.t -= m;
        }
        out
    }

    async fn play(&self, pos: usize) {
        self.h.handle(MpdCommand::Play(Some(pos))).await;
        tokio::time::sleep(Duration::from_millis(1500)).await;
    }
}

pub(crate) const CAUSES: &[&str] = &[
    "next",
    "previous",
    "eof-other-album",
    "eof-same-album",
    "play-n-while-playing",
    "playid",
    "delete-playing-row",
    "clear-add-play",
    "move-playing-row",
    "plan-play",
    "radio-start-while-playing",
    // Also emits the `undo-head-change` row: the undo of this same edit.
    "plan-remove-current",
];

async fn run_cause(port: u16, cause: &str) -> Vec<Row> {
    let row = |s: &[Sample]| {
        dump(cause, s);
        classify(cause, s)
    };
    match cause {
        "next" => {
            let r = Rig::new(port, &["A", "B", "C"]).await;
            r.play(0).await;
            vec![row(&r.record(async { r.h.handle(MpdCommand::Next).await; }).await)]
        }
        "previous" => {
            let r = Rig::new(port, &["A", "B", "C"]).await;
            r.play(1).await;
            vec![row(&r.record(async { r.h.handle(MpdCommand::Previous).await; }).await)]
        }
        "eof-other-album" => {
            let r = Rig::new(port, &["X", "Y"]).await;
            r.h.handle(MpdCommand::Play(Some(0))).await;
            vec![row(&r.record_until_head_moves(Duration::from_secs(8)).await)]
        }
        "eof-same-album" => {
            let r = Rig::new(port, &["P", "Q"]).await;
            r.h.handle(MpdCommand::Play(Some(0))).await;
            vec![row(&r.record_until_head_moves(Duration::from_secs(8)).await)]
        }
        "play-n-while-playing" => {
            let r = Rig::new(port, &["A", "B", "C"]).await;
            r.play(0).await;
            vec![row(&r.record(async { r.h.handle(MpdCommand::Play(Some(2))).await; }).await)]
        }
        "playid" => {
            let r = Rig::new(port, &["A", "B", "C"]).await;
            r.play(0).await;
            let id = qid_at(&r.h, 2);
            vec![row(&r.record(async { r.h.handle(MpdCommand::PlayId(Some(id))).await; }).await)]
        }
        "delete-playing-row" => {
            let r = Rig::new(port, &["A", "B", "C"]).await;
            r.play(0).await;
            vec![row(
                &r.record(async { r.h.handle(MpdCommand::Delete(Some("0".into()))).await; })
                    .await,
            )]
        }
        "clear-add-play" => {
            let r = Rig::new(port, &["A", "B"]).await;
            r.play(0).await;
            vec![row(
                &r.record(async {
                    r.h.handle(MpdCommand::Clear).await;
                    r.h.enqueue_song_for_test(song("D")).await;
                    r.h.enqueue_song_for_test(song("E")).await;
                    r.h.handle(MpdCommand::Play(Some(0))).await;
                })
                .await,
            )]
        }
        "move-playing-row" => {
            let r = Rig::new(port, &["A", "B", "C"]).await;
            r.play(0).await;
            vec![row(
                &r.record(async {
                    r.h.handle(MpdCommand::Move { start: 0, end: Some(1), to: 2 }).await;
                })
                .await,
            )]
        }
        "plan-play" => {
            let r = Rig::new(port, &["A", "B", "C"]).await;
            r.play(0).await;
            let act = Action::Play { sel: QueueSelector::Position(3) };
            vec![row(&r.record(async { r.h.run_action_outcome(&act, "mcp:witness").await; }).await)]
        }
        "radio-start-while-playing" => {
            let r = Rig::new(port, &["A", "B"]).await;
            r.play(0).await;
            r.h.push_radio_songs(Ok(vec![song("R")]));
            r.h.push_autofill_batch(Ok(vec![]));
            vec![row(
                &r.record(async {
                    r.h.handle(MpdCommand::Radio(crate::mpd::RadioCmd::Start(
                        crate::mpd::RadioSeed::Uri("song/R".into()),
                    )))
                    .await;
                })
                .await,
            )]
        }
        // One rig, two transitions: the plan edit that moves the head, then its undo.
        "plan-remove-current" => {
            let r = Rig::new(port, &["A", "B", "C"]).await;
            r.play(0).await;
            let act = Action::Remove { sel: QueueSelector::Current };
            let first = row(
                &r.record(async { r.run_plan(&act).await; }).await,
            );
            let undo = classify(
                "undo-head-change",
                &r.record(async { r.h.handle(MpdCommand::Undo(None)).await; }).await,
            );
            vec![first, undo]
        }
        other => panic!("unknown witness cause {other}"),
    }
}

impl Rig {
    async fn run_plan(&self, act: &Action) {
        self.h.run_action_outcome(act, "mcp:witness").await;
    }
}

pub(crate) fn parse_tsv(text: &str) -> Vec<Vec<String>> {
    text.lines()
        .filter(|l| !l.is_empty() && !l.starts_with("cause\t") && !l.starts_with('#'))
        .map(|l| l.split('\t').map(str::to_string).collect())
        .collect()
}

fn baseline_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/transition-baseline.tsv")
}

fn latest_path() -> PathBuf {
    std::env::var_os("HYPODJ_WITNESS_OUT")
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::temp_dir().join("hypodj-transition-witness.tsv"))
}

// Run: nix develop --command cargo test -j1 -p hypodj-core -- --ignored transition_witness_record --nocapture
// Optional: HYPODJ_WITNESS_BLESS=1 (rewrite the committed baseline), HYPODJ_WITNESS_ONLY=next,eof-same-album
#[test]
#[ignore = "needs a real libmpv runtime; records the audible shape of every head change"]
fn transition_witness_record() {
    let port = serve_tones();
    let only: Option<Vec<String>> = std::env::var("HYPODJ_WITNESS_ONLY")
        .ok()
        .map(|v| v.split(',').map(str::to_string).collect());
    let mut rows = Vec::new();
    for cause in CAUSES {
        if only.as_ref().is_some_and(|o| !o.iter().any(|c| c == cause)) {
            continue;
        }
        // A fresh runtime per cause: dropping it tears down the director, the fades and
        // (through the last handle) both mpv decks, so no cause inherits another's deck.
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("tokio rt");
        let got = rt.block_on(run_cause(port, cause));
        rt.shutdown_timeout(Duration::from_secs(2));
        for r in got {
            eprintln!("witness {}", r.tsv());
            rows.push(r);
        }
    }
    let mut text = String::new();
    text.push_str(HEADER);
    text.push('\n');
    for r in &rows {
        text.push_str(&r.tsv());
        text.push('\n');
    }
    println!("\n{text}");
    let out = if std::env::var("HYPODJ_WITNESS_BLESS").is_ok_and(|v| v == "1") {
        baseline_path()
    } else {
        latest_path()
    };
    if let Some(dir) = out.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    std::fs::write(&out, &text).expect("write witness table");
    println!("witness table written to {}", out.display());
}

/// Old vs new, cause by cause: the baseline's shape and overlap against a recorded run.
/// Prints; never fails (the redesign decides what it WANTS to change).
// Run: nix develop --command cargo test -j1 -p hypodj-core -- --ignored transition_witness_diff --nocapture
#[test]
#[ignore = "diffs a recorded witness run against the committed baseline"]
fn transition_witness_diff() {
    let base = std::fs::read_to_string(baseline_path()).expect("baseline tsv");
    let latest = std::fs::read_to_string(latest_path())
        .unwrap_or_else(|_| panic!("no recorded run at {}", latest_path().display()));
    print!("{}", diff_tables(&base, &latest));
}

pub(crate) fn diff_tables(base: &str, latest: &str) -> String {
    let b = parse_tsv(base);
    let l = parse_tsv(latest);
    let find = |t: &[Vec<String>], c: &str| t.iter().find(|r| r[0] == c).cloned();
    let mut causes: Vec<String> = b.iter().map(|r| r[0].clone()).collect();
    for r in &l {
        if !causes.contains(&r[0]) {
            causes.push(r[0].clone());
        }
    }
    let mut out = String::from("cause\told_shape\tnew_shape\told_overlap_ms\tnew_overlap_ms\tchanged\n");
    for c in causes {
        let o = find(&b, &c);
        let n = find(&l, &c);
        let get = |r: &Option<Vec<String>>, i: usize| {
            r.as_ref().and_then(|r| r.get(i).cloned()).unwrap_or_else(|| "-".into())
        };
        let changed = get(&o, 1) != get(&n, 1);
        out.push_str(&format!(
            "{c}\t{}\t{}\t{}\t{}\t{}\n",
            get(&o, 1),
            get(&n, 1),
            get(&o, 2),
            get(&n, 2),
            if changed { "SHAPE" } else { "" }
        ));
    }
    out
}

// The classifier itself is pure, so it is pinned headlessly on synthetic samples.
#[test]
fn classifier_tells_the_shapes_apart() {
    let s = |t: i64, decks: &[(Option<&str>, f64)]| Sample {
        t,
        decks: decks
            .iter()
            .map(|(s, v)| DeckSample { src: s.map(str::to_string), vol: *v })
            .collect(),
        head: None,
    };
    let xf = vec![
        s(-10, &[(Some("A"), 100.0), (None, 0.0)]),
        s(0, &[(Some("A"), 90.0), (Some("B"), 50.0)]),
        s(10, &[(Some("A"), 50.0), (Some("B"), 90.0)]),
        s(20, &[(None, 0.0), (Some("B"), 100.0)]),
    ];
    assert_eq!(classify("x", &xf).shape, "CROSSFADE");
    assert_eq!(classify("x", &xf).overlap_ms, 20);
    let dip = vec![
        s(-10, &[(Some("A"), 100.0)]),
        s(0, &[(Some("A"), 40.0)]),
        s(10, &[(Some("B"), 40.0)]),
        s(20, &[(Some("B"), 100.0)]),
    ];
    assert_eq!(classify("x", &dip).shape, "DIP");
    let cut = vec![s(-10, &[(Some("A"), 100.0)]), s(0, &[(Some("B"), 100.0)])];
    assert_eq!(classify("x", &cut).shape, "CUT");
    let mut gap = vec![s(-10, &[(Some("A"), 100.0)])];
    for t in 0..10 {
        gap.push(s(t * 10, &[(None, 0.0)]));
    }
    gap.push(s(100, &[(Some("B"), 100.0)]));
    assert_eq!(classify("x", &gap).shape, "SILENCE-START");
    let none = vec![s(-10, &[(Some("A"), 100.0)]), s(0, &[(Some("A"), 100.0)])];
    assert_eq!(classify("x", &none).shape, "NONE");
    let d = diff_tables("cause\tshape\toverlap_ms\nnext\tDIP\t0\n", "next\tCROSSFADE\t900\n");
    assert!(d.contains("next\tDIP\tCROSSFADE\t0\t900\tSHAPE"), "{d}");
}
