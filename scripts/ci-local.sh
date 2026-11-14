#!/usr/bin/env bash
# CI on this machine: the jobs of .github/workflows/ci.yml, run on the merge
# of a commit into main (the tree GitHub tests for a pull request) and summed
# up as the "CI result" check sums them up. With --post-status the outcome is
# recorded on GitHub as the commit status `ci-local` of the commit tested, so
# a pull request that passed here can be merged without waiting for GitHub.
#
#   scripts/ci-local.sh                          # HEAD merged into main, every job
#   scripts/ci-local.sh --pr 203 --post-status   # a pull request's head, from GitHub
#   scripts/ci-local.sh test-chloedb miri         # some jobs only
#   scripts/ci-local.sh --list                   # the jobs and what each runs
#   scripts/ci-local.sh --fast                   # quicker, and less like GitHub
#
# A whole run is slower than GitHub's, which spreads the jobs over 17
# runners: on an 8-core M2 with warm caches it took 70 minutes, against 27
# for the same commit on GitHub (1 November 2026). CI compiles ChloeDB in
# about ten feature sets, and a Mac builds them one after another. It is
# for some jobs while working on a change, a pull request GitHub has not
# got to, or a check of what GitHub cannot run. --fast runs the crates'
# test suites with cargo-nextest, two jobs at once, and only the jobs a
# change can affect.
#
# It runs on macOS (Apple silicon) and on Linux. On a Mac every job runs on
# macOS, the ones GitHub runs on Linux only included, and so:
#
# - x86-64 is not tested natively. The kernels job builds ChloeDB for a
#   generic x86-64 CPU as CI does and runs it under Rosetta 2, which has no
#   AVX2 or AVX-512: the scalar and SSE kernels are tested there, ChloeDB's
#   AVX2 and AVX-512 kernels only on GitHub's Linux runners.
# - Linux's file systems and syncs are not exercised (the fault-injection
#   tests run on macOS, as main's own runs do on GitHub).
# - The Kafka consumer's test needs a broker: --kafka starts Redpanda in
#   Docker for it. Without it the test is skipped, as on GitHub's macOS
#   runners.
# - Miri interprets x86-64 Linux, as on GitHub: it runs neither the NEON
#   intrinsics nor the inline assembly ChloeDB compiles in on aarch64.
# - The tests' temporary files are on a RAM disk (--no-ramdisk keeps them on
#   the disk): a durable sync takes 20 ms on a Mac's SSD, and ChloeDB's
#   tests sync at every commit.
#
# A push to main still runs GitHub's whole matrix, Linux and macOS, so what a
# Mac cannot test is tested after the merge.
#
# The commands are ci.yml's, copied here. Each job of ci.yml is listed below
# (CI_JOBS) with a hash of its lines: when ci.yml changes, a run says which
# jobs no longer match this script, and posts no success status until the
# script is brought up to date (--ci-hashes prints the new hashes).
#
# What it keeps is under $CHLOEDB_CI_HOME ($STRATA_CI_HOME, its old name, works
# too; default ~/.cache/strata-ci-local):
# work/, a clone the jobs run in, whose target directories stay between runs
# as rust-cache keeps them on GitHub; rustup/, the toolchains CI uses (the
# current stable and Miri's pinned nightly), apart from the machine's own;
# a Python 3.12 virtualenv; the BEIR collections, model and ratings the gates
# read; and runs/, each run's logs.
#
# Sharing the machine with benchmarks: while $CHLOEDB_CI_HOME/running exists a
# run is going (the file holds its pid). Creating $CHLOEDB_CI_HOME/pause stops a
# run before its next job and freezes Miri, which runs beside the other jobs;
# $CHLOEDB_CI_HOME/paused appears once nothing of the run is working, and the
# run goes on when pause is removed. A run does not start while pause exists.
set -o pipefail

CI_HOME=${CHLOEDB_CI_HOME:-${STRATA_CI_HOME:-$HOME/.cache/strata-ci-local}}
WORK=$CI_HOME/work
PAUSE=$CI_HOME/pause
STATUS_CONTEXT=ci-local
MIN_FREE_GB=${CHLOEDB_CI_MIN_FREE_GB:-${STRATA_CI_MIN_FREE_GB:-10}}
SELF=$(cd "$(dirname "$0")" && pwd)/$(basename "$0")

# ci.yml's jobs, each with the first 12 hex digits of the SHA-256 of its lines
# without comments and blank lines (ci_job_hashes).
CI_JOBS="
changes 0543cb44ccca
test bee71619d2fe
macos 12396d934241
kernels bbdbd5cc6f67
vision f285537564ca
audio 32586eda6771
datafusion 967c0f3e2e5e
faults 1ab1d52fa757
relevance-gate c4733098fbd0
relevance-gate-cross-encoder 578f9a601bd6
miri 9a2f1972f4b9
server-smoke 071041a3fea9
python bb3189501156
javascript 9c48a39f7630
java d5d337dbe574
ci-result 05940e7f0565
"

# This script's jobs: name, the ci.yml job it mirrors, what it runs.
JOBS="
test-chloedb|test|chloedb's tests (feature sqlite) and clippy
test-chloectx|test|chloectx's tests and clippy
test-chloedb-server|test|chloedb-server's tests, the simulator's seeds, and clippy
test-chloedb-crawl|test|chloedb-crawl's tests and clippy
test-chloedb-sheets|test|chloedb-sheets' tests and clippy
test-chloedb-netops|test|chloedb-netops' tests and clippy
test-chloedb-workbench|test|chloedb-workbench's tests and clippy
kernels|kernels|ChloeDB's SIMD kernels in a generic build, then with SIMD off (x86-64 under Rosetta on a Mac, then the native generic build)
faults|faults|power losses, full disks and failing syncs (feature fault-injection)
datafusion|datafusion|DataFusion as the engine (feature datafusion) and the compound-query suites on it
audio|audio|sound (feature audio): tests and clippy, ChloeDB and chloedb-server
vision|vision|images (feature vision): tests and clippy, ChloeDB and chloedb-server
relevance-gate|relevance-gate|BM25 on three BEIR collections, and by analyzer on two MIRACL collections, against the committed baselines
relevance-gate-cross-encoder|relevance-gate-cross-encoder|BM25 reranked by ms-marco-MiniLM-L-6-v2 against its baseline
server-smoke|server-smoke|a release chloedb-server with a demo catalogue, queried over HTTP; imports and exports; a crawl into a growing cluster
python|python|the Python package's tests against a real server, the recommender gate, clippy
javascript|javascript|the JavaScript package on Node, Bun and Deno, against a real server; clippy
java|java|the Java package on Java 11 and 25, the native library's tests, clippy
miri|miri|Miri over ChloeDB's byte views, in CI's four parts, beside the other jobs
"

# Miri's four parts, as ci.yml's matrix has them.
MIRI_PARTS="pyramid|postings::codecs::pyramid::tests::miri_
bit vectors|postings::bits --skip postings::bits::tests::next_one_and_ones
bytes, loom, bit vector ones, query pool|postings::bytes postings::codecs::loom::tests::miri_ postings::bits::tests::next_one_and_ones query::pool
columns, graph, keys, text codec|forward:: graph:: storage::pk::tests::collisions storage::pk::tests::section_written storage::live storage::updates::tests::round_trip text::codec --skip many_int_keys"

CLIPPY_LINTS="-A clippy::style -A clippy::complexity -A clippy::perf -D warnings"
REDPANDA=docker.redpanda.com/redpandadata/redpanda:v24.2.7
ML1M_URL=https://huggingface.co/datasets/hazemessam/ml-1m/resolve/b7ed0ffb2157bfbc794a1e47e9044ab323bee57f/ratings.csv
ML1M_SHA=d2baa264e8ef4c7fb0c555acae5dc80239612481efff5814df9eab98cb9e1fe0

