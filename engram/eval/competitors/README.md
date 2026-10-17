# Competing memory systems for `scripts/memory_bench.py`

`scripts/memory_bench.py` runs a multi-session scenario (from
`eval/ab/scenarios/`) once per *lane*: each lane is a memory system (or none)
working through the same history, followed by the same graded follow-up tasks.
This directory holds the setup code and adapters for the lanes that are not
Engram. Everything here is installed into the lane's own `HOME`, never into the
machine's.

## Running

```bash
cd engram
# everything (history per lane, then the runs, then the report); lanes run in parallel
python3 scripts/memory_bench.py all --scenario bakehouse --repeats 3 \
    --lanes none ccmem transcripts claude-mem mem0 engram@main engram engram+deep full
# or step by step; `run` records a missing history first, finished runs are skipped on a rerun
python3 scripts/memory_bench.py history --scenario stockroom --lanes claude-mem
python3 scripts/memory_bench.py run --scenario stockroom --lanes claude-mem --repeats 3
python3 scripts/memory_bench.py report --scenario stockroom
# a lane derived on the command line: BASE+anything, with its own variables or binary
python3 scripts/memory_bench.py all --scenario stockroom --lanes engram+graph \
    --lane-env engram+graph=ENGRAM_GRAPH=1 --lane-bin engram+graph=/path/to/engram
```

