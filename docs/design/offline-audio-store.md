# hypodj offline store: final design

## 1. What this is for

hypodj gains an on-disk audio store so that starred songs and the upcoming queue play from local bytes when the Navidrome server is slow, flaky, or gone, with the daemon itself starting, serving MPD, and restoring its saved queue fully offline. One background reconciler task owns every store mutation, diffing a desired set (starred pins plus the live queue window) against sidecar-committed on-disk truth, so crash recovery, external tampering, server drift, and cold start are all the same code path - the next pass. Playback never writes and never waits: `resolve_play` does one sync index probe plus one stat and prefers local always, making the offline path the everyday path so it cannot rot as an untested branch.

## 2. The validity model

Only `/rest/download` originals are ever stored, which deletes the entire transcoding-settings validity class by construction: server-side transcoding changes what `/rest/stream` returns, never what `download` returns, and the sidecar records `endpoint = "download"` so a future format-aware mode cannot silently mix provenances.

A cached entry for song id X is VALID iff:

1. The sidecar `<store>/<X>.toml` exists, parses, and carries the current `STORE_SCHEMA_VERSION` with `endpoint = "download"`.
2. The audio file `<store>/<X>.<suffix>` exists and its byte length equals `sidecar.fingerprint.size` exactly.
3. X is not marked suspect (in-memory flag, set by an errored end-of-play on a locally resolved track).
4. Background only: the sidecar's identity fingerprint `(size, suffix, created)` - captured from the server's `Child` at download time - still matches what the server currently reports.

**Play time** (sync, per the `resolve_play` contract at handler.rs:5866-5869): `AudioStore::lookup(&SongId)` probes the in-memory index under a short std Mutex (never held across an await), rejects suspects, then does ONE `fs::metadata` stat confirming existence and exact recorded length. Any failure falls through to today's stream URL. No hashing, no parsing, no network.

**Commit point**: the sidecar rename. Audio bytes are fully written to a tmp file, length-verified against the server-reported size, fsynced, and renamed into place BEFORE the sidecar appears - so a valid-looking truncation is structurally impossible. An audio file without a sidecar is an orphan the next scan deletes.

**Background revalidation**: each full reconcile pass does one `getStarred2` round trip whose `Child`s carry fresh fingerprints (they map through the shared `map_song`, subsonic.rs:334), revalidating the ENTIRE pinned mirror for free every cycle. Verdicts: equal means confirmed; differ means stale - marked in the sidecar but the entry KEEPS SERVING until a verified replacement is committed over it (keep-until-replaced, see the write protocol); absent from the pin set means demote to evictable, never eager delete. `created` is compared as a parsed instant (small in-repo RFC3339-to-epoch helper, falling back to string equality when unparseable), not as a raw string, so a timezone re-rendering of the offset-bearing string cannot mass-invalidate the mirror. ANY transient server error skips ALL verdicts for the pass - nothing is deleted, demoted, or marked stale because the server flapped; transient-keeps-the-claim IS offline mode.

**The suspect signal** (wrong local bytes): a locally resolved track that ends in an error marks the entry suspect. Attribution is race-free by construction: `ResolvedPlay` gains `local: bool`; `play_url`/`switch_warmed` pass it; the actor latches it beside `current`/`current_qid`; and `PlayerEvent::Eof` gains `errored: bool` and `was_local: bool` alongside the `song`/`queue_id` it already carries (player.rs:83). The suspect mark keys off the EVENT's song id, never off `st.current` re-read at processing time. `errored` is set by parameterizing the single `emit_honest_stop` function and covering ALL THREE of its routes: the EndFile arm at player.rs:1094 (derive from `EndReason::Error`), the plain arm at player.rs:1113, and - critically - the top-level `is_active_load_failure` route at player.rs:1147, which is where a local file replaced with garbage actually surfaces, because mpv's loadfile Ok is premature (player.rs:786-798). The continuation-landed emit at player.rs:1070 is a natural-EOF gapless handoff and never carries an error. A suspect is de-offered by `resolve_play` immediately, but its bytes are DELETED only after a replacement is downloaded, length-verified, and renamed over it - an offline pass can never destroy what it cannot replace, so an ao/pipewire hiccup on suspend costs at worst a stream fallback until the server returns, never the loss of a pinned file.

