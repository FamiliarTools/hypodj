# Transition engine - synthesis (architect Phase B)

Base: **Candidate B (declarative head + one reconciler)**. Cross-judge skipped: the pool has only Claude models and the parent is Claude, so no other-family judge exists; picked against the rubric alone.

## Rubric scores (1-5)
| Criterion | A actor | B declarative |
|---|---|---|
| Proof case falls out with no special case | 5 (fade-out re-aim) | 4 (needs A's fade-out re-aim for wire-level clear;add;play) |
| One way to change head, bypass unrepresentable | 5 (Decks write-half private) | 4 (mutation and transition cannot disagree; player still reachable) |
| Invariants kept (lossless EOF, fade atomicity, no stranded deck) | 3 (replaces FadeSlot/epoch wholesale; slow loadfile blocks setvol - a regression) | 5 (fade slot, epoch, player API unchanged) |
| Interface depth | 5 | 5 |
| Blast radius of migration | 2 (moves all volume/ramp/transport into a new actor) | 4 (head paths only) |
| Mid-flight totality with 2 decks | 5 (Keep/Reverse/DropQuieter/RedirectTrough) | 4 (Reverse/FinishThenReplan) |

B wins on the two criteria that are one-way doors (invariants, blast radius) and matches Guilherme's reframe literally (a removal is the declaration of a new state). A's actor would replace the proven fade machinery and add a volume-latency regression for no capability the goal needs.

## Grafts from A
1. **Encode the single path**: the player's head-changing primitives (play_url for a head, crossfade_arm/commit/abort, switch to a deck) move behind a `Decks` handle owned only by the reconcile module, so a handler path that loads a head without declaring does not compile.
2. **Fade-out re-aim**: `head None` (clear, remove last) plans a one-legged fade-out (Quiesce); a declaration that arrives during it crossfades the new head in FROM THE LIVE GAIN of the fading leg. This is what makes ncmpcpp's `clear; add...; play 0` crossfade on the wire.
3. **Reverse runs the live envelope backwards** (no settle, no step) when the declared head returns to the outgoing leg.
4. **`Cause` derives** gesture-ness, finish attribution and resync (`is_gesture()`, `attributes_finish()`), never a bare bool at a call site.
5. **Planner table tests first**: `plan` is pure and pinned by a table over (Audible, Declared, Caps) covering the grounding's nine semantics, plus an idempotence property (plan of a settled snapshot is Settled).

## Rejected
- A's single-gain-writer actor (replaces FadeSlot/epoch; slow load blocks setvol).
- A's DropQuieter in favour of B's FinishThenReplan only if the latter is audibly cleaner; decide by the harness (both bounded by 2 decks). Default: FinishThenReplan with a short bounded settle.
- B's `settled(rev)` stays (replies return before audio changes), A's Superseded reply is not needed.

## Behaviour changes (intended, user-visible)
- play N / playid / plan Play / radio / autofill start / MPRIS next now crossfade while playing (were cuts).
- clear while playing fades out over the skip crossfade length instead of stopping instantly; status reports stopped at once.

## Verification contract
The baseline harness (agents/te-harness, transition-baseline.tsv) re-run after the change must show: every gesture cause while playing -> CROSSFADE; clear+add+play -> CROSSFADE; same-album EOF -> GAPLESS; skip never scrobbles; single-deck (null player) -> DIP. Plus workspace + nix gates, live-libmpv ignored tests, and an interrogate pass on the diff.
