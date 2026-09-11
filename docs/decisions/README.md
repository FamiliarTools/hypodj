# Decisions

Architecture Decision Records. Numbered, write-once, never deleted - superseded
by a later ADR instead.

- [0001. The user sets the offline cache size from the app, not from config](0001-user-settable-cache-limit.md) - proposed. Why a runtime limit beats a config key, and what it costs in reproducibility.
- [0002. The continuation walk matches the shape of what is playing](0002-album-shaped-continuation.md) - accepted. Why album mode is derived from the queue's own shape instead of a setting or a flag.