usage() {
    sed -n '2,12p' "$SELF" | sed 's/^# \{0,1\}//'
    cat <<'EOF'

Options:
  --pr N           test pull request N's head, fetched from GitHub
  --ref REF        test REF of this repository (default HEAD; committed changes only)
  --base BRANCH    merge into BRANCH of GitHub's repository (default main)
  --no-merge       test the commit as it is, not merged into the base
  --all            run every job even when only docs/ and Markdown changed
  --kafka          start Redpanda in Docker for the Kafka consumer's test
  --post-status    post the result as commit status `ci-local` on GitHub
  --fail-fast      stop at the first failed job (default: run them all)
  --verbose        show each job's output as it runs (default: the log's tail on failure)
  --fast           --nextest, --lanes 2, and only the jobs the change can affect (main tested the rest)
  --lanes N        run the jobs in N lanes at once (1 or 2; default 1)
  --nextest        run the three crates' test suites with cargo-nextest (built once): every binary's tests at once
  --no-ramdisk     on a Mac, leave the tests' temporary files on the disk (default: a RAM disk)
  --keep-binaries  keep the test executables after each job (default: delete them, as they are most of the disk used)
  --list           list the jobs
  --ci-hashes      print the hashes of ci.yml's jobs, for CI_JOBS
  --setup          install the toolchains and the virtualenv, run nothing
EOF
}

say() { printf '%s\n' "$*"; [ -n "$RUN_DIR" ] && printf '%s\n' "$*" >>"$RUN_DIR/run.log"; return 0; }
die() { echo "ci-local: $*" >&2; exit 2; }
now() { date +%s; }
mins() { awk -v s="$1" 'BEGIN { printf "%.1f", s / 60 }'; }
sha256() { if command -v sha256sum >/dev/null; then sha256sum "$1"; else shasum -a 256 "$1"; fi | cut -d' ' -f1; }
free_gb() { df -Pk "$CI_HOME" | awk 'NR == 2 { print int($4 / 1048576) }'; }

host_triple() {
    case "$(uname -s)-$(uname -m)" in
        Darwin-arm64) echo aarch64-apple-darwin ;;
        Darwin-x86_64) echo x86_64-apple-darwin ;;
        Linux-x86_64) echo x86_64-unknown-linux-gnu ;;
        Linux-aarch64) echo aarch64-unknown-linux-gnu ;;
        *) die "unsupported machine: $(uname -s) $(uname -m)" ;;
    esac
}

# Each job of ci.yml with a hash of its lines, comments and blank lines left
# out so that a reworded comment is no change.
ci_job_hashes() {
    local file=$1 name
    for name in $(awk '/^jobs:/ { j = 1; next } j && /^[^ #]/ { j = 0 } j && /^  [A-Za-z0-9_-]+:/ { sub(/:.*/, ""); print $1 }' "$file"); do
        printf '%s %s\n' "$name" "$(awk -v want="$name" '
            /^jobs:/ { j = 1; next }
            j && /^[^ #]/ { j = 0 }
            !j || /^[[:space:]]*(#|$)/ { next }
            /^  [A-Za-z0-9_-]+:/ { cur = $1; sub(/:.*/, "", cur) }
            cur == want { print }
        ' "$file" | if command -v sha256sum >/dev/null; then sha256sum; else shasum -a 256; fi | cut -c1-12)"
    done
}

# The jobs of ci.yml whose lines differ from what this script was written
# against, or that it does not know; empty when they all match.
ci_drift() {
    local file=$1 name hash want
    ci_job_hashes "$file" | while read -r name hash; do
        want=$(printf '%s\n' "$CI_JOBS" | awk -v n="$name" '$1 == n { print $2 }')
        if [ -z "$want" ]; then echo "$name (new)"; elif [ "$want" != "$hash" ]; then echo "$name (changed)"; fi
    done
    printf '%s\n' "$CI_JOBS" | awk 'NF { print $1 }' | while read -r name; do
        ci_job_hashes "$file" | awk -v n="$name" '$1 == n { f = 1 } END { exit !f }' || echo "$name (gone)"
    done
}

job_names() { printf '%s\n' "$JOBS" | awk -F'|' 'NF { print $1 }'; }

# --- options ---------------------------------------------------------------

ARGS=("$@")
PR="" REF=HEAD BASE=main MERGE=1 ALL=0 KAFKA=0 POST=0 FAIL_FAST=0 VERBOSE=0 SETUP_ONLY=0 KEEP_BINARIES=0 NEXTEST=0 LANES=1 ONLY_AFFECTED=0 RAMDISK=1 SELECTED=""
while [ $# -gt 0 ]; do
    case "$1" in
        --pr) PR=${2:?--pr needs a number}; shift ;;
        --ref) REF=${2:?--ref needs a ref}; shift ;;
        --base) BASE=${2:?--base needs a branch}; shift ;;
        --no-merge) MERGE=0 ;;
        --all) ALL=1 ;;
        --kafka) KAFKA=1 ;;
        --post-status) POST=1 ;;
        --fail-fast) FAIL_FAST=1 ;;
        --verbose|-v) VERBOSE=1 ;;
        --keep-binaries) KEEP_BINARIES=1 ;;
        --no-ramdisk) RAMDISK=0 ;;
        --nextest) NEXTEST=1 ;;
        --lanes) LANES=${2:?--lanes needs 1 or 2}; shift ;;
        --fast) NEXTEST=1 LANES=2 ONLY_AFFECTED=1 ;;
        --setup) SETUP_ONLY=1 ;;
        --list)
            printf '%s\n' "$JOBS" | awk -F'|' 'NF { printf "  %-30s %s\n", $1, $3 }'
            exit 0 ;;
        --ci-hashes)
            ci_job_hashes "$(git rev-parse --show-toplevel)/.github/workflows/ci.yml"
            exit 0 ;;
        -h|--help) usage; exit 0 ;;
        -*) die "unknown option $1 (see --help)" ;;
        *)
            if [ "$1" = test ]; then
                SELECTED="$SELECTED test-chloedb test-chloectx test-chloedb-server test-chloedb-crawl test-chloedb-sheets test-chloedb-netops test-chloedb-workbench"
            elif job_names | grep -qx -- "$1"; then
                SELECTED="$SELECTED $1"
            else
                die "no job $1 (see --list)"
            fi ;;
    esac
    shift
done
case "$LANES" in 1|2) ;; *) die "--lanes takes 1 or 2" ;; esac
[ -n "$SELECTED" ] || SELECTED=$(job_names | tr '\n' ' ')
selected() { case " $SELECTED " in *" $1 "*) return 0 ;; esac; return 1; }
FULL=1
for j in $(job_names); do selected "$j" || FULL=0; done
RUN_DIR=""

if [ "$POST" = 1 ]; then
    [ "$FULL" = 1 ] || die "--post-status needs every job: a status for some of them would read as all of them"
    [ "$MERGE" = 1 ] || die "--post-status needs the merge into $BASE: what merges is what is tested"
    command -v gh >/dev/null || die "--post-status needs the GitHub CLI (gh)"
fi
mkdir -p "$CI_HOME"

# --- 1. the tree to test ---------------------------------------------------
#
# A clone of its own (work/), at the merge of the commit into the base as
# GitHub fetched them now; then this script goes on as the copy in that tree,
# so that a pull request changing ci.yml is run by its own ci-local.sh.

post_status() {
    [ "$POST" = 1 ] && [ -n "$HEAD_SHA" ] || return 0
    gh api -X POST "repos/$SLUG/statuses/$HEAD_SHA" -f state="$1" -f context="$STATUS_CONTEXT" -f description="$(printf '%s' "$2" | cut -c1-140)" >/dev/null \
        || echo "ci-local: could not post the status to $SLUG@${HEAD_SHA:0:7}" >&2
}

FINISHED=0 HEAD_SHA="" RAM_DEV=""
cleanup() {
    local p
    for p in $MIRI_PIDS $LANE_PIDS; do signal_tree CONT "$p"; signal_tree TERM "$p"; done
    [ "$FINISHED" = 1 ] || post_status error "Interrupted"
    [ -n "$RAM_DEV" ] && hdiutil detach -force "$RAM_DEV" >/dev/null 2>&1
    rm -f "$CI_HOME/running" "$CI_HOME/paused"
}