**The honest gap, stated**: a locally cached file with a valid header and a rotted tail most plausibly ends with reason EOF, not Error, and this design does NOT detect it. The fingerprint still matches, the stat still matches, and the entry re-confirms valid every pass. The repair verb is manual and already falls out of the scan-heal design: delete `<store>/<id>.*` by hand; the next pass re-downloads it if desired. Whether to add an early-EOF heuristic instead is Guilherme's call (section 9).

## 3. The design

### On-disk layout and keys

Root: `<state_dir>/store` (state_dir resolved exactly as today, main.rs:208-223; on this machine `/home/guibaeta/.local/state/hypodj/store`). No state_dir means the store is disabled with a warn - resume's posture. Flat directory, two files per song:

- `<id>.<suffix>` - the audio bytes. Suffix from the server `Child`, sanitized to `[a-z0-9]{1,8}`, else `bin`.
- `<id>.toml` - the sidecar.

Ids not matching `[A-Za-z0-9_-]+` are excluded from the store entirely (resolve falls through to streaming). This plus sanitized suffixes makes the `quote()` literal-double-quote trap (player.rs:1453) structurally impossible. In-flight files: `tmp.<pid>.<seq>` with a process-wide `AtomicU64` seq (the resume.rs:129-131 discipline). The key is the song id - the only stable key; stream URLs re-salt on every call (opensubsonic auth.rs:98-106) and are never keys.

Sidecar contents: `schema_version` (const `STORE_SCHEMA_VERSION = 1`), `fingerprint { size: u64, suffix: String, created: Option<String> }`, `content_type`, `endpoint = "download"`, `pinned: bool`, `stale: bool`, `fetched_at_unix`, `last_played_unix`, and an embedded full `Song` table (requires adding serde derives to `Song` at model.rs:96 - today it has only Debug, Clone). Additive fields use `#[serde(default)]` (the resume.rs:54-55 pattern). A sidecar that fails to parse or version-mismatches invalidates the ENTRY (from_toml-returns-None bar, resume.rs:91-97); the pass deletes both files and re-downloads if desired - blast radius is one song, which is why per-song sidecars beat a single manifest.

### The write protocol (reconciler only)

