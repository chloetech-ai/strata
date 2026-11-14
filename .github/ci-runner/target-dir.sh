#!/usr/bin/env bash
# On the on-demand runner (CI_RUNNER_CACHE, set by boot.sh), builds go to a
# folder on its volume, outside the checkout, which actions/checkout cleans:
# from one run to the next cargo rebuilds only the crates whose files
# changed. Elsewhere this does nothing, and rust-cache restores the
# dependencies as before. Usage: target-dir.sh <name of the build>
set -euo pipefail
if [ -n "${CI_RUNNER_CACHE:-}" ]; then
  dir="$CI_RUNNER_CACHE/target/$1"
  # cargo never removes what a change of version or dependency made
  # stale, so the folders grow until the volume is full (11-14: a link
  # died of "No space left on device" on the 0.3.3 bump). Under 40 GB
  # free, this job's folder starts again from nothing; the other
  # runner's, which may be building, is left alone.
  free_gb=$(( $(df --output=avail -k "$CI_RUNNER_CACHE" | tail -1) / 1048576 ))
  if [ "$free_gb" -lt 40 ] && [ -d "$dir" ]; then
    echo "only ${free_gb} GB free on the volume: removing $dir"
    rm -rf "$dir"
  fi
  mkdir -p "$dir"
  echo "CARGO_TARGET_DIR=$dir" >> "$GITHUB_ENV"
  # The compile cache (provision.sh installs sccache): a build folder that
  # starts over recompiles from it, not from source. Each runner has its
  # own folder and its own server: sccache's clients use whatever server
  # is up on their port, and a server dies with the job that started it,
  # so a shared one failed the other runner's builds (11-14: "Permission
  # denied" on a folder that runner's job never named).
  if command -v sccache >/dev/null; then
    cache="$CI_RUNNER_CACHE/sccache"
    mkdir -p "$cache"
    n=$(basename "$CI_RUNNER_CACHE" | tr -dc 0-9)
    { echo "RUSTC_WRAPPER=sccache"; echo "SCCACHE_DIR=$cache"; echo "SCCACHE_SERVER_PORT=$((4300 + ${n:-0}))"; echo "SCCACHE_CACHE_SIZE=30G"; } >> "$GITHUB_ENV"
  fi
  echo "building in $dir ($(df -h --output=avail "$dir" | tail -1 | tr -d ' ') free)"
fi