if [ "$SETUP_ONLY" = 1 ]; then
    CI_LOCAL_TREE=$(git rev-parse --show-toplevel 2>/dev/null) || die "run it from a clone of the repository"
elif [ -z "$CI_LOCAL_TREE" ]; then
    # The run checks out other commits in work/: bash would read the rest of
    # this file from under itself.
    case "$SELF" in "$WORK"/*) die "run it from a clone of your own, not from $WORK" ;; esac
    # One run at a time, and none while a benchmark holds the machine.
    if [ -e "$PAUSE" ]; then
        echo "Waiting: $PAUSE exists (a benchmark has the machine)."
        while [ -e "$PAUSE" ]; do sleep 10; done
    fi
    if [ -e "$CI_HOME/running" ] && kill -0 "$(head -1 "$CI_HOME/running")" 2>/dev/null; then
        die "a run is going already (pid $(head -1 "$CI_HOME/running"))"
    fi
    printf '%s\n' "$$" >"$CI_HOME/running"
    trap 'rm -f "$CI_HOME/running"' EXIT
    REPO=$(git rev-parse --show-toplevel 2>/dev/null) || die "run it from a clone of the repository"
    URL=$(git -C "$REPO" remote get-url origin 2>/dev/null || echo https://github.com/chloetech-ai/chloedb)
    SLUG=$(printf '%s\n' "$URL" | sed -E 's#^(https://|git@|ssh://git@)github\.com[:/]##; s#\.git$##')
    case "$SLUG" in */*) ;; *) SLUG=chloetech-ai/chloedb ;; esac
    if [ ! -d "$WORK/.git" ]; then
        echo "Cloning the repository into $WORK"
        git clone -q --no-checkout "$REPO" "$WORK" || die "could not clone $REPO into $WORK"
    fi
    git -C "$WORK" remote set-url origin "https://github.com/$SLUG"
    git -C "$WORK" fetch -q --no-tags origin "+refs/heads/$BASE:refs/remotes/origin/$BASE" || die "could not fetch $BASE from github.com/$SLUG"
    if [ -n "$PR" ]; then
        git -C "$WORK" fetch -q --no-tags origin "+refs/pull/$PR/head:refs/ci-local/head" || die "could not fetch pull request $PR"
    else
        sha=$(git -C "$REPO" rev-parse --verify -q "$REF^{commit}") || die "no commit $REF"
        if [ "$REF" = HEAD ] && [ -n "$(git -C "$REPO" status --porcelain --untracked-files=no)" ]; then
            echo "Note: $REPO has uncommitted changes; they are not tested (only commit ${sha:0:7} is)."
        fi
        git -C "$WORK" fetch -q --no-tags "$REPO" "$sha" && git -C "$WORK" update-ref refs/ci-local/head "$sha" || die "could not fetch $sha from $REPO"
    fi
    HEAD_SHA=$(git -C "$WORK" rev-parse refs/ci-local/head)
    BASE_SHA=$(git -C "$WORK" rev-parse "refs/remotes/origin/$BASE")
    git -C "$WORK" merge --abort >/dev/null 2>&1
    git -C "$WORK" checkout -q -f --detach "refs/remotes/origin/$BASE"
    # Untracked files of an earlier run go; the target directories stay.
    git -C "$WORK" clean -ffdxq -e target
    if [ "$MERGE" = 1 ]; then
        if ! git -C "$WORK" -c user.name=ci-local -c user.email=ci-local@localhost \
                merge -q --no-ff --no-edit -m "ci-local: ${HEAD_SHA:0:7} merged into $BASE ${BASE_SHA:0:7}" refs/ci-local/head; then
            git -C "$WORK" merge --abort
            die "${HEAD_SHA:0:7} does not merge cleanly into $BASE ${BASE_SHA:0:7}: rebase it first"
        fi
    else
        git -C "$WORK" checkout -q -f --detach refs/ci-local/head
    fi
    next=$WORK/scripts/ci-local.sh
    [ -f "$next" ] || next=$SELF
    # A copy of its own: bash reads a script as it goes, so an edit or a
    # checkout of the file during the hour of a run would change its end.
    cp "$next" "$CI_HOME/ci-local.run.sh"
    export CI_LOCAL_TREE=$WORK CI_LOCAL_HEAD=$HEAD_SHA CI_LOCAL_BASE=$BASE_SHA CI_LOCAL_SLUG=$SLUG CI_LOCAL_PR=$PR
    # exec keeps the pid that `running` holds.
    exec bash "$CI_HOME/ci-local.run.sh" "${ARGS[@]}"
fi

# --- 2. the run, in the tree ---------------------------------------------

cd "$CI_LOCAL_TREE" || die "no tree $CI_LOCAL_TREE"
HEAD_SHA=$CI_LOCAL_HEAD BASE_SHA=$CI_LOCAL_BASE SLUG=$CI_LOCAL_SLUG PR=$CI_LOCAL_PR
HOST=$(host_triple)
CI_YML=$CI_LOCAL_TREE/.github/workflows/ci.yml
MIRI_TOOLCHAIN=$(sed -n 's/^ *toolchain: *\(nightly-[0-9-]*\).*/\1/p' "$CI_YML" | head -1)
[ -n "$MIRI_TOOLCHAIN" ] || die "no Miri nightly in ci.yml"
MACHINE=$( (sysctl -n machdep.cpu.brand_string 2>/dev/null || sed -n 's/^model name[[:space:]]*: //p' /proc/cpuinfo | head -1) )
OS=$(uname -s | sed 's/Darwin/macOS/')

export CARGO_TERM_COLOR=never CARGO_INCREMENTAL=0
unset RUSTUP_TOOLCHAIN RUSTFLAGS CARGO_ENCODED_RUSTFLAGS CARGO_TARGET_DIR

# The Rust toolchains CI uses, in a rustup home of their own: the stable of
# today (dtolnay/rust-toolchain@stable) with clippy, and on a Mac the x86-64
# target the kernels job runs under Rosetta; Miri's nightly when it runs.
find_rustup() {
    local r
    # Homebrew's rustup is keg-only: not on PATH unless linked.
    for r in "$(command -v rustup 2>/dev/null)" /opt/homebrew/opt/rustup/bin/rustup /usr/local/opt/rustup/bin/rustup "$CI_HOME/bin/rustup"; do
        [ -n "$r" ] && "$r" --version >/dev/null 2>&1 && { echo "$r"; return 0; }
    done
    # Older Homebrew formulas install only rustup-init, which is rustup when
    # it runs under that name.
    mkdir -p "$CI_HOME/bin"
    for r in /opt/homebrew/opt/rustup/bin/rustup-init /usr/local/opt/rustup/bin/rustup-init "$CI_HOME/bin/rustup-init"; do
        if [ -x "$r" ]; then
            ln -sf "$r" "$CI_HOME/bin/rustup"
            "$CI_HOME/bin/rustup" --version >/dev/null 2>&1 && { echo "$CI_HOME/bin/rustup"; return 0; }
        fi
    done
    echo "Downloading rustup-init from static.rust-lang.org" >&2
    curl -fsSL --retry 3 -o "$CI_HOME/bin/rustup-init" "https://static.rust-lang.org/rustup/dist/$HOST/rustup-init" || return 1
    chmod +x "$CI_HOME/bin/rustup-init"
    ln -sf "$CI_HOME/bin/rustup-init" "$CI_HOME/bin/rustup"
    "$CI_HOME/bin/rustup" --version >/dev/null 2>&1 && echo "$CI_HOME/bin/rustup"
}

