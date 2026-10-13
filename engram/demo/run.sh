#!/bin/sh
# Builds Engram if needed and starts the live inspector (http://127.0.0.1:8765).
# Arguments go to demo/server.py, e.g. --model <model> --distill-model <model>.
set -e
cd "$(dirname "$0")/.."
cargo build --release --bin engram
exec python3 demo/server.py "$@"
