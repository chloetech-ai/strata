#!/bin/sh
# Builds Engram from this checkout, installs the binary, downloads the
# embedding model and wires Engram into Claude Code. Run it again after
# `git pull` to upgrade. Non-interactive; exits non-zero when a step fails or
# when `engram doctor` reports a failed check. Agents: see AGENTS.md.
#
#   ./install.sh                         every Claude Code session on this machine
#   ./install.sh --project ~/code/app    one project only
#
# Options:
#   --project DIR          install for one project instead of every session
#   --distill-model NAME   model that writes down decisions after each turn
#                          (default: haiku; "" uses the Claude Code CLI's default)
#   --prefix DIR           where the binary goes (default: ~/.local, so ~/.local/bin/engram)
#   --no-wire              build and install the binary only; leave Claude Code alone
set -eu

PREFIX="$HOME/.local"
PROJECT=""
DISTILL="haiku"
WIRE=1
while [ $# -gt 0 ]; do
  case "$1" in
    --project) PROJECT="$2"; shift 2 ;;
    --distill-model) DISTILL="$2"; shift 2 ;;
    --prefix) PREFIX="$2"; shift 2 ;;
    --no-wire) WIRE=0; shift ;;
    -h|--help) sed -n '2,15p' "$0"; exit 0 ;;
    *) echo "install.sh: unknown option $1 (see --help)" >&2; exit 2 ;;
  esac
done

say() { printf '==> %s\n' "$*"; }
die() { printf 'install.sh: %s\n' "$*" >&2; exit 1; }

ROOT="$(cd "$(dirname "$0")" && pwd)"
[ -f "$ROOT/engram/Cargo.toml" ] || die "run this from a checkout of https://github.com/rkonow/engram"

case "$(uname -s)" in
  Darwin|Linux) ;;
  *) die "Engram needs macOS or Linux (on Windows, use WSL)" ;;
esac

# Rust, a C compiler (for the tokenizer's regex library) and curl.
if ! command -v cargo >/dev/null 2>&1; then
  [ -x "$HOME/.cargo/bin/cargo" ] && PATH="$HOME/.cargo/bin:$PATH"
fi
command -v cargo >/dev/null 2>&1 || die "cargo not found; install Rust from https://rustup.rs, then run this again"
command -v cc >/dev/null 2>&1 || die "no C compiler (cc); on macOS run xcode-select --install, on Debian/Ubuntu apt install build-essential"
command -v curl >/dev/null 2>&1 || die "curl not found"
if ! command -v claude >/dev/null 2>&1; then
  [ "$WIRE" = 1 ] && die "the Claude Code CLI (claude) is not on PATH; install it from https://claude.com/claude-code, or pass --no-wire"
fi

say "building engram ($(cargo --version))"
(cd "$ROOT/engram" && cargo build --release --bin engram)

BIN="$PREFIX/bin/engram"
mkdir -p "$PREFIX/bin"
# Replace the file rather than writing into it: running daemons keep the old one.
TARGET="${CARGO_TARGET_DIR:-target}"
case "$TARGET" in /*) ;; *) TARGET="$ROOT/engram/$TARGET" ;; esac
cp "$TARGET/release/engram" "$BIN.new" && mv -f "$BIN.new" "$BIN"
say "installed $BIN"
case ":$PATH:" in
  *":$PREFIX/bin:"*) ;;
  *) say "note: $PREFIX/bin is not on your PATH; add it to use engram from a shell (the hooks use the full path)" ;;
esac

# Daemons from an earlier build pick up the new one when they restart.
"$BIN" stop --all >/dev/null 2>&1 || true

if [ -f "${ENGRAM_MODELS:-${ENGRAM_HOME:-${XDG_DATA_HOME:-$HOME/.local/share}/engram}/models}/potion-retrieval-32M/model.safetensors" ]; then
  say "embedding model already downloaded"
else
  say "downloading the embedding model (potion-retrieval-32M, 129 MB)"
  "$BIN" fetch-model
fi

[ "$WIRE" = 1 ] || { say "done (Claude Code not changed: --no-wire)"; exit 0; }

if [ -n "$PROJECT" ]; then
  say "wiring Engram into $PROJECT"
  "$BIN" install --project "$PROJECT" ${DISTILL:+--distill-model "$DISTILL"}
  cd "$PROJECT"
else
  say "wiring Engram into every Claude Code session"
  "$BIN" install --user ${DISTILL:+--distill-model "$DISTILL"}
fi

say "checking"
STATUS=0
"$BIN" doctor || STATUS=$?

cat <<EOF

Next:
  - Start Claude Code (claude) in a project and work as usual. Sessions that
    were already running pick Engram up when they restart.
  - $BIN ui            browse what Engram stored, per project and session
  - $BIN doctor        check the setup again
  - $BIN uninstall --user   (or: uninstall in the project) to remove it; stores are kept
EOF
[ "$STATUS" = 0 ] || say "engram doctor reported a problem above; fix it and run $BIN doctor again"
exit "$STATUS"