setup_rust() {
    local rustup targets=""
    export RUSTUP_HOME=$CI_HOME/rustup
    rustup=$(find_rustup) || die "no rustup, and none could be downloaded"
    [ "$HOST" = aarch64-apple-darwin ] && targets="--target x86_64-apple-darwin"
    # CARGO_HOME of its own, so that rustup leaves the machine's proxies alone.
    CARGO_HOME=$CI_HOME/cargo-rustup "$rustup" toolchain install stable --profile minimal --component clippy $targets --no-self-update >"$CI_HOME/rustup.log" 2>&1 \
        || { cat "$CI_HOME/rustup.log" >&2; die "could not install Rust stable"; }
    if [ "$SETUP_ONLY" = 1 ] || selected miri; then
        CARGO_HOME=$CI_HOME/cargo-rustup "$rustup" toolchain install "$MIRI_TOOLCHAIN" --profile minimal --component miri --component rust-src --no-self-update >>"$CI_HOME/rustup.log" 2>&1 \
            || { cat "$CI_HOME/rustup.log" >&2; die "could not install $MIRI_TOOLCHAIN with Miri"; }
    fi
    STABLE_BIN=$RUSTUP_HOME/toolchains/stable-$HOST/bin
    NIGHTLY_BIN=$RUSTUP_HOME/toolchains/$MIRI_TOOLCHAIN-$HOST/bin
    export PATH=$STABLE_BIN:$CI_HOME/tools/bin:$PATH
    RUST_VERSION=$(rustc --version | cut -d' ' -f2)
    if [ "$NEXTEST" = 1 ] && ! cargo nextest --version >/dev/null 2>&1; then
        echo "Building cargo-nextest into $CI_HOME/tools (once)"
        cargo install --locked --quiet --root "$CI_HOME/tools" cargo-nextest || die "could not build cargo-nextest"
    fi
}

# Python 3.12, as CI's setup-python, in a virtualenv kept between runs.
PY_VENV=$CI_HOME/venv-py312
# On a Mac the tests' temporary files go to a RAM disk: a durable sync there
# (F_FULLFSYNC, which Rust's sync_all is on macOS) takes 0.2 ms rather than
# the 20 ms it takes on this Mac's SSD, and ChloeDB's tests sync at every
# commit: the fault-injection tests alone took half an hour on the SSD.
# Builds stay on the disk. Detached when the run ends.
RAMDISK_GB=${CHLOEDB_CI_RAMDISK_GB:-${STRATA_CI_RAMDISK_GB:-6}}
RAM_VOL=strata-ci-ram
setup_ramdisk() {
    [ "$RAMDISK" = 1 ] && [ "$(uname -s)" = Darwin ] || return 0
    # A run that was killed left its disk.
    [ -d "/Volumes/$RAM_VOL" ] && hdiutil detach -force "/Volumes/$RAM_VOL" >/dev/null 2>&1
    RAM_DEV=$(hdiutil attach -nobrowse -nomount "ram://$((RAMDISK_GB * 2097152))" | awk 'NR == 1 { print $1 }')
    [ -n "$RAM_DEV" ] || { echo "ci-local: no RAM disk; temporary files stay on the disk" >&2; return 0; }
    if diskutil erasevolume APFS "$RAM_VOL" "$RAM_DEV" >/dev/null 2>&1 && [ -d "/Volumes/$RAM_VOL" ]; then
        export TMPDIR=/Volumes/$RAM_VOL/
    else
        hdiutil detach -force "$RAM_DEV" >/dev/null 2>&1
        RAM_DEV=""
        echo "ci-local: no RAM disk; temporary files stay on the disk" >&2
    fi
}

setup_python() {
    local py
    py=$(command -v python3.12 || command -v python3) || die "no Python 3"
    [ -x "$PY_VENV/bin/python" ] || "$py" -m venv "$PY_VENV" || die "could not make a virtualenv in $PY_VENV"
}

# --- the jobs --------------------------------------------------------------
#
# Each runs in a subshell of its own in the tree's root, with -e and -x: the
# log shows every command, and the first failure ends the job.

# A crate's whole test suite, as `cargo test --profile ci`. With --nextest,
# cargo-nextest runs it: the tests of every test binary at once rather than
# one binary after another (chloedb has 137), each test in a process of its
# own; then the doctests, which nextest does not run.
crate_tests() {
    if [ "$NEXTEST" = 1 ]; then
        # The test binaries, for prune_job (listing builds them).
        cargo nextest list --cargo-profile ci "$@" --message-format json \
            | python3 -c 'import json, sys; [print(s["binary-path"]) for s in json.load(sys.stdin)["rust-suites"].values()]' >"$JOB_LOG.binaries"
        # Each test in a process of its own, and every binary's at once,
        # exposes tests that take a free port and bind it later: they get a
        # second try, and the log says FLAKY.
        cargo nextest run --cargo-profile ci --no-fail-fast --retries 2 "$@"
        cargo test --profile ci --doc "$@"
    else
        cargo test --profile ci --no-fail-fast "$@"
    fi
}

# Builds in chloedb/ on x86-64 Linux name x86-64-v3, as CI's do (so that one
# cache serves every runner there); on other machines the variable does
# nothing. Builds in chloedb-server/ leave it unset, as CI does.
in_chloedb() {
    cd "$CI_LOCAL_TREE/chloedb"
    export CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUSTFLAGS="-C target-cpu=x86-64-v3"
}
in_server() {
    cd "$CI_LOCAL_TREE/chloedb-server"
    unset CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUSTFLAGS
}

job_test_chloedb() {
    in_chloedb
    crate_tests --features sqlite
    cargo clippy --release --all-targets --features sqlite -- $CLIPPY_LINTS
}

job_test_chloectx() {
    cd chloectx
    crate_tests
    cargo clippy --release --all-targets -- $CLIPPY_LINTS
}

job_test_chloedb_server() {
    cd chloedb-server
    crate_tests
    # The cluster simulator over more seeds than its tests run (the build
    # above made chloedb-sim): the first 3,000 of each world, as on every
    # change on GitHub, where only Linux runs them.
    ./target/ci/chloedb-sim --seeds 0..3000
    ./target/ci/chloedb-sim --seeds 0..1000 --drop-ppm 150000
    ./target/ci/chloedb-sim --cluster --seeds 0..3000
    cargo clippy --release --all-targets -- $CLIPPY_LINTS
}

job_test_chloedb_crawl() {
    cd chloedb-crawl
    crate_tests
    cargo clippy --release --all-targets -- $CLIPPY_LINTS
}

job_test_chloedb_sheets() {
    cd chloedb-sheets
    crate_tests
    cargo clippy --release --all-targets -- $CLIPPY_LINTS
}

job_test_chloedb_netops() {
    cd chloedb-netops
    crate_tests
    cargo clippy --release --all-targets -- $CLIPPY_LINTS
}

job_test_chloedb_workbench() {
    cd chloedb-workbench
    crate_tests
    cargo clippy --release --all-targets -- $CLIPPY_LINTS
}

# RUSTFLAGS, even empty, replaces chloedb/.cargo/config.toml's target-cpu=native.
kernels() {
    export RUSTFLAGS=$1
    shift
    cargo test --profile ci "$@" --lib -- vector:: postings:: forward:: simd
    CHLOEDB_SIMD=portable cargo test --profile ci "$@" --lib -- vector:: postings:: forward::
}

job_kernels() {
    cd chloedb
    case "$HOST" in
        x86_64-*)
            kernels "-C target-cpu=x86-64" ;;
        aarch64-apple-darwin)
            # CI's build, for a generic x86-64 CPU, run under Rosetta 2.
            kernels "-C target-cpu=x86-64" --target x86_64-apple-darwin
            # And the build chloedb-server and the Python package have here:
            # no target-cpu, so ChloeDB picks its kernels at run time.
            kernels "" ;;
        *)
            kernels "" ;;
    esac
}

job_faults() {
    in_chloedb
    cargo test --profile ci --features fault-injection --test storage_faults
}

