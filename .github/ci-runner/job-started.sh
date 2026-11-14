#!/usr/bin/env bash
# The runner's job-started hook (ACTIONS_RUNNER_HOOK_JOB_STARTED): it runs
# before any step of a job, and a job it fails runs nothing. This
# repository is public, and a pull request from a fork brings its own
# ci.yml, which could name this runner's label: such a job is refused here,
# on the machine, whatever the workflow says. Only this repository's own
# branches, its merge queue, its pushes, the nightly run and runs by hand
# build on this runner.
set -euo pipefail
repo=chloetech-ai/chloedb
# Each runner keeps its build folders under its own work folder
# (CI_RUNNER_CACHE), and two runners' folders outgrew a 100 GB volume on
# 11-14, failing jobs before their checkout. Under 60 GB free, this
# runner's build folders go: it is starting a job, so none of them is in
# use, and the other runner's are left alone.
if [ -n "${CI_RUNNER_CACHE:-}" ] && [ -d "$CI_RUNNER_CACHE/target" ]; then
  free_gb=$(( $(df --output=avail -k "$CI_RUNNER_CACHE" | tail -1) / 1048576 ))
  if [ "$free_gb" -lt 60 ]; then
    echo "only ${free_gb} GB free on the runner's volume: removing $CI_RUNNER_CACHE/target"
    rm -rf "$CI_RUNNER_CACHE/target"
  fi
fi
if [ "${GITHUB_REPOSITORY:-}" != "$repo" ]; then
  echo "::error::this runner builds $repo only, not ${GITHUB_REPOSITORY:-?}"
  exit 1
fi
case "${GITHUB_EVENT_NAME:-}" in
  push|merge_group|schedule|workflow_dispatch) exit 0 ;;
  pull_request)
    head=$(jq -r '.pull_request.head.repo.full_name // ""' "$GITHUB_EVENT_PATH")
    if [ "$head" = "$repo" ]; then exit 0; fi
    echo "::error::a pull request from $head (a fork) does not run on this runner"
    exit 1 ;;
  *)
    echo "::error::event ${GITHUB_EVENT_NAME:-?} does not run on this runner"
    exit 1 ;;
esac