Defaults: `--lanes none ccmem transcripts claude-mem mem0 engram@main engram
engram+deep full`, `--repeats 3`, `--model haiku`, `--distill-model haiku`
(the memory systems' own model calls), `--work /home/claude/bench/work`,
`--results results/memory_bench/<scenario>/`, `--shared
/home/claude/bench/shared` (downloads reused across lanes), `--budget 3`
(`--max-budget-usd` per `claude` call). To re-record a lane's history, delete
its `history_<lane>.json` from the results directory.

Scenarios are read from `eval/ab/scenarios/<name>/` (or a directory path):
`scenario.json` (history blocks, tasks with free-form `kind` labels, optional
`teammate_commits`), `seed/` and `graders.py`. A teammate commit
`{after_block, author, email, message, files: {path: content | null}}` is
applied as a git commit by that author right after block `after_block`,
without telling the agent; it commits only its own paths, so whatever the
agent left uncommitted stays as it was.

## Isolation (all lanes)

Every `claude` call runs with an empty environment plus only:

- `PATH=<lane home>/.local/bin:<work>/bin:/usr/local/bin:/usr/bin:/bin`, where
  `<work>/bin` holds links to `claude`, `node`, `npm`, `npx`, `bun`, `bunx`,
  `uv`, `uvx`;
- `HOME=<lane home>`, so `~/.claude` (settings, plugins, transcripts, Claude
  Code's auto memory) and every memory system's data are per lane;
- `ANTHROPIC_BASE_URL`;
- the lane's own variables (below), e.g. `CLAUDE_CODE_DISABLE_AUTO_MEMORY=1`
  in every lane except `ccmem`.

No proxy variables reach a run, so a run has no network except the Anthropic
API. Hooks, daemons, workers and MCP servers inherit this environment. The lane
home holds a `.gitconfig` (user Dana, `dana@example.com`, the user the
scenarios' seed commits use). After each session the harness lets the memory
system finish (Engram: `engram stop` and wait for its socket to go; claude-mem:
wait until its queue is empty, then `claude-mem stop`) and then stops anything
still running with that `HOME`. The repository, the home and (for
`transcripts`) the transcript folder are snapshotted after the history and
restored before every task run.

Common flags: `--model haiku --output-format json --max-budget-usd 3
--allowedTools Bash,Edit,Write,Read,Glob,Grep,MultiEdit,TodoWrite[,<lane's MCP
prefix>] --disallowedTools WebFetch,WebSearch`.

Claude Code version used for validation: 2.1.300. Its built-in plugins
(`cc-plugin-agents-md`, `cc-plugin-telemetry`, `cc-plugin-plugin-authoring`)
load in every lane.

## none

No memory: auto memory off (`CLAUDE_CODE_DISABLE_AUTO_MEMORY=1`), no plugins,
no hooks, no MCP servers.

## ccmem: Claude Code's built-in auto memory

Auto memory left on (Claude Code's default). In `-p` mode the system prompt
carries the auto-memory instructions and the memory directory
`<home>/.claude/projects/<project>/memory/` (with `MEMORY.md` as the index that
is loaded into each session); the agent itself decides what to write there
with the Write tool. The harness records the files it wrote
(`history_ccmem.json` → `memory_state.auto_memory_files`) and counts tool calls
that touch `~/.claude/projects` (`memory_path_tool_calls`).

## transcripts: raw history + agentic search

`transcripts/export_transcripts.py`. The history is recorded as in `none`.
Afterwards each history session's Claude Code transcript is exported to
`<lane dir>/transcripts/session-NN.md` (outside the repository): user prompts,
assistant text, every tool call with the useful part of its input, and the
first 600 characters of each tool result. Task runs get `--add-dir <folder>`
and `--append-system-prompt` saying that transcripts of earlier work sessions
are there and can be searched with grep. `memory_path_tool_calls` counts tool
calls that touch the folder.

## claude-mem

- What: [thedotmack/claude-mem](https://github.com/thedotmack/claude-mem), a
  Claude Code plugin: lifecycle hooks, a local worker (Bun, HTTP on
  127.0.0.1), SQLite (`~/.claude-mem/claude-mem.db`), Chroma for vector search
  (via `uvx chroma-mcp`), an MCP server (`mcp__plugin_claude-mem_mcp-search__*`:
  `search`, `smart_search`, `timeline`, `get_observations`, `save_memory`, …)
  and an "observer" that sends each tool call to a model and stores the
  observations it writes, plus a summary per turn.
- Version: npm `claude-mem@13.39.0` (latest release on 2026-10-15, tag
  v13.39.0).
- Install: `claude_mem/setup.py HOME` (called by the harness), which runs the
  package's own installer `npx claude-mem install --ide claude-code --provider
  claude --no-auto-start` with `HOME=<lane home>`, then:
  - **Stop hook made synchronous** in the installed plugin's `hooks.json`.
    Claude Code cancels async Stop hooks when a headless `claude -p` exits
    ("Hook Stop (Stop) cancelled" in its debug log), so without this
    claude-mem never receives the request for its session summary. The other
    hooks are unchanged (PostToolUse stays async; it completes in `-p`).
  - `~/.claude-mem/settings.json`: `CLAUDE_MEM_MODEL` and every tier model
    (`CLAUDE_MEM_TIER_{SIMPLE,FAST,SMART,SUMMARY}_MODEL`) = `haiku` (the
    installer only accepts full model ids; its defaults are
    a dated Haiku id and, for the "smart" tier, sonnet); a worker port
    of its own (`CLAUDE_MEM_WORKER_PORT`, chosen free per lane);
    `CLAUDE_CODE_PATH` = a wrapper that runs the real `claude` unchanged and
    appends each result message (they carry `total_cost_usd`) to
    `~/.claude-mem-bench/observer-results.jsonl`, which is how the harness
    counts claude-mem's own model cost.
  - Telemetry off (`~/.claude-mem/telemetry.json`), cloud sync and the hosted
    "observer" off (`--provider claude` runs the observer on the local
    `claude` login through the Agent SDK).
  - Chroma offline: the `chroma-mcp` uv environment the worker asks for
    (`--with chromadb==1.5.9 … --from chroma-mcp==0.2.6`) is built once into
    a shared uv cache, and Chroma's ONNX embedding model (all-MiniLM-L6-v2,
    sha256-checked) is copied to `~/.cache/chroma`. The lane runs with
    `UV_CACHE_DIR=<shared>/uv-cache UV_OFFLINE=1`.
  - Claude Code's auto memory is off in this lane like every lane but
    `ccmem` (claude-mem's installer leaves it as it was).
- Draining: after each session the harness waits until no claude-mem hook is
  running, the worker's `/api/processing-status` reports an empty queue, the
  database has no pending messages and the observer has been quiet for 10 s,
  then runs `claude-mem stop`.
- Recorded: per history and per run, the observer's cost and calls
  (`memory.claude_mem`), observations, summaries and prompts in its database,
  and the context its hooks injected (`hook_context_chars`).

## mem0

- What: [mem0](https://github.com/mem0ai/mem0), the open-source library
  `mem0ai==2.2.1`, behind a small stdio MCP server, `mem0/mem0_mcp.py`, with
  tools `add_memory(text)`, `search_memory(query, limit)` and
  `list_memories()` (Claude sees them as `mcp__mem0__*`). All memories of a
  lane share one user id (`project`).
- Model calls: there is no API key in the container, only the `claude` CLI, so
  mem0's LLM is `ClaudeCliLLM`, which runs `claude -p --model haiku --tools ""
  --no-session-persistence --output-format json --system-prompt <mem0's
  prompt>` with only `PATH`, `HOME` and `ANTHROPIC_BASE_URL`, and logs each
  result (with its cost) to `~/.mem0-bench/llm-results.jsonl`. mem0 2.x makes
  one extraction call per `add`.
- Embeddings: Chroma's ONNX export of all-MiniLM-L6-v2 (384 dimensions) on the
  CPU with onnxruntime (`OnnxMiniLM`). Hugging Face is not reachable from this
  container, so fastembed and sentence-transformers models cannot be
  downloaded; for the same reason mem0's optional spaCy lemmatisation/entity
  extraction and its fastembed BM25 keyword search are not installed (mem0
  logs that it runs without them), so search is vector-only.
- Store: mem0's default Qdrant in local on-disk mode (`~/.mem0-bench/qdrant`)
  and its SQLite history (`~/.mem0-bench/history.db`).
- Setup: `mem0/setup.py HOME` builds a shared virtualenv
  (`<shared>/mem0-venv`: mem0ai, `mcp<2`, onnxruntime, tokenizers, numpy) and
  writes `~/.mem0-bench/mcp.json`; the harness passes it with `--mcp-config`
  and adds `--append-system-prompt` telling the agent to search memory before
  work and to save requirements, rules, decisions and facts that will matter
  later (mem0 has no hooks; the agent must call it).

## Engram lanes

Not set up here: `engram@main` copies `/home/claude/engram-main/engram/target/release/engram`
and `engram` copies `/home/claude/engram/engram/target/release/engram` (the
binary is a lane parameter, `--lane-bin LANE=PATH`) into the lane directory
when its history starts (so a rebuild later does not change a recorded lane;
`lane.json` keeps the binary's sha256 and the source commit), then runs
`engram install --project <repo>` with `ENGRAM_HOME=<lane home>/engram`,
`ENGRAM_MODELS=/home/claude/data/models` (potion-retrieval-32M) and
`ENGRAM_DISTILL_MODEL=haiku`. The install writes the hook timeouts that build
needs (the development build: Stop 40 s, UserPromptSubmit 90 s; main: 10 s).
`.claude/settings.local.json` and `.mcp.json` go into the repository's
`.git/info/exclude` so `git status` looks as in the other lanes. Task runs set
`ENGRAM_DISTILL=0`.

Predefined variants of the development build: `engram` (its default,
`ENGRAM_DEEP=async`), `engram+async` (the same, named explicitly),
`engram+deep` (`ENGRAM_DEEP=sync`: the query engine runs before the prompt is
answered) and `engram+nodeep` (`ENGRAM_DEEP=off`). Others with `--lane-env`,
e.g. `--lane-env engram+x=ENGRAM_DEEP=tool`. Every model call Engram makes
(`distill`, `consolidate`, `deep`, `ask`, ...) logs `cost_usd` in its
`events.log`; the harness adds them up as the lane's memory cost and counts
`deep` runs, how many returned picks, and their time.

## full

Upper bound: the whole history in one session (blocks continue with
`--resume`), each task a fork of it (`--resume <id> --fork-session`); auto
memory off. `compactions` in each run row and `last_session_compactions` in the
history file show whether it compacted.

## Costs

Claude Code's `total_cost_usd` is cumulative over a session: it grows with
each `--resume`, and a fork's includes everything its parent spent. The harness
therefore records a history turn's cost as the difference from the previous
turn of the same session (`session_cost_usd` keeps the reported value), and a
`full` run's cost as the fork's reported cost minus the history session's
final cost (`cost_usd_reported` keeps the reported value). claude-mem's
observer results are counted the same way, per session. `memory $` is what the
memory system itself spent on model calls (Engram's `events.log`,
claude-mem's observer, mem0's extraction calls), during the history and during
each run; the agent's own cost is `$/run`.
