{ lib
, rustPlatform
, pkg-config
, wrapGAppsHook4
, desktop-file-utils
, gtk4
, libadwaita
, buildInfo ? ""
, cargoVersion ? "0.1.0"
}:

# djg - the GNOME-native HypoDJ app (crates/hypodj-gtk). A thin client: hypodj-client
# over MPD/TCP plus GTK4 + libadwaita. It never depends on hypodj-core, so unlike
# clients.nix there is no libmpv here at all - `cargo build --bin djg` compiles only
# its own dependency graph out of the shared lock.
rustPlatform.buildRustPackage {
  pname = "hypodj-gtk";
  version = cargoVersion;

  src = lib.cleanSource ../.;
  cargoLock.lockFile = ../Cargo.lock;

  nativeBuildInputs = [ pkg-config wrapGAppsHook4 desktop-file-utils ];
  buildInputs = [ gtk4 libadwaita ];

  cargoBuildFlags = [ "--bin" "djg" ];
  # The pure view models and the GTK CSS adapter. Neither needs a display.
  doCheck = true;
  cargoTestFlags = [ "-p" "hypodj-gtk" "-p" "hypodj-app" ];

  preFixup = ''
    gappsWrapperArgs+=(--set-default HYPODJ_BUILD_INFO ${lib.escapeShellArg buildInfo})
  '';

  postInstall = ''
    install -Dm644 crates/hypodj-gtk/data/tech.familiartools.HypoDJ.desktop \
      $out/share/applications/tech.familiartools.HypoDJ.desktop
    desktop-file-validate $out/share/applications/tech.familiartools.HypoDJ.desktop
  '';

  meta = {
    description = "djg - HypoDJ as a GNOME app (GTK4 + libadwaita)";
    mainProgram = "djg";
    license = lib.licenses.fsl11Mit;
  };
}
