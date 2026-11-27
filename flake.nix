{
  description = "hypodj - MPD-speaking OpenSubsonic client daemon";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = { self, nixpkgs, flake-utils }:
    let
      # ONE shared module definition, parameterized by flavor. Lives outside the
      # per-system block so the modules evaluate system-independently.
      mkHypodjModule = import ./nix/hypodj-module.nix;
    in
    flake-utils.lib.eachDefaultSystem (system:
      let
        # hypodj is FSL-1.1-MIT (unfree in nixpkgs terms until each release turns MIT),
        # so the flake's own package set allows exactly its own two packages.
        pkgs = import nixpkgs {
          inherit system;
          config.allowUnfreePredicate = p:
            builtins.elem (nixpkgs.lib.getName p) [ "hypodj" "hypodj-clients" "hypodj-gtk" "hypodj-qt" "dj-sense" ];
        };

        # The released semver, read from the workspace manifest so the derivation
        # version tracks the tag and is bumped in exactly one place at release.
        cargoVersion = (builtins.fromTOML (builtins.readFile ./Cargo.toml)).workspace.package.version;

        # DISPLAY-only build info baked into the runtime env of the wrapped bins,
        # so a nix-built binary (no .git in the sandbox) still shows its short
        # hash via `--version`. Mirrors the HYPODJ_BUILD_INFO grammar emitted by
        # the build.rs on source builds: `count=<N> hash=<short> dirty=<0|1>`.
        #
        # - biHash: bare short commit hash. self.dirtyShortRev carries a trailing
        #   "-dirty" suffix, so strip it - the dirty state is carried by biDirty.
        #   Parens are load-bearing: `or` binds looser than function application.
        # - biDirty: a clean tree exposes self.rev; a dirty one does not.
        # - anchor: optional released-commit revCount (nix/release-anchor.nix); it
        #   lets count= be commits-since-release. Omitted (no count=) when absent.
        anchor = if builtins.pathExists ./nix/release-anchor.nix then import ./nix/release-anchor.nix else null;
        biHash = self.shortRev or (nixpkgs.lib.removeSuffix "-dirty" (self.dirtyShortRev or ""));
        biDirty = !(self ? rev);
        biCount = if (self ? revCount) && anchor != null then self.revCount - anchor.revCount else null;
        buildInfo =
          if biHash == "" then
            ""
          else
            nixpkgs.lib.concatStringsSep " " (
              (nixpkgs.lib.optional (biCount != null && biCount >= 0) "count=${toString biCount}")
              ++ [ "hash=${biHash}" "dirty=${if biDirty then "1" else "0"}" ]
            );
        # The Qt prefix djq builds against (one qmake that sees qtdeclarative).
        qtEnv = pkgs.qt6.env "hypodj-qt" [ pkgs.qt6.qtdeclarative ];
      in
      {
        packages.hypodj = pkgs.callPackage ./nix/package.nix { inherit buildInfo cargoVersion; };
        # The client bins (djc + dj + dj-gui) - libmpv-free, so a separate, lighter
        # derivation that a workstation can install without pulling mpv.
        packages.hypodj-clients = pkgs.callPackage ./nix/clients.nix { inherit buildInfo cargoVersion; };
        # djg, the GNOME app: GTK4 + libadwaita over hypodj-client, no libmpv.
        packages.hypodj-gtk = pkgs.callPackage ./nix/gtk.nix { inherit buildInfo cargoVersion; };
        # djq, the Qt app: Qt 6 / QML (cxx-qt) over hypodj-client, no libmpv.
        packages.hypodj-qt = pkgs.callPackage ./nix/qt.nix { inherit buildInfo cargoVersion; };
        # The default local NL model for an `llm-llama` daemon (HYPODJ_NL_MODEL):
        # Qwen2.5-1.5B-Instruct Q4_K_M, the official Qwen GGUF, Apache-2.0, ~1.1 GB.
        # A fixed-output fetch, so the file is only downloaded when this is built.
        packages.nl-model = pkgs.fetchurl {
          name = "qwen2.5-1.5b-instruct-q4_k_m.gguf";
          url = "https://huggingface.co/Qwen/Qwen2.5-1.5B-Instruct-GGUF/resolve/main/qwen2.5-1.5b-instruct-q4_k_m.gguf";
          hash = "sha256-ahouttFWIr88loVyBjUbqX4a8Www16dO44lw5DTpQH4=";
        };
        # The MTG Essentia models (CC BY-NC-SA 4.0, personal use only); see
        # nix/mir-models.nix and docs/mir.md. Never part of the default build.
        packages.mir-models = pkgs.callPackage ./nix/mir-models.nix { };
        # The essentia-tensorflow Python env for the learned heads (upstream wheel,
        # ~290 MB, only fetched when built). See nix/essentia-tensorflow.nix.
        packages.mir-tf-python = pkgs.callPackage ./nix/essentia-tensorflow.nix { };
        # dj-sense's face model: SeetaFace frontal detector v1.0 as shipped by
        # rustface (BSD-2-Clause), 1.2 MB. Fixed-output fetch, pinned commit.
        packages.dj-sense-model = pkgs.fetchurl {
          name = "seeta_fd_frontal_v1.0.bin";
          url = "https://raw.githubusercontent.com/atomashpolskiy/rustface/fa3d5b5d576c91d1c8a099af381e0f0906a0d3a3/model/seeta_fd_frontal_v1.0.bin";
          hash = "sha256-xGGdBm7TXoTZqOhChgsN/1Z6ugy7E5iBB1U4dh2z/10=";
        };
        # dj-sense: head tempo-lock from a video file (sensing slice 1). No libmpv.
        packages.dj-sense = pkgs.callPackage ./nix/sense.nix {
          inherit cargoVersion;
          model = self.packages.${system}.dj-sense-model;
        };
        packages.default = self.packages.${system}.hypodj;

        apps.hypodj = {
          type = "app";
          program = "${self.packages.${system}.hypodj}/bin/hypodj";
        };
        apps.djc = {
          type = "app";
          program = "${self.packages.${system}.hypodj-clients}/bin/djc";
        };
        apps.dj = {
          type = "app";
          program = "${self.packages.${system}.hypodj-clients}/bin/dj";
        };
        apps.dj-gui = {
          type = "app";
          program = "${self.packages.${system}.hypodj-clients}/bin/dj-gui";
        };
        apps.djg = {
          type = "app";
          program = "${self.packages.${system}.hypodj-gtk}/bin/djg";
        };
        apps.djq = {
          type = "app";
          program = "${self.packages.${system}.hypodj-qt}/bin/djq";
        };
        apps.dj-sense = {
          type = "app";
          program = "${self.packages.${system}.dj-sense}/bin/dj-sense";
        };
        apps.default = self.apps.${system}.dj;

        devShells.default = pkgs.mkShell {
          # Rust toolchain + pkg-config + the audio system lib (libmpv).
          # Reproducible: the build finds mpv via pkg-config here.
          nativeBuildInputs = [
            pkgs.rustc
            pkgs.cargo
            pkgs.pkg-config
            pkgs.rust-analyzer
            pkgs.clippy
            # The daemon's own runtime tools (see nix/package.nix). Not a convenience:
            # the exactness of the songrec sample and the tape's re-cut are ffmpeg
            # behaviours, and a test that cannot run them proves nothing about the
            # formats his stations actually serve. Same derivation the wrapper puts on
            # the daemon's PATH, so the devshell and production agree.
            pkgs.ffmpeg-headless
            # The opt-in local model runtime (hypodj-nl feature `llm-llama`):
            # llama-cpp-sys-2 compiles llama.cpp with cmake and generates its FFI
            # with bindgen. bindgenHook exports LIBCLANG_PATH plus the libc/C++
            # include flags bindgen needs. Build-time only: the default build and
            # both packages never compile llama.cpp.
            pkgs.cmake
            pkgs.rustPlatform.bindgenHook
            # djq (crates/hypodj-qt): cxx-qt-build finds Qt through ONE `qmake -query`
            # prefix, so qmake comes from a qt6.env holding qtdeclarative next to
            # qtbase (nixpkgs splits them). See nix/qt.nix.
            qtEnv
          ];
          buildInputs = [
            # provides libmpv + mpv.pc for libmpv2 (Phase 1); the foundation is
            # link-light and does not yet depend on it, but the devshell ships it
            # so adding the dep in Phase 1 is a one-line change that will link.
            pkgs.mpv-unwrapped
            # djg (crates/hypodj-gtk): GTK4 + libadwaita, found through pkg-config.
            pkgs.gtk4
            pkgs.libadwaita
            # djq: QtQuick links libGLX / libOpenGL (glvnd), which qt6.env does not carry.
            pkgs.libGL
            # NOTE: no openssl. opensubsonic 0.3 pulls reqwest 0.13 with
            # default-features=false + "rustls" (verified in its Cargo.toml), so
            # TLS is aws-lc-rs/rustls, not OpenSSL. Adding openssl here would be a
            # devshell input that nothing actually links against.
          ];
          # Honor the hard build cap: never saturate the machine.
          CARGO_BUILD_JOBS = "2";
          PKG_CONFIG_PATH = "${pkgs.mpv-unwrapped.dev}/lib/pkgconfig";
          QMAKE = "${qtEnv}/bin/qmake";
          # dj-sense finds its face model here (tests skip without it).
          DJ_SENSE_MODEL = "${self.packages.${system}.dj-sense-model}";
          # A devshell-built djq finds its QML imports and platform plugins here
          # (the package gets them from wrapQtAppsHook instead).
          QML_IMPORT_PATH = "${qtEnv}/lib/qt-6/qml";
          QT_PLUGIN_PATH = "${qtEnv}/lib/qt-6/plugins";
        };
      })
    // {
      # System-independent outputs: the two modules + the overlay.
      overlays.default = import ./nix/overlay.nix;

      nixosModules.default = mkHypodjModule { flavor = "nixos"; };
      homeManagerModules.default = mkHypodjModule { flavor = "home-manager"; };
    };
}
