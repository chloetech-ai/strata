# Image and text retrieval through MEDIA fields

`strata-media-eval` (`strata/src/bin/strata-media-eval.rs`, feature `vision`)
loads a retrieval test split into two tables: the images as a `MEDIA` field
read from their directory, whose embedded `VECTOR` field the database computes
as they load, and the captions with their text vectors from the same model.
Text-to-image retrieval ranks the images by each caption's vector,
image-to-text the captions by each image's, through SQL
(`ORDER BY similarity(emb, :q) DESC LIMIT 10`) with exact (flat) search, so the
numbers are of the vectors, not of an index. Before searching, it checks that
a query by an image file gives the vector its row holds.

Model: `siglip-base-patch16-224` (`google/siglip-base-patch16-224`, 768
dimensions), in candle. Splits: Karpathy's test splits as nlphuji's Hugging
Face datasets give them (`flickr_1k_test_image_text_retrieval`,
`mscoco_2014_5k_test_image_text_retrieval`). Published numbers: open_clip's
`ViT-B-16-SigLIP` (`webli`) row of
[`openclip_retrieval_results.csv`](https://github.com/mlfoundations/open_clip/blob/main/docs/openclip_retrieval_results.csv).

Measured in the cloud container (4 vCPU Xeon 2.10 GHz, 15 GB), 2026-10-30.
Recall does not depend on the machine.

| split | text to image R@1 / R@5 / R@10 | image to text R@1 / R@5 / R@10 |
|---|---|---|
| Flickr30K, 1,000 images, 5,000 captions | 74.66 / 92.30 / 95.66 | 89.20 / 97.90 / 99.30 |
| *published* | *74.68 / 92.30 / 95.62* | *89.10 / 98.00 / 99.30* |
| COCO, 5,000 images, 25,010 captions | 47.85 / 72.39 / 80.92 | 65.72 / 85.58 / 91.44 |
| *published* | *47.78 / 72.44 / 81.00* | *65.74 / 85.42 / 91.26* |

Times (in the JSON files) are upper bounds: other builds ran on the same
machine during both runs. Flickr30K took 486 ms an image (read, decoded,
described and embedded on the CPU) and 79 ms a caption; COCO, with more of the
machine taken, 691 ms and 119 ms.

```text
cargo build --release --features vision --bin strata-media-eval
STRATA_MACHINE="..." ./target/release/strata-media-eval --models ~/models \
    --csv ~/data/flickr1k/test_1k_flickr.csv --images ~/data/flickr1k/images_flickr_1k_test \
    --out results/media/flickr1k-siglip.json
```

## Captions made from the images

With `--captioner blip-image-captioning-base`, the images table also has a
caption of each image, a `TEXT` field the database makes from the `MEDIA`
field as the images load (`"derive": {"from": "image", "model":
"blip-image-captioning-base"}`, BLIP base in candle, greedy, at most 20
tokens). Each direction is then ranked six ways through SQL: by the vectors
alone, by BM25 alone, and by both fused, with `rrf(similarity(emb, :q),
bm25(caption, :t))` and with `w * minmax(similarity(emb, :q)) + (1 - w) *
minmax(bm25(caption, :t))` for w of 0.8, 0.9 and 0.95. Text to image by BM25
matches a test caption against the images' generated captions; image to text
matches an image's generated caption against the test captions.

| Flickr30K, 1,000 images, 5,000 captions | text to image R@1 / R@5 / R@10 | image to text R@1 / R@5 / R@10 |
|---|---|---|
| vectors | 74.66 / 92.30 / 95.66 | 89.20 / 97.90 / 99.30 |
| BM25 over the captions | 14.58 / 29.70 / 38.46 | 21.10 / 42.00 / 52.10 |
| rrf | 33.98 / 58.76 / 69.72 | 52.80 / 77.10 / 88.40 |
| 0.8 vectors + 0.2 BM25 | 68.36 / 90.34 / 94.74 | 82.70 / 97.10 / 99.10 |
| 0.9 + 0.1 | 74.18 / 91.90 / 95.48 | 88.00 / 98.10 / 99.10 |
| 0.95 + 0.05 | 74.72 / 92.34 / 95.66 | 89.00 / 98.30 / 99.30 |

| COCO, the first 1,000 images of the 5K split, 5,001 captions | text to image R@1 / R@5 / R@10 | image to text R@1 / R@5 / R@10 |
|---|---|---|
| vectors | 67.63 / 90.02 / 95.10 | 82.80 / 97.20 / 98.90 |
| BM25 over the captions | 22.66 / 45.43 / 58.07 | 34.50 / 59.60 / 70.50 |
| rrf | 39.87 / 71.33 / 83.64 | 58.40 / 84.50 / 93.40 |
| 0.8 vectors + 0.2 BM25 | 63.65 / 89.08 / 95.20 | 78.10 / 96.80 / 98.90 |
| 0.9 + 0.1 | 67.49 / 90.20 / 95.60 | 83.10 / 97.60 / 99.20 |
| 0.95 + 0.05 | 67.97 / 90.06 / 95.62 | 84.30 / 97.60 / 99.20 |

One short generated caption an image is a weak index next to SigLIP's
vectors: BM25 over it ranks a fifth to two fifths as many first. `rrf`
weighs the two lists alike and so loses a third to over half of the vectors'
R@1. A sum that keeps the vectors at 0.95 is level with them on Flickr30K
(text to image within 0.06, image to text 0.2 lower at R@1 and 0.4 higher at
R@5) and a little better on COCO
(image-to-text R@1 84.3 against 82.8, text-to-image R@1 68.0 against 67.6),
whose training captions BLIP was fine-tuned on. At 0.8 the fused ranking is
worse than the vectors on both. So for finding images by text the captions
add little to an image model's vectors; what they give is text the rest of
the database reads (`MATCH`, phrases, filters, synonyms, a row's
description), at about 2 s an image to make.

The COCO subset is the first 1,000 images of the 5K split in its CSV order
(a full 5K run would caption for about three hours on this machine), so its
numbers are not comparable with the 5K table above. Times: making the
captions took 2,636 ms an image on Flickr30K and 2,253 ms on COCO, with the
image's vector and other builds on the machine; the second Flickr30K run gave
the first run's captions with the rows (`--captions`), so the captioner made
none of them, and the field kept them as a write gives them. The captions are
`flickr1k-blip-captions.json` and `coco1k-blip-captions.json` here.

The captions are the `transformers` library's: on 100 Flickr30K test images
(the first by file name), `transformers` 4.46.3 wrote the same caption as
Strata for 97; the other three part at a word that two choices nearly tie
for (`a man in a red shirt` against `a man wearing a red shirt`).
`blip_reference.py` writes the `transformers` captions (`python blip_reference.py
MODEL_DIR IMAGE_DIR 100`).

```text
STRATA_MACHINE="..." ./target/release/strata-media-eval --models ~/models \
    --csv ~/data/flickr1k/test_1k_flickr.csv --images ~/data/flickr1k/images_flickr_1k_test \
    --captioner blip-image-captioning-base --captions-out flickr1k-blip-captions.json \
    --out results/media/flickr1k-siglip-blip.json
STRATA_MACHINE="..." ./target/release/strata-media-eval --models ~/models \
    --csv ~/data/coco5k/test_5k_mscoco_2014.csv --images ~/data/coco5k/images_mscoco_2014_5k_test \
    --captioner blip-image-captioning-base --limit 1000 --captions-out coco1k-blip-captions.json \
    --out results/media/coco1k-siglip-blip.json
```

## Transcripts of speech

`strata-asr-eval` (`strata/src/bin/strata-asr-eval.rs`, feature `audio`)
loads LibriSpeech utterances into a table whose `transcript` is a `TEXT`
field the database makes from the `MEDIA` field of each FLAC file as the rows
load (`"derive": {"from": "audio", "model": "whisper-small"}`), reads the
transcripts back through SQL and scores them against what was said. Then it
searches for each utterance by what was said (`ORDER BY bm25(transcript, :t)
DESC LIMIT 10`) among the others.

Model: `whisper-small` (`openai/whisper-small`, 244M parameters), in candle:
each 30 seconds of a file (these utterances are all shorter) decoded greedily
without timestamps, in the language Whisper detects, as the `transformers`
library generates by default. Split: 200 utterances of LibriSpeech's
`test-clean`, every 13th by ID, so that all 40 of its speakers are in (25
minutes of speech). Words are compared lower case and without punctuation
(apostrophes inside words kept), which counts `St.` against `saint` and `10`
against `ten` as errors: Whisper's own English normalizer spells those out,
so the 3.4% its authors report for `whisper-small` on all of `test-clean` is
not comparable with the numbers here.

| transcripts | word error rate | utterances with every word right |
|---|---|---|
| Strata | 3.81% | 120 of 200 |
| `transformers` 4.46.3 | 3.81% | 120 of 200 |

The two wrote the same 200 transcripts, character for character.
`whisper_reference.py` writes the `transformers` ones (`python
whisper_reference.py MODEL_DIR SPLIT_DIR 200 OUT.json`), cutting each file as
Strata does. The transcripts are `librispeech200-whisper-small-transcripts.json`
and `librispeech200-transformers-transcripts.json` here.

Found by what was said, `bm25(transcript, :t)`: R@1 99.0, R@5 99.5, R@10
99.5. The two that miss are made of stopwords, which the `english` analyzer
drops: "why should he not be as other men" is second, and "ham to be or not
to be" (transcribed "Him to be or not to be.") is not in the first ten.

Time: 1,077 s for the 200 (1,503 s of speech), 5.4 s an utterance, against
583 s, 2.9 s an utterance, through `transformers` with PyTorch 2.5.1 on the
same machine, both with nothing else running. Each utterance is one window,
and the encoder reads all 30 seconds of a window however short the speech in
it. The encoder takes most of the time (about 3.7 s a window, in batches of
four), and attention 60% of that: candle writes each layer's 1500 by 1500
attention scores to memory, takes their softmax and reads them back, where
`transformers` calls PyTorch's fused `scaled_dot_product_attention`.

```text
cargo build --release --features audio --bin strata-asr-eval
STRATA_MACHINE="..." ./target/release/strata-asr-eval --models ~/models \
    --librispeech ~/data/LibriSpeech/test-clean --limit 200 \
    --reference librispeech200-transformers-transcripts.json \
    --transcripts-out librispeech200-whisper-small-transcripts.json \
    --out results/media/librispeech200-whisper-small.json
```

## Vectors of sound

`strata-clap-eval` (`strata/src/bin/strata-clap-eval.rs`, feature `audio`)
loads Clotho's evaluation split into two tables: the recordings, as a
`MEDIA` field of kind audio with a `VECTOR` field the database computes from
it as the rows load (`"embed": {"from": ["audio"], "model":
"clap-htsat-unfused"}`), and their captions, five each, with their text
vectors from the same model. Text to audio ranks the recordings by each
caption's vector (`ORDER BY similarity(emb, :q) DESC LIMIT 10`, exact
search); audio to text ranks the captions by each recording's, and counts
the first of its five.

