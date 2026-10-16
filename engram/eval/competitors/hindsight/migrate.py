#!/usr/bin/env python3
"""Moves every loaded Hindsight bank to the instance that answers the questions.

The banks are loaded against a server whose embeddings come from hash_embed.py
(this container had no copy of Hindsight's embedding model while loading).
Each bank is then exported from that database (the archive holds no
embeddings) and imported into a second database whose server embeds with
bge-small-en-v1.5. Import replays the retain pipeline after extraction: it
re-embeds every fact, resolves entities again and rebuilds the links, with
no model call.

  python3 migrate.py [--export-only] WORKDIR...

WORKDIR/hindsight/<bank>.ingested marks a loaded bank (competitor_contexts.py
writes it); <bank>.imported marks one already moved. Environment:
HS_SRC_DB and HS_DST_DB (Postgres URLs), HS_DST_TEI (the embeddings server),
HS_ADMIN (hindsight-admin), HS_XFER (where archives are kept).
"""
import glob
import os
import subprocess
import sys
import time

ADMIN = os.environ.get("HS_ADMIN", "/home/claude/ext/hs-venv/bin/hindsight-admin")
BASE = {"PATH": os.environ["PATH"], "HOME": os.environ.get("HOME", "/tmp"),
        "HINDSIGHT_API_EMBEDDINGS_PROVIDER": "tei", "HINDSIGHT_API_RERANKER_PROVIDER": "rrf",
        "HINDSIGHT_API_LLM_PROVIDER": "claude-code", "HINDSIGHT_API_LLM_MODEL": "haiku"}
SRC = dict(BASE, HINDSIGHT_API_DATABASE_URL=os.environ.get("HS_SRC_DB", "postgresql://postgres:postgres@127.0.0.1:5433/postgres"),
           HINDSIGHT_API_EMBEDDINGS_TEI_URL="http://127.0.0.1:8080")
DST = dict(BASE, HINDSIGHT_API_DATABASE_URL=os.environ.get("HS_DST_DB", "postgresql://postgres:postgres@127.0.0.1:5433/hsfinal"),
           HINDSIGHT_API_EMBEDDINGS_TEI_URL=os.environ.get("HS_DST_TEI", "http://127.0.0.1:8082"))
XFER = os.environ.get("HS_XFER", "/home/claude/bench/hs/xfer")


def main():
    os.makedirs(XFER, exist_ok=True)
    export_only = "--export-only" in sys.argv
    n = 0
    for work in [a for a in sys.argv[1:] if not a.startswith("--")]:
        for marker in sorted(glob.glob(os.path.join(work, "hindsight", "*.ingested"))):
            bank = os.path.basename(marker)[: -len(".ingested")]
            done = marker[: -len(".ingested")] + ".imported"
            if os.path.exists(done):
                continue
            t0 = time.time()
            z = os.path.join(XFER, bank + ".zip")
            if not (os.path.exists(z) and os.path.getsize(z) > 0):
                subprocess.run([ADMIN, "export-bank", "--bank", bank, "--output", z + ".part"], env=SRC, check=True, capture_output=True)
                os.replace(z + ".part", z)
            if export_only:
                n += 1
                continue
            r = subprocess.run([ADMIN, "import-bank", "--archive", z, "--target-bank", bank], env=DST, capture_output=True, text=True)
            if r.returncode != 0:
                print(f"{bank}: import failed: {r.stdout[-500:]} {r.stderr[-500:]}", flush=True)
                continue
            line = [x for x in r.stdout.splitlines() if x.startswith("Imported bank")]
            open(done, "w").write((line[-1] if line else "imported") + "\n")
            n += 1
            print(f"{time.strftime('%H:%M:%S')} {bank}: {line[-1] if line else 'imported'} in {time.time() - t0:.0f} s", flush=True)
    print(f"migrated {n} banks", flush=True)


if __name__ == "__main__":
    main()
