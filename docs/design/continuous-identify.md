## 1. The answer in three sentences

Continuous identification already ships and is running on your machine right now (`DEFAULT_RECOGNIZE_AUTO = true`, config.rs:204; deployed binary is `hash=36ccf83`, master tip), so the real question is not whether to build it but that it keeps nothing and, on the mixtapes, names maybe one track in five. Continuous is the right shape for **capture** (ICY costs nothing and always worked) and the wrong shape for the **product**, because the artifact it produces is the one you already had for free from mpv scrollback and abandoned six months before hypodj existed. So: keep the always-on layer as invisible substrate, and make the thing you press when something catches your ear the actual feature - that is the one you have never had, and your own files say it is the half that converts.

## 2. When it samples, and on which of your 22 stations it works

Two independent triggers, and they are near-perfectly complementary in the wrong direction.

**ICY titles** arrive losslessly from mpv on every title change (player.rs:1113-1132, deliberately `blocking_send`), cost zero network and zero CPU, and produced 43 of the 59 entries you have ever logged - 92%. On Modular Station this is already a complete, correct, track-level feed. On NTS 1 it gives a title. On the three NTS mixtapes it gives nothing. On NTS 2 your own file (5) shows it gives `Airtime - offline` and `NTS 2 - KIM LANA (R)` - a placeholder and a show name. On Moon Mission (file 4) it gives `Ken Sekiguchi - Moon Mission Recordings Show Vol.34` - show-level, three times. So ICY is not "present or absent"; it is present-and-right, present-and-junk, or absent, and the daemon currently cannot tell those apart.

**Shazam** fires 8s after a stream starts, then blind every 300s, and only where ICY is silent - any non-blank ICY title permanently disarms it for that entry (handler.rs:8760). That means auto-identify runs *only* on the mixtapes, which is exactly where Shazam is weakest, and it is disarmed by `Airtime - offline` on the station where you most needed it.

There is no content-derived trigger and I am not proposing one. astats gives only `Overall.RMS/Peak` (player.rs:1653), lossy twice, with no track identity on the frame (viz.rs:45). A silence gate finds nothing on a beatmatched mixtape (beatmatching is the craft of removing that boundary) and fires constantly mid-piece on modular ambient. It works where it is not needed and fails where it is.

One correction to what I said before: the capture is a **second HTTP fetch** of the stream (recognize.rs:141-152), not a tap of what mpv is decoding, and I claimed that was "correct for Icecast and HLS live edges". That was asserted, not measured. ffmpeg's HLS demuxer defaults to `-live_start_index -3` and mpv's own read position is offset by its demuxer cache, which nothing in this tree reads (`grep demuxer-cache crates/` returns nothing). Two of your stations are m3u8. The offset is real, unmeasured, and it means an 11s window can straddle a transition and resolve to neither track - which the ledger would then record as "Shazam does not know this music".

I also drop the static-URL guard I proposed. On your actual list it is 0-for-6 useful (all six KFJC archive URLs are 404 today, so ffmpeg already fails them for free) and 2-for-2 harmful (Weather Warlock and Savage Radio are live and end in `.mp3`).

## 3. What it costs

**Measured on this machine**, one full cycle against NTS 1: ffmpeg 0.17s CPU / 9.73s wall / 401 KB down; songrec 0.19s CPU / 1.10s wall / ~6 KB up. Total ~0.36s CPU, ~10.8s wall, ~0.4 MB down per call. CPU is a non-issue and always was. The costs are wall-clock occupancy (90% of it ffmpeg in a realtime read) and a full duplicate download of a stream you are already playing - 65 bytes pulled per byte of fingerprint sent.

**Rate limit, verified:** songrec 0.7.4 handles 429 with no retry and its error string is "Your IP has been rate-limited" - the limiter is IP-keyed. Upstream raised its own default interval 4s to 10s to 8s to reduce it. The only concrete threshold anywhere is one bug report against a different client, ~5 req/min. At 300s you are at 0.2/min, 25x under that and 37x gentler than songrec's own shipped GUI.