Model: `clap-htsat-unfused` (`laion/clap-htsat-unfused`, Apache-2.0), CLAP's
HTSAT audio tower and RoBERTa text tower, in candle. A recording is
resampled to 48 kHz and cut into 10-second windows (a shorter last window is
repeated to fill 10 seconds); its vector is the normalized mean of its
windows' vectors. Split: Clotho's evaluation split, 1,045 recordings of 15 to
30 seconds (6.5 hours, 2,767 windows) with 5,225 captions. Clotho is by the
Audio Research Group of Tampere University (Drossos, Lipping and Virtanen,
"Clotho: an Audio Captioning Dataset", ICASSP 2020); its captions are for
experimental and non-commercial use, and neither they nor the recordings
are in this repository.

| vectors of the recordings | text to audio R@1 | R@5 | R@10 | audio to text R@1 | R@5 | R@10 |
|---|---|---|---|---|---|---|
| Strata, the mean of 10-second windows | 15.9 | 40.1 | 53.6 | 23.2 | 48.6 | 61.7 |
| `transformers` 4.46.3, the same windows | 15.9 | 40.0 | 53.5 | 23.3 | 48.3 | 61.9 |
| `transformers`' default, one 10 seconds at random | 15.8 | 37.8 | 51.2 | 21.7 | 46.4 | 59.8 |