1. The fingerprint for the bytes comes from the same `getStarred2`/`getSong` response that scheduled the download - never a stale cache.
2. Stream the download URL to `tmp.<pid>.<seq>` with the bounded reqwest chunk loop (the `fetch_remote_cover` shape, handler.rs:8088-8104; client built like handler.rs:1325-1332 with a connect timeout, a 30s per-chunk inactivity timeout instead of a total timeout, and a running cap of `size + slack`). Sync `std::fs` writes per chunk; the download URL comes from a new `SubsonicClient::download_url(&id)` in subsonic.rs, derived from the public sync `stream_url` (subsonic.rs:251) by swapping the last path segment `stream` to `download` (the auth query carries over; the crate's own `download()` buffers whole files in RAM at client.rs:237 and is rejected).
3. On EOF, the length must equal `fingerprint.size` exactly, else delete the tmp, warn, and apply an in-memory per-id backoff (not persisted; the pass cadence is the retry schedule).
4. fsync the file and the directory in `spawn_blocking`, then rename. For a FRESH entry, rename to `<id>.<suffix>` then write the sidecar (tmp+fsync+rename) - the sidecar is the commit. For a STALE or SUSPECT replacement with the same suffix, rename OVER the existing audio file (atomic swap; mpv's open fd survives via unlink semantics) then rewrite the sidecar; on a suffix change, write the new pair first, then delete the old pair. **Old bytes are never unlinked before their verified replacement exists.**
5. Insert/update the in-memory index under a short lock.

Sidecar writes go through a shared `pub fn atomic_write_bytes` extracted from resume.rs:123-139 (same-dir unique tmp, write_all, sync_all, rename), used by both resume and the store. A crash at any point leaves a swept tmp, an orphan, or the previous valid state - never a valid-looking partial.

### Startup: the daemon must exist offline

This is the amendment without which everything else is decoration. Today `client.ping().await?` at main.rs:76 exits the process when the server is down, and the unit crash-loops every 5s (nix/hypodj-module.nix Restart=on-failure, RestartSec=5) - restore, the store, and even the MPD bind are never reached.

Changes:

- `SubsonicClient::connect` stays as-is (it is sync, pure construction, never touches the network). `ping()` failure becomes warn-and-continue; `probe_extensions()` already swallows errors and its results default conservative. The MPD socket binds regardless of server reachability. The reconciler's first successful pass is the "server is back" edge (it can re-run the extension probe opportunistically if that ever matters).
- Inject a timeout-bounded reqwest client at `SubsonicClient::connect` via the crate's public `with_http_client` (opensubsonic client.rs:74) - connect timeout 5s, total per-request timeout 15s. Verified: hypodj never calls `with_http_client` today, so the metadata client is `reqwest::Client::new()` with NO timeout, and a blackholed host (suspended box, captive portal, dead VPN) means ~127s of kernel SYN retries per call. This one line in subsonic.rs (the sanctioned wire file) is what makes "transient failure" mean seconds everywhere: restore, `enqueue_uri`, the reconciler. It is safe for every current use of the crate client (JSON endpoints and small covers); audio never flows through it - playback is mpv's own HTTP, and store downloads use the store's own chunk-loop client.
- nix/hypodj-module.nix: update the comment at the Restart lines ("Restart=on-failure covers a cold network" is no longer the mechanism); Restart=on-failure itself stays, now covering only real crashes.

### The resolution path, per call site

The ONE seam is `resolve_play` (handler.rs:5870), `QueueEntry::Song` arm only. Before minting `stream_url` (handler.rs:5873): if the store is present and `store.lookup(&song.id)` returns a path, return a local `ResolvedPlay { local: true, url: <absolute path> }` and bump that entry's in-memory `last_played` under the same short lock (in-memory only - the reconciler flushes dirty recency to sidecars opportunistically each pass, so the one-disk-writer rule survives; a crash loses at most one interval of recency). `lookup` stays sync: index probe plus one stat, and `resolve_play` is never called under any lock (verified at 5985-5990 and 6144-6188) and never under the fade slot lock.

Every downstream consumer is already safe, per the ground truth:

- Site 1, `play_url` at handler.rs:6190 (`play_index_inner`): mpv `loadfile` takes the path verbatim; scrobbling keys on SongId; ICY extraction no-ops on local tags (player.rs:974-976).
- Site 2, `switch_warmed` at handler.rs:1222 (skip terminal): the exact-string warm identity check (player.rs:1296) holds because ONE `ResolvedPlay` from `skip_with_fade` (handler.rs:5990) is carried into both `prefetch_warm` (6040) and the terminal - resolution cannot flip local/remote within a gesture. EOF advance re-resolves per gesture, so a flip between tracks is invisible.
- Site 3, `prefetch_warm` at handler.rs:6040: same carried string.
- Site 4, `prefetch_continuation` (handler.rs:5836): streams only, untouched.
- MPD clients still see `file: song/<id>` (handler.rs:6346, 8596) - resolution is unobservable.
- `play_url` and `switch_warmed` gain the `local: bool` parameter the actor latches for the Eof event (section 2). This is a shared-crate signature change: whole `--workspace` build and test, per CLAUDE.md.

**Suspect hook**: the director already destructures the Eof payload (director.rs:467); `advance_on_eof` (handler.rs:5275) gains the event's `song`, `errored`, and `was_local` and calls `store.mark_suspect(id)` plus `store.kick()` when `errored && was_local` - keyed on the event's id, immune to the interleaved-play race, and the racy `last_play_local` State bool from the earlier draft is deleted entirely.

**Offline seams**:

- `restore` (handler.rs:4883): on a transient `client.song` failure (now bounded in seconds), flip an offline flag for the remainder of the restore. Resolve each remaining Song id store-first from the sidecar-embedded `Song`. Ids with NO cached entry are NOT dropped - they become minimal id-only placeholder entries (`stream_url` needs only the id, so they remain attempt-playable; title falls back to the id until refreshed). The hard-won never-shrink-the-saved-queue guarantee (handler.rs:4916-4927) is thereby PRESERVED, not traded away: the installed queue has the same length as the saved one, and the checkpoint loop cannot persist a shrunken session. `NotFound` (API 70) keeps its authoritative drop-just-this-entry arm. When restoring offline with a saved `Playing` state and an uncached current entry, restore `Paused` instead, so the daemon does not cascade failed loads through the queue and burn the saved position. The reconciler refreshes placeholder metadata on its first successful pass after an offline restore (cosmetic only - persistence stores ids).
- `enqueue_uri` `song/<id>` (handler.rs:6217 region): on transient `client.song` failure, consult the store's sidecar-embedded Song before ACKing, so hearted songs are addable offline within seconds, not minutes.

### Eviction and pinning

Pinned set: the `getStarred2` song set when `pin_starred` is true. Protected from eviction: pins, the current song, the queue window (current plus `queue_ahead` upcoming Songs), and the pending-skip target - wired concretely: `skip_with_fade` adds its pre-resolved target id to the store's protected set when it sets `pending_skip`, and the skip terminal clears it (this closes the previously claimed-but-unwired promise).

Eviction: sum bytes; while over `max_bytes`, evict unpinned entries by oldest `last_played` (real LRU now, thanks to the resolve-time recency bump). If pinned bytes alone exceed `max_bytes`: warn once per pass naming the shortfall and stop pin downloads at the cap - never silently evict a pin, never exceed the budget, no download-evict thrash.

Unstar: `bust_star_caches` (handler.rs:8381, sync) additionally calls `store.kick()`; the entry demotes to unpinned evictable - bytes kept, reclaimed only under budget pressure. An accidental unstar/re-star round trip costs zero bytes.

### The sync loop

In `store.rs`: `pub async fn run<C: Clock>(store: Arc<AudioStore>, client: Arc<SubsonicClient>, clock: C)` - generic over `Clock` with absolute deadlines (`t0 + k*interval`, the clock.rs:10-17 convention) so the whole loop is fake-clockable. `loop { select! { clock.sleep_until(next), store.kick.notified() } }`.

**Kicks are scoped by reason** (this answers the disproportion attack):

- LIGHT kick (queue-window change from `update_store_window`, suspect mark, skip-target pin): re-run the pure `plan_pass` against the CACHED pin set and the in-memory index only - no directory scan, no `getStarred2`. Executes only window downloads and suspect replacements. A track boundary costs zero network beyond the downloads it actually needs.
- FULL pass (interval tick, star-flip kick, startup): sweep tmps; directory scan (orphan audio, orphan sidecars, unparseable sidecars deleted; index rebuilt from disk truth, lengths stat-checked); one `getStarred2` (transient failure logs info and skips ALL verdicts); fingerprint verdicts (keep-until-replaced, section 2); then execute.

Execution order, all SEQUENTIAL and bounded to at most one batch of downloads per pass (the loop re-enters immediately while work remains, so a huge backlog cannot pin the task): suspect replacements first (they gate a de-offered song's return), then window downloads (they gate the next audible advance), then stale replacements and starred backfill newest-starred-first - stale processing shares the same per-pass bound as downloads, so a mass `created` bump (full re-import) refills incrementally while every old entry KEEPS PLAYING; then eviction. Bulk backfill (not window/suspect work) is deferred while the current track is a remote stream or a remotely resolved song, so initial sync cannot stall live playback on a thin link.

**Wiring for the window**: a small `update_store_window` helper on the handler (snapshot current plus next `queue_ahead` Song ids under one short lock, `store.set_window` plus light kick on change), called at the end of `play_index_inner`, from `advance_on_eof`, and from queue-mutating commands' notify path.

### Concurrency model

Exactly one disk writer (the reconciler task); playback and restore only read (index probe under a short std Mutex plus a stat), with the sole in-memory exception of the recency bump and suspect flag - both index-map mutations under the same short lock, never disk. No new cross-await state: the handler's std Mutex discipline, the fade slot, and the lossless Eof channel are untouched (`errored`/`was_local` ride the existing lossless mpsc(64); the kick is a `Notify` and correctness is level-triggered, so a missed kick costs latency, never correctness). Eviction and replacement mid-play are inaudible (Linux unlink/rename-over keep mpv's open inode alive). Two daemons on one state dir are out of scope by the resume.toml precedent; pid+seq tmps and atomic renames bound the damage to duplicate work, and the scan converges.

### Wiring summary, by file

- `model.rs:96` - `Song` += `size: Option<u64>`, `suffix`, `content_type`, `created` (all Option) plus Serialize/Deserialize derives.
- `subsonic.rs` - `map_song` (774-805) maps the four new fields; new `download_url(&self, id)`; the bounded metadata client injected in `connect` via `with_http_client`. Stays the only wire-type file; the GET itself lives in store.rs like the cover fetcher.
- `player.rs` - `ResolvedPlay`-fed `local` flag latched by the actor; `Eof { errored, was_local, .. }`; `emit_honest_stop` parameterized with `errored` across all three routes (1094 from `EndReason`, 1113, 1147 from `is_active_load_failure`).
- `director.rs` - thread the Eof payload (`song`, `errored`, `was_local`) into `advance_on_eof`.
- `handler.rs` - `Option<Arc<AudioStore>>` field plus `set_audio_store` setter (the `set_recognize_config` convention); `resolve_play` local branch plus recency bump; `advance_on_eof` suspect hook; `skip_with_fade` skip-target pin; `bust_star_caches` kick; `update_store_window`; restore and `enqueue_uri` offline arms.
- `resume.rs` - extract `atomic_write_bytes`.
- `config.rs` - `[store]` section (below).
- `crates/hypodj-daemon/src/main.rs` - ping/probe warn-and-continue; inside the state_dir block: `AudioStore::open(dir.join("store"), cfg.store)` (sync startup scan; failure = warn + store disabled, never fatal), `set_audio_store` BEFORE `resume::load`/`restore`, `tokio::spawn(store::run(..., TokioClock))` next to `checkpoint_loop`.
- `lib.rs` - `pub mod store`.
- `nix/hypodj-module.nix` - comment update at the Restart lines.
- New file: `crates/hypodj-core/src/store.rs`.

Zero new dependencies. Sync `std::fs` in short sections plus `spawn_blocking` for fsyncs (workspace tokio has no `fs` feature and resume.rs sets the precedent); `tokio::sync::Notify` is already available.

## 4. Config surface

`[store]`, `#[serde(default)]` on the `Config` field, `d_*` default fns pointing at `pub const`s, MANUAL `impl Default` matching the serde defaults (the ContinuationConfig rule, config.rs:146-158), `normalize()` called in BOTH `Config::load` and `Config::from_str`.

| key | type | default | purpose |
|---|---|---|---|
| `store.enable` | bool | `true` | Master switch; additionally disabled when no state_dir resolves. On by default so the offline path is the everyday path; the safety bound is `max_bytes`, not the flag. |
| `store.dir` | Option\<PathBuf\> | `None` (resolves to `<state_dir>/store`) | Override for non-systemd runs, mirroring `restart.state_dir`; must share a filesystem with its tmp files for atomic renames. |
| `store.max_bytes` | u64 | `8589934592` (8 GiB; normalize floors at 64 MiB) | Hard byte budget for audio (originals, so FLACs count full). Unpinned evict by oldest last_played; pins are never silently evicted - overflow warns per pass and halts pin downloads at the cap. |
| `store.queue_ahead` | u32 | `3` | Upcoming queue Song entries beyond current in the desired set, so ordinary EOF advance is a disk open. 0 disables queue-ahead downloads. |
| `store.sync_interval_secs` | u64 | `900` (floor 60) | FULL-pass cadence. Star flips kick a full pass; window changes, suspects, and skip pins kick a light pass immediately. |
| `store.pin_starred` | bool | `true` | Whether the `getStarred2` set is the authoritative pin set - the hearted-songs-work-offline promise. |

`verify_interval_secs` and the unpinned `getSong` verify batch are DROPPED from v1 (see section 7, attack 13): unpinned validity is existence plus exact length at resolve time plus the suspect path, and `NotFound` deletion happens where it already falls out for free. The metadata-client timeouts are hardcoded consts in subsonic.rs, not config.

## 5. Implementation order

Each step ends whole-workspace green (`nix develop --command cargo build -j4 --workspace && ... test`), and steps touching packaging end with `nix build .#hypodj` green.

1. **Offline-tolerant startup + bounded metadata client.** Ping warn-and-continue in main.rs; `with_http_client` injection in subsonic.rs; module comment. Standalone value: kills the crash-loop and the two-minute blackhole hangs today, before any store exists. Live check: start the daemon with an unreachable `server.url`, confirm the MPD socket serves.
2. **Model and mapper.** `Song` serde derives plus four fields; `map_song`; fixture test in subsonic.rs's existing style. Shared-crate change: whole workspace.
3. **`atomic_write_bytes` extraction** from resume.rs, both callers-to-be tested.
4. **`store.rs` core, unwired.** `AudioStore::open` (scan), `lookup`, sidecar round-trip, `plan_pass` as a pure function, eviction ordering - all unit-tested against tempdirs.
5. **`[store]` config section** plus normalize plus the manual-Default-parity test.
6. **Wire the read path.** `set_audio_store` in main.rs, `resolve_play` local branch plus recency bump, `ResolvedPlay.local`. Store is empty so behavior is unchanged; green and shippable.
7. **The reconciler.** `store::run`, downloads, keep-until-replaced, eviction, kicks (`bust_star_caches`, `update_store_window`, skip-target pin). Live proof phase 1 (below).
8. **The suspect path.** `Eof { errored, was_local }`, `emit_honest_stop` parameterization across all three routes, actor latch, director threading, `advance_on_eof` hook. Cross-crate enum change: whole workspace, and the live-libmpv `#[ignore]` tests.
9. **Offline seams.** Restore offline arm with placeholders and paused-if-uncached, `enqueue_uri` fallback, placeholder refresh. Live proof phases 2-4.
10. **Gates before merge**: whole-workspace build+test, `nix build .#hypodj` and `.#hypodj-clients`, full live proof, every confirmed critical/high review finding resolved.

## 6. Test strategy

All non-ignored tests are filesystem-and-fake-clock only, safe in the certless, network-less Nix sandbox.

1. **Pure logic**: `plan_pass(pins, window, scan) -> Vec<Action>` table-tested over the full divergence matrix (missing, fingerprint drift, demote, orphan audio, orphan sidecar, stale tmp, suspect, over-cap, pins-exceed-budget, keep-until-replaced ordering, light-vs-full kick scoping); eviction ordering and protected-set coverage (including the skip-target pin); the created-parse helper.
2. **Store I/O against tempdirs**: sidecar round-trip; corruption-returns-None (resume.rs bar); atomic visibility (a tmp or sidecar-less audio file is never returned by `lookup`); lookup verdict table (missing, wrong length, suspect, happy); startup scan heal; rename-over replacement preserving continuous validity.
3. **The loop**: generic over `Clock` plus a small `PinSource` trait (`SubsonicClient` implements it), under `#[tokio::test(start_paused = true)]` plus `tokio::time::advance` - cadence, Notify kick, light-vs-full behavior, skip-all-verdicts-on-transient, backoff. Never wall-clock.
4. **Handler seams** (`resolve_play` local preference, recency bump, restore offline degrade with placeholders, suspect attribution from the event payload): `handler_with_null_player` with the mandatory `let Some((h, _)) = ... else { return };` skip - never unwrap.
5. **Config**: manual-Default-matches-serde-defaults parity; `map_song` fixture for the four new fields.

**What unit tests CANNOT prove**, and therefore need live proof:

- That mpv actually plays a stored local path to natural EOF, that a whole-file-garbage local file really surfaces on the top-level `Raw` route (not `EndFile(Error)`), and that the errored flag rides all three `emit_honest_stop` routes: one `#[ignore]` live-libmpv test that downloads a real short song, asserts length equals `getSong` size, plays the local path to EOF, then corrupts it and asserts the suspect mark.
- That `getStarred2`'s `Child`s actually populate `created` (live-verified for `getSong` only): assert on first contact; if absent, pins fall back to a bounded `getSong` batch - slower, not wrong.
- The end-to-end offline story: the liveProof script. In a mktemp dir, copy `/run/user/1000/hypodj/config.toml`, set `mpd.bind=127.0.0.1:6699`, `mpris.enable=false`, `restart.state_dir=<tmp>/state`, small `max_bytes`, `sync_interval_secs=60`, and FORCE `HYPODJ_AUDIO=null` - silent throughout, never the real device. Phase 1 (online): star a short song, assert the kicked sync produces the file with byte length equal to `getSong` size and a matching sidecar; play it and assert the trace shows the local path. Phase 2 (offline startup, the headline): SIGTERM; point `server.url` at an unreachable loopback port; restart with the same state dir; assert the daemon SERVES (impossible today), the saved queue restores with cached entries playable and uncached ones as placeholders, elapsed advances off local bytes, and `add song/<cached-id>` ACKs cleanly. Phase 2b (blackhole, not just connection-refused): point `server.url` at an unroutable RFC 5737 address and assert startup and restore complete within seconds, proving the bounded client. Phase 3 (truncation): truncate the cached file by one byte, restore the real url, restart; assert the stat rejects it, playback falls back to streaming, and the next pass repairs by rename-over. Phase 4 (suspect): overwrite with same-length garbage, play, assert the errored Eof marks it suspect and the pass replaces it without a delete-first window. Teardown in the same motion: kill the daemon, confirm nothing listens on 6699, rm -rf the tmp dir - no sound, no leftover process, no stray files.

## 7. What the attacks changed

1. **Fatal (attacks 1 and 9, confirmed): offline startup was unreachable code.** `client.ping().await?` at main.rs:76 exits the process before restore, the store, or the MPD bind, and systemd crash-loops. The design now makes startup offline-tolerant as its FIRST implementation step, and liveProof phase 2 gates it.
2. **Serious (attack 2, confirmed): the metadata client has no timeout.** `reqwest::Client::new()` upstream, `with_http_client` never called. Every "flip offline on first transient failure" was unbounded at TCP-blackhole minutes. Now: a bounded client injected in subsonic.rs, and a blackhole liveProof case.
3. **Serious (attack 5, confirmed; plus the grounding audit's factual correction): the errored-flag spec was wrong about player.rs.** Site 1070 is the continuation-landed natural-EOF emit; there is no separate `EndFile(Error)` emit; and the top-level open-failure route at 1147 - where whole-file garbage actually lands - was missing. Now: `emit_honest_stop` parameterized across all three routes. The tail-rot claim ("same-length corruption healed automatically") is retracted and replaced by the honest gap statement plus the manual repair verb; the heuristic alternative goes to Guilherme (section 9).
4. **Serious (attacks 6 and 11, confirmed): converge-by-delete destroyed offline value.** Delete-then-redownload with unbounded deletes meant a mass `created` bump could unlink the whole mirror in one pass, and an offline suspect pass could delete a pinned file it could not replace. Now: keep-until-replaced everywhere (stale keeps serving, replacement renames over, suspect deletion gated on a secured replacement), stale processing bounded to the download batch, `created` compared parsed not as a string.
5. **Serious (attack 10, confirmed): offline restore silently destroyed the saved queue.** Dropping uncached ids plus the edge-triggered checkpoint converted one transient blip into permanent queue loss, gutting the hard-won guarantee at handler.rs:4916-4927. Now: unresolved ids become id-only placeholders, the queue never shrinks on transients, and offline restore with an uncached current entry restores Paused.
6. **Serious (attack 12, confirmed): every track boundary ran a full pass.** Full dir scan plus an uncached `getStarred2` per song boundary, downloads competing with live streaming. Now: light kicks replan against cached state only; full passes run on the interval and star flips; bulk backfill defers while playback is remote.
7. **Minor (attacks 3 and 7, confirmed race): suspect attribution via a State bool.** `st.current` is repointed after the `play_url` await, so an interleaved play misattributed the mark. Now: `was_local` rides the load into the actor and the Eof event; the mark keys off the event's own song id; the State bool is gone.
8. **Minor (attack 4, confirmed omission): the pending-skip eviction protection was claimed but unwired.** Now wired: `skip_with_fade` pins its target when setting `pending_skip`; the terminal clears it.
9. **Minor (attack 8, confirmed omission): `last_played_unix` was never updated.** LRU would have degenerated to FIFO-by-download-date. Now: `resolve_play` bumps in-memory recency; the reconciler flushes it to sidecars opportunistically.
10. **Minor (attack 13, accepted on footprint grounds): the unpinned verify subsystem is dropped from v1.** Its entire payoff was bounding version-staleness on opportunistic cache entries to 24h; its cost was a config key, sidecar bookkeeping, and a batch scheduler. Resurrect only if stale replays are ever actually observed.

## 8. Attacks judged false positives, and why

Almost none - the refuters were substantially right, and the design changed accordingly. The residue:

- **Attack 11's proposed fix "keep suspect-but-present entries as an offline last resort" is rejected** (its diagnosis was right and is answered by keep-until-replaced). `resolve_play` is sync and offline-unaware, so it cannot conditionally re-offer suspects "when offline"; and re-offering known-bad bytes loops corrupt playback. De-offering a suspect while offline costs a stream-URL attempt that fails - the same audible outcome - while the bytes stay on disk for the online repair. Nothing is destroyed, which was the attack's real point.
- **Attack 5's tail-rot-ends-as-EOF sub-claim is plausible but not repo-proven** (mpv behavior, corroborated only by the repo's own EndReason doc scoping Error to post-load failures). It is treated as true anyway, because the honest-gap posture is correct under either behavior.
- **Attack 12's "Navidrome rescan surfaces transiently different sizes" sub-claim is speculative**, but keep-until-replaced plus kick scoping defuses it regardless: a transient bad verdict now costs one wasted bounded download, never a deleted file.
- **Attack 13's framing of the verify subsystem as pure waste overstated it** (the steel-man judged inclusion sound too); it was dropped on the project's every-piece-earns-its-keep bar, not because the attack proved it unsound.

## 9. Open questions and decisions for Guilherme

1. **The tail-rot poison pick.** The design ships the honest gap: a valid-header, corrupt-tail local file ends as natural EOF and re-confirms valid forever; the repair is `rm <store>/<id>.*` (the scan heals it). The alternative is an early-EOF heuristic - on natural EOF of a locally resolved track, compare mpv's observed position against the sidecar `duration_secs` and mark suspect when it ends more than N seconds short - which rides data already in hand but produces false suspects on VBR duration disagreement, each costing a full original re-download. Ship the gap, or tune the heuristic?
2. **Range resume for interrupted downloads.** v1 restarts a failed download from byte 0 (delete-tmp-on-failure). Whether Navidrome's `/rest/download` honors Range is unverified. Worth adding for large FLACs on slow links, or is the bounded-batch retry cadence enough?
3. **Filtering environment errors out of the suspect signal.** libmpv's end-file event carries an error code; excluding ao/output-init failures would prevent a suspend-hiccup from de-offering a pinned song until the next pass. Costs libmpv API digging in player.rs. Keep-until-replaced already bounds the damage to a temporary de-offer - is that enough?
4. **The pin set.** `getStarred2` is v1's whole answer to "what should be local". Do explicit playlist pins (for example, a `Pinned` playlist) belong on the roadmap, or is starred-is-offline the product?
5. **`max_bytes` default.** 8 GiB of originals is roughly 350 to 400 FLACs at the live-observed ~22 MB. Right size for bubble-gum's disk and the starred set?
6. **Unit posture after offline-tolerant startup.** With warn-and-continue, `Restart=on-failure` no longer doubles as network-wait. Keep it as-is (crash coverage only), or take the occasion to revisit the HM unit's ordering comments in the same change?
7. **Confirmed by design, restated for the record**: the store is on by default, only `/rest/download` originals are stored (a transcoded-cache mode is explicitly out), and two daemons sharing one state dir remain unsupported per the resume.toml precedent. Say the word if any of those three postures should flip before implementation starts.