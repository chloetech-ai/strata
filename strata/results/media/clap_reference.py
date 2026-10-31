"""CLAP's vectors of Clotho recordings and captions by the `transformers`
library, to compare with the ones Strata makes (`strata-clap-eval
--reference`), and the retrieval they give.

    python clap_reference.py MODEL_DIR AUDIO_DIR CAPTIONS.csv MODE OUT.json [N]

AUDIO_DIR holds the recordings, the first N by file name (all without N).
MODE is how a recording becomes a vector:

- `windows`: as Strata does it. The recording is cut into 10-second
  windows, each window (the last one repeated to fill 10 seconds) gets its
  vector, and the recording's vector is the normalized mean of its
  windows' normalized vectors. Point AUDIO_DIR at the WAV files
  `strata-clap-eval --wav-out` writes (the recordings as Strata hears them,
  at 48 kHz) to compare the models alone.
- `crop`: as `transformers` does it by default (`truncation="rand_trunc"`):
  one 10-second stretch at random (seeded) of a longer recording.

A recording not at 48 kHz is resampled with `scipy.signal.resample_poly`.
Prints text-to-audio and audio-to-text recall at 1, 5 and 10, ranked as
`strata-clap-eval` ranks them. Needs `transformers`, `torch`, `soundfile`,
`scipy` and `numpy`.
"""

import csv
import json
import math
import os
import sys
import time

import numpy as np
import soundfile
import torch
from scipy.signal import resample_poly
from transformers import ClapModel, ClapProcessor

model_dir, audio_dir, captions_csv, mode, out = sys.argv[1:6]
n = int(sys.argv[6]) if len(sys.argv) > 6 else None
assert mode in ("windows", "crop"), mode
rows = sorted((r["file_name"], [r[f"caption_{i}"] for i in range(1, 6)]) for r in csv.DictReader(open(captions_csv, newline="")))
rows = rows[:n] if n else rows

torch.manual_seed(0)
np.random.seed(0)
processor = ClapProcessor.from_pretrained(model_dir)
model = ClapModel.from_pretrained(model_dir).eval()
RATE, WINDOW = 48_000, 480_000


def unit(v):
    return v / np.linalg.norm(v, axis=-1, keepdims=True).clip(1e-12)


def audio_vectors(pieces):
    with torch.no_grad():
        f = processor(audios=pieces, sampling_rate=RATE, return_tensors="pt")
        return unit(model.get_audio_features(**f).numpy())


t0 = time.time()
audio = {}
for file, _ in rows:
    x, rate = soundfile.read(os.path.join(audio_dir, file), dtype="float32", always_2d=True)
    x = x.mean(axis=1)
    if rate != RATE:
        g = math.gcd(RATE, rate)
        x = resample_poly(x, RATE // g, rate // g).astype(np.float32)
    if mode == "windows":
        v = audio_vectors([x[i : i + WINDOW] for i in range(0, len(x), WINDOW)]).sum(axis=0)
    else:
        v = audio_vectors([x])[0]
    audio[file] = unit(v)
audio_s = time.time() - t0
t0 = time.time()
texts = [c for _, cs in rows for c in cs]
text = []
with torch.no_grad():
    for i in range(0, len(texts), 32):
        t = processor(text=texts[i : i + 32], padding=True, return_tensors="pt")
        text.extend(unit(model.get_text_features(**t).numpy()))
text_s = time.time() - t0
print(f"{len(rows)} recordings in {audio_s:.1f} s, {len(texts)} captions in {text_s:.1f} s", file=sys.stderr)

files = [f for f, _ in rows]
clip_of = [f for f, cs in rows for _ in cs]
A, T = np.stack([audio[f] for f in files]), np.stack(text)
scores = T @ A.T
t2a, a2t = [], []
for i, s in enumerate(scores):
    top = [files[j] for j in np.argsort(-s, kind="stable")[:10]]
    t2a.append(top.index(clip_of[i]) if clip_of[i] in top else None)
for j, s in enumerate(scores.T):
    top = [clip_of[i] for i in np.argsort(-s, kind="stable")[:10]]
    a2t.append(top.index(files[j]) if files[j] in top else None)
report = {"mode": mode, "recordings": len(rows), "captions": len(texts), "audio_seconds_to_embed": audio_s}
print(f"{'clotho ' + mode:<16} {'R@1':>7} {'R@5':>7} {'R@10':>7} {'mAP@10':>7}")
for name, ranks in (("text_to_audio", t2a), ("audio_to_text", a2t)):
    r = {f"R@{k}": sum(1 for x in ranks if x is not None and x < k) / len(ranks) for k in (1, 5, 10)}
    r["mAP@10"] = sum(1 / (x + 1) for x in ranks if x is not None) / len(ranks)
    report[name] = r
    print(f"{name:<16} {r['R@1']:>7.4f} {r['R@5']:>7.4f} {r['R@10']:>7.4f} {r['mAP@10']:>7.4f}")
json.dump({"audio": {f: v.tolist() for f, v in audio.items()}, "text": dict(zip(texts, (v.tolist() for v in text))), "report": report}, open(out, "w"))
