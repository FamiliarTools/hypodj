# Beyond: reaching past the shelf

## 1. What this is for

hypodj can already play an infinite mix of music Guilherme owns; it cannot surface anything he does not. This adds one thing: a list of what the world thinks comes next after a track he is playing, marked owned or not, with a real 30-second audition on the ones he does not have and a durable wantlist on the ones he decides to chase. It is a triage tool for acquisition, not a listening mode, and section 8 says plainly where that leaves YouTube.

## 2. The crux, answered once

**Pressing play on an unowned row plays a real 30-second preview from Apple's or Deezer's CDN, through the exact path internet radio already uses. Nothing else happens: no download, no scrobble, no acquisition.**

Mechanically: a Beyond row's `file:` value is `preview/<recording_mbid>`. `enqueue_uri` (`/home/guibaeta/familiartools/hypodj/crates/hypodj-core/src/handler.rs:8016`) gets one new arm sitting exactly where the `station/<name>` arm sits (8059-8072): resolve the MBID to a URL with an await, before the std Mutex is ever taken, then fall through into the existing `is_stream_uri` push at 8073 as `QueueEntry::Stream`. Failure returns `Err`, which ACKs, exactly like `unsupported uri` at 8084. Everything downstream is untouched: `resolve_play`'s Stream leg, the resume mapping, `song_pairs`, mpv's `loadfile`. ncmpcpp needs no changes at all.

**The resolver is a ladder, not one endpoint.** This is the single largest change the attacks forced, and it is not a refinement, it is the difference between the feature working and the feature being a list of things he cannot hear:

1. `POST labs.api.listenbrainz.org/apple-music-id-from-mbid/json` (exact, batched, free) then `GET itunes.apple.com/lookup?id=..`
2. `GET itunes.apple.com/search?term=<artist> <title>&entity=song&limit=5`, confirmed by normalized **containment** on artist and title
3. `GET api.deezer.com/search?q=track:"<title>" artist:"<artist>"`

Measured today, live, on three seeds taken from his own library:

| seed | rows | labs id-map only | with the ladder |
|---|---|---|---|
| C418 "Alpha" (in his queue right now) | 15 | **0 (0%)** | **15 (100%)** |
| Portishead "Glory Box" (first 40 rows) | 40 | 23 (57%) | **40 (100%)** |
| Thievery Corporation "Lebanese Blonde" (first 15) | 15 | 6 (40%) | 14 (93%) |
| **aggregate** | 70 | **29 (41%)** | **69 (99%)** |

The original design's Apple-id-only resolver produces **zero playable rows on the exact seed it nominated as its own live proof**. The audio exists; the MetaBrainz id-mapping dataset simply has no coverage on game soundtracks. Name search recovers all of it.

Two details that matter and are easy to get wrong. **Matching must be containment, not equality**: iTunes returns `artistName: "Lena Raine & Minecraft"` for artist `Lena Raine`, so an exact-artist rule fails all 15 rows. **Deezer's URLs are signed** (`hdnea=exp=...`, verified today, ~17 minutes out) and Apple's are not (verified: plain `https://audio-ssl.itunes.apple.com/itunes-assets/...m4a`, HTTP 200, 832,929 bytes, `audio/x-m4p`). So Apple resolves at enqueue and caches; Deezer resolves lazily and must be re-resolved if the entry sits in the queue.

That last point is why **`QueueEntry::Stream` grows two fields in one pass**: `key: Option<String>` and `duration: Option<f64>`. The key carries `preview/<mbid>` so the entry can always be re-resolved, wanted from the queue, deduped, and survives `resume.toml`; without it, `song_pairs` (`handler.rs:8205-8218`) renders a Stream's `file:` as the raw 180-character CDN URL and the MBID is gone forever. The duration gives the preview an honest `Time:`. Roughly ten mechanical sites, all inside `hypodj-core` (verified: `hypodj-cli`, `hypodj-tui` and `hypodj-client` have no dependency on `hypodj-core` at all, so this is not a cross-crate change).

**Still no third `QueueEntry` variant.** Everything a preview must never do (scrobble, enter the store, be starred, become `last_finished`, warm-prefetch) is keyed off having a `SongId`, and `Stream` already answers all of it by having none. Two `Option` fields cost less than twenty-five new match arms encoding "no" a second time.

