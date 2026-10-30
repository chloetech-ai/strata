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
