#!/bin/sh
# Installs ChloeCtx from this checkout: builds the binary (or takes the
# release's prebuilt one, below), installs it, downloads the embedding model
# and wires ChloeCtx into Claude Code. Run it again after `git pull` to
# upgrade. Non-interactive; exits non-zero when a step fails or when
# `chloectx doctor` reports a failed check. Agents: see AGENTS.md.
#
#   ./install.sh                         every Claude Code session on this machine
#   ./install.sh --project ~/code/app    one project only
#
# Options:
#   --project DIR          install for one project instead of every session
#   --distill-model NAME   model that writes down decisions after each turn
#                          (default: haiku; "" uses the Claude Code CLI's default)
#   --prefix DIR           where the binary goes (default: ~/.local, so ~/.local/bin/chloectx;
#                          ~/.local/bin/engram, its name before the rename, is a link to it)
#   --no-wire              install the binary only; leave Claude Code alone
#   --no-model             do not download the embedding model (fetch it later
#                          with chloectx fetch-model)
#   --prebuilt             take the release's prebuilt chloectx; never build
#   --source               build from this checkout; never download the binary
#
# Prebuilt or built: each release (chloedb-server-v<version>, from 0.3.2)
# has an archive of chloectx per platform (Linux x86_64 and aarch64 with
# glibc 2.35 or later, macOS 11 or later on arm64 and x86_64). By default
# this takes it, checked against the release's SHA-256, when the checkout
# is that release's (the tag of chloectx/Cargo.toml's version, with
# chloectx/ and chloedb/ unchanged since), or when there is no Rust here
# (then the release of the checkout's version, which a checkout of main
# can be ahead of); otherwise, or when the download fails and Rust is
# here, it builds. CHLOEDB_RELEASES_URL replaces the releases' address
# (laid out as <url>/chloedb-server-v<version>/chloectx-<platform>.tar.gz).
set -eu

PREFIX="$HOME/.local"
PROJECT=""
DISTILL="haiku"
WIRE=1
MODEL=1
MODE=auto
while [ $# -gt 0 ]; do
  case "$1" in
    --project|--distill-model|--prefix)
      [ $# -gt 1 ] || { echo "install.sh: $1 needs a value (see --help)" >&2; exit 2; }
      case "$1" in
        --project) PROJECT="$2" ;;
        --distill-model) DISTILL="$2" ;;
        --prefix) PREFIX="$2" ;;
      esac
      shift 2 ;;
    --no-wire) WIRE=0; shift ;;
    --no-model) MODEL=0; shift ;;
    --prebuilt) MODE=prebuilt; shift ;;
    --source) MODE=source; shift ;;
    -h|--help) sed -n '2,/^set -eu$/p' "$0" | sed -e '$d' -e 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "install.sh: unknown option $1 (see --help)" >&2; exit 2 ;;
  esac
done

say() { printf '==> %s\n' "$*"; }
die() { printf 'install.sh: %s\n' "$*" >&2; exit 1; }

ROOT="$(cd "$(dirname "$0")" && pwd)"
[ -f "$ROOT/chloectx/Cargo.toml" ] || die "run this from a checkout of https://github.com/chloetech-ai/chloedb"

case "$(uname -s)" in
  Darwin|Linux) ;;
  *) die "ChloeCtx needs macOS or Linux (on Windows, use WSL)" ;;
esac

command -v curl >/dev/null 2>&1 || die "curl not found"
if ! command -v claude >/dev/null 2>&1; then
  if [ "$WIRE" = 1 ]; then die "the Claude Code CLI (claude) is not on PATH; install it from https://claude.com/claude-code, or pass --no-wire"; fi
fi
# Rust and a C compiler (for the tokenizer's regex library), to build.
if ! command -v cargo >/dev/null 2>&1; then
  [ -x "$HOME/.cargo/bin/cargo" ] && PATH="$HOME/.cargo/bin:$PATH"
fi
toolchain() {
  command -v cargo >/dev/null 2>&1 || { echo "cargo not found; install Rust from https://rustup.rs, then run this again"; return 1; }
  command -v cc >/dev/null 2>&1 || { echo "no C compiler (cc); on macOS run xcode-select --install, on Debian/Ubuntu apt install build-essential"; return 1; }
}

BIN="$PREFIX/bin/chloectx"
VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' "$ROOT/chloectx/Cargo.toml" | head -n 1)
TAG="chloedb-server-v$VERSION"
RELEASES_URL="${CHLOEDB_RELEASES_URL:-https://github.com/chloetech-ai/chloedb/releases/download}"