job_datafusion() {
    in_chloedb
    cargo test --profile ci --features datafusion --test datafusion_engine
    cargo test --profile ci --features datafusion,sqlite --test window_functions --test set_operations
    CHLOEDB_ENGINE=datafusion cargo test --profile ci --features datafusion --test compound_queries --test joins_across_tables --test operators --test distinct_on --test per_cap --test one_evaluator --test sql_tables --test correlated --test query_memory --test plan_ir --test json_fields --test json_columns
    cargo test --profile ci --features datafusion,parquet,s3 --test parquet
    cargo test --profile ci --features datafusion,parquet,s3 --lib -- parquet:: objects:: arrow_rows::
    cargo clippy --profile ci --features datafusion,parquet,s3 --all-targets -- $CLIPPY_LINTS
}

job_audio() {
    in_chloedb
    cargo test --profile ci --features audio --test derived_audio --test audio_vectors
    cargo test --profile ci --features audio --lib -- audio:: clap::
    cargo clippy --profile ci --features audio --all-targets -- $CLIPPY_LINTS
    in_server
    cargo clippy --profile ci --features audio -- $CLIPPY_LINTS
}

job_vision() {
    in_chloedb
    cargo test --profile ci --features vision --test media_vectors --test media_fields --test derived_text --test rank_cross
    cargo clippy --profile ci --features vision --all-targets -- $CLIPPY_LINTS
    in_server
    cargo test --profile ci --features vision --test media_vectors --test media
}

job_relevance_gate() {
    chloedb/tests/fixtures/beir/fetch.sh "$CI_HOME/beir"
    chloedb/tests/fixtures/miracl/fetch.sh "$CI_HOME/miracl" yo sw
    in_chloedb
    CHLOEDB_BEIR_DIR="$CI_HOME/beir" CHLOEDB_MIRACL_DIR="$CI_HOME/miracl" CHLOEDB_RELEVANCE_GATE=required cargo test --profile ci --test relevance_gate -- --nocapture
}

job_relevance_gate_cross_encoder() {
    chloedb/tests/fixtures/beir/fetch.sh "$CI_HOME/beir"
    chloedb/tests/models/fetch_cross_encoder.sh "$CI_HOME/models"
    in_chloedb
    CHLOEDB_BEIR_DIR="$CI_HOME/beir" CHLOEDB_MODELS="$CI_HOME/models" CHLOEDB_RELEVANCE_GATE=required cargo test --profile ci --features bert --test relevance_gate cross_encoder -- --nocapture
}

# A free port on the loopback: CI's 7700 and 7702 may be taken here.
free_port() { python3 -c 'import socket; s = socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1])'; }

job_server_smoke() {
    local t p1 p2
    t=$(mktemp -d "$JOB_DIR/smoke.XXXX")
    p1=$(free_port) p2=$(free_port)
    trap 'kill $(jobs -p) 2>/dev/null' EXIT
    cd chloedb-server
    cargo build --release
    ./target/release/chloedb-server --demo 5000 --db "$t/demo.strata" --listen 127.0.0.1:$p1 --grpc off &
    for i in $(seq 1 60); do curl -sf http://127.0.0.1:$p1/v1/health && break; sleep 1; done
    curl -sf http://127.0.0.1:$p1/v1/health | grep -q '"ok"'
    curl -sf -X POST http://127.0.0.1:$p1/v1/query -H 'content-type: application/json' \
        -d '{"sql": "SELECT id, title, score() FROM products ORDER BY bm25(body, :q) DESC LIMIT 5", "params": {"q": "waterproof jacket"}}' \
        | grep -q '"rows"'
    curl -sf -X POST http://127.0.0.1:$p1/v1/query -H 'content-type: application/json' \
        -d '{"sql": "SELECT category, COUNT(*) FROM products GROUP BY category ORDER BY 2 DESC LIMIT 5"}' \
        | grep -q '"rows"'
    curl -sf -X POST http://127.0.0.1:$p1/v1/query -H 'content-type: application/json' \
        -d '{"sql": "WITH top AS (SELECT id FROM products ORDER BY bm25(body, '"'"'jaket'"'"', fuzzy => 1) DESC LIMIT 50) SELECT category, COUNT(*), AVG(price) FROM top GROUP BY category"}' \
        | grep -q '"rows"'
    curl -sf http://127.0.0.1:$p1/metrics | grep -q chloedb_queries_total
    curl -sf http://127.0.0.1:$p1/ | grep -qi '<html'
    # Rows into a new database from the command line, then over HTTP.
    printf '{"table": "t", "key": "id", "fields": [{"name": "id", "type": "keyword"}, {"name": "title", "type": "text"}]}' > "$t/t.json"
    printf '{"id": "a", "title": "first row"}\n{"id": "b", "title": "second row", "extra": 1}\n' > "$t/t.jsonl"
    ./target/release/chloedb-server import --db "$t/t.strata" --schema "$t/t.json" "$t/t.jsonl"
    ./target/release/chloedb-server --db "$t/t.strata" --listen 127.0.0.1:$p2 --grpc off &
    for i in $(seq 1 60); do curl -sf http://127.0.0.1:$p2/v1/health && break; sleep 1; done
    printf 'id,title\nc,third row\n' | curl -sf -X POST "http://127.0.0.1:$p2/v1/import?format=csv" -H 'content-type: text/csv' --data-binary @- | grep -q '"written":1'
    curl -sf http://127.0.0.1:$p2/v1/health | grep -q '"docs":3'
    # A SQLite database's tables into a new database, and back out.
    python3 -c "import sqlite3, sys; c = sqlite3.connect(sys.argv[1]); c.executescript(\"CREATE TABLE notes (id INTEGER PRIMARY KEY, body TEXT, at DATETIME); INSERT INTO notes VALUES (1, 'a first note about rivers and mountains', '2026-10-01 12:00:00');\"); c.commit()" "$t/s.sqlite"
    ./target/release/chloedb-server import --db "$t/s.strata" "$t/s.sqlite"
    ./target/release/chloedb-server export --db "$t/s.strata" "$t/s2.sqlite"
    python3 -c "import sqlite3, sys; c = sqlite3.connect(sys.argv[1]); assert c.execute(\"SELECT notes.rowid, at FROM notes_fts JOIN notes ON notes.rowid = notes_fts.rowid WHERE notes_fts MATCH 'river'\").fetchall() == [(1, '2026-10-01 12:00:00')]" "$t/s2.sqlite"
    # And over HTTP: the SQLite file into the running server, a new table, and the database back out.
    curl -sf -X POST "http://127.0.0.1:$p2/v1/import?format=sqlite" -H 'content-type: application/vnd.sqlite3' --data-binary @"$t/s.sqlite" | grep -q '"written":1'
    curl -sf http://127.0.0.1:$p2/v1/export -o "$t/s3.sqlite"
    python3 -c "import sqlite3, sys; c = sqlite3.connect(sys.argv[1]); assert c.execute('SELECT count(*) FROM t').fetchone() == (3,); assert c.execute('SELECT body FROM notes').fetchall() == [('a first note about rivers and mountains',)]" "$t/s3.sqlite"
    # The crawler on a site its test serves, into a cluster of this build's
    # servers that it starts and grows to three shards.
    cd ../chloedb-crawl
    CHLOEDB_SERVER_BIN="$CI_LOCAL_TREE/chloedb-server/target/release/chloedb-server" CHLOEDB_CRAWL_NEED_SERVER=1 cargo test --profile ci --test site
    # Spreadsheets kept in a server of this build, read back after a restart.
    cd ../chloedb-sheets
    CHLOEDB_SERVER_BIN="$CI_LOCAL_TREE/chloedb-server/target/release/chloedb-server" CHLOEDB_SHEETS_NEED_SERVER=1 cargo test --profile ci --test cluster
    # The network operations app on a server of this build: its faults diagnosed, and read back after a restart.
    cd ../chloedb-netops
    CHLOEDB_SERVER_BIN="$CI_LOCAL_TREE/chloedb-server/target/release/chloedb-server" CHLOEDB_NETOPS_NEED_SERVER=1 cargo test --profile ci --test cluster
}