**Why 30 seconds is the ceiling and not a stepping stone.** Full-length playback of an unowned commercial track does not exist through any free legal API. Deezer and Apple cap at 30s; Spotify removed previews for new apps; Bandcamp has no discovery API; FMA's API is dead; Jamendo and archive.org have full audio that will never contain the Lena Raine track. The road past 30 seconds is acquisition, and that is a different task.

## 3. Where the signal comes from

### VERIFIED today, from this machine, no auth, no account, no key

`POST https://labs.api.listenbrainz.org/similar-recordings/json`, body `[{"recording_mbids":["<mbid>"],"algorithm":"session_based_days_9000_..."}]`. The field is the plural array or it 400s. Track-level global collaborative filtering over real listening sessions, not clipped to any collection. Verified against MBIDs read off his live queue.

- `POST .../acr-lookup/json` (artist+title to canonical MBID). Verified: Massive Attack "Teardrop" to `f3bba4cd-...`, Thievery Corporation "Lebanese Blonde" to `27a32525-...`.
- `POST .../apple-music-id-from-mbid/json`. Works, but with the coverage measured above.
- `POST https://api.listenbrainz.org/1/popularity/recording`. Verified: Lena Raine "Chrysopoeia" 3,797 listens / 662 users vs Portishead "Glory Box" 823,572 / 85,164. This is the obscurity knob and it is free.
- Rate limits, measured from real headers: LB main API `x-ratelimit-limit: 30`, `x-ratelimit-reset-in: 8`, i.e. 30 requests per 10 seconds per IP unauthenticated. The **labs** host advertises no limit and returns no rate headers at all. Undocumented means unknown, not absent: serialize, cache by seed MBID with a long TTL, never run on a timer.
- His library carries the MBIDs. `Song.musicbrainz_id` already exists (`model.rs:133`, mapped at `subsonic.rs:884`) and every live queue entry reports `MUSICBRAINZ_TRACKID`.
- `GET /1/explore/lb-radio?prompt=...` still returns **HTTP 500, "LB Radio currently disabled due to high load"**, re-confirmed today. Do not build on it.

### The reframing that makes this cheap

Navidrome 0.61.1 already ships a ListenBrainz metadata agent hitting this same labs host. The wall is not the missing `sonicSimilarity` extension and not signal quality: it is that `getSimilarSongs2`'s contract returns rows **from the library**. Navidrome computes a rich global list and throws away everything he does not own. hypodj calling labs itself does not buy a better recommender; it buys the discards, and the discards are the entire feature.

### ASSUMED, flagged

Apple `previewUrl` longevity beyond today. Apple's ~20 requests/minute limit (affiliate-terms folklore, not read). iTunes Search's affiliate terms generally, which are the least clearly established piece of the stack. Deezer's 50-per-5-seconds figure (never burst-tested, deliberately).

### What a token or account would buy

Nothing in stages 0 through 3. The personalized tier needs no token either, only that **his Navidrome user has the ListenBrainz toggle flipped** (Settings, Services, paste a LB token). That is zero hypodj code, because hypodj already scrobbles to Navidrome. It unlocks `GET /1/user/{u}/listens` (the play history Subsonic structurally refuses to expose), `cf/recommendation` with `latest_listened_at` (null means he has genuinely never heard it), Weekly Exploration's 50 unheard tracks a week, and `fresh_releases`. All readable unauthenticated once the user exists on LB.

**Flip that toggle today, before any code is written.** The tier is a weekly batch job with an undocumented cold start; the clock only starts once listens arrive.

## 4. The design

One module, one verb, one uri prefix, one file. `crates/hypodj-core/src/beyond.rs` becomes the only file in the tree touching non-Subsonic music APIs, the same one-file blast radius `subsonic.rs` holds. Its template is `station_identity.rs`, which is already exactly this shape: an unauthenticated outside catalogue behind a long-TTL cache, with the match written as a pure function tested offline against a captured fixture.

