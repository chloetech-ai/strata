#!/usr/bin/env bash
# Fetches the Linux x86-64 build of a recent commit of main (CI's job
# main-builds keeps the last 40 in the pre-release `main-builds`):
# chloedb-server (release), chloedb-sim, chloectx, chloedb-crawl,
# chloedb-sheets and chloedb-netops (the ci profile).
#   scripts/fetch-build.sh [COMMIT] [DIR]    # default: origin/main, ./build-<commit>
# Needs gh, signed in. The archive is checked against its SHA-256.
set -euo pipefail
ref="${1:-origin/main}"
sha=$(git rev-parse --short=12 "$ref")
dir="${2:-build-$sha}"
name="chloedb-main-$sha-linux-x86_64.tar.gz"
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
if ! gh release download main-builds --repo chloetech-ai/chloedb --pattern "$name" --pattern "$name.sha256" --dir "$tmp" 2>/dev/null; then
  echo "no build of $sha in main-builds: it is not a commit of main, older than the last 40, or its CI has not finished" >&2
  exit 1
fi
(cd "$tmp" && sha256sum -c "$name.sha256")
mkdir -p "$dir"
tar -xzf "$tmp/$name" -C "$dir"
echo "$dir:"; ls -l "$dir"
