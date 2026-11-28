# Fuzzy search (spike, task 01m8bhyp)

Question: how does hypodj search tolerate typos, partial words, missing accents,
word order and transliteration? Spike only - nothing in production changed.

## Where search runs today

- `subsonic.rs` `search3` / `search3_paged` (fixed caps 20 artists, 50 albums, 200 songs).
- `handler.rs`: `searchall` / `search` / `find` (~L16215-16260, client-side tag post-filter
  over search3 hits), NL keyword grounding (~L5011-5052, one search3 per content keyword),
  library match for recognised titles (`library_match.rs`, title-only query by design).
- Clients: `dj-gui` Find (`hypodj-tui/src/worker.rs` -> `searchall`), `djg`, `djq`/`djc`
  all go through the daemon, so ONE daemon-side change covers every front end.

## What Navidrome already tolerates (real library, 137 artists / 263 albums / 1966 songs)

36 flawed queries built from real names, hit@5 = intended item in the first 5 of its kind
(`crates/hypodj-core/benches-spike/bench.py`, read-only, all calls to the live server):

- Already fine: missing accents ("bjork", "fohn", "kesz az egesz", "bananenrauber"), word
  order, partial words ("simon turner"), dropped punctuation, a missing middle word,
  plain prefixes. Navidrome folds accents and ANDs tokens.
- Fails: every real typo or dropped letter ("masive attack", "hyperon", "feeling sleezy"),
  de-transliteration ("foehn" for Foehn/Föhn, "fluegel"), split/joined tokens
  ("sakurazaka 46", "rollingstones"). search3 is a token LIKE match, no edit distance.

## Approaches, same 36 queries

| approach | hits@5 | median latency | max | notes |
|---|---|---|---|---|
| baseline search3 | 15/36 | 57 ms | 156 ms | 1 call |
| (b) rewrite only (fold, umlaut digraphs, digit split) | 15/36 | 68 ms | 182 ms | 2-4 calls; gains nothing, Navidrome already folds |
| (b2) rewrite + truncate tokens by 2 | 24/36 | 110 ms | 412 ms | 3-5 calls; fixes tail typos only, adds noise |
| (a) local fuzzy index only | 36/36 | 48 ms* | 606 ms* | no server call |
| (c) search3 then local fuzzy fallback | 36/36 | 108 ms* | 722 ms* | local pass runs only if search3 looks thin |

\* Python prototype (pure-python Levenshtein, O(names x tokens)); the local part is the
cost. A Rust port (strsim is already in Cargo.lock transitively) over ~2.4k names is
estimated at low single-digit ms per query - UNMEASURED, port before trusting.

Scoring used: 0.5 * max(trigram Jaccard of folded name, of space-stripped name) + 0.5 *
mean over query tokens of best normalised Levenshtein vs an entry token (tokens <=2 chars
exact). Cut-off 0.35. Folding: NFKD strip marks, sz->ss, punctuation to space, digit/letter
boundary split.

Caveats on 36/36: (1) queries were written by me against the same names, so recall is
optimistic; (2) precision was NOT measured - (a) always returns its top-5 above 0.35, so
a nonsense query still returns something. search3 returning nothing is a useful signal
the fallback must preserve (show fuzzy hits labelled as "did you mean"); (3) the library
is small, a 50k-song one makes the O(n) scan cost real (needs a trigram posting-list
prefilter).

Memory: index of 2366 names with folded form and two trigram sets was 9.0 MB python heap
(measured, tracemalloc). In Rust with packed u32 trigram hashes expect roughly 0.3-0.6 MB
(estimate, unmeasured). Build cost: 563 ms python.

## Index source

The daemon holds no complete name list today (`cache.rs` is a TTL LRU, not an index). Fetching
costs: `getArtists` (1 call), `getAlbumList2` paged 500 (1 call here), songs via
`search3 query="" songCount=500` paging (4 calls here; Navidrome supports the empty query).
Titles are the only big kind; artists + albums alone (~400 names here) would already have fixed
every artist and album query above. Refresh on the existing library-changed signal / TTL.

## Recommendation

Approach (c), implemented once in the daemon `searchall` path, artists + albums first, songs
second:

1. search3 as today. If every kind has >= N hits (N ~ 3) return as is: zero cost for correct queries.
2. Otherwise score the query against the local name index (trigram prefilter, then token
   Levenshtein) and append fuzzy hits after the real ones, flagged so Find can show them as
   approximate. Never replace server hits.
3. Skip (b): measured no gain over what Navidrome already does; a bare typo cannot be rewritten
   without an index, which is what (a) is.

Composition with w26 (relevance re-rank by play_count / starred, task ms4acsn): orthogonal.
Fuzzy widens RECALL (which candidates exist), w26 orders them. Run fuzzy to produce the
candidate set, then w26's ranker over it; the fuzzy score should enter as a feature, not be
overridden by habits, or a starred but wrong item would outrank the typed one.

Open before building: precision test with queries that match nothing; Rust port + timing;
CJK titles (trigrams over folded text are fine, Levenshtein per token is not - use trigram only
for non-Latin); index freshness after a Navidrome rescan.

Reproduce: `fetch.py out.json` then `bench.py out.json` in `crates/hypodj-core/benches-spike/`
(needs `/run/user/1000/hypodj/config.toml`; reads credentials in-process, prints none).

## Implemented (approach c, `crates/hypodj-core/src/fuzzy.rs`)

`searchall` runs search3 as before; a kind with fewer than 3 server hits gets up to 5
approximate hits appended after the real ones, each flagged `X-Approximate: 1` (`X-Hits`
still counts server hits only). Scoring is the spike's formula over a trigram posting-list
prefilter (256 candidates per kind), cut-off 0.5. The fuzzy score enters `find_rank` as
`Precision::Approx`, a band below every real one keyed by the score, so habit only breaks
ties between equally close spellings. The index is built in the background at daemon start
and rebuilt when a finished rescan moves `getScanStatus` count (polled every 10 min) or
after 6 h; each kind is capped at 100k entries. dj-gui Find shows "did you mean?" in the
trailer, djg/djq "Did you mean?" in the subtitle.

Measured with `cargo run --release --bin fuzzy-bench -- <config>` on the same live library
(137 artists / 263 albums / 1956 songs):

| | value |
|---|---|
| hits@5 baseline (search3 + find_rank) | 12/36 |
| hits@5 with fuzzy | 36/36 (lowest intended score 0.55) |
| local augment + rank | median 0.14 ms, max 0.47 ms (rerun 0.15 / 0.50) |
| whole library path (incl. search3) | median 107 ms, p90 185 ms; rerun median 140 ms, p90 499 ms (server-bound, varies with Navidrome load) |
| index heap (counting allocator, incl. stored rows) | 2.7 MB |
| index fetch + build | 4.5 to 11.6 s (network paging), background only |
| nonsense precision (10 queries) | 0 approximate rows; best raw score 0.37 |

The baseline is 12 here vs the spike's 15. The spike scored raw search3 order; this scores
the served path (song tag post-filter, then `find_rank`). Which 3 queries differ was not
investigated.