# Redpanda for the Kafka consumer's exactly-once test: CI's container on
# Linux; on a Mac, Docker's VM has no host network, so its port is published.
start_redpanda() {
    local name=ci-local-redpanda-$$
    if [ "$(uname -s)" = Linux ]; then
        docker run -d --name "$name" --network host "$REDPANDA" \
            redpanda start --mode dev-container --smp 1 --memory 1G --overprovisioned \
            --kafka-addr 127.0.0.1:19092 --advertise-kafka-addr 127.0.0.1:19092 \
            --rpc-addr 127.0.0.1:33145 --advertise-rpc-addr 127.0.0.1:33145 \
            --pandaproxy-addr 127.0.0.1:18082 --schema-registry-addr 127.0.0.1:18081
    else
        docker run -d --name "$name" -p 127.0.0.1:19092:19092 "$REDPANDA" \
            redpanda start --mode dev-container --smp 1 --memory 1G --overprovisioned \
            --kafka-addr 0.0.0.0:19092 --advertise-kafka-addr 127.0.0.1:19092 \
            --rpc-addr 127.0.0.1:33145 --advertise-rpc-addr 127.0.0.1:33145 \
            --pandaproxy-addr 127.0.0.1:18082 --schema-registry-addr 127.0.0.1:18081
    fi
    trap "docker rm -f $name >/dev/null 2>&1" EXIT
    for i in $(seq 1 60); do docker exec "$name" rpk cluster health 2>/dev/null | grep -q 'Healthy: *true' && break; sleep 1; done
    docker exec "$name" rpk cluster health
    export CHLOEDB_KAFKA=127.0.0.1:19092
}

job_python() {
    local ratings=$CI_HOME/data/ml-1m-ratings.csv
    # shellcheck disable=SC1091
    . "$PY_VENV/bin/activate"
    pip install -q --upgrade pip
    pip install -q --upgrade maturin pytest numpy confluent-kafka
    pip install -q --upgrade pyarrow polars pandas "moto[server]" boto3
    # strata.train.sequence's test: CPU PyTorch (PyPI's macOS wheel is one)
    if [ "$(uname -s)" = Linux ]; then pip install -q --upgrade torch --index-url https://download.pytorch.org/whl/cpu; else pip install -q --upgrade torch; fi
    (cd chloedb-server && cargo build --release)
    if [ "$KAFKA" = 1 ]; then start_redpanda; fi
    cd chloedb-py
    maturin develop --profile ci
    pytest tests -v
    # The recommender gate (Linux only on GitHub), on the same pinned ratings.
    if [ ! -f "$ratings" ] || [ "$(sha256 "$ratings")" != "$ML1M_SHA" ]; then
        mkdir -p "$(dirname "$ratings")"
        curl -fsSL --retry 3 -o "$ratings.tmp" "$ML1M_URL"
        [ "$(sha256 "$ratings.tmp")" = "$ML1M_SHA" ]
        mv "$ratings.tmp" "$ratings"
    fi
    # Both protocols at once, as on GitHub.
    local protocol status=0
    for protocol in loo time; do
        python ../chloedb/results/sequence/gate.py --ratings "$ratings" --protocol "$protocol" >"$JOB_DIR/gate-$protocol.log" 2>&1 &
        echo $! >"$JOB_DIR/gate-$protocol.pid"
    done
    for protocol in loo time; do
        wait "$(cat "$JOB_DIR/gate-$protocol.pid")" || status=1
        cat "$JOB_DIR/gate-$protocol.log"
    done
    [ "$status" = 0 ]
    cargo clippy --release --all-targets -- $CLIPPY_LINTS
}

# The tools a job needs that this script does not install: it fails at once,
# saying what is missing, rather than pass with a part left out.
need() {
    local missing="" t
    for t in "$@"; do command -v "$t" >/dev/null 2>&1 || missing="$missing $t"; done
    [ -z "$missing" ] || { echo "ci-local: this job needs$missing (on a Mac: brew install$missing)" >&2; return 1; }
}

# A JDK of the given major version (macOS's java_home, or JAVA_HOME_<N>_X64
# as on GitHub's runners).
jdk() {
    local home
    # -F: no other version when there is none of this one.
    home=$( (/usr/libexec/java_home -F -v "$1" 2>/dev/null) || eval "echo \${JAVA_HOME_$1_X64:-}")
    [ -n "$home" ] || { echo "ci-local: this job needs a JDK $1 (on a Mac: brew install --cask temurin@$1)" >&2; return 1; }
    echo "$home"
}

