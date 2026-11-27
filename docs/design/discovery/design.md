# Discovery beyond the library: design

Status: research, 2026-11-26. Supersedes nothing; it re-reads `docs/design/discovery-beyond.md` (2026-08-02) against four months of evidence that doc did not have. Everything marked MEASURED was observed from this machine on 2026-11-26; everything marked DOC is a primary source read the same day.

## 1. The want, distilled

Guilherme's words, recorded in Memoria (k3vv5mo, 47vjgly): the reason he still opens YouTube is that hypodj can only recirculate what he owns. The want is **"press play and the evening keeps going with music I do not have, in the lane I am in, and the good ones do not get lost."** Three separable parts:

1. **A stream** - full length, endless, zero gestures after the first.
2. **In the lane** - his need moves through the day (bright motion in the morning, instrumental propulsion for focus, hazy ambient at night; Memoria 01kxsff). One station is not one lane.
3. **Capture** - the track that lands becomes something he owns later.

The August design answered a fourth, different question (triage by 30 second preview) and said so itself. That verdict stands. What changed is that parts 1 and 3 are no longer hypothetical.

### What the ledger already says (MEASURED, `~/.local/state/hypodj/heard/`)

The cheap experiment (47vjgly) was never formally closed, but it was lived. Between 2026-11-13 and 2026-11-21:

| measure | value |
|---|---|
| days with stream listening | 6 |
| stream time (lower bound, 5 minute buckets holding a ledger row) | about 15 h |
| tracks identified on streams / distinct artists | 138 / 124 |
| of those, already owned (`owned: true`) | 1 (under 1%) |
| identify attempts that named nothing | 46 of 177 (26%) |
| `mark` presses | 17 on 5 days, all with a tape (8 named a track, 6 a bare moment, 3 a show) |
| stations used | NTS 1, NTS 2, 4 To The Floor, Poolside (one row of Modular Station) |
| saved stations with no ledger row | KFJC, Radar, Oxigenio, RNE 3, The Lot, Savage, Moon Mission, 6 KFJC archives |

And the marks were acted on: Memoria task 01m0wkz records "Tan Grande Nada, heard on NTS 2026-11-13" fetched the same day, Martsman "Bunkers" (marked 11-14) chased to Bandcamp, and 12 releases from one NTS show acquired on 11-17.

Reading, with its limit stated: this is evidence of USE, not a statement from him. It shows an unowned stream holds him for hours and that capture leads to acquisition. It does not show the stream is in the lane (he only ever used one broadcaster, and 124 artists in 138 tracks on a hand-picked broadcaster says nothing about a machine-picked one), and stream rows stop on 11-21, which the ledger cannot explain (no row records why a station was left).

So the open question narrowed. It is no longer "does an infinite unowned stream satisfy the want" but **"can hypodj pick the stream, so it works in every lane and not only where NTS happens to have a channel".**

## 2. The cheapest experiment, with no curating by hand

### What already exists and costs nothing

`add <url>` plays any stream as `QueueEntry::Stream`; `[continuation] mode = "radio"` already plays a station at the drain edge; auto-identify writes `heard` rows every 300 s; `station_meta.rs` already learns a station's genres from what it was heard playing; `mark` already captures. The only missing piece is the PICK.

### Where a pick can come from (MEASURED)

