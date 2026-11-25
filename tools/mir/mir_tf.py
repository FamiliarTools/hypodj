"""hypodj-mir TensorFlow heads helper.

usage: python3 -I mir_tf.py <models-dir> <audio-file>

Prints ONE JSON line: voice, relaxed, danceable (probabilities 0..1), arousal and
valence (DEAM, 1..9), the mean-pooled discogs-effnet embedding, and the model
stems used. Needs the `essentia-tensorflow` wheel and the MTG model files
(CC BY-NC-SA 4.0, fetched by the user, never shipped; see docs/mir.md).
Run by `hypodj-mir`, which owns validity, headroom and the atomic write.
"""
import json
import os
import sys

os.environ.setdefault("TF_CPP_MIN_LOG_LEVEL", "3")

# TensorFlow sizes its thread pools from the CPU affinity mask, and unbounded it
# takes every core (measured: 530% CPU on 8). HYPODJ_MIR_CPUS narrows the mask
# BEFORE TensorFlow loads, so the worker uses exactly the cores it was given.
_cpus = int(os.environ.get("HYPODJ_MIR_CPUS", "0") or 0)
if _cpus > 0:
    _mine = sorted(os.sched_getaffinity(0))
    _off = int(os.environ.get("HYPODJ_MIR_CPU_OFFSET", "0") or 0) % len(_mine)
    os.sched_setaffinity(0, (_mine[_off:] + _mine[:_off])[:_cpus])

from essentia import log  # noqa: E402

log.infoActive = False
log.warningActive = False

from essentia.standard import (  # noqa: E402
    MonoLoader,
    TensorflowPredict2D,
    TensorflowPredictEffnetDiscogs,
    TensorflowPredictMusiCNN,
)

EFFNET = "discogs-effnet-bs64-1"
MUSICNN = "msd-musicnn-1"
HEADS = {
    # name -> (model stem, index of the class we report)
    "voice": ("voice_instrumental-discogs-effnet-1", 1),  # [instrumental, voice]
    "relaxed": ("mood_relaxed-discogs-effnet-1", 1),  # [non_relaxed, relaxed]
    "danceable": ("danceability-discogs-effnet-1", 0),  # [danceable, not_danceable]
}
DEAM = "deam-msd-musicnn-2"  # [valence, arousal]


def mean(rows):
    n = len(rows)
    return [sum(c) / n for c in zip(*rows)] if n else []


def main():
    models, path = sys.argv[1], sys.argv[2]
    pb = lambda stem: os.path.join(models, stem + ".pb")  # noqa: E731
    audio = MonoLoader(filename=path, sampleRate=16000, resampleQuality=4)()
    out = {"models": []}
    if os.path.exists(pb(EFFNET)):
        emb = TensorflowPredictEffnetDiscogs(
            graphFilename=pb(EFFNET), output="PartitionedCall:1"
        )(audio)
        out["models"].append(EFFNET)
        out["embedding"] = [round(float(x), 5) for x in mean(emb.tolist())]
        for name, (stem, idx) in HEADS.items():
            if os.path.exists(pb(stem)):
                p = TensorflowPredict2D(graphFilename=pb(stem), output="model/Softmax")(emb)
                out[name] = round(float(mean(p.tolist())[idx]), 4)
                out["models"].append(stem)
    if os.path.exists(pb(MUSICNN)) and os.path.exists(pb(DEAM)):
        emb = TensorflowPredictMusiCNN(graphFilename=pb(MUSICNN), output="model/dense/BiasAdd")(audio)
        va = TensorflowPredict2D(graphFilename=pb(DEAM), output="model/Identity")(emb)
        v, a = mean(va.tolist())
        out["valence"], out["arousal"] = round(float(v), 4), round(float(a), 4)
        out["models"] += [MUSICNN, DEAM]
    out["models"].sort()
    print(json.dumps(out, separators=(",", ":")))


if __name__ == "__main__":
    main()
