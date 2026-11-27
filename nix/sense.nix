{ lib
, rustPlatform
, makeWrapper
, ffmpeg-headless
, model
, cargoVersion ? "0.1.0"
}:

# dj-sense: does the head move with the beat? Reads a video FILE (ffmpeg decodes
# it), finds the face with rustface, scores tempo lock. Pure Rust plus ffmpeg at
# runtime; never links libmpv, never opens a device.
rustPlatform.buildRustPackage {
  pname = "dj-sense";
  version = cargoVersion;

  src = lib.cleanSource ../.;
  cargoLock.lockFile = ../Cargo.lock;

  nativeBuildInputs = [ makeWrapper ];
  cargoBuildFlags = [ "--bin" "dj-sense" ];

  # The estimator table plus the end-to-end synthetic-video test, which needs
  # ffmpeg and the model - both provided here, so the sandbox runs it rather than
  # skipping.
  doCheck = true;
  cargoTestFlags = [ "-p" "hypodj-sense" ];
  nativeCheckInputs = [ ffmpeg-headless ];
  DJ_SENSE_MODEL = "${model}";

  postInstall = ''
    wrapProgram $out/bin/dj-sense \
      --set-default DJ_SENSE_MODEL ${model} \
      --set-default DJ_SENSE_FFMPEG ${lib.getExe ffmpeg-headless}
  '';

  meta = {
    description = "dj-sense: head tempo-lock from a video file (HypoDJ sensing, slice 1)";
    mainProgram = "dj-sense";
    license = lib.licenses.fsl11Mit;
  };
}