# Replace the file rather than writing into it: running daemons keep the old one.
put() {
  mkdir -p "$PREFIX/bin"
  cp "$1" "$BIN.new" && chmod 755 "$BIN.new" && mv -f "$BIN.new" "$BIN"
}

build() {
  why=$(toolchain) || die "$why"
  say "building ChloeCtx ($(cargo --version))"
  # --locked: the versions in chloectx/Cargo.lock (the ones CI tested), not
  # whatever newer ones the registry offers today.
  (cd "$ROOT/chloectx" && cargo build --locked --release --bin chloectx) ||
    die "the build failed; if cargo says the lock file needs updating, this checkout's chloectx/Cargo.lock is out of date (git pull, or git checkout chloectx/Cargo.lock)"
  TARGET="${CARGO_TARGET_DIR:-target}"
  case "$TARGET" in /*) ;; *) TARGET="$ROOT/chloectx/$TARGET" ;; esac
  put "$TARGET/release/chloectx"
}

# The release's platform name for this machine, as the release archives
# have it; prints why not and fails when there is none.
platform() {
  os=$(uname -s) arch=$(uname -m)
  case "$os" in Linux) os=linux ;; Darwin) os=macos ;; esac
  case "$arch" in
    x86_64|amd64) arch=x86_64 ;;
    aarch64|arm64) if [ "$os" = macos ]; then arch=arm64; else arch=aarch64; fi ;;
    *) echo "no prebuilt chloectx for $arch"; return 1 ;;
  esac
  # A shell under Rosetta on Apple silicon says x86_64: take the native build.
  if [ "$os" = macos ] && [ "$arch" = x86_64 ] && [ "$(sysctl -n sysctl.proc_translated 2>/dev/null || echo 0)" = 1 ]; then arch=arm64; fi
  if [ "$os" = linux ]; then
    glibc=$( (getconf GNU_LIBC_VERSION 2>/dev/null || true) | sed -n 's/^glibc \([0-9][0-9]*\.[0-9][0-9]*\).*/\1/p')
    [ -n "$glibc" ] || { echo "the prebuilt chloectx needs glibc, and this Linux has none (musl?)"; return 1; }
    major=${glibc%%.*} minor=${glibc#*.}
    if [ "$major" -lt 2 ] || { [ "$major" -eq 2 ] && [ "$minor" -lt 35 ]; }; then
      echo "the prebuilt chloectx needs glibc 2.35 or later, and this machine has $glibc"; return 1
    fi
  fi
  echo "$os-$arch"
}

sha256() {
  if command -v sha256sum >/dev/null 2>&1; then sha256sum "$1" | cut -d' ' -f1
  else shasum -a 256 "$1" | cut -d' ' -f1; fi
}

# Downloads release $VERSION's chloectx for this machine, checks it against
# the release's SHA-256 and installs it; says why and fails when it cannot.
prebuilt() {
  plat=$(platform) || { say "$plat"; return 1; }
  name="chloectx-$plat"
  url="$RELEASES_URL/$TAG/$name.tar.gz"
  tmp=$(mktemp -d 2>/dev/null || mktemp -d -t chloectx)
  say "downloading the prebuilt chloectx $VERSION for $plat"
  say "  from $url"
  if ! curl -fsSL --retry 3 -o "$tmp/$name.tar.gz" "$url" || ! curl -fsSL --retry 3 -o "$tmp/$name.tar.gz.sha256" "$url.sha256"; then
    say "release $VERSION has no chloectx for $plat there (releases have it from 0.3.2)"; rm -rf "$tmp"; return 1
  fi
  want=$(cut -d' ' -f1 < "$tmp/$name.tar.gz.sha256") got=$(sha256 "$tmp/$name.tar.gz")
  if [ -z "$want" ] || [ "$want" != "$got" ]; then
    say "the download's SHA-256 is $got, and the release says $want: not installing it"; rm -rf "$tmp"; return 1
  fi
  if ! tar -xzf "$tmp/$name.tar.gz" -C "$tmp" || [ ! -f "$tmp/$name/chloectx" ]; then
    say "the archive has no $name/chloectx"; rm -rf "$tmp"; return 1
  fi
  if ! "$tmp/$name/chloectx" --version >/dev/null 2>&1; then
    say "the prebuilt chloectx does not start on this machine"; rm -rf "$tmp"; return 1
  fi
  put "$tmp/$name/chloectx"
  rm -rf "$tmp"
}

# Is this checkout the release's: the tag of its version, with ChloeCtx and
# the database it is built on unchanged since (committed or not)?
release_checkout() {
  command -v git >/dev/null 2>&1 && [ -e "$ROOT/.git" ] || return 1
  git -C "$ROOT" rev-parse -q --verify "refs/tags/$TAG^{commit}" >/dev/null 2>&1 || return 1
  git -C "$ROOT" diff --quiet "$TAG" -- chloectx chloedb 2>/dev/null
}

case "$MODE" in
  source) build ;;
  prebuilt) prebuilt || die "no prebuilt chloectx $VERSION to install (above); build it from this checkout with --source" ;;
  auto)
    if release_checkout; then
      say "this checkout is release $VERSION's ($TAG): taking its prebuilt chloectx (--source builds instead)"
      prebuilt || { say "building instead"; build; }
    elif why=$(toolchain); then
      build
    else
      say "$why"
      say "no Rust here: taking the prebuilt chloectx of release $VERSION (this checkout may be newer; install Rust and pass --source to build it)"
      prebuilt || die "no prebuilt chloectx $VERSION either (above); $why"
    fi ;;