**`beyond.rs`** exports `Candidate { recording_mbid, title, artist, release, release_mbid, caa_id, score: i64, seed_mbid }` (score is an **integer rank**, verified: 1263, 756, 573; the design's `X-Score: 0.83` was wrong), pure fixture-tested `parse_similar` / `parse_apple_ids` / `parse_itunes_search` / `parse_deezer_search`, and thin `async fn similar` / `async fn preview_url`. HTTP client declared beside `station_identity_http`, same bounded timeouts and same `unwrap_or_else(|_| reqwest::Client::new())` fallback so the certless Nix sandbox cannot fail construction, plus a real `User-Agent: hypodj/<version> ( <contact> )`. Caching via the existing `TtlLru` from `cache.rs`, three keyspaces (`similar`, `preview`, `owns`), get-release-await-put, never held across an await.

**Empty results are ambiguous and must be handled as two cases, not one.** An empty `similar-recordings` response can mean a non-canonical MBID (verified: two plausible Portishead MBIDs return `[]` where the canonical one returns 100) **or** genuinely no neighbours (the refuter found Thomas Vaquié "Crabe" returns 0 rows even after successful canonicalization). So: retry once through `acr-lookup`, and if the canonical MBID is also empty, say "no neighbours for this seed" rather than looking broken.

**The classifier** batches: one `search3` per distinct **artist** in the candidate set, then filter locally by exact `musicbrainz_id`, falling back to normalized artist+title only for hits with no MBID. Measured on his live daemon, a sequential `search` round trip is 65-98ms; one per candidate at a cap of 40 would be ~3.5s against `hypodj-client`'s 5s `IO_TIMEOUT` (`crates/hypodj-client/src/mpd.rs:15`). Per-artist batching keeps it under a second at any realistic cap. Rows the classifier cannot decide are marked `owned?` and **shown**, never dropped: a false "unowned" that previews something he already has is the failure that gets the feature switched off, and a false "owned" is invisible.

**The verb** is `MpdCommand::Beyond(BeyondCmd)` parsed on `parse_radio`'s shape (`mpd.rs:358-373`) with the same whitelist discipline, registered beside `"radio"` at `mpd.rs:1237`. Reply on the `search_all` wire shape with the `X-Hits` preamble, one block per row: `file: preview/<mbid>`, `Title`, `Artist`, `Album`, `X-Rank`, `X-Owned`, `X-Preview`, `X-Listens` (the popularity number, so obscurity is visible), `X-Seed`. Owned rows carry `file: song/<id>` instead and add as ordinary full-quality library plays.

**Three behaviours must be corrected in the same stage as playback, because they are not cosmetic:**

*Auto-identify must be suppressed.* `reschedule_auto_identify` (`handler.rs:4150-4180`) arms for **any** `QueueEntry::Stream` when `recognize.auto` is on, and it defaults on (`config.rs:1046`). Eight seconds into a preview the gate returns Proceed (no ICY title, deck Playing), `ffmpeg` re-fetches the same CDN URL for 11 seconds, songrec fires, and around t=20s `apply_stream_meta` overwrites the `(preview)` title with the recognized track name. The daemon would spend a Shazam call and a second CDN fetch per audition in order to tell itself something it already knows, and then lie about what is playing. Fix: `reschedule_auto_identify` declines when the entry's new `key` field is `Some`. One condition.

*The drain edge must not fire the walk.* `AUTOFILL_MIN_INTERVAL` is exactly 30 seconds (`handler.rs:1836`) and a preview is 29.98 seconds. With the walk armed, a preview at the queue tail reads as a true drain (`is_true_drain`, 699-717, does not exclude streams), so every audition triggers a refill; two auditions back to back land inside the interval and trip the fuse whose own comment calls it a "pathological instant-EOF spiral (corrupt library)". Fix: an audition inserts **after current**, not at the tail, so it is never the last entry. The precedent is already in the tree: `radio` explicitly resets `last_autofill_at` at `handler.rs:5789` because "a deliberate human gesture is not degenerate input". This needs a live proof with the walk armed and `HYPODJ_AUDIO=null`, not a null-player unit test.

*Auditions must be recorded.* A preview is a dead end in every feedback loop: no scrobble (`scrobble.rs:130` matches only `Eof { song: Some(id) }`), never `last_finished` (`handler.rs:6906`), no `SongId`. So the daemon writes its own: every audition appends `{mbid, artist, title, seed, verdict, ts}` to the same file as the wants. That file, not the queue, is what makes the next `beyond` different from the last one, by seeding multi-MBID from what he actually auditioned and liked.

**`wants.toml`** at the state_dir root beside `resume.toml` (not inside `store/`, whose reconciler treats filenames as song ids), on `resume.rs`'s exact discipline: `schema_version` gate, atomic write, load returns empty for missing, unreadable, garbage, truncated, or schema mismatch, and never blocks startup. Three lists: wanted, dismissed (permanent, must survive restart, which the in-memory `autofill_seen` ring cannot), auditioned. This is the interface contract with the soulseek task a00fwns, and hypodj's job ends at writing it. His slskd download directory is Navidrome's MusicFolder, so an acquired track becomes an ordinary `Song` and every capability lights up with no further code. hypodj gets no slskd key and never auto-downloads. A recommender suggesting a track is not consent to pull a copyrighted file off a stranger's machine.

**TUI**: `Screen::Beyond`, F6 beside the F5 binding, a new `crates/hypodj-tui/src/beyond.rs` on `find.rs`'s shape (flat `Vec<Row>`, one cursor, the `Phase::{Cold, Loading(q), Done, Failed}` staleness gate). `Intent::Beyond` on the same dedicated non-mutating find socket, obeying its stated rule: never set `sent_mutation`, never trail `request_refresh`. Keys: Enter auditions, `w` wants, `x` dismisses, `R` reseeds from current, `o` xdg-opens a YouTube search for the full track.

## 5. Stage 0 and Stage 1

**Stage 0 costs nothing and should happen tonight.** Flip the ListenBrainz toggle in his Navidrome user settings. Zero code. It starts the cold-start clock on the only personalized signal in the whole stack, and it independently gives hypodj a readable play history that Subsonic will never expose.

**Stage 1 is the signal plus the audition, together.** The original design split them: list at S1, playback at S2. That split is what let the Apple coverage hole survive design review, because S1 would have "passed" while producing a list where nothing could be played. They ship together or the proof is meaningless.

What it delivers: `beyond` and `beyond song/<id>` returning ranked rows marked owned or not, with a working `preview/<mbid>` uri, so `add preview/<mbid>` then `play` produces 30 seconds of real audio from ncmpcpp with no client changes. The live proof is the assertion I already ran by hand: seed from the C418 track in his queue, get Lena Raine back marked NEW, press play, hear Chrysopoeia. Plus a coverage number printed by `beyond status` over his starred set, because that is the number the whole feature lives or dies on and it should be measured, not assumed.

Roughly: `beyond.rs` around 350 lines plus fixture tests, the resolver ladder around 120, the classifier around 100, the verb and reply builder around 100, the `enqueue_uri` arm around 40, the two `QueueEntry::Stream` fields around 10 mechanical sites, the auto-identify and drain corrections two conditions. Call it two focused days including the live proof. No new Rust crates: `reqwest`, `serde_json` and `toml` are already direct dependencies of `hypodj-core` and already used for exactly this class of work.

Stage 2 is the F6 screen. Stage 3 is `wants.toml` and the audition log. Stage 4 is the personalized tier, once Stage 0 has had a few weeks to accumulate.

## 6. What it will not do

- **It will not give him full-length unowned tracks.** Thirty seconds is the legal ceiling, not a milestone.
- **It will not play by itself.** Every gesture is his. A preview spliced into an ambient set costs two cold loads (`current_can_warm` declines for `Stream`, `handler.rs:7660`) and truncates mid-idea. That is worse than no discovery. Interleaving is not in this design at any stage.
- **It will not tell him what is new to him, only what is not in his library.** Seeding Sneaker Pimps returns Portishead and Tricky; those are canonical records he chose not to buy, not discoveries. Popularity ranking (verified free) narrows the gap. `latest_listened_at` from the personalized tier actually closes it, which is another reason Stage 0 is tonight.
- **It will not make his existing endless walk better.** See section 7.
- **It will not acquire anything.** It writes a file. Something else has to read it.
- **It leaks.** MetaBrainz sees the MBIDs he seeds from, correlatable by IP. Apple and Deezer see which tracks he auditions. The class is not new (his Navidrome already ships artist and album names to Last.fm on every scrobble) but the granularity is finer and it is per-gesture. He should know before Stage 1 ships.

## 7. What the attacks changed, and what they got wrong

**Changed the design:**

The Apple-only resolver is dead. Confirmed independently: 0/15 on his own seed, 41% aggregate across 70 rows. Replaced by the three-rung ladder measured at 99%. This was correctly called fatal.

Apple's mapping gap and the short row list are **correlated on exactly his taste**: the niche seed gave both 15 rows and zero coverage, the mainstream seed gave 100 rows and 57%. The design's argument that "one call returns up to 100 rows, so attrition is affordable" was drawn from Portishead, an artist he owns **zero** songs by (verified live) and can therefore never seed from.

`QueueEntry::Stream` gains `key` and `duration`. Without the key, an added preview loses its MBID to `song_pairs` forever.

Auto-identify suppression and the drain-edge fix moved from unwritten to Stage 1 requirements.

The audition log is new. Without it the feature genuinely cannot compound: everything a preview does is forgotten the moment it ends.

Stage 0 (the Navidrome toggle) was promoted from an S6 footnote to the first thing that happens.

The classifier batches per artist; `score` is an integer rank.

**Confirmed as false, with evidence:**

The design claimed "`resolve_play` MUST stay synchronous because the fade terminal calls it under the slot lock." Not true. `resolve_play` has exactly two non-test callers, `handler.rs:7737` inside `async fn skip_with_fade` and `handler.rs:7981` inside `play_index_inner`, and both pre-resolve before any fade is installed. `Terminal::SkipLoad` carries the already-resolved `play`. The refuter was right, and the false invariant was doing real damage: it was the stated reason for refusing Deezer, precisely where Deezer was most needed.

The design's own risk list claimed "a new `MpdCommand` variant breaks exhaustive matches in hypodj-cli and hypodj-tui." Also false. Neither crate depends on `hypodj-core` (their `Cargo.toml`s list only `hypodj-client`, `hypodj-nl` and `hypodj-build-info`; `grep MpdCommand::` in both returns nothing). `hypodj-client`'s own header says it explicitly: "NO dependency on hypodj-nl, hypodj-core, or any model." The historical dj-gui break came from a plan-layer type via `hypodj-nl`, not from `MpdCommand`. Whole-workspace build and `nix build` are still mandatory; the stated mechanism was wrong.

One attack claimed empty similar-recordings results mean a wrong MBID variant. Half right. Both cases exist: non-canonical MBIDs return empty and canonicalization recovers 100 rows, **and** genuinely-empty seeds exist after successful canonicalization. Treating either as the only case produces a wrong message.

One attack proposed requiring "normalized artist AND title agreement" on the name-search rung. As literally specified with equality, that rule fails all 15 C418 rows, because iTunes credits them to "Lena Raine & Minecraft". Containment is required.

**Not reproduced:** the claim that 100 returned rows collapse to 96 distinct MBIDs. My Portishead pull today gave 100 distinct out of 100. Dedup is still cheap and still worth doing.

## 8. What is genuinely Guilherme's to decide

**First, and this is the honest one: for the thing he actually described, opening YouTube is still better, and it will still be better after all of this ships.** YouTube gives full-length tracks, forever, from a mix that learns from every play, at the cost of one click. Beyond gives a list he works through, thirty-second clips, from a neighbourhood that on his C418 seeds is essentially **one artist** (13 of 15 rows Lena Raine; every C418 seed returns her at the top), that until Stage 3 teaches nothing. Beyond wins at a different question: "is this worth owning, and remember that I wanted it." That is triage, not listening. If what he wants is the evening to keep going by itself, this is not it, and calling it discovery would be a lie.

**Second: run the cheap experiment first.** An internet-radio stream is already fully playable today, full-length, infinite, legal, entirely beyond his library, one gesture, zero new code, with `identify` already wired to name what is playing. He has **zero saved stations right now** (verified: `lsinfo Stations` returns empty). A week with two or three stations and `identify` bound to a key answers a question no amount of design can: does an infinite unowned stream actually satisfy him? If yes, the whole LB build shrinks to one useful piece, picking which stream, and `beyond.rs`, the F6 screen and the Apple dependency all evaporate. That week costs nothing and there is an agent working on radio discoverability in parallel already.

**Third: is he willing to close the loop?** Stage 3 hands off to soulseek where he currently shares nothing, so expect long queue positions, and an acquired file needs its recording MBID stamped or the classifier keeps showing it as unowned and the loop does not compound. If a00fwns stalls, what he has is a well-built list of things he cannot hear properly. There is no hedge here: the original design offered the walk upgrade as one, and it is not. His endless walk's LB neighbours on the material he is playing right now are Lena Raine, Kumi Tanioka, Toby Fox and Jeremy Soule, and he owns **zero songs by all four** (verified live). The owned half is empty. On his niche clusters the walk upgrade is a no-op that costs one extra POST per refill.

**Fourth: the privacy trade.** Per-gesture MBID-level taste data to MetaBrainz, audition-level data to Apple and Deezer. Small, real, new.

**Fifth: does he want the Navidrome ListenBrainz toggle on.** It is free, it is one field in a web form, it is the only thing in this entire stack that gets **more useful over time rather than staying flat**, and it also hands hypodj back the play history Subsonic refuses to expose. Whatever he decides about the rest, this one should be yes, and it should be tonight.