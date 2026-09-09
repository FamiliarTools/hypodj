# Design

Living documents: architecture, system specs, technical exploration. Updated as
understanding evolves, unlike the dated snapshots in `../research/`.

- [hypodj offline store: final design](offline-audio-store.md) - the on-disk audio store: validity model, commit protocol, reconciler, eviction.
- [The offline store's user-facing surface](offline-store-user-surface.md) - what the badge and `dj store` may say, and the width constraint that governs it.
- [Audio capture from a stream: rewind, do not record](audio-capture.md) - the tape, and why it is a rewind buffer rather than a recorder.
- [Continuous identification](continuous-identify.md) - why the always-on layer is substrate and the press is the feature.
- [Beyond: reaching past the shelf](discovery-beyond.md) - discovery outside the owned library.
- [LibSearch: a first-class Find tab for the HypoDJ TUI](tui-find-tab.md) - the Find tab design.
- [Waveform visualizer](waveform-visualizer.md) - the music-responsive HUD wave.
