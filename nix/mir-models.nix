# The MTG Essentia models hypodj-mir's learned heads use (voice/instrumental,
# relaxed, danceability on discogs-effnet; DEAM arousal/valence on MusiCNN).
# LICENCE: CC BY-NC-SA 4.0 (essentia.upf.edu/models) - personal, non-commercial
# use only. Fixed-output fetches, so nothing is downloaded unless this is built;
# never part of the default build. See docs/mir.md.
{ fetchurl, linkFarm }:

let
  base = "https://essentia.upf.edu/models";
  m = path: hash: {
    name = "${baseNameOf path}";
    path = fetchurl { url = "${base}/${path}"; inherit hash; };
  };
in
linkFarm "hypodj-mir-models" [
  (m "feature-extractors/discogs-effnet/discogs-effnet-bs64-1.pb" "sha256-PtmvUNU2fAuceVspSwDnWZ5JQyRPTL03aGnzv8h3IbE=")
  (m "classification-heads/voice_instrumental/voice_instrumental-discogs-effnet-1.pb" "sha256-yAM1SOF8KSh0Jl22LoKgUSR3aNEDdaNadp579pXxas8=")
  (m "classification-heads/mood_relaxed/mood_relaxed-discogs-effnet-1.pb" "sha256-LFqmZmtY/oBCmi3Gd6E16ZlZIoibWjma+H0cFfDrtx0=")
  (m "classification-heads/danceability/danceability-discogs-effnet-1.pb" "sha256-4SUfAs3IRkReK8H7P+mWPjJyjFwADgCRiK5HyJY8tMQ=")
  (m "feature-extractors/musicnn/msd-musicnn-1.pb" "sha256-zeoHIrzuf3MShoQ/IjPjqmmIe7XD4tzgEe/1XzjQTz4=")
  (m "classification-heads/deam/deam-msd-musicnn-2.pb" "sha256-vrXusJCSZu63i41rsTI7EIKc8v5V48AaE/oYRvqYs3E=")
]