job_javascript() {
    need node bun deno
    (cd chloedb-server && cargo build --release)
    cd chloedb-js
    node scripts/build.mjs --profile ci
    node --test test/*.test.mjs
    bun test/runtimes.mjs
    deno run --allow-ffi --allow-read --allow-write --allow-env --allow-net --allow-run --allow-sys test/runtimes.mjs
    cargo clippy --release --all-targets -- $CLIPPY_LINTS
}

job_java() {
    local jdk11 jdk25
    need mvn
    jdk11=$(jdk 11)
    jdk25=$(jdk 25)
    cd chloedb-java
    JAVA_HOME=$jdk11 mvn -B -ntp -Dcargo.profile=ci package
    JAVA_HOME=$jdk25 mvn -B -ntp -Dcargo.profile=ci -Dcargo.skip=true test
    cargo test --profile ci
    cargo clippy --release --all-targets -- $CLIPPY_LINTS
}

# Miri, on the pinned nightly, with no RUSTFLAGS (a portable build: Miri has
# no CPUID), interpreting x86-64 Linux as on GitHub whatever the machine: on
# aarch64 ChloeDB's NEON kernels are compiled in unconditionally, and Miri
# runs neither their intrinsics nor their inline assembly. Each part has a
# target directory of its own, so that the three run at once, each on one
# thread as Miri does.
MIRI_TARGET=x86_64-unknown-linux-gnu
miri() {
    local dir=$1 sub=$2
    shift 2
    PATH=$NIGHTLY_BIN:$PATH RUSTFLAGS="" MIRIFLAGS=-Zmiri-disable-isolation CARGO_TARGET_DIR=target/$dir \
        cargo miri "$sub" --target "$MIRI_TARGET" "$@"
}
miri_part() {
    cd chloedb
    miri "miri-$1" test --no-default-features --lib -- $2
}

# --- running them ----------------------------------------------------------
#
# The jobs run in lanes: one by default, the jobs one after another; with
# --lanes 2 (or --fast) two, so that while one lane links, runs a
# single-threaded step or waits on a download, the other has the cores.
# Lane 2 has the jobs that build outside chloedb/target. Miri runs beside the
# lanes. Each job's outcome is a line of $RUN_DIR/results.

MIRI_PIDS="" LANE_PIDS=""
LANE2_JOBS="test-chloectx test-chloedb-server test-chloedb-crawl test-chloedb-sheets test-chloedb-netops test-chloedb-workbench server-smoke python javascript java"
record() { printf '%s|%s|%s\n' "$1" "$2" "$3" >>"$RUN_DIR/results"; }
failed_any() { [ -f "$RUN_DIR/results" ] && grep -qv '|success|' "$RUN_DIR/results"; }
lane_of() {
    if [ "$LANES" -ge 2 ]; then case " $LANE2_JOBS " in *" $1 "*) echo 2; return ;; esac; fi
    echo 1
}

# The pids of a process's descendants, children first.
descendants() { local p; for p in $(pgrep -P "$1" 2>/dev/null); do echo "$p"; descendants "$p"; done; }
signal_tree() {
    local p list
    list=$(descendants "$2")
    kill -"$1" "$2" 2>/dev/null
    for p in $list; do kill -"$1" "$p" 2>/dev/null; done
}

# The executables a job linked, tests and the crates' tools (chloedb's are
# 50 to 115 MB each, one set per feature set): most of a target directory,
# and linked again after any change to their crate, so keeping them saves
# little. Compiled dependencies, release builds and Miri's stay.
prune_dirs() {
    find "$@" \( -path '*/release' -o -path '*/miri-*' \) -prune -o \
        -path '*/deps/*' -type f -perm -u+x ! -name '*.*' -exec rm -f {} + 2>/dev/null
    return 0
}

# After a job: with one lane, every crate's; with two, the target directories
# only this lane's jobs link into (chloedb's for lane 1, chloectx's and the
# Python package's for lane 2), and in chloedb-server's, which both use, the
# test executables the job's log names (cargo test's Running lines,
# nextest's list). The rest goes when the run ends.
prune_job() {
    local rel crate
    if [ "$LANES" = 1 ]; then
        prune_dirs chloedb/target chloedb-server/target chloectx/target chloedb-py/target chloedb-crawl/target chloedb-sheets/target chloedb-netops/target chloedb-workbench/target chloedb-js/target chloedb-java/target
        return 0
    fi
    case "$LANE" in
        1) prune_dirs chloedb/target ;;
        2) prune_dirs chloectx/target chloedb-py/target chloedb-crawl/target chloedb-sheets/target chloedb-netops/target chloedb-workbench/target chloedb-js/target chloedb-java/target ;;
    esac
    { sed -n 's/^ *Running .* (\(target\/[^)]*\))$/\1/p' "$1"; cat "$1.binaries" 2>/dev/null; } | sort -u | while read -r rel; do
        case "$rel" in
            /*) rm -f "$rel" ;;
            *) rm -f "chloedb-server/$rel" ;;
        esac
    done
    return 0
}

run_job() {
    local name=$1 fn=job_$(echo "$1" | tr - _) log=$RUN_DIR/$1.log start rc secs result
    if [ "$(free_gb)" -lt "$MIN_FREE_GB" ]; then
        say "$(printf '  %-42s not run: less than %s GB free' "$name" "$MIN_FREE_GB")"
        record "$name" "not run (disk)" 0
        return
    fi
    # One lane: the job's name while it runs, its outcome on the same line.
    [ "$LANES" = 1 ] && [ "$VERBOSE" = 0 ] && printf '  %-42s ' "$name"
    start=$(now)
    JOB_DIR=$RUN_DIR JOB_LOG=$log
    if [ "$VERBOSE" = 1 ]; then
        (set -e -x; "$fn") 2>&1 | tee "$log"
        rc=${PIPESTATUS[0]}
    else
        (set -e -x; "$fn") >"$log" 2>&1
        rc=$?
    fi
    secs=$(($(now) - start))
    if [ "$rc" = 0 ]; then
        result="passed  $(mins $secs) min"
        record "$name" success "$secs"
    else
        result="FAILED  $(mins $secs) min  ($log)"
        record "$name" failure "$secs"
    fi
    if [ "$LANES" = 1 ] && [ "$VERBOSE" = 0 ]; then echo "$result"; else printf '  %-42s %s\n' "$name" "$result"; fi
    printf '  %-42s %s%s\n' "$name" "$result" "$([ -n "$RAM_DEV" ] && df -Pk "$TMPDIR" | awk 'NR == 2 { printf "  (RAM disk: %d MB used)", $3 / 1024 }')" >>"$RUN_DIR/run.log"
    [ "$rc" = 0 ] || [ "$VERBOSE" = 1 ] || tail -n 40 "$log" | sed 's/^/      | /'
    grep -E '^ +FLAKY ' "$log" 2>/dev/null | sort -u | sed "s/^ */      flaky: /" | tee -a "$RUN_DIR/run.log"
    [ "$KEEP_BINARIES" = 1 ] || prune_job "$log"
}

# A lane's jobs in order. Before each, it waits while $PAUSE exists, saying
# so with lane-N.paused; the supervisor freezes Miri once every lane waits.
run_lane() {
    local lane=$1 name
    LANE=$lane
    for name in $(job_names); do
        [ "$name" = miri ] && continue
        selected "$name" && affected "$name" || continue
        [ "$(lane_of "$name")" = "$lane" ] || continue
        [ "$FAIL_FAST" = 1 ] && failed_any && break
        if [ -e "$PAUSE" ]; then
            touch "$RUN_DIR/lane-$lane.paused"
            while [ -e "$PAUSE" ]; do sleep 5; done
            rm -f "$RUN_DIR/lane-$lane.paused"
        fi
        run_job "$name"
    done
    touch "$RUN_DIR/lane-$lane.done"
}

supervise() {
    local frozen=0 alive idle lane p
    while :; do
        alive=0
        for p in $LANE_PIDS $MIRI_PIDS; do kill -0 "$p" 2>/dev/null && alive=1; done
        [ "$alive" = 1 ] || break
        if [ -e "$PAUSE" ] && [ "$frozen" = 0 ]; then
            idle=1
            for lane in $(seq 1 "$LANES"); do
                [ -e "$RUN_DIR/lane-$lane.paused" ] || [ -e "$RUN_DIR/lane-$lane.done" ] || idle=0
            done
            if [ "$idle" = 1 ]; then
                for p in $MIRI_PIDS; do signal_tree STOP "$p"; done
                date '+%Y-%m-%dT%H:%M:%S%z' >"$CI_HOME/paused"
                frozen=1
                say "Paused: $PAUSE exists. Going on once it is removed."
            fi
        elif [ ! -e "$PAUSE" ] && [ "$frozen" = 1 ]; then
            rm -f "$CI_HOME/paused"
            for p in $MIRI_PIDS; do signal_tree CONT "$p"; done
            frozen=0
            say "Resumed."
        fi
        sleep 5
    done
    wait
}

start_miri() {
    local i=0 label tests
    # The sysroot first, once: the three parts would each build it.
    (cd chloedb && miri miri-1 setup) >"$RUN_DIR/miri-setup.log" 2>&1 \
        || { record "miri (setup)" failure 0; say "  miri: the sysroot did not build ($RUN_DIR/miri-setup.log)"; return; }
    while IFS='|' read -r label tests; do
        i=$((i + 1))
        {
            s=$(now)
            (set -e -x; miri_part "$i" "$tests") >"$RUN_DIR/miri-$i.log" 2>&1
            echo "$? $(($(now) - s))" >"$RUN_DIR/miri-$i.rc"
        } &
        MIRI_PIDS="$MIRI_PIDS $!"
    done <<EOF
$MIRI_PARTS
EOF
}

finish_miri() {
    local i=0 label tests rc secs
    [ -n "$MIRI_PIDS" ] || return 0
    while IFS='|' read -r label tests; do
        i=$((i + 1))
        read -r rc secs <"$RUN_DIR/miri-$i.rc"
        if [ "$rc" = 0 ]; then
            say "$(printf '  %-42s passed  %s min' "miri ($label)" "$(mins "$secs")")"
            record "miri ($label)" success "$secs"
        else
            say "$(printf '  %-42s FAILED  %s min  (%s)' "miri ($label)" "$(mins "$secs")" "$RUN_DIR/miri-$i.log")"
            record "miri ($label)" failure "$secs"
            [ "$VERBOSE" = 1 ] || tail -n 40 "$RUN_DIR/miri-$i.log" | sed 's/^/      | /'
        fi
    done <<EOF
$MIRI_PARTS
EOF
    MIRI_PIDS=""
}

