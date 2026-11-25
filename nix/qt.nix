{ lib
, rustPlatform
, pkg-config
, qt6
, desktop-file-utils
, libGL
, buildInfo ? ""
, cargoVersion ? "0.1.0"
}:

# djq - the Qt 6 / QML HypoDJ app (crates/hypodj-qt, cxx-qt). A thin client:
# hypodj-app over hypodj-client plus Qt. It never depends on hypodj-core, so there is
# no libmpv here - `cargo build --bin djq` compiles only its own dependency graph.
let
  # cxx-qt-build locates Qt through a single `qmake -query` prefix
  # (QT_INSTALL_HEADERS / LIBS / LIBEXECS). nixpkgs splits qtbase and qtdeclarative
  # into separate store paths, so qtbase's own qmake cannot see QtQml / QtQuick or
  # qmlcachegen; a qt6.env joins them under one prefix.
  qtEnv = qt6.env "hypodj-qt" [ qt6.qtdeclarative ];
in
rustPlatform.buildRustPackage {
  pname = "hypodj-qt";
  version = cargoVersion;

  src = lib.cleanSource ../.;
  cargoLock.lockFile = ../Cargo.lock;

  nativeBuildInputs = [ pkg-config qt6.wrapQtAppsHook desktop-file-utils ];
  # qtwayland so a Wayland session runs natively; the controls style comes from the
  # host's platform theme at run time (the room is the OS).
  buildInputs = [ qt6.qtbase qt6.qtdeclarative qt6.qtwayland libGL ];

  # The qtbase setup hook exports its own QMAKE, overriding any derivation attr,
  # so the joined prefix is set again right before the cargo build.
  preConfigure = ''
    export QMAKE=${qtEnv}/bin/qmake
  '';

  cargoBuildFlags = [ "--bin" "djq" ];
  # The bridge's word folds and the shared view models / instrument adapter. None
  # needs a display.
  doCheck = true;
  cargoTestFlags = [ "-p" "hypodj-qt" "-p" "hypodj-app" ];
  preCheck = ''
    export QMAKE=${qtEnv}/bin/qmake
  '';

  # buildInfo holds spaces, so it goes in as one shell-quoted word.
  preFixup = ''
    qtWrapperArgs+=(--set-default HYPODJ_BUILD_INFO ${lib.escapeShellArg buildInfo})
  '';

  postInstall = ''
    install -Dm644 crates/hypodj-qt/data/tech.familiartools.HypoDJ.Qt.desktop \
      $out/share/applications/tech.familiartools.HypoDJ.Qt.desktop
    desktop-file-validate $out/share/applications/tech.familiartools.HypoDJ.Qt.desktop
  '';

  meta = {
    description = "djq - HypoDJ as a Qt 6 / QML app";
    mainProgram = "djq";
    license = lib.licenses.fsl11Mit;
  };
}
