# 0001. The user sets the offline cache size from the app, not from config

Status: accepted

## Context

The badge tells the user that favourites did not fit in the offline mirror. It
offers no way to do anything about it. `store.max_bytes` is a config key, and on
bubble-gum the config is rendered by home-manager, so changing the cache size
means editing a Nix expression and rebuilding the system.

That makes the one actionable fact on the surface unactionable in practice. A
status line that names a problem and hides its only lever is worse than silent:
it spends the user's attention and returns nothing.

Stated goal (Guilherme, 2026-09-09): the user can change cache size via the app,
CLI or TUI.

## Options considered

- **A runtime limit persisted in the store root (chosen).** A `.hypodj-limit`
  file holding a decimal byte count, read at open, overriding `store.max_bytes`
  when present.
- **Config only, improve the error message.** Rejected: it does not meet the
  goal. Telling someone to edit a Nix file and rebuild their operating system to
  free 2 GiB of music cache is not a lever.
- **A runtime limit held in memory only, like `store pause`.** Rejected: a pause
  is an action whose safe state is the default, so forgetting it on restart is
  correct. A cache size is a setting. Someone who shrinks the mirror because the
  disk is tight has not asked for that to last until the next restart, and
  silently reverting would refill a disk they just cleared.
- **Write back into the config file.** Rejected: the config is a home-manager
  symlink into the Nix store. It is not writable, and making it writable would
  break the reproducibility the whole machine is built on.

## Decision

A user-set limit persists in `.hypodj-limit` inside the store root, and
`AudioStore::budget_limit()` is the single reader that decides whether the user's
limit or the configured cap is in force.

Four properties make this safe rather than a second source of truth:

**One reader.** `budget_limit()` is called in exactly one place, where a pass
assembles its input. The pass and every status surface therefore cannot disagree
about which number is in force.

**The free-space clamp is untouched.** `derive_budget` still applies
`min(limit, (free + own) - reserve)` on top. A user limit can only ever *lower*
what the store uses, never let it outgrow the disk, so "hypodj cannot fill the
disk" remains structural rather than a promise this knob could break.

**It lives where the store already has exclusive ownership.** The store root is
the one directory the store owns and converges destructively. The limit file is
therefore excluded from `scan_dir` exactly as the ownership marker is - without
that exclusion, convergence would delete the setting that decides how large the
directory may become.

**Write-through before the in-memory value changes.** A failed write must not
leave a limit that is in force this run and gone the next; the user would watch
it take effect and find the mirror re-grown after a restart.

Clamping follows the config path's posture: clamp up to `STORE_MIN_MAX_BYTES`,
never reject. Refusing a too-small value is the one outcome that leaves the user
with no mirror and no explanation.

## Consequences

**Easier.** The badge's "would not fit" gains a lever, which is what makes it
worth printing. The cache becomes tunable without a system rebuild, at the moment
the user notices the problem rather than the next time they edit Nix.

**Harder.** The effective cache size is no longer derivable from the Nix
configuration alone. A machine rebuilt from `os-configurations` does not
reproduce a user-set limit, and this is a real departure from the rule that
configuration on this machine is declarative. The justification is that a cache
size is a runtime decision about the current state of a disk, in the same family
as `store pause`, rather than a description of how the machine is built. This is
the part of the decision most worth revisiting.

**Also harder.** Two numbers now exist where there was one, so any surface
reporting the budget has to say *which* it is showing. `budget_limit_is_users()`
exists for that reason: "16.0 GiB" with no source reads as immovable, which is
the problem this decision set out to solve.

## Questions resolved

Both were put to Guilherme on 2026-09-09 and answered "all of those":

1. **CLI and TUI, not one or the other.** `dj store limit <size>` is the CLI path.
   For dj-gui, `store limit` is routed by the shared client router
   (`hypodj-client/src/route.rs`), so the command bar reaches it - every other
   `store` word is intercepted by the `dj` CLI before `route()` and is therefore
   CLI-only, which is acceptable for read-only views but not for the one setting
   a user is told to change.
2. **The departure from declarative configuration is accepted.** The cost stands
   as written in Consequences: a rebuilt machine does not reproduce a user-set
   limit.

## Status of the implementation

Complete. `.hypodj-limit` persistence excluded from convergence, `budget_limit()`
as the single reader, `set_budget_limit()` writing through before mutating memory,
the `store limit <bytes>|default` MPD verb, `dj store limit <size>` with a shared
binary size parser, the router entry for dj-gui, and the `limit:` line naming
which source is in force.

Covered by five tests: the limit survives a store reopen and clears back to the
config, convergence does not eat the saved file, the size parser is binary and
refuses nonsense, the CLI sends bytes and `default` clears, and the router turns
`store limit 24G` into a command rather than an NL shrug.
