# A question gets an answer

`djc whats offline available` used to print "I did not understand that" (after a
Claude Code failure line), and `djc nl "what is offline"` offered "do nothing - confirm?
[y/N]". A person asking a question wants an answer, not a plan.

## What was actually wrong (verified 2026-11-24, master a0f6af3)

1. **Questions did have a representation, but only two, and the client skipped it.**
   The daemon's `nl` already answers READ-side questions without a plan
   (`NlQuery`, `nl_answer`): favorites and feature-cancel. Nothing covered the
   offline mirror, so the rules path said "not understood". Worse, the client-side
   Claude Code door runs BEFORE the daemon, and Claude only emits actions: handed any
   question it returns `{"type":"noop"}` (observed: three runs of the exact `claude
   -p` call for "whats offline available" all returned `noop`).
2. **A noop was offered for confirmation.** Both the CLI CC path (`render_dsl` of a
   Noop is `action noop`, echoed "do nothing ... confirm?") and the daemon `nl` path
   (the local-model translator emits Noop; the handler tokenised it) asked the human
   to approve an absence.
3. **The CC failure line hid the reason.** `claude -p --output-format json` reports a
   failed run in the stdout envelope (`is_error`, `subtype`, `errors`) and exits 1
   with stderr EMPTY (observed with a forced budget stop: exit 1, 0 bytes stderr,
   `errors: ["Reached maximum budget ($0.0001)"]`). `run_claude` read only stderr, so
   every such failure collapsed to "claude exited with exit status: 1".
4. **The miss hint listed only actions**, never the read verbs that answer.
5. **No album view of the mirror existed** on any verb: `store` counts songs and
   bytes; `add album/<id>` needs the server, so a held album could not be played
   offline by album either.

## Options

**A. Plan IR gains a Query kind.** The translator output (`LlmRawPlan`, the JSON
schema, the GBNF, the cc system prompt, `RawPlan`/`Action`) grows a read-only answer
kind routed to read verbs. Every translator could then answer. Cost: touches the
cross-crate `Action` enum (every exhaustive match, the journal, `run_action_outcome`
must learn to refuse it), the model grammar another agent owns, and the prompt. It
also puts a read on the agent's WRITE channel (`plan add`), which is the wrong
shape for the retractability invariant even if it changes nothing. Offline and
without claude it adds nothing: a model is still needed unless the rules learn it.

**B. Client-side intent routing in `hypodj-client::route`.** Question forms map to
read verbs before any translator. Works offline and without claude. Cost: a SECOND
question grammar beside the daemon's `NlQuery` rules, and `route` is a verb-vs-NL
split shared with dj-gui that would start to understand language.

**C. Extend the daemon's existing question path and make every client consult it
first (chosen).** One grammar (`hypodj-nl` rules `query`, now with
`offline_rules`) decides what is a question; the daemon answers it from live state
(`nl_answer` + rows); the CLI and dj-gui ask `hypodj_nl::is_question` before the
Claude door and send questions to the daemon `nl` instead. A noop from any
translator is reported as not understood.

| | A | B | C |
|---|---|---|---|
| one place to add a question | no (schema + prompt + rules + Action) | no (second grammar) | yes: `rules.rs` + one `nl_answer` arm |
| offline, no claude | only via rules | yes | yes (rules are deterministic, the answer reads disk) |
| agent surface retractable | reads ride `plan add` | untouched | untouched: `nl` answers never arm, no `Action` |
| surface | largest | small, duplicated | small: one `NlQuery` variant, one `StoreCmd` variant |

A and C were evaluated against the code they would touch rather than built in
full. C won on the first criterion alone: the question grammar already lived in
the daemon, and it was the client order that skipped it.

## What shipped

- `store albums` (daemon verb): what plays offline, grouped by album, read from
  the sidecars every held song carries. No server call, so it answers while the
  server is unreachable and before the reconciler's first pass. Rows are
  `OfflineAlbum` (worded) + `OfflineAlbumUri` (`album/<id>`). This is the same
  source dj-gui's offline Albums fallback should use.
- `NlQuery::Offline` answers "what's offline / what can I play offline / what is
  downloaded" with the same rows under an `nl_answer` headline.
- `add album/<id>` falls back to the held songs of that album when the server
  errors, so `djc store play <name>` plays a held album offline.
- The CLI and dj-gui route questions to the daemon before Claude; a Noop is never
  offered for confirmation; the CC failure names the envelope's reason in one line;
  the miss hint lists the questions and read verbs.

Not done here: a `dj_offline` read tool for dj-mcp (the agent still cannot ask
what is offline); it would be a plain read verb beside `dj_heard`.