esac
# ChloeCtx was called engram: for a release, that name runs it too (an old
# binary there is replaced; daemons running it keep it until they stop).
ln -sf chloectx "$PREFIX/bin/engram.new" && mv -f "$PREFIX/bin/engram.new" "$PREFIX/bin/engram" # the name before the rename
say "installed $BIN (and $PREFIX/bin/engram, its name before the rename, as a link to it)"
case ":$PATH:" in
  *":$PREFIX/bin:"*) ;;
  *) say "note: $PREFIX/bin is not on your PATH; add it to use chloectx from a shell (the hooks use the full path)" ;;
esac

# Daemons from an earlier build pick up the new one when they restart.
"$BIN" stop --all >/dev/null 2>&1 || true

if [ "$MODEL" = 0 ]; then
  [ "$WIRE" = 1 ] && say "note: --no-model: run $BIN fetch-model before the first session; recall needs the model"
fi

# Where the stores and the model live, as chloectx picks it: a machine that
# has the folder from before the rename (engram) and not the new one keeps
# using it, and nothing is moved.
DATA="${XDG_DATA_HOME:-$HOME/.local/share}"
if [ -d "$DATA/engram" ] && [ ! -e "$DATA/chloectx" ]; then HOMEDIR="$DATA/engram"; else HOMEDIR="$DATA/chloectx"; fi
# ENGRAM_* are the settings' names before the rename, which chloectx reads too.
HOMEDIR="${CHLOECTX_HOME:-${ENGRAM_HOME:-$HOMEDIR}}" # before the rename: ENGRAM_HOME
MODELS="${CHLOECTX_MODELS:-${ENGRAM_MODELS:-$HOMEDIR/models}}" # before the rename: ENGRAM_MODELS
# fetch-model keeps the files already there with the pinned bytes and
# downloads the others (a download cut short among them), each checked
# against its SHA-256 before it is moved into place.
if [ "$MODEL" = 0 ]; then
  :
elif [ -f "$MODELS/potion-retrieval-32M/model.safetensors" ]; then
  say "checking the embedding model (potion-retrieval-32M)"
else
  say "downloading the embedding model (potion-retrieval-32M, 129 MB)"
fi
if [ "$MODEL" = 1 ]; then
  "$BIN" fetch-model || die "the embedding model did not download or did not match its pinned SHA-256; run ./install.sh again"
fi

[ "$WIRE" = 1 ] || { say "done (Claude Code not changed: --no-wire)"; exit 0; }

if [ -n "$PROJECT" ]; then
  say "wiring ChloeCtx into $PROJECT"
  "$BIN" install --project "$PROJECT" ${DISTILL:+--distill-model "$DISTILL"}
  cd "$PROJECT"
else
  say "wiring ChloeCtx into every Claude Code session"
  "$BIN" install --user ${DISTILL:+--distill-model "$DISTILL"}
fi

say "checking"
STATUS=0
"$BIN" doctor || STATUS=$?

cat <<EOF

Next:
  - Start Claude Code (claude) in a project and work as usual. Sessions that
    were already running pick ChloeCtx up when they restart.
  - $BIN ui            browse what ChloeCtx stored, per project and session
  - $BIN doctor        check the setup again
  - $BIN uninstall --user   (or: uninstall in the project) to remove it; stores are kept
EOF
[ "$STATUS" = 0 ] || say "chloectx doctor reported a problem above; fix it and run $BIN doctor again"
exit "$STATUS"