The vectors are `transformers`' own: `strata-clap-eval --wav-out` wrote the
first 20 recordings as Strata hears them (one channel at 48 kHz), and
`clap_reference.py` embedded those samples and the 100 captions through
`ClapModel`; every one of the 120 vectors had a cosine of 1.000000 with
Strata's. On the whole split `transformers` read the 44.1 kHz files
resampled by `scipy.signal.resample_poly`, so its recall differs from
Strata's in the third digit. Its default (`truncation="rand_trunc"`, seeded)
reads one 10 seconds of each recording at random; the mean over all of it is
0.1 to 1.5 points better at R@1 and 1.9 to 2.4 at R@5 and R@10.

Time, on the same machine with nothing else running: Strata read, decoded,
resampled and embedded the 1,045 recordings in 743 s (0.27 s a window) and
the captions in 108 s; `transformers` with PyTorch 2.5.1 took 386 s and 58 s
for the same windows and captions. The 6,270 searches took 0.6 s.

`clotho-clap.json` is Strata's run and `clotho-transformers.json` the two
`transformers` runs (numbers only).

```text
cargo build --release --features audio --bin strata-clap-eval
STRATA_MACHINE="..." ./target/release/strata-clap-eval --models ~/models \
    --audio ~/data/clotho/audio/evaluation --captions ~/data/clotho/clotho_captions_evaluation.csv \
    --out results/media/clotho-clap.json
# The check against transformers, on the first 20 recordings:
./target/release/strata-clap-eval --models ~/models --audio ... --captions ... --limit 20 --wav-out /tmp/clap-wav
python clap_reference.py ~/models/clap-htsat-unfused /tmp/clap-wav ~/data/clotho/clotho_captions_evaluation.csv windows ref20.json 20
./target/release/strata-clap-eval --models ~/models --audio ... --captions ... --limit 20 --reference ref20.json
# transformers on the whole split:
python clap_reference.py ~/models/clap-htsat-unfused ~/data/clotho/audio/evaluation ~/data/clotho/clotho_captions_evaluation.csv windows hf-windows.json
python clap_reference.py ~/models/clap-htsat-unfused ~/data/clotho/audio/evaluation ~/data/clotho/clotho_captions_evaluation.csv crop hf-crop.json
```