**Unverified and load-bearing:** whether Apple's limiter also has a daily or IP-reputation component. SongRec #222 has a user blocked for weeks despite minimal use, and the community remedy is "VPN to another IP". If reputation is real, the exposed axis is total daily volume, and that is exactly the axis a tighter clock maximizes.

I was wrong on one number and it matters. I said flattening the miss backoff was "less than 2x". On an all-miss mixtape evening the real delta is **15 calls today versus 96** - 6.4x, and 6 MB to 38 MB. So I am not flattening it: content misses cap at **one doubling (300 to 600s flat)**, transport failures keep the full exponential. That is ~48 calls/day, still catches the next recognizable track within ten minutes, and stops the current 40-minute deafness.

**Battery: not measured.** AC-connected, `power_now` unreadable. Mechanically the dominant term is the wifi radio held awake ~11s per call, and playback already holds a stream open, so while playing the marginal cost is a second TCP stream on an awake radio - probably small. While paused, `RearmPaused` already prevents any wake. I would add no battery knob.

## 4. The miss problem

Two losses compound, and only one of them is Shazam's fault.

**Sampling loss:** a blind 300s clock against ~4-minute club edits never samples about 45% of tracks at all. Those produce no hit line and no miss line - no trace whatsoever.

**Recognition loss:** on white labels, club edits and modular improv, unknown. The honest evidence is n=2: one hit on NTS 1 live, one `no match` on "4 To The Floor".

Realistically that is **15-25% of mixtape tracks named**, and a short file next morning is indistinguishable from a quiet evening. So every render carries a coverage line: `sampled 9 times over 4h; ~45% of tracks never sampled`. A thin sample must read as a thin sample.

The refuters also killed my "Stalled means rate-limited" heuristic, correctly, and handed me something better that I missed: **songrec's stderr already says which failure it is**, and recognize.rs:164 sets `.stderr(Stdio::null())`. With a file input it prints `No match for this song` and exits, or `Network unreachable` and exits, and on a genuine 429 it prints nothing and hangs forever. Capture stderr - it is a one-word change on an existing `.output()` call - and you get the taxonomy free: content miss, transport failure, and timeout-with-silent-stderr as the only 429 suspicion. No timing inference, no false day-long self-suspension.

## 5. Where the results go, and what happens to your text files

It **complements, and it does not replace** - and I have to say plainly that on Modular Station your hand method beat what this will do. mpv printed 100% of announced titles and you pasted 100% of them. Automation's only edge there is that it never stops.

This is the fact that changed my mind about the whole shape: the raw dumps in files (2)-(5) *are* the proposed ICY ledger, minus the directory. You had it, for free, and the last one is dated 2026-01-31. You added two more stations in March 2026 and logged zero tracks. hypodj's first commit is 2026-07-04, five months later - so "the tool hid the scrollback" is not available as an excuse. You abandoned that artifact while still listening.

So the ICY ledger ships as **substrate**: an append-only session file at `~/.local/state/hypodj/heard/2026-08-04-<station>.md` (session-scoped, not per-station-forever - one file per session is what you did five times out of five, and 92% of your listening is one station, so a forever-file passes 1,500 lines inside a month). It makes no promises and you are not expected to open it.

The **human artifact** is `dj heard`, defaulting to the last session, unowned only, capped near 20 lines - the size you actually produced by hand. Marked entries first and separate. `--all` for the dump.

Corrections to what I proposed: dedupe by normalized artist+title over a window, not by adjacency, because your own duplicates are interleaved (file 3 is A,B,A,B twice over - consecutive-dedupe suppresses none of them). Junk ICY (`Airtime - offline`, station-name prefixes, `Show Vol.34`) becomes a heading, not a bullet, and must **not** disarm the recognizer. No auto-starring: a match is an ownership flag that suppresses the row and renders `[owned]`; a star stays something you pressed. And the line carries the **wall-clock time and the show URL** as well as the ISRC - a URL reconstructs the listen, which is what the three YouTube captures that converted actually had; an ISRC is one more lookup.

