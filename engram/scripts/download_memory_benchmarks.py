#!/usr/bin/env python3
"""Download the raw LongMemEval and LoCoMo benchmark files.

Files land under <out>/raw/ (default out = /home/user/data/memory) and a
MANIFEST.json is written next to them recording source URL, pinned revision,
byte size and sha256 for every file.

LongMemEval: the 2025-09 "cleaned" release (HF dataset
xiaowu0162/longmemeval-cleaned) is the officially recommended version per the
GitHub README; the original repo (xiaowu0162/longmemeval) is marked deprecated.
We download the cleaned S + oracle (+ M if it is under --max-m-bytes), and also
the original (deprecated) S for reference/comparability with older papers.

LoCoMo: data/locomo10.json from github.com/snap-research/locomo, pinned to a
commit resolved with `git ls-remote` (or --locomo-rev).

Requires: huggingface_hub, requests.
"""
import argparse
import hashlib
import json
import os
import shutil
import subprocess
import sys
import time

import requests
from huggingface_hub import HfApi, hf_hub_download

LME_CLEAN_REPO = "xiaowu0162/longmemeval-cleaned"
LME_ORIG_REPO = "xiaowu0162/longmemeval"
LOCOMO_REPO = "https://github.com/snap-research/locomo"
LOCOMO_PATH = "data/locomo10.json"


def sha256_of(path, bufsize=1 << 22):
    h = hashlib.sha256()
    with open(path, "rb") as f:
        while True:
            b = f.read(bufsize)
            if not b:
                break
            h.update(b)
    return h.hexdigest()


def hf_listing(repo):
    info = HfApi().dataset_info(repo, files_metadata=True)
    files = {s.rfilename: {"size": s.size, "sha256": (s.lfs.sha256 if s.lfs else None)}
             for s in info.siblings}
    return info.sha, files


def hf_get(repo, revision, filename, dest_dir, dest_name):
    """Download one file from an HF dataset repo into dest_dir/dest_name."""
    os.makedirs(dest_dir, exist_ok=True)
    final = os.path.join(dest_dir, dest_name)
    if os.path.exists(final):
        print(f"  exists: {final}")
        return final
    tmp_dir = os.path.join(dest_dir, ".hf_tmp")
    p = hf_hub_download(repo_id=repo, filename=filename, repo_type="dataset",
                        revision=revision, local_dir=tmp_dir)
    shutil.move(p, final)
    shutil.rmtree(tmp_dir, ignore_errors=True)
    return final


def git_head(repo_url):
    out = subprocess.run(["git", "ls-remote", repo_url, "HEAD"], capture_output=True,
                         text=True, timeout=120, check=True).stdout
    return out.split()[0]


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--out", default="/home/user/data/memory")
    ap.add_argument("--max-m-bytes", type=float, default=3e9,
                    help="download LongMemEval_M only if smaller than this many bytes")
    ap.add_argument("--skip-original-s", action="store_true")
    ap.add_argument("--locomo-rev", default=None)
    args = ap.parse_args()

    raw = os.path.join(args.out, "raw")
    lme_dir = os.path.join(raw, "longmemeval")
    lme_orig_dir = os.path.join(raw, "longmemeval_original_deprecated")
    loc_dir = os.path.join(raw, "locomo")
    manifest = {"downloaded_at": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()), "files": []}

    # ---- LongMemEval (cleaned, official) ----
    clean_rev, clean_files = hf_listing(LME_CLEAN_REPO)
    orig_rev, orig_files = hf_listing(LME_ORIG_REPO)
    manifest["longmemeval_listing"] = {
        LME_CLEAN_REPO: {"revision": clean_rev, "files": clean_files},
        LME_ORIG_REPO: {"revision": orig_rev, "files": orig_files},
    }
    wanted = [("longmemeval_s_cleaned.json", True), ("longmemeval_oracle.json", True)]
    m_size = clean_files["longmemeval_m_cleaned.json"]["size"]
    wanted.append(("longmemeval_m_cleaned.json", m_size < args.max_m_bytes))
    for fn, do in wanted:
        entry = {"dataset": "longmemeval", "repo": LME_CLEAN_REPO, "revision": clean_rev,
                 "remote_path": fn, "remote_size": clean_files[fn]["size"],
                 "remote_sha256": clean_files[fn]["sha256"],
                 "url": f"https://huggingface.co/datasets/{LME_CLEAN_REPO}/resolve/{clean_rev}/{fn}"}
        if not do:
            entry["skipped"] = f"size {clean_files[fn]['size']} >= {args.max_m_bytes:.0f}"
            print(f"skip {fn}: {entry['skipped']}")
        else:
            print(f"download {LME_CLEAN_REPO}/{fn} ({clean_files[fn]['size']} bytes)")
            entry["local_path"] = hf_get(LME_CLEAN_REPO, clean_rev, fn, lme_dir, fn)
        manifest["files"].append(entry)

    # ---- LongMemEval original S (deprecated; kept for reference) ----
    if not args.skip_original_s:
        fn = "longmemeval_s"  # no .json extension in the original repo
        print(f"download {LME_ORIG_REPO}/{fn} ({orig_files[fn]['size']} bytes)")
        entry = {"dataset": "longmemeval_original", "repo": LME_ORIG_REPO, "revision": orig_rev,
                 "remote_path": fn, "remote_size": orig_files[fn]["size"],
                 "remote_sha256": orig_files[fn]["sha256"],
                 "url": f"https://huggingface.co/datasets/{LME_ORIG_REPO}/resolve/{orig_rev}/{fn}",
                 "local_path": hf_get(LME_ORIG_REPO, orig_rev, fn, lme_orig_dir, "longmemeval_s.json")}
        manifest["files"].append(entry)

    # ---- LoCoMo ----
    rev = args.locomo_rev or git_head(LOCOMO_REPO)
    url = f"https://raw.githubusercontent.com/snap-research/locomo/{rev}/{LOCOMO_PATH}"
    os.makedirs(loc_dir, exist_ok=True)
    dest = os.path.join(loc_dir, "locomo10.json")
    if not os.path.exists(dest):
        print(f"download {url}")
        r = requests.get(url, timeout=300)
        r.raise_for_status()
        with open(dest + ".part", "wb") as f:
            f.write(r.content)
        os.replace(dest + ".part", dest)
    manifest["files"].append({"dataset": "locomo", "repo": LOCOMO_REPO, "revision": rev,
                              "remote_path": LOCOMO_PATH, "url": url, "local_path": dest})

    # ---- verify + record ----
    for e in manifest["files"]:
        lp = e.get("local_path")
        if not lp:
            continue
        e["local_size"] = os.path.getsize(lp)
        e["local_sha256"] = sha256_of(lp)
        if e.get("remote_sha256") and e["remote_sha256"] != e["local_sha256"]:
            print(f"SHA MISMATCH for {lp}", file=sys.stderr)
            sys.exit(1)
        print(f"ok {lp} {e['local_size']} bytes sha256={e['local_sha256'][:12]}")
    with open(os.path.join(raw, "MANIFEST.json"), "w") as f:
        json.dump(manifest, f, indent=2)
    print("wrote", os.path.join(raw, "MANIFEST.json"))


if __name__ == "__main__":
    main()
