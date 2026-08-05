{ lib
, rustPlatform
, pkg-config
, mpv-unwrapped
, makeWrapper
# Runtime tools for on-demand now-playing recognition (task f7vnd3i): ffmpeg
# captures a short side-band clip of the stream, songrec (open-source Shazam)
# fingerprints it. Put on the daemon's PATH by the wrapper below so the feature is
# self-contained (no os-configurations change). ffmpeg-headless is the leaner
# closure and is sufficient - only wav capture is used.
, songrec
, ffmpeg-headless
# DISPLAY-only enrichment (from flake.nix): the semver read from Cargo.toml and
# the HYPODJ_BUILD_INFO grammar baked into the runtime env (the nix sandbox has
# no .git, so this is what `hypodj --version` shows).
, buildInfo ? ""
, cargoVersion ? "0.1.0"
}:

# Plain Cargo workspace with a committed Cargo.lock -> buildRustPackage is the
# lower-footprint choice over crane. Only the `hypodj` daemon bin is installed;
# probe/play-probe are dev-only provers.
rustPlatform.buildRustPackage {
  pname = "hypodj";
  version = cargoVersion;

  src = lib.cleanSource ../.;

  cargoLock.lockFile = ../Cargo.lock;

  nativeBuildInputs = [ pkg-config makeWrapper ];
  # mpv.pc (for libmpv2-sys at build) + libmpv.so (DT_NEEDED at runtime).
  buildInputs = [ mpv-unwrapped ];

  # Install only the daemon. probe/play-probe are live-server provers, not
  # something a deployed service needs.
  cargoBuildFlags = [ "--bin" "hypodj" ];

  # Run only the cheap, offline hypodj-core tests (config parse + null-player
  # transitions). Scoping this keeps the check phase from compiling the
  # probe/play-probe bins (reqwest/rustls/aws-lc-rs) under the 2-core cap.
  doCheck = true;
  cargoTestFlags = [ "-p" "hypodj-core" ];

  # ffmpeg/ffprobe for the check phase too, not only the wrapper. The exactness of the
  # 12.000 s songrec sample and the tape's `-c copy` re-cut are ffmpeg behaviours that
  # differ by CODEC (a cut flush against a declared duration is exact on PCM and short on
  # mp3), so the tests that prove them have to be able to run a real one. Already in this
  # derivation's closure via the wrapper below, so it costs no extra build.
  nativeCheckInputs = [ ffmpeg-headless ];

  # libmpv2-sys uses pkg-config to find mpv at build time.
  PKG_CONFIG_PATH = "${mpv-unwrapped.dev}/lib/pkgconfig";

  # LOAD-BEARING, not insurance: libmpv2-sys emits a bare
  # `cargo:rustc-link-lib=mpv` (hard DT_NEEDED libmpv.so.2) with no link-search,
  # so buildRustPackage does not reliably bake the RPATH. Without this wrap the
  # binary fails in ld.so at exec, BEFORE main() - the NullPlayer fallback (which
  # only covers an Mpv::init Err) never runs. LD_LIBRARY_PATH points at the dir
  # that actually contains libmpv.so.2.
  # --set-default so an explicit runtime HYPODJ_BUILD_INFO still wins; the baked
  # value is what makes `--version` show the hash on a nix build (no .git here).
  postInstall = ''
    wrapProgram $out/bin/hypodj \
      --prefix LD_LIBRARY_PATH : ${lib.makeLibraryPath [ mpv-unwrapped ]} \
      --prefix PATH : ${lib.makeBinPath [ songrec ffmpeg-headless ]} \
      --set-default HYPODJ_BUILD_INFO ${lib.escapeShellArg buildInfo}
  '';

  meta = {
    description = "MPD-speaking OpenSubsonic client daemon (the DJ underneath)";
    mainProgram = "hypodj";
    license = with lib.licenses; [ mit asl20 ];
  };
}
