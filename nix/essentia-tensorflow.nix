# The Python environment hypodj-mir's learned heads run in (tools/mir/mir_tf.py):
# the upstream `essentia-tensorflow` manylinux wheel (AGPL-3.0, bundles its own
# libtensorflow) on the nixpkgs Python it was built for, patched with
# autoPatchelfHook so it runs without nix-ld. It is NOT in nixpkgs, hence this.
#
# Only built when something asks for it (services.hypodj.features.models), so the
# default hypodj build never downloads the ~290 MB wheel. Upstream ships the
# current dev build for cp314 only, so this pins python314.
{ lib, python314, fetchurl, autoPatchelfHook, stdenv }:

let
  py = python314;
  essentia-tensorflow = py.pkgs.buildPythonPackage rec {
    pname = "essentia-tensorflow";
    version = "2.1b6.dev1438";
    format = "wheel";
    src = fetchurl {
      url = "https://files.pythonhosted.org/packages/e1/64/a105f342b01776aafde9f17c6d9011c4f27c6d1b3c9abe4820e2339d7b0f/essentia_tensorflow-${version}-cp314-cp314-manylinux2014_x86_64.manylinux_2_17_x86_64.whl";
      hash = "sha256-QplZobDvGvPcQNVqrIrV2Jma9DyBWaEGYtZLqrYCgx8=";
    };
    nativeBuildInputs = [ autoPatchelfHook ];
    buildInputs = [ stdenv.cc.cc.lib ];
    dependencies = with py.pkgs; [ numpy pyyaml six ];
    pythonImportsCheck = [ "essentia.standard" ];
    meta = {
      description = "Essentia with TensorFlow inference (upstream wheel)";
      homepage = "https://essentia.upf.edu";
      license = lib.licenses.agpl3Only;
      platforms = [ "x86_64-linux" ];
      sourceProvenance = [ lib.sourceTypes.binaryNativeCode ];
    };
  };
in
py.withPackages (_: [ essentia-tensorflow ])
