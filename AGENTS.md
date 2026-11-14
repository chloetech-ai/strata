# ChloeDB and ChloeCtx: instructions for coding agents

This repository holds ChloeDB, an embedded search database with SQL, and
ChloeCtx, long-term memory for Claude Code built on it. This file is for a
coding agent (Claude Code or another) that was asked to set ChloeCtx up for its
user, or to work on this repository. People should start with
[README.md](README.md) and [the install guide](docs/chloectx/setup.md).

ChloeDB was called Strata until November 2026, and the rename lands in steps.
The folders, crates, programs, settings (`CHLOEDB_*`), headers
(`x-chloedb-*`), metrics (`chloedb_*`) and packages (`import chloedb`,
`ai.chloetech.chloedb`) carry the new name, and the old ones are still
accepted for a release. The container image is
`ghcr.io/chloetech-ai/chloedb-server`, published as `strata-server` too for
a release. The repository is `chloetech-ai/chloedb` (the old address,
`chloetech-ai/strata`, redirects; the website is
https://chloetech-ai.github.io/chloedb/, and its old address does not
redirect). The Kubernetes operator's resources (the
`StrataCluster` kind, the `strata-system` namespace; the operator's source
is in `chloedb-server/src/operator/` and `deploy/k8s/`, and its pods start
the programs by their old names, which every image has), the app engine
(`chloedb-apps/`) and the internals (the on-disk format, the gRPC services,
`memory.strata`) keep the old name for now: use the names as
this file and the code spell them, and say ChloeDB in prose. A branch started
before the folders moved comes across with `scripts/chloedb-port.sh`.

ChloeCtx was called Engram until November 2026. Its folder (`chloectx/`),
program (`chloectx`; `engram` is a link to it for a release), MCP server
(`chloectx`, tools `mcp__chloectx__*`) and settings (`CHLOECTX_*`, with
`ENGRAM_*` still read) carry the new name, and `chloectx install` moves a
user's Claude Code entries from the old names. A machine that has the old
store folder, `~/.local/share/engram`, keeps using it, and the stores'
files (`memory.strata`, `engram.sock`) keep their names. A branch started
before the folder moved comes across with `scripts/chloectx-port.sh`.

## Set ChloeCtx up for the user's Claude Code

ChloeCtx plugs into Claude Code with hooks and an MCP server, so installing it
changes the user's Claude Code configuration. **Ask the user which scope they
want before running anything:**

| scope | command | what it changes |
|---|---|---|
| every session on this machine | `./install.sh` | merges hooks, a permission and `env.CHLOECTX_DISTILL_MODEL` into `~/.claude/settings.json` (backed up first as `settings.json.chloectx-<time>.bak`) and registers the `chloectx` MCP server at user scope |
| one project (good for a first try) | `./install.sh --project /absolute/path` | writes the project's `.claude/settings.local.json` and `.mcp.json` only |

