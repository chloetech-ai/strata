# Engram: instructions for coding agents

This file is for a coding agent (Claude Code or another) that was asked to set
Engram up for its user, or to work on this repository. People should start
with [README.md](README.md) and [the install guide](docs/engram/setup.md).

## Set Engram up for the user's Claude Code

Engram plugs into Claude Code with hooks and an MCP server, so installing it
changes the user's Claude Code configuration. **Ask the user which scope they
want before running anything:**

| scope | command | what it changes |
|---|---|---|
| every session on this machine | `./install.sh` | merges hooks, a permission and `env.ENGRAM_DISTILL_MODEL` into `~/.claude/settings.json` (backed up first as `settings.json.engram-<time>.bak`) and registers the `engram` MCP server at user scope |
| one project (good for a first try) | `./install.sh --project /absolute/path` | writes the project's `.claude/settings.local.json` and `.mcp.json` only |

Both install the binary to `~/.local/bin/engram` and download a 129 MB
embedding model into `~/.local/share/engram/models`.

### 1. Check the prerequisites

Check, and do not install anything without asking the user:

```bash
uname -s                 # Darwin or Linux (on Windows, work inside WSL 2)
cargo --version          # Rust, stable; if missing, ask before installing it from https://rustup.rs
cc --version             # a C compiler (macOS: xcode-select --install; Debian/Ubuntu: build-essential)
curl --version
claude auth status       # must show "loggedIn": true
```

If `claude auth status` shows `"loggedIn": false`, ask the user to run
`claude auth login` in their own terminal (it is interactive). Engram still
records and recalls without it, but its distiller runs `claude -p` after each
turn and fails, so no decisions or rules get written down.

### 2. Install

From the repository root. It is non-interactive, and the first run compiles
for several minutes (a release build), so give the command a long timeout
(10 minutes) or run it in the background and wait for it:

```bash
./install.sh                              # or: ./install.sh --project /absolute/path
```

Options: `--distill-model NAME` (default `haiku`), `--prefix DIR` (default
`~/.local`), `--no-wire` (build and install the binary only). The script stops
at the first failed step with a message saying what is missing, ends by running
`engram doctor`, and exits non-zero if any doctor check failed.

### 3. Verify

```bash
cd /path/to/the/project && ~/.local/bin/engram doctor
```

Exit code 0 means ready. Each line is `ok`, `warn` (advisory) or `FAIL`, and a
`FAIL` line says how to fix it. Report any `FAIL` to the user rather than
editing their settings by hand.

### 4. Tell the user what happens next

- Engram takes effect in **new** Claude Code sessions. The session you are
  running in started before the hooks existed: it records nothing and has no
  `memory_*` tools until the user starts a new one.
- `~/.local/bin/engram ui` opens a local inspector of what was stored, per
  project and per session. It prints a URL with an access token; on a remote
  machine, forward the port first (`ssh -L 8765:127.0.0.1:8765 host`).
- To remove it: `~/.local/bin/engram uninstall --user` (or
  `engram uninstall --project /absolute/path`). Stores are kept under
  `~/.local/share/engram/projects/`.

### Do not

- edit `~/.claude/settings.json`, `~/.claude.json` or a project's `.mcp.json`
  by hand; `engram install` and `engram uninstall` merge their entries and
  leave everything else alone;
- commit a project's `.claude/settings.local.json` or `.mcp.json` after a
  project install unless the user wants that: both contain the absolute path
  of the engram binary on this machine;
- copy, upload or paste the contents of a store (`memory.strata`,
  `events.log`): they are transcripts of the user's sessions.

## Work on this repository

| path | what |
|---|---|
| `engram/` | the memory system (Rust crate): hooks client, per-project daemon, MCP server, recall, distiller, `engram ui`, `engram doctor` |
| `engram/demo/index.html` | the inspector page; it is compiled into the binary with `include_str!`, so rebuild after editing it |
| `strata/` | the database (Rust crate), including the posting-list study in `src/postings/` |
| `strata/fuzz/` | cargo-fuzz targets (nightly: `cargo +nightly fuzz run <target>` there) feeding Strata damaged files and arbitrary SQL; `strata/tests/corrupt_input.rs` runs the same targets on stable |
| `strata-server/` | Strata over HTTP JSON and gRPC, a coordinator over shards, and the query UI (`ui/index.html`, compiled in with `include_str!`) |
| `strata-py/` | Strata from Python (`import strata`): a PyO3 extension built with maturin, and an HTTP client of strata-server; tests in `strata-py/tests` |
| `.github/workflows/ci.yml` | CI: tests and clippy for the three crates on Linux and macOS, Strata's fault-injection tests (`--features fault-injection`), a relevance gate (BM25 on three BEIR collections), the Python package's tests, and a server smoke test |
| `docs/` | the documentation site (Jekyll, just-the-docs); `docs/demo/inspector.html` is a separate static copy of the inspector |
| `install.sh` | the installer described above |

Build and test each crate from its own directory (`engram` and
`strata-server` depend on `strata` by path):

```bash
cd strata && cargo test --release
cd engram && cargo test --release
cd strata-server && cargo test --release
```

The Python package builds into a virtualenv (its client tests start
`strata-server/target/release/strata-server`, so build that first):

```bash
cd strata-py && python3 -m venv .venv && . .venv/bin/activate
pip install maturin pytest && maturin develop --release && pytest tests
```

CI also runs `cargo clippy --release --all-targets -- -A clippy::style -A
clippy::complexity -A clippy::perf -D warnings` in each crate.

Benchmark numbers in `docs/`, the reports and `*/results/` are measured, not
edited: change them only by re-running the benchmark, and name the machine a
number came from.

A change to Strata's ranking fails CI's relevance gate until the new numbers
are committed. The test writes them:

```bash
strata/tests/fixtures/beir/fetch.sh ~/beir    # pinned by hash
cd strata && STRATA_BEIR_DIR=~/beir STRATA_RELEVANCE_GATE=update cargo test --release --test relevance_gate
```

Say in the pull request why the numbers moved.
