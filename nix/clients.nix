{ lib
, rustPlatform
, pkg-config
, mpv-unwrapped
, makeWrapper
, installShellFiles
# DISPLAY-only enrichment (from flake.nix): the semver read from Cargo.toml and
# the HYPODJ_BUILD_INFO grammar baked into the runtime env (the nix sandbox has
# no .git, so this is what `dj --version` / `dj-gui --version` shows).
, buildInfo ? ""
, cargoVersion ? "0.1.0"
}:

# The HypoDJ clients: the `dj` jukebox CLI and the `dj-gui` interactive TUI.
#
# The `cc` feature is now DEFAULT (a plain `cargo build` compiles the client-side
# Claude Code CLI backend), so both bins pull `hypodj-nl` -> `hypodj-core`. That
# transitively links libmpv (hypodj-core's real player), so - unlike the old
# thin-client build - this derivation DOES need mpv at build + link time. The owner
# deliberately dropped the thin-client "never link libmpv" invariant to make cc the
# default. `cargo build --bin dj --bin dj-gui` still only compiles the dependency
# graph of the two bins (now including hypodj-nl + hypodj-core, not hypodj-daemon).
#
# Runtime cc is gated on `claude` being on PATH (cc_available), so this package runs
# unchanged on a machine without it (falls back to the daemon `nl` path). The check
# phase is CERTLESS + network-less: cc_available() is false, so no live `claude`
# call ever runs - only the PURE cc unit tests (prompt build, envelope parse +
# off-surface rejection) execute.
rustPlatform.buildRustPackage {
  pname = "hypodj-clients";
  version = cargoVersion;

  src = lib.cleanSource ../.;

  cargoLock.lockFile = ../Cargo.lock;

  nativeBuildInputs = [ pkg-config makeWrapper installShellFiles ];
  # mpv.pc (for libmpv2-sys at build, pulled transitively via hypodj-core) +
  # libmpv.so (DT_NEEDED at link/runtime).
  buildInputs = [ mpv-unwrapped ];

  # dj-mcp rides along here rather than in its own derivation: it is a CLIENT (pure
  # MPD/TCP, no libmpv - `ldd` on it shows only libc and libgcc_s) and it shares the
  # one lock and the one source tree with the other two. Building it beside them
  # costs one extra link.
  # dj-search-provider (the GNOME Shell search provider) is the same kind of client:
  # hypodj-client + zbus, no hypodj-core, no libmpv.
  cargoBuildFlags = [ "--bin" "dj" "--bin" "dj-gui" "--bin" "dj-mcp" "--bin" "dj-search-provider" ];

  # Test only the client crates (offline, no network). The cc tests are pure - a
  # missing `claude` just means cc_available() is false, never a live call.
  # hypodj-mcp's integration test drives the real binary over a pipe against a
  # loopback stand-in daemon; both are inside the sandbox, so it needs no network.
  doCheck = true;
  cargoTestFlags = [ "-p" "hypodj-client" "-p" "hypodj-cli" "-p" "dj-gui" "-p" "hypodj-mcp" "-p" "hypodj-search" ];

  # libmpv2-sys uses pkg-config to find mpv at build time.
  PKG_CONFIG_PATH = "${mpv-unwrapped.dev}/lib/pkgconfig";

  # libmpv2-sys emits a bare `cargo:rustc-link-lib=mpv` (hard DT_NEEDED libmpv.so.2)
  # with no link-search, so the RPATH is not reliably baked. Wrap both bins so
  # ld.so finds libmpv.so.2 at exec (the clients never call the player, but the
  # hard DT_NEEDED must still resolve).
  # --set-default so an explicit runtime HYPODJ_BUILD_INFO still wins; the baked
  # value is what makes `--version` show the hash on a nix build (no .git here).
  # dj-mcp is deliberately NOT in this loop: it has no libmpv DT_NEEDED to resolve
  # (it never pulls hypodj-core) and no `--version` enrichment to bake, and harn
  # spawns it as a child with a cleared environment - so a shell wrapper would be a
  # process and an indirection bought for nothing.
  postInstall = ''
    for b in dj dj-gui; do
      wrapProgram $out/bin/$b \
        --prefix LD_LIBRARY_PATH : ${lib.makeLibraryPath [ mpv-unwrapped ]} \
        --set-default HYPODJ_BUILD_INFO ${lib.escapeShellArg buildInfo}
    done
    # Generated from the verb tables (route::ROUTED_VERBS + the CLI's own), never
    # hand-written, so a rebuild ships completions that match the router. Run via
    # the wrapper so libmpv.so.2 resolves.
    $out/bin/dj --completions fish > dj.fish
    installShellCompletion --fish dj.fish

    # GNOME Shell search provider registration: the provider .ini (read by the Shell
    # from share/gnome-shell/search-providers), the .desktop it names (the Shell
    # requires an installed app id, and Settings > Search lists it by that app), and
    # the D-Bus session activation file, so the service starts on the first search.
    d=crates/hypodj-search/data
    install -Dm644 $d/tech.familiartools.HypoDJ.SearchProvider.ini \
      $out/share/gnome-shell/search-providers/tech.familiartools.HypoDJ.SearchProvider.ini
    install -Dm644 $d/hypodj.desktop $out/share/applications/hypodj.desktop
    substituteInPlace $out/share/applications/hypodj.desktop \
      --replace-fail "Exec=dj-gui" "Exec=$out/bin/dj-gui"
    install -Dm644 $d/tech.familiartools.HypoDJ.SearchProvider.service \
      $out/share/dbus-1/services/tech.familiartools.HypoDJ.SearchProvider.service
    substituteInPlace $out/share/dbus-1/services/tech.familiartools.HypoDJ.SearchProvider.service \
      --replace-fail "@bindir@" "$out/bin"
  '';

  meta = {
    description = "HypoDJ clients: the dj jukebox CLI, the dj-gui TUI, the dj-mcp agent surface, and the GNOME search provider";
    mainProgram = "dj";
    license = with lib.licenses; [ mit asl20 ];
  };
}