# --fast runs only the jobs a change can affect: a job whose crate, and the
# crates it builds on, are as on main runs main's code, which main's own run
# tested. chloedb, ci.yml and this script reach every job; anything outside
# the packages' directories but docs and Markdown is taken to as well.
AFFECTED=""
find_affected() {
    local f all=0
    for f in $(git diff --name-only "$BASE_SHA" HEAD); do
        case "$f" in
            docs/*|*.md) ;;
            chloectx/*|install.sh) AFFECTED="$AFFECTED test-chloectx" ;;
            chloedb-py/*) AFFECTED="$AFFECTED python" ;;
            chloedb-js/*) AFFECTED="$AFFECTED javascript" ;;
            chloedb-java/*) AFFECTED="$AFFECTED java" ;;
            chloedb-crawl/*) AFFECTED="$AFFECTED test-chloedb-crawl server-smoke" ;;
            chloedb-sheets/*) AFFECTED="$AFFECTED test-chloedb-sheets server-smoke" ;;
            chloedb-netops/*) AFFECTED="$AFFECTED test-chloedb-netops server-smoke" ;;
            chloedb-workbench/*) AFFECTED="$AFFECTED test-chloedb-workbench" ;;
            chloedb-server/*) AFFECTED="$AFFECTED test-chloedb-server server-smoke python javascript audio vision" ;;
            *) all=1 ;;
        esac
    done
    [ "$all" = 1 ] && AFFECTED=$(job_names | tr '\n' ' ')
    return 0
}
affected() { [ "$ONLY_AFFECTED" = 1 ] || return 0; case " $AFFECTED " in *" $1 "*) return 0 ;; esac; return 1; }

# The run itself.

if [ "$SETUP_ONLY" = 1 ]; then
    setup_rust
    setup_python
    echo "Rust $RUST_VERSION and $MIRI_TOOLCHAIN in $RUSTUP_HOME; Python in $PY_VENV"
    exit 0
fi

printf '%s\n%s\n%s\n' "$$" "${HEAD_SHA:0:12}" "$(date '+%Y-%m-%dT%H:%M:%S%z')" >"$CI_HOME/running"
trap cleanup EXIT
trap 'exit 130' INT TERM
setup_rust
setup_python
setup_ramdisk

RUN_DIR=$CI_HOME/runs/$(date +%Y%m%d-%H%M%S)-${HEAD_SHA:0:7}
mkdir -p "$RUN_DIR"
RUN_START=$(now)

say "ci-local: ${HEAD_SHA:0:7}$([ "$MERGE" = 1 ] && echo " merged into $BASE ${BASE_SHA:0:7}")$([ -n "$PR" ] && echo " (pull request #$PR)")"
say "  on $MACHINE, $OS $(uname -m), Rust $RUST_VERSION$([ "$LANES" = 2 ] && echo ", 2 lanes")$([ "$NEXTEST" = 1 ] && echo ", nextest")$([ -n "$RAM_DEV" ] && echo ", temporary files on a ${RAMDISK_GB} GB RAM disk"), logs in $RUN_DIR"

DRIFT=$(ci_drift "$CI_YML")
if [ -n "$DRIFT" ]; then
    say "  Warning: ci.yml's jobs differ from what this script runs: $(echo $DRIFT | sed 's/) /), /g')."
    say "  Compare them with this script and update it (--ci-hashes prints the new hashes); no success status is posted until then."
fi

if [ "$POST" = 1 ]; then
    gh api "repos/$SLUG/commits/$HEAD_SHA" --silent 2>/dev/null || die "${HEAD_SHA:0:7} is not on github.com/$SLUG: push it first"
    post_status pending "Running on $MACHINE ($OS), merged into $BASE ${BASE_SHA:0:7}"
fi

# changes: only docs/ and Markdown files changed, so nothing is built.
CODE=true
if [ "$ALL" = 0 ] && [ "$MERGE" = 1 ]; then
    changed=$(git diff --name-only "$BASE_SHA" HEAD)
    if [ -n "$changed" ] && ! printf '%s\n' "$changed" | grep -qvE '^docs/|\.md$'; then
        CODE=false
    fi
fi

if [ "$CODE" = false ]; then
    say "  Only docs/ and Markdown files changed: every job is skipped, as on GitHub."
else
    if [ "$ONLY_AFFECTED" = 1 ]; then
        find_affected
        main_ci=$(gh api "repos/$SLUG/commits/$BASE_SHA/check-runs?check_name=CI%20result" --jq '[.check_runs[] | .conclusion // .status][0] // "none"' 2>/dev/null || echo unknown)
        case "$main_ci" in
            success|in_progress|queued) ;;
            *)
                say "  $BASE ${BASE_SHA:0:7}'s own CI result is $main_ci: running every job."
                AFFECTED=$(job_names | tr '\n' ' ') ;;
        esac
        for name in $(job_names); do
            selected "$name" && ! affected "$name" && record "$name" "skipped (unchanged)" 0
        done
        say "  Jobs the change can affect:$(for n in $(job_names); do selected "$n" && affected "$n" && printf ' %s' "$n"; done)"
    fi
    selected miri && affected miri && start_miri
    for lane in $(seq 1 "$LANES"); do
        run_lane "$lane" &
        LANE_PIDS="$LANE_PIDS $!"
    done
    supervise
    [ "$KEEP_BINARIES" = 1 ] || prune_dirs chloedb/target chloedb-server/target chloectx/target chloedb-py/target chloedb-crawl/target chloedb-sheets/target chloedb-netops/target chloedb-workbench/target chloedb-js/target chloedb-java/target
    finish_miri
fi

TOTAL=$(($(now) - RUN_START))
touch "$RUN_DIR/results"
PASSED=$(grep -c '|success|' "$RUN_DIR/results")
SKIPPED=$(grep -c '|skipped' "$RUN_DIR/results")
COUNT=$(grep -c '|' "$RUN_DIR/results")
FAILED=0
grep -v -e '|success|' -e '|skipped' "$RUN_DIR/results" | grep -q . && FAILED=1
{
    echo "head ${HEAD_SHA}"
    echo "base ${BASE_SHA} ($BASE, merged: $MERGE)"
    echo "machine $MACHINE, $OS $(uname -m), Rust $RUST_VERSION, lanes $LANES, nextest $NEXTEST"
    awk -F'|' '{ printf "%-40s %-20s %6.1f min\n", $1, $2, $3 / 60 }' "$RUN_DIR/results"
    echo "total $(mins $TOTAL) min"
} >"$RUN_DIR/summary.txt"

case "$HOST" in
    *-apple-darwin) not_covered="Linux and x86-64 AVX2/AVX-512 not run" ;;
    *) not_covered="macOS not run" ;;
esac
[ "$KAFKA" = 1 ] || not_covered="$not_covered, Kafka test skipped"
FINISHED=1
if [ "$FAILED" = 0 ]; then
    if [ "$CODE" = false ]; then
        say "CI result: passed (only documentation changed)"
        post_status success "Only documentation changed: nothing to build"
    else
        say "CI result: passed, $PASSED of $COUNT jobs$([ "$SKIPPED" -gt 0 ] && echo " ($SKIPPED skipped: unchanged since $BASE)"), $(mins $TOTAL) min"
        if [ -n "$DRIFT" ]; then
            post_status error "Passed, but ci.yml has changed since ci-local.sh was written: update the script"
        else
            post_status success "$PASSED/$COUNT jobs passed$([ "$SKIPPED" -gt 0 ] && echo ", $SKIPPED unchanged") on $MACHINE$([ "$NEXTEST" = 1 ] && echo ", nextest"), merged into $BASE ${BASE_SHA:0:7}, $(mins $TOTAL) min; $not_covered"
        fi
    fi
    if [ "$POST" = 1 ] && [ -z "$DRIFT" ]; then
        pr=$PR
        [ -n "$pr" ] || pr=$(gh api "repos/$SLUG/commits/$HEAD_SHA/pulls" --jq '[.[] | select(.state == "open")][0].number // empty' 2>/dev/null)
        [ -n "$pr" ] && say "To merge what was tested: gh pr merge $pr -R $SLUG --merge --match-head-commit $HEAD_SHA"
    fi
    exit 0
else
    failed=$(awk -F'|' '$2 != "success" && $2 !~ /^skipped/ { printf "%s%s", sep, $1; sep = ", " }' "$RUN_DIR/results")
    say "CI result: FAILED ($failed), $(mins $TOTAL) min"
    post_status failure "Failed on $MACHINE: $failed"
    exit 1
fi