**Radio Browser** (DOC: https://de1.api.radio-browser.info/ ; no key, no account, asks for a descriptive User-Agent and server discovery through DNS `all.api.radio-browser.info`; "can be used freely but without guarantee to work"; no rate limit stated; 53,424 stations, 336 broken). `GET /json/stations/search?tag=..&hidebroken=true&order=votes&reverse=true` against his core lanes:

| lane tag | stations | at 128 kbps or better | top hit |
|---|---|---|---|
| downtempo | 127 | 93 | SomaFM Groove Salad |
| trip-hop | 26 | 23 | OFFradio |
| ambient | 400+ (capped) | 318 | Ambient Sleeping Pill |
| deep house | 223 | 193 | 1.FM Deep House |
| acid jazz | 16 | 15 | 100% Acid Jazz |
| dub | 142 | 107 | polluted: `tag=` is a SUBSTRING match and returned dubstep/EDM |
| detroit techno | 0 | 0 | none |

Two facts fall out. `tagExact` / `tagList` is mandatory (`tagList=downtempo,trip-hop` returned 10 clean stations). And Radio Browser cannot search "stations that play artist X": its index is self-declared tags only. Similar-artist station search does not exist there.

**NTS mixtapes** (MEASURED: `GET https://www.nts.live/api/v2/mixtapes`, no auth, 16 always-on channels each with a one line description and `audio_stream_endpoint`; `GET /api/v2/live` gives both live channels with per-show `genres` and `moods`). It is the broadcaster his ledger says he actually uses, and the list maps onto his lanes almost one to one: Slow Focus (ambient, night), 4 To The Floor (house/techno, focus), Island Time (reggae/dub), Expansions (jazz), Poolside (balearic, morning). Caveat: this API is undocumented and unofficial; it can change without notice. Side finding: two saved stations carry stale names ("NTS Infinite Mixtapes 1" is `mixtape4` = Poolside, "2" is `mixtape` = Slow Focus).

**The bridge from artist to tag** (so the pick follows what is PLAYING rather than a static genre): `GET https://labs.api.listenbrainz.org/similar-artists/json` (MEASURED 200, unauthenticated; Tosca returns Thievery Corporation, Bonobo, Nightmares on Wax, Massive Attack, Zero 7, Air ...) and MusicBrainz artist tags (DOC: https://musicbrainz.org/doc/MusicBrainz_API/Rate_Limiting - 1 request per second per IP, meaningful User-Agent required, 503 on excess). The cheaper bridge needs no network at all: the library genre of the current or last finished song, which `Song.genre` already carries.

### The experiment

`stations suggest [lane]` - a read-only verb. Lane defaults to the genre of the last finished library song, else the top genres of the starred set. It queries the NTS mixtape list plus Radio Browser `tagExact`, filters `lastcheckok = 1`, bitrate at least 128, drops anything already saved, ranks by votes, and PROBES the top few before showing them (his own `.pls` collection already contained one URL that was an HTML player page and one 503; a HEAD/short GET must return an audio content type). It returns rows shaped like Find's station hits with the raw stream URL as `file:`. Nothing is written to Navidrome. `radio beyond` = suggest for the current lane and play the top row. That is the whole build: one outbound module on the `station_identity.rs` shape, one verb, one alias.

Then two weeks of ordinary listening, no instructions to him beyond "try `radio beyond` when you would have opened YouTube".

### What to measure (the ledger does most of it already)

One addition is needed: a `tune` row when a stream starts and when it is left (station, origin = saved | suggested | continuation, seconds listened). Today dwell can only be inferred from the 300 s identify cadence, and the reason a station was abandoned is invisible. With it, `heard report` prints per origin:

- **dwell**: median minutes per tune-in. Under about 10 minutes for suggested stations means the pick is wrong.
- **marks per stream hour**: the baseline is 17 marks in about 15 h, roughly 1.1/h on hand-picked NTS. A suggested station far below that is wallpaper.
- **unowned share and new-artist share** of identified tracks (baseline: 99% unowned, 124 distinct artists in 138 tracks).
- **return rate**: suggested stations tuned again on a later day.
- **follow-through**: marks that later match the library (`library_match.rs` re-run over old mark rows).
- the one number hypodj cannot measure: did he open YouTube anyway. Ask once at the end.

Decision rule, fixed now: if suggested stations reach at least half the hand-picked dwell and mark rate, the stream design (B) is the product and the list design (A) shrinks to a lane-picker input. If suggested stations fail while NTS keeps working, the want is curation by PEOPLE, and the right build is NTS-shaped (shows, tracklists), not a wider station index.

## 3. Three structurally different designs

### A. The list (recommendations, no full playback)

- **Sources.** ListenBrainz labs `similar-recordings` / `similar-artists` (MEASURED: both 200 without auth today). `GET /1/lb-radio/artist/<mbid>` and `/1/lb-radio/tags` (MEASURED 200 without auth; the artist call for Tosca returned 3 recordings each for Boards of Canada, K&D, Kid Loco, Moby, Portishead, Bonobo, Zero 7; DOC: https://listenbrainz.readthedocs.io/en/latest/users/api/core.html). `GET /1/cf/recommendation/user/<name>/recording` (DOC: experimental, 204 until generated; https://listenbrainz.readthedocs.io/en/latest/users/api/recommendation.html). His Navidrome has forwarded scrobbles to ListenBrainz since 2026-08-02, so about 16 weeks of listens exist; whether recommendations are generated yet is one GET that needs his username.
- **Changed since August.** `GET /1/explore/lb-radio` no longer returns 500; it returns **401 "you need to provide an Auth token for this endpoint"** (MEASURED). The prompt-based radio is usable again but only with his token.
- **Limits.** `X-RateLimit-Limit: 30` per roughly 10 s window observed unauthenticated; DOC says clients must follow the `X-RateLimit-*` headers and that a valid token may raise limits (https://listenbrainz.readthedocs.io/en/latest/users/api/index.html). Do not page a whole listen history through `/listens`; the docs forbid it.
- **Last.fm is leaving the free tier.** From 2026-12-01 an API key works only if the owning account has Last.fm Pro (announcement: https://support.last.fm/t/api-keys-will-require-pro-from-1st-december/121030 ; Navidrome docs change: https://github.com/navidrome/website/pull/561). The terms were already non-commercial only, 100 MB data cap, attribution and link-back (https://www.last.fm/api/tos clauses 3.1, 4.3.4, 2.7). Do not build on it. **This also touches what already ships:** his Navidrome runs `LastFM.Enabled = true` (`hosts/canned-ideapad/music-server.nix`), and `getSimilarSongs2`, which feeds the owned-library walk, resolves through the agent order `deezer,lastfm,listenbrainz` (https://www.navidrome.org/docs/usage/integration/external-services/). In five days the Last.fm rung goes dark unless the key owner pays; the walk then leans on Deezer and ListenBrainz. Worth watching the walk's quality across that date.
- **What play does.** Nothing, or a 30 s preview (the August ladder: Apple id map, iTunes search, Deezer; 99% coverage measured then).
- **Failure modes.** Niche seeds collapse to one artist (C418 to Lena Raine); "not in the library" is not "new to him"; empty result is ambiguous.
- **Cost.** About two days for the August stage 1. No account.
- **Invariants.** Read-only, so retractability is untouched. A preview enqueue is a queue mutation and would need to go through an `Action` to be agent-reachable; simplest is to keep it a wire verb only.
- **Verdict.** It is not a listening mode. Its durable value is as the INPUT to B (artist to lane) and to C (what to want).

### B. The stream (stations picked for the lane)

- **Sources.** Section 2: NTS mixtapes, Radio Browser, saved stations, with the library genre or LB similar-artists as the bridge.
- **What play does.** Plays a full-length live stream through the existing `QueueEntry::Stream` path. Identify, tape, mark, station identity all already work on it. `[continuation] mode = "radio"` could take "station = auto" so the drain edge of a library session hands over to the lane's stream.
- **Failure modes.** Tags are self-declared and coarse (no "detroit techno" at all); high-vote stations are the blandest; ads and talk; dead URLs between check and play; a stream is not skippable, so a bad track costs a station change; NTS API is unofficial; mixtape streams carry no ICY, so every name costs a recognition call (46 misses against 131 hits in his ledger, 26% unnamed).
- **Cost.** Smallest of the three: one outbound module, one verb, one ledger row kind. No account, no token.
- **Invariants.** An agent-triggered tune-in is an append plus a deck move, which `run_action_outcome` already snapshots and retracts; it would enter as a new `Selector` arm on `Action::PlayNow` (for example `Selector::Station(lane)`), not as a new execution path. Undo removes the row and restores the deck; it cannot give back the seconds heard, which the invariant already says. Two things stay OFF the agent surface: saving a suggestion into Navidrome (`createInternetRadioStation`, one of the 12 direct writes), and Radio Browser's click counter (`/json/url/<uuid>`, which the API asks clients to call on play and which is an unretractable outward write). The click call should fire only on a human gesture, or not at all.

### C. The acquisition path (heard to owned)

- **Sources.** The `mark` rows he already produces (artist, title, album, ISRC from recognition), plus A's list if built. Sink: his slskd on canned-ideapad, whose download directory is Navidrome's music folder, and Bandcamp for what Soulseek lacks (the Martsman EP was Bandcamp-only at EUR 4).
- **What exists.** The whole loop already runs BY HAND through agents: Memoria holds the slskd recipe (01m7b1aj) and `~/familiartools/music-tracker/queue.csv` is the wantlist. hypodj's own part would be small: a durable `wants` file (or simply appending to music-tracker's queue) written from `mark`, and a re-check that flips a want to owned when the library matches.
- **What play does.** Nothing at press time; the track becomes an ordinary library song later and then every capability applies.
- **Failure modes.** Recognition names an album as "- Single" and the real parent release must be found; Soulseek availability and queue times; an acquired file without its recording MBID keeps reading as unowned; wrong-version matches.
- **Legal shape, facts only.**
  - Portugal's code allows reproduction for exclusively private use (CDADC art. 75(2)(a), art. 81(b)), paired with a private-copy levy (Lei 62/98; cited from memory, not re-read today).
  - In 2012 the Lisbon DIAP archived ACAPOR's complaint against about 2,000 P2P users, writing that it regarded private-use reproductions by P2P participants as lawful and that an IP address does not identify a person (https://www.publico.pt/2012/09/27/tecnologia/noticia/diap-arquiva-processo-contra-partilha-de-ficheiros-por-ser-impossivel-identificar-responsaveis-1564850 , https://www.publico.pt/2012/10/08/jornal/uso-privado-e-legal-25379801). That is a prosecutor's archiving order, not a court precedent, and Publico notes earlier convictions exist.
  - In 2014 the CJEU held that the private copying exception covers only copies made from LAWFUL sources (ACI Adam, C-435/12: https://curia.europa.eu/site/upload/docs/application/pdf/2014-04/cp140058en.pdf , https://ipcuria.eu/case?reference=C-435%2F12). It postdates the DIAP order and binds the reading of national law.
  - Making available (uploading/sharing) is a separate exclusive right that no private-copy exception covers. On Soulseek the exposure is the share folder, not the download; the August doc records that he shares nothing, which also means long remote queues.
  - Not established here: any Portuguese court ruling after 2014 on an individual downloader. None was searched out; treat the current enforcement practice as unknown rather than as safe.
  - Bandcamp purchase is the unambiguous path and has no purchase API.
- **Invariants.** A download is a network write plus a file landing in the library: outside the `Action` layer, not journaled, not undoable. It must never be a `dj-mcp` tool. Writing a want is local and reversible and could be journaled, but there is no reason to put it on the agent surface either. Agents that fetch today do so through a shell with his credentials, which is outside hypodj's retractability claim by its own text.

## 4. Recommended first slice

**Lane-picked streams with honest measurement (design B, experiment-sized), and marks that persist as wants (the one cheap piece of C).** Not A.

Why: the ledger shows streams plus marks are already the behaviour; the unknown is the pick; the pick is the cheapest thing to build and the only one whose result changes what gets built next.

Build, in order:

1. `tune` rows in the heard ledger (stream start/leave, origin, seconds).
2. `stations suggest [lane]` and `radio beyond`, as in section 2. Wire verbs only; no agent tool yet, no Navidrome write.
3. `heard report`: the section 2 numbers, split by origin.
4. Wants: each `mark` on an unowned track appends one line to a wants file (artist, title, album, ISRC, station, time, tape name); the report shows wants that have since become owned. hypodj fetches nothing.

Falsifiable acceptance checks:

- `stations suggest ambient` on the live daemon returns at least 5 rows, none already saved, and every row's URL answers with an audio content type when fetched at that moment. Fails if any row is HTML or dead.
- `stations suggest` with no argument after a Tosca track names a downtempo/trip-hop lane and returns rows tagged so; after a Derrick May track it must NOT return the same rows.
- With Radio Browser unreachable (blackholed in an isolated daemon), the verb answers within its timeout with NTS rows only and a line saying what is missing; with both sources down it ACKs, it never hangs and never returns an empty OK.
- `radio beyond` on an isolated daemon (`HYPODJ_AUDIO=null`, store off) leaves the deck Playing a `Stream` entry, and a `tune` row with `origin: suggested` exists on disk in the ledger afterwards; leaving the station writes the closing row with a non-zero seconds value.
- Nothing in Navidrome changes: `getInternetRadioStations` count is identical before and after a suggest and a play.
- A `mark` on an unowned recognized track adds exactly one wants line; a `mark` on an owned track adds none (it stars, as today).
- Workspace test, `nix build .#hypodj` and `.#hypodj-clients` green; parsers tested on captured fixtures so the certless sandbox passes.
- After two weeks: `heard report` prints dwell, marks per hour, unowned share and return rate for `saved` and `suggested` origins, and the section 2 decision rule yields one of its two outcomes. If the report cannot separate the origins, the slice failed regardless of how the listening felt.

What needs Guilherme:

| item | why only he can | blocking? |
|---|---|---|
| Two weeks of listening, and one answer at the end: did YouTube still get opened | the measurement is his behaviour | yes, it is the experiment |
| Tolerance for the unofficial NTS API | it is his most-used source and it can break | no, Radio Browser alone still runs |
| His ListenBrainz username (and, for `explore/lb-radio`, his token through the secret handoff) | personalized tier; token-gated endpoint | no, only for design A |
| Last.fm Pro or not before 2026-12-01 | his Navidrome key stops working; affects the owned walk, not this slice | no, but dated |
| The legal choice on acquisition: Soulseek, Bandcamp-only, or both, and whether hypodj may ever trigger a fetch | section 3C facts; his risk to weigh | no for wants; yes before any fetch automation |
| Radio Browser click reporting on play, yes or no | an outward write in his name | no (default: off) |

Not in this slice: previews, the F6 screen, any `dj-mcp` tool, saving suggestions, any download. Each waits on the two-week numbers.
