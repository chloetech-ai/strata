"""Whisper's transcripts of LibriSpeech utterances by the `transformers`
library, to compare with the ones Strata makes (`strata-asr-eval
--reference`).

    python whisper_reference.py MODEL_DIR SPLIT_DIR N OUT.json

The utterances are N spread evenly over the split by ID, as
`strata-asr-eval` takes them. Each
file is cut into 30-second windows as Strata cuts it (a last window shorter
than a tenth of a second is dropped), each window transcribed by
`generate` with the defaults: greedy, the language detected, no
timestamps. A file's windows' texts are joined by spaces. Needs
`transformers`, `torch` and `soundfile`.
"""

import json
import os
import sys
import time

import soundfile
import torch
from transformers import WhisperForConditionalGeneration, WhisperProcessor

model_dir, split, n, out = sys.argv[1], sys.argv[2], int(sys.argv[3]), sys.argv[4]
utterances = []
for root, _, files in os.walk(split):
    for f in files:
        if f.endswith(".trans.txt"):
            for line in open(os.path.join(root, f)):
                uid = line.split(" ", 1)[0]
                utterances.append((uid, os.path.join(root, uid + ".flac")))
utterances = sorted(utterances)
utterances = utterances[:: max(1, len(utterances) // n)][:n]

processor = WhisperProcessor.from_pretrained(model_dir)
model = WhisperForConditionalGeneration.from_pretrained(model_dir).eval()
WINDOW, BATCH = 480_000, 4
windows = []
for uid, path in utterances:
    x, rate = soundfile.read(path, dtype="float32")
    assert rate == 16_000, path
    windows += [(uid, x[i : i + WINDOW]) for i in range(0, len(x), WINDOW) if len(x[i : i + WINDOW]) >= 1_600]

texts = {}
t0 = time.time()
with torch.no_grad():
    for b in range(0, len(windows), BATCH):
        batch = windows[b : b + BATCH]
        features = processor.feature_extractor([w for _, w in batch], sampling_rate=16_000, return_tensors="pt").input_features
        ids = model.generate(features, task="transcribe")
        for (uid, _), t in zip(batch, processor.batch_decode(ids, skip_special_tokens=True)):
            if t.strip():
                texts.setdefault(uid, []).append(t.strip())
print(f"{len(utterances)} utterances, {len(windows)} windows in {time.time() - t0:.1f} s", file=sys.stderr)
json.dump({uid: " ".join(texts.get(uid, [])) for uid, _ in utterances}, open(out, "w"), indent=2, sort_keys=True)
