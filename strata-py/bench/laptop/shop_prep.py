"""Prepares the Shop dataset of the laptop benchmark's Part A from Amazon
Reviews 2023 (McAuley Lab): one category's product metadata and reviews,
cleaned into the rows every system loads, and a bge-small-en-v1.5 vector
(384 dimensions) per product.

    python shop_prep.py --meta meta_Video_Games.jsonl --reviews Video_Games.jsonl --out SHOP_DIR

Downloads (https://huggingface.co/datasets/McAuley-Lab/Amazon-Reviews-2023):
raw/meta_categories/meta_Video_Games.jsonl (437 MB) and
raw/review_categories/Video_Games.jsonl (2.7 GB).

SHOP_DIR gets:

* products.jsonl: id (the parent ASIN), title, brand, platform, category,
  price (null when unlisted), rating, rating_count, stock (made up, seeded:
  the data has none), year (first available), rated, description, attrs
  (the details as a JSON object, its keys left as Amazon wrote them),
  bought_together, image;
* reviews.jsonl: id, user_id, product_id, rating, title, text, ts
  (milliseconds), helpful, verified; only reviews of kept products, in the
  file's order;
* product_vecs.f32: the products' vectors in products.jsonl's order,
  float32, normalised; embedded from the title and features;
* meta.json: counts, dimension, model, sources.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import re
import sys
import time
from pathlib import Path

import numpy as np

MODEL = "BAAI/bge-small-en-v1.5"
YEAR = re.compile(r"(19[7-9]\d|20[0-2]\d)")


def price_of(p) -> float | None:
    if p is None:
        return None
    try:
        v = float(str(p).replace("$", "").replace(",", ""))
    except ValueError:
        return None
    return v if v > 0 else None


def stock_of(key: str) -> int:
    """A made-up stock level, the same on every run: a tenth are out of stock."""
    h = int.from_bytes(hashlib.blake2b(key.encode(), digest_size=8).digest(), "little")
    return 0 if h % 10 == 0 else 1 + (h >> 8) % 200


def product_of(m: dict) -> dict | None:
    title = (m.get("title") or "").strip()
    pid = m.get("parent_asin")
    if not title or not pid:
        return None
    cats = m.get("categories") or []
    details = dict(m.get("details") or {})
    details.pop("Pricing", None)
    # 115 Video Games products have a detail with an empty name; Strata's
    # JSON columns refuse an empty key, so every system gets them without.
    details = {k: v for k, v in details.items() if k.strip()}
    year = None
    if (d := details.get("Date First Available")) and (y := YEAR.search(str(d))):
        year = int(y.group(1))
    platform = cats[1] if len(cats) > 1 else ""
    image = next((i.get("large") for i in m.get("images") or [] if i.get("large")), None)
    return {
        "id": pid,
        "title": title,
        "brand": (m.get("store") or details.get("Manufacturer") or "").strip() or None,
        "platform": platform or None,
        "category": (cats[2] if len(cats) > 2 else platform) or None,
        "price": price_of(m.get("price")),
        "rating": float(m.get("average_rating") or 0.0),
        "rating_count": int(m.get("rating_number") or 0),
        "stock": stock_of(pid),
        "year": year,
        "rated": details.get("Rated"),
        "description": " ".join([*(m.get("features") or []), *(m.get("description") or [])]).strip(),
        "attrs": details,
        "bought_together": m.get("bought_together") or [],
        "image": image,
    }


def embed_text(p: dict) -> str:
    # About 100 tokens: a product's title and the start of its features.
    # bge-small reads at most 512, and a full description costs 5 times
    # as much to embed for little gain on titles this specific.
    return (p["title"] + ". " + p["description"])[:500]


def embed_all(texts: list[str], how: str, threads: int) -> np.ndarray:
    t0 = time.perf_counter()
    if how == "fastembed":
        from fastembed import TextEmbedding

        m = TextEmbedding(MODEL, threads=threads)
        out = np.zeros((len(texts), 384), dtype=np.float32)
        for i, v in enumerate(m.embed(texts, batch_size=128)):
            out[i] = v
            if i % 10000 == 0:
                print(f"embedded {i} of {len(texts)} in {time.perf_counter() - t0:.0f} s", file=sys.stderr)
    elif how == "strata":
        # Strata's own model (strata-py built with the `bert` or `accelerate` feature).
        import tempfile
        from concurrent.futures import ThreadPoolExecutor

        import strata

        d = tempfile.mkdtemp()
        db = strata.open(f"{d}/e.strata", schema={"table": "t", "key": "id", "fields": [{"name": "id", "type": "keyword"}, {"name": "x", "type": "text"}, {"name": "v", "type": "vector", "embed": {"model": "bge-small-en-v1.5", "from": ["x"]}}]})
        with ThreadPoolExecutor(threads) as ex:
            out = np.array(list(ex.map(lambda t: db.embed(t, "v"), texts, chunksize=64)), dtype=np.float32)
        db.close()
    else:  # "random": for checking the harness only
        rng = np.random.default_rng(0)
        out = rng.standard_normal((len(texts), 384)).astype(np.float32)
    out /= np.maximum(np.linalg.norm(out, axis=1, keepdims=True), 1e-12)
    print(f"embedded {len(texts)} in {time.perf_counter() - t0:.0f} s", file=sys.stderr)
    return out


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--meta", required=True)
    ap.add_argument("--reviews", required=True)
    ap.add_argument("--out", required=True)
    ap.add_argument("--limit-products", type=int, default=None, help="keep the first N products (to check the harness)")
    ap.add_argument("--embed", choices=["fastembed", "strata", "random"], default="fastembed")
    ap.add_argument("--threads", type=int, default=4)
    a = ap.parse_args()
    out = Path(a.out)
    out.mkdir(parents=True, exist_ok=True)

    products, seen = [], set()
    with open(a.meta) as f:
        for line in f:
            p = product_of(json.loads(line))
            if p is None or p["id"] in seen:
                continue
            seen.add(p["id"])
            products.append(p)
            if a.limit_products and len(products) >= a.limit_products:
                break
    with open(out / "products.jsonl", "w") as f:
        for p in products:
            f.write(json.dumps(p) + "\n")
    print(f"{len(products)} products", file=sys.stderr)

    n_reviews, users, bad = 0, set(), 0
    with open(a.reviews) as f, open(out / "reviews.jsonl", "w") as w:
        for line in f:
            try:
                r = json.loads(line)
            except json.JSONDecodeError:
                bad += 1  # a cut last line of a partial download
                continue
            if r.get("parent_asin") not in seen or not r.get("user_id"):
                continue
            w.write(
                json.dumps(
                    {
                        "id": f"r{n_reviews}",
                        "user_id": r["user_id"],
                        "product_id": r["parent_asin"],
                        "rating": int(r.get("rating") or 0),
                        "title": r.get("title") or "",
                        "text": r.get("text") or "",
                        "ts": int(r.get("timestamp") or 0),
                        "helpful": int(r.get("helpful_vote") or 0),
                        "verified": bool(r.get("verified_purchase")),
                    }
                )
                + "\n"
            )
            users.add(r["user_id"])
            n_reviews += 1
    print(f"{n_reviews} reviews by {len(users)} users ({bad} lines unreadable)", file=sys.stderr)

    vecs = embed_all([embed_text(p) for p in products], a.embed, a.threads)
    vecs.tofile(out / "product_vecs.f32")
    (out / "meta.json").write_text(
        json.dumps(
            {
                "products": len(products),
                "reviews": n_reviews,
                "users": len(users),
                "dim": int(vecs.shape[1]),
                "model": MODEL if a.embed != "random" else "random (harness check only)",
                "embedded_from": "title + '. ' + features and description, first 500 characters",
                "sources": {"meta": Path(a.meta).name, "reviews": Path(a.reviews).name},
            },
            indent=1,
        )
    )


if __name__ == "__main__":
    main()
