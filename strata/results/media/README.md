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