Both install the binary to `~/.local/bin/chloectx` and download a 129 MB
embedding model into `~/.local/share/chloectx/models` (on a machine that
already has `~/.local/share/engram`, from before the rename, into that one).

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
`claude auth login` in their own terminal (it is interactive). ChloeCtx still
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
`~/.local`), `--no-wire` (build and install the binary only), `--workbench`
(also build and install `chloedb-workbench`, which wires ChloeCtx into each
session it starts and leaves Claude Code's settings alone). The script stops
at the first failed step with a message saying what is missing, ends by running
`chloectx doctor`, and exits non-zero if any doctor check failed.

### 3. Verify

```bash
cd /path/to/the/project && ~/.local/bin/chloectx doctor
```

Exit code 0 means ready. Each line is `ok`, `warn` (advisory) or `FAIL`, and a
`FAIL` line says how to fix it. Report any `FAIL` to the user rather than
editing their settings by hand.

### 4. Tell the user what happens next

- ChloeCtx takes effect in **new** Claude Code sessions. The session you are
  running in started before the hooks existed: it records nothing and has no
  `memory_*` tools until the user starts a new one.
- `~/.local/bin/chloectx ui` opens a local inspector of what was stored, per
  project and per session. It prints a URL with an access token; on a remote
  machine, forward the port first (`ssh -L 8765:127.0.0.1:8765 host`).
- To upgrade it later: `~/.local/bin/chloectx upgrade` (pulls the checkout
  it was built from, rebuilds and reinstalls; `--check` only says whether a
  newer version is there). `chloectx stores` lists every store and
  `chloectx prune` removes those of folders that are gone or temporary; it
  asks first, so let the user run it or confirm the list with them.
- To remove it: `~/.local/bin/chloectx uninstall --user` (or
  `chloectx uninstall --project /absolute/path`). Stores are kept under
  `~/.local/share/chloectx/projects/` (or `~/.local/share/engram/projects/`).

### Do not

- edit `~/.claude/settings.json`, `~/.claude.json` or a project's `.mcp.json`
  by hand; `chloectx install` and `chloectx uninstall` merge their entries and
  leave everything else alone;
- commit a project's `.claude/settings.local.json` or `.mcp.json` after a
  project install unless the user wants that: both contain the absolute path
  of the chloectx binary on this machine;
- copy, upload or paste the contents of a store (`memory.strata`,
  `events.log`): they are transcripts of the user's sessions.

## Work on this repository

| path | what |
|---|---|
| `chloectx/` | the memory system (Rust crate): hooks client, per-project daemon, MCP server, recall, distiller, `chloectx ui`, `chloectx doctor` |
| `chloectx/src/app.rs`, `chloectx/ui/view.js` | `chloectx app`: ChloeCtx as an app of a cluster (`chloedb-server --app chloectx=URL`), its spaces in the cluster's tables (`ctx_<space>`, through `Store::open_remote` and `src/cluster.rs`), with Context and Seen before; the view is compiled in (`CHLOECTX_UI_DIR=chloectx/ui` reads it from disk); `tests/app_cluster.rs` runs it against a real cluster; on Kubernetes the operator's `spec.chloectx` runs it beside a cluster, from chloedb-server's image, which builds it too ([docs](docs/chloectx/app.md)) |
| `chloectx/demo/index.html` | the inspector page; it is compiled into the binary with `include_str!`, so rebuild after editing it |
| `chloedb/` | the database (Rust crate), including the posting-list study in `src/postings/` |
| `chloedb/fuzz/` | cargo-fuzz targets (nightly: `cargo +nightly fuzz run <target>` there) feeding ChloeDB damaged files and arbitrary SQL; `chloedb/tests/corrupt_input.rs` runs the same targets on stable |
| `chloedb-server/` | ChloeDB over HTTP JSON and gRPC, a coordinator over shards, and the UI: the command center (`ui/index.html` and the modules under `ui/app/`, listed in `src/ui.rs`) and the query console (`ui/classic.html`), compiled in with `include_str!`; `CHLOEDB_UI_DIR=chloedb-server/ui` serves them from disk without a rebuild |
| `chloedb-crawl/` | the crawler (Rust, a client of chloedb-server over HTTP): `chloedb-crawl crawl URL --local DIR` crawls into a cluster it starts and grows; `serve` is its API, which the command center shows as an app (`chloedb-server --app crawler=URL`); its view is `ui/view.js`, compiled in (`CHLOEDB_CRAWL_UI_DIR=chloedb-crawl/ui` reads it from disk) |
| `chloedb-sheets/` | the spreadsheet app (Rust, a client of chloedb-server over HTTP, as the crawler is): workbooks whose cells are rows of ChloeDB tables, with IronCalc's formulas, the cluster's tables opened as live sheets, SQL query blocks, xlsx, CSV and SQLite in and out; `serve --local DIR` starts a server and is its app `sheets`; its view is `ui/view.js`, compiled in (`CHLOEDB_SHEETS_UI_DIR=chloedb-sheets/ui` reads it from disk); `tests/cluster.rs` runs it against a real chloedb-server |
| `chloedb-netops/` | network operations on ChloeDB (Rust, a client of chloedb-server over HTTP, as the crawler is): the shift's worklist, alert rules as SQL, incidents grouped over the topology graph with a board of panels per incident (`src/hub.rs`), a Diagnose page that says where in the stack and the topology something is broken (`src/debug.rs`), a zoomable map of a simulated estate of about 200,000 objects behind the backbone, with the walk around one object (`src/estate.rs`) and a search asked in a sentence that becomes one statement (`src/ask.rs`), changes, customers, carriers' notices, agents that keep their SQL, a leadership view; `serve --local DIR` starts a server and is its app `netops` with a simulated carrier network (`src/world.rs`, `src/telemetry.rs`), `--no-simulate` reads the network from the inventory tables; its view is `ui/view.js`, compiled in (`CHLOEDB_NETOPS_UI_DIR=chloedb-netops/ui` reads it from disk); `tests/cluster.rs` runs it against a real chloedb-server; on Kubernetes the operator's `spec.netops` ([docs](docs/chloedb/netops.md)) |
| `chloedb-workbench/` | Workbench (Rust, an early local app): many Claude Code sessions side by side, each in a real terminal and most often its own git worktree, with their state and events as rows of an embedded ChloeDB; `serve` is the supervisor, the HTTP API, a stream of changes, a WebSocket per terminal and the UI (`ui/`, compiled in; `CHLOEDB_WORKBENCH_UI_DIR=chloedb-workbench/ui` reads it from disk); `hook` is what each session's hooks run, and `mcp` gives agents `spawn_agent` and its siblings to start agents you watch; [SPEC.md](chloedb-workbench/SPEC.md) is the contract; its tests use a fake `claude` and call no model |
| `chloedb-js/` | ChloeDB from JavaScript (`import { open } from "chloedb"`): a Node-API addon (napi-rs) that Node, Bun and Deno load, and a `fetch` client of chloedb-server (`client.js`); `npm run build -- --profile ci && npm test` there |
| `chloedb-py/` | ChloeDB from Python (`import chloedb`): a PyO3 extension built with maturin, and an HTTP client of chloedb-server; the framework adapters in pure Python (`chloedb.langchain`, `chloedb.langgraph`, `chloedb.llama_index`, over `_vectors.py` and `_adapters.py`) and a DB-API 2.0 driver (`chloedb.dbapi`), [docs](docs/chloedb/frameworks.md); tests in `chloedb-py/tests` |
| `chloedb-java/` | ChloeDB from Java (`ai.chloetech.chloedb`, and a JDBC driver, `jdbc:chloedb:`, in `ai.chloetech.chloedb.jdbc`): a JNI library (the Rust crate, `src/*.rs`) and the Java package (`src/main/java`), built together by `mvn package` (Maven's output in `target/java`); JUnit tests in `src/test/java` |
| `chloedb-apps/` | a prototype app engine on ChloeDB (Node): apps built from a description and changed by request, their code generated by `claude -p`, checked, stored as rows in ChloeDB and run sandboxed (views in iframes, functions in QuickJS); `./run.sh` starts it, `npm test` there (`STRATA_APPS_TEST_SHARDS=2` on a cluster); beside a cluster it is the app `apps` (the operator's `spec.apps`, its image from `chloedb-apps/deploy/Dockerfile`, built by `.github/workflows/image-apps.yml`; its command center page is `web/cc-view.js`) |
| `.github/workflows/ci.yml` | CI: tests and clippy for the crates (chloedb, chloectx, chloedb-server, chloedb-crawl, chloedb-sheets, chloedb-netops, chloedb-workbench) on Linux and macOS, ChloeDB's fault-injection tests (`--features fault-injection`), a relevance gate (BM25 on three BEIR collections, and the analyzers of other languages on two MIRACL collections), Miri over ChloeDB's byte views (nightly; tests sized for it are named `miri_*`), the Python and Java packages' tests, and a server smoke test; a change only under an app or a package runs that one's jobs and the jobs that use it (`.github/scripts/ci_changes.py`), chloedb-server's binary is built once per source tree (job `server-bin`) and its longest jobs run on an on-demand DigitalOcean runner when it is set up ([.github/ci-runner](.github/ci-runner/README.md)); one job, `CI result`, sums them up and is the check main requires (a pull request and the merge queue run nothing on macOS; a push to main runs ChloeDB's macOS tests, which it does not wait for, and the nightly run all of them) |
| `.github/workflows/chaos.yml` | every night (or by hand, with a list of scenarios), `chloedb-chaos run` plays the chaos monkey's scenarios on a local cluster on one runner, as root, and fails when a write was lost or applied twice; its results.md is the run's summary |
| `.github/workflows/fuzz.yml`, `supply-chain.yml`, `docs-urls.yml`, `nightly-report.yml`, `release-gate.yml` | every night `cargo fuzz` runs each target in `chloedb/fuzz/` for 20 minutes on a corpus kept between nights (a crash is uploaded as an artifact with the command to replay it); every day `cargo deny` checks each crate's Cargo.lock against RustSec's advisories and the licences and sources in `deny.toml` (fix an advisory with `cargo update -p <crate>`; ignore one in `deny.toml` only with its reason); every night, on a push to main that changes the docs and after each release, `scripts/check-doc-urls.py` checks that the downloads, image tags, Homebrew tap and PyPI release the docs tell people to install exist (`--list` shows what it reads); a failed scheduled run of CI, Chaos, Fuzz, Supply chain or Docs URLs opens or updates an issue labelled `nightly-failure`, and the next green night closes it; a release tag (and an image's version tag) builds only a commit of main on which CI's full matrix passed, a nightly run or one by hand (`gh workflow run ci.yml --ref <tag>`) |
| `.github/workflows/perf.yml`, `scripts/perf-gate.py` | the speed gate: two commits built and timed in turns on one runner (BM25 through SQL on the relevance gate's collections, loads, and `chloedb-server-bench` on SciFact over HTTP), failing when the head is more than 10% slower on its median and slower in every round; every night main against main 24 hours earlier, on a pull request labelled `perf`, or by hand (`gh workflow run perf.yml -f base=<ref> -f head=<ref>`); locally, `scripts/perf-gate.py --base DIR --head DIR --beir DIR` with each build's binaries in a directory |
| `scripts/fetch-build.sh` | fetches the Linux x86-64 build of a recent commit of main (CI's job `main-builds` keeps the last 40 in the pre-release `main-builds`: chloedb-server, chloedb-sim, chloectx, chloedb-crawl, chloedb-sheets, chloedb-netops), to replay a seed or bisect without compiling |
| `scripts/ci-local.sh` | CI's jobs on this machine (macOS or Linux), on the merge of a commit into main; `--post-status` records the result on GitHub as commit status `ci-local`, `--list` lists the jobs, and the header says what a Mac cannot test |
| `.github/CODEOWNERS`, `.github/pull_request_template.md` | the one-way doors (on-disk format, shard protocol, replication, auth), which wait for a human review; a pull request says whether it touches one |
| `docs/` | the website and the documentation: GitHub Pages publishes the folder as it is (no Jekyll, `docs/.nojekyll`), its root redirects to the website in `docs/site/`, and the Markdown pages are read on GitHub; `docs/demo/inspector.html` is a separate static copy of the inspector |
| `docs/guide/`, `docs/site/docs/` | the documentation section of the website: `docs/site/scripts/build_docs.py` renders the guide (`docs/guide/*.md`, `docs/quickstart.md`) and the reference Markdown (`docs/chloedb/*.md`, `docs/chloectx/*.md`) into `docs/site/docs/`, which is committed (`pip install -r docs/site/scripts/requirements.txt`, then run it after editing any of them; CI checks it is up to date); `docs/guide/run.py` runs every code block of the guide pages against this checkout's builds and checks the output they show (`--update` writes what they printed, `--list` shows the blocks) |
| `install.sh` | the installer described above |
| `packaging/chloedb/` | placeholder packages that hold the name `chloedb` on PyPI, npm and crates.io; `.github/workflows/reserve-names.yml` publishes them, run by hand |
| `chloedb-swarm/` | simulated users (Node): personas doing tasks with ChloeDB, through its MCP tools or the command center in Chromium (Playwright), each session in a sandbox chloedb-server; checks decide success, a stronger model reviews each session and groups the problems into findings; `serve --local DIR` is its app `swarm` in the command center; `npm test` there (a scripted model, no API calls; CI's server smoke test runs it against this build) ([README](chloedb-swarm/README.md)) |
| `examples/feature-store/` | a feature store demo (Python, standard library): a shop ranked by a model on features ChloeDB keeps (streaming on write, a batch SQL job, snapshots, ASOF JOIN training sets, models as rows, serving by key; feature workflows defined as rows, chained, mapped and searched by their statistics, in `workflows.py`), shown as the command center app `features`; `featurestore.py serve --local DIR` starts a server with it; `python3 -m pytest tests` there ([README](examples/feature-store/README.md)) |
| `examples/parley/` | a prototype app on ChloeDB (Python, standard library plus `chloedb`): a social network whose users are LLM agents (`claude -p` sessions with its MCP tools), run on a file, one server or a coordinator; `pytest tests` there runs each test on all three, and `FOUNDER_LOG.md` is the account of building it |

Build and test each crate from its own directory (`chloectx` and
`chloedb-server` depend on `chloedb` by path):

```bash
cd chloedb && cargo test --release
cd chloectx && cargo test --release
cd chloedb-server && cargo test --release
cd chloedb-crawl && cargo test --release   # its crawl into a cluster needs chloedb-server built (or CHLOEDB_SERVER_BIN)
cd chloedb-sheets && cargo test --release  # its cluster test needs chloedb-server built (or CHLOEDB_SERVER_BIN)
cd chloedb-netops && cargo test --release  # its cluster test needs chloedb-server built (or CHLOEDB_SERVER_BIN)
cd chloedb-workbench && cargo test --release
```

chloedb-server's integration tests build as one test binary, `tests/all.rs`,
with a module per file: `cargo test --test all sim::` runs `tests/sim.rs`.
A new test file needs `mod <name>;` in `tests/all.rs`, or a `[[test]]` in
chloedb-server's Cargo.toml when it needs the process to itself (`all.rs` says
which do and why); a test fails when a file is in neither.

The Java package builds with Maven, which runs cargo for its native library
(`-Dcargo.profile=ci` skips link-time optimisation when only the tests
matter):

```bash
cd chloedb-java && mvn -Dcargo.profile=ci package
```

The Python package builds into a virtualenv (its client tests start
`chloedb-server/target/release/chloedb-server`, so build that first):

```bash
cd chloedb-py && python3 -m venv .venv && . .venv/bin/activate
pip install maturin pytest && maturin develop --release && pytest tests
```

The JavaScript package builds its addon with cargo (its client tests start
`chloedb-server/target/release/chloedb-server` too):

```bash
cd chloedb-js && npm run build -- --profile ci && npm test
```

CI also runs `cargo clippy --release --all-targets -- -A clippy::style -A
clippy::complexity -A clippy::perf -D warnings` in each crate.

CI runs the tests with `cargo test --profile ci`: release without link-time
optimisation and with 16 codegen units (each crate's `Cargo.toml`), which
builds ChloeDB's hundred test binaries in minutes rather than half an hour.
Use it locally too when only the tests matter; benchmarks keep `--release`.

Benchmark numbers in `docs/`, the reports and `*/results/` are measured, not
edited: change them only by re-running the benchmark, and name the machine a
number came from.

A change to ChloeDB's ranking fails CI's relevance gate until the new numbers
are committed. The test writes them:

```bash
chloedb/tests/fixtures/beir/fetch.sh ~/beir    # pinned by hash
chloedb/tests/fixtures/miracl/fetch.sh ~/miracl yo sw    # the analyzers' collections
cd chloedb && CHLOEDB_BEIR_DIR=~/beir CHLOEDB_MIRACL_DIR=~/miracl CHLOEDB_RELEVANCE_GATE=update cargo test --release --test relevance_gate
```

Say in the pull request why the numbers moved.