The heading will be the saved station name (`Modular Station - The Modular Music Radio`), not your `modular_station:`. `QueueEntry::Stream` carries only a URL and a display title (model.rs:194-202) - the .pls basename never reaches the daemon. So my "merges byte-for-byte with your file" claim was false and I am dropping it.

## 6. Stage 1: the smallest thing that changes a single evening

Three days, one new module, four hook points.

1. **The mark gesture, made total.** `identify` already exists as an MPD verb with single-flight. Make Ctrl-s in the TUI (`keymap.rs:186`) never a silent no-op on a stream: star if owned, write a `marked` row if not. One press, one line, one URL, one timestamp. This is the piece you have never had.
2. **ICY rows to a ledger task.** Not from `set_stream_meta` - I was wrong there and it would have been a real bug. `director.rs:543-551` calls it **synchronously on the director spine**, the same task that drives EOF and queue advance, and the write pattern I cited (`resume.rs:143 atomic_write_bytes`) does a whole-file rewrite plus `sync_all`. That is an fsync on the playback spine, and CLAUDE.md records a skip-EOF audible bleed shipping once already. Instead: unbounded mpsc to a dedicated task holding an `O_APPEND` handle, file I/O in `spawn_blocking`, which is the pattern store.rs:1423 already documents.
3. **Capture songrec's stderr** and split the outcome taxonomy at the source.
4. **`dj heard`** as an explicit `MpdCommand` variant, last session, unowned first, marked at the top, with the coverage line.

Cadence changes by one clamp (600s cap on content misses). Fix the stale comment at mpd.rs:246 while you are there - it still claims identify is "on-demand only (never continuous)", false since 7d57c9e.

## 7. What it will not do

It will not name most of what plays on the mixtapes. It will not detect track boundaries. It will not tell you it has been rate-limited with certainty - only suspect it. It will not know whether the 11s it captured is the audio you heard. It will not measure Shazam's true hit rate on your material: I claimed the ledger would produce that number and it cannot, because with no boundary signal the fire count is uncorrelated with the track count. It measures sampler yield, not coverage. If you genuinely want the hit rate, that is a bounded experiment - one station, a tight interval, a fixed window - not a permanently-on sampler.

## 8. The decisions that are yours

**The one I would push hardest on.** My evidence for "the pointer is what converts" (3 of 7 YouTube captures acquired, 0 of 24 radio names) is confounded and I did not see it until the refuters did. The YouTube captures were also *chosen*, one file at a time across four commits over eight weeks. The radio entries were whole sessions pasted unfiltered, mpv underrun messages and all. Chosen-and-pointer converted at 43%; exhaustive-and-bare at 0%. Two variables move together, and the competing explanation is that **the choosing is the conversion step** - in which case the gesture is the entire product and the always-on layer is scaffolding. Stage 1 above logs a `marked` flag precisely so two weeks tells you which, at no extra cost.

**Yes: a one-key gesture probably beats always-on here**, and I say that against my own earlier position. Your record says you abandon anything requiring attention - the curated list froze on 2026-01-23 while dumps kept piling up. But the same record says the artifact you produced deliberately is the one that turned into six Takuya Nakamura albums, heard on The Lot Radio, now in your library. One honest caveat: the press is structurally late, because the 11s capture starts when you press. On an ICY stream that is fixable by marking the previous title. On a mixtape it is not, without a continuously running PCM ring.

**And yes: for Modular Station specifically, your hand-kept log is better than what this produces.** ICY gave you every title; you pasted them. Nothing here improves on that except persistence. Where this genuinely beats you is the mixtapes, where you have never had anything at all - and there it will name one track in five.

Three calls are yours: whether to keep Shazam on at all given that rate (I would, at 600s, since it is already paid for), whether `~/.local/state` is a directory you will ever open or whether `dj heard` needs to be the only surface, and whether to spend one live call checking if `songrec --json` passes the full Shazam envelope through - if it carries `matches[].offset`, a hit buys a longer sleep and the cost model improves for free. I deliberately did not spend that call.