# HypoDJ Flatpak (djg)

`tech.familiartools.HypoDJ` packages `djg`, the GTK4 + libadwaita app
(`crates/hypodj-gtk`). It is a thin MPD/TCP client: the hypodj daemon (libmpv,
the offline store) runs outside the sandbox, and the app reaches it through
`--share=network`.

Files:

- `tech.familiartools.HypoDJ.yml` - the manifest (GNOME 51 runtime, rust-stable
  26.08 SDK extension, offline cargo build).
- `cargo-sources.json` - every crate from `Cargo.lock`, so the build runs with no
  network. Regenerate it whenever `Cargo.lock` changes (below).
- `tech.familiartools.HypoDJ.metainfo.xml` - AppStream metadata.

The icon comes from `crates/hypodj-search/data/icons` and is installed inside the
Flatpak under the app id (plus the in-sandbox `hypodj` name, which djg sets as its
window icon), so the Flatpak does not need hypodj-clients on the host.

## Tools

None of these are on PATH by default; a throwaway Nix shell carries them:

```
nix shell nixpkgs#flatpak nixpkgs#flatpak-builder nixpkgs#appstream \
  --impure --expr '(builtins.getFlake "nixpkgs").legacyPackages.x86_64-linux.python3.withPackages (p: [p.aiohttp p.tomlkit])'
```

## Regenerate cargo-sources.json

```
curl -LO https://raw.githubusercontent.com/flatpak/flatpak-builder-tools/master/cargo/flatpak-cargo-generator.py
python3 flatpak-cargo-generator.py Cargo.lock -o flatpak/cargo-sources.json
```

## Build and install

```
flatpak --user remote-add --if-not-exists flathub https://dl.flathub.org/repo/flathub.flatpakrepo
flatpak --user install flathub org.gnome.Platform//51 org.gnome.Sdk//51 \
  org.freedesktop.Sdk.Extension.rust-stable//26.08
flatpak-builder --user --install --force-clean --jobs=4 \
  build-dir flatpak/tech.familiartools.HypoDJ.yml
```

Cost on first build (measured 2026-11-25 on bubble-gum): the runtimes are a
one-time download of about 1.9 GB (Platform 434 MB, Sdk 876 MB, rust-stable
557 MB, plus GL/codec extensions), 5.3 GB installed, 3m19s to install. The app
build itself takes about 1 minute at `--jobs=4` (cargo release build 53s).

`--disable-rofiles-fuse` is needed where FUSE is unavailable (for example inside
an agent sandbox); it is harmless elsewhere.

## Run

```
flatpak run tech.familiartools.HypoDJ
```

The daemon defaults to `127.0.0.1:6600`. To point at another port or host:

```
flatpak run --env=HYPODJ_PORT=6611 tech.familiartools.HypoDJ
flatpak override --user --env=HYPODJ_HOST=10.0.0.5 tech.familiartools.HypoDJ
```

## Lint

Last run (2026-11-25): `appstreamcli validate --pedantic` passes (one pedantic
note: the id contains uppercase letters, which is allowed). `flatpak-builder-lint
manifest` and `appstream` pass. `flatpak-builder-lint repo` fails only on
`metainfo-missing-screenshots` / `appstream-screenshots-not-mirrored-in-ostree`:
the metainfo needs djg screenshots at public URLs (the existing `Screenshots/` are
of the TUI).

```
appstreamcli validate --pedantic flatpak/tech.familiartools.HypoDJ.metainfo.xml
flatpak run --command=flatpak-builder-lint org.flatpak.Builder manifest flatpak/tech.familiartools.HypoDJ.yml
flatpak run --command=flatpak-builder-lint org.flatpak.Builder repo repo
```

## Not done yet

- Flathub submission (needs a maintainer account, screenshots at public URLs in
  the metainfo, and a manifest that fetches a tagged release instead of `dir: ..`).
- A Flatpak for the Qt app (djq).
