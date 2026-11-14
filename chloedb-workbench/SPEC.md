# ChloeDB Workbench: the MVP's contract

This file is the contract the first build works to: the backend (Rust,
`src/`), the UI (`ui/`) and ChloeCtx's changes are written in parallel
against it. Concept and roadmap:
[An agentic IDE on ChloeDB](https://claude.ai/artifact/UTvWqZFB3tgFgjkVLMzCZw).

Workbench is a local app that runs many Claude Code sessions side by side,
each in a real terminal (a pseudo-terminal), most often each in its own git
worktree, and keeps every session's state and events as rows of a ChloeDB
database. It is a UI-friendly tmux whose truth is a database.

The first build is a local web app (`chloedb-workbench serve` prints a URL
with a token and opens nothing by itself); a desktop shell (Tauri) can wrap
the same pages later.

## Scope of this build

1. `chloedb-workbench serve`: the supervisor, the state database, the HTTP
   API, the server-sent events stream, a WebSocket per terminal, and the UI.
2. `chloedb-workbench hook`: what each session's hooks run; it posts the
   hook's JSON to the server and never fails or prints.
3. `chloedb-workbench mcp`: an MCP server over stdio with the tools that let
   an agent start and steer other agents (`spawn_agent` and its siblings).
4. ChloeCtx (separate crate, `chloectx/`): one store per repository rather
   than per worktree, the `Notification` and `SubagentStop` hooks recorded,
   and `chloectx report`, where a project's time went.

Not in this build: the board (blackboard), capsules on handoff, review with
blame, change requests for shell actions, the Tauri shell, team mode.

## Crate layout

```
chloedb-workbench/
  Cargo.toml          # crate and binary `chloedb-workbench`; chloedb by path
  src/main.rs         # clap: serve | hook | mcp | --version
  src/db.rs           # the state database (ChloeDB, embedded)
  src/supervisor.rs   # PTYs (portable-pty), processes, scrollback rings
  src/git.rs          # worktrees
  src/server.rs       # axum: HTTP API, SSE, WebSocket, the UI's files
  src/hook.rs         # the hook subcommand
  src/mcp.rs          # the MCP subcommand (a client of the HTTP API)
  ui/index.html, ui/app.js, ui/style.css   # compiled in with include_str!
  tests/              # with a fake `claude` (a shell script), no model calls
```

`CHLOEDB_WORKBENCH_UI_DIR=chloedb-workbench/ui` serves the UI from disk
without a rebuild, as the other apps do. Tests run with
`cargo test --profile ci`.

## Settings

| flag | env | default |
|---|---|---|
| `--addr` | `CHLOEDB_WORKBENCH_ADDR` | `127.0.0.1:7740` |
| `--data-dir` | `CHLOEDB_WORKBENCH_HOME` | `~/.local/share/chloedb-workbench` |
| `--claude-bin` | `CHLOEDB_WORKBENCH_CLAUDE` | `claude` |
| `--max-agents` | | 8 running at once |
| `--max-depth` | | 2 (an agent started by an agent started by you may start no more) |
| `--token` | `CHLOEDB_WORKBENCH_TOKEN` | random, printed once in the URL |

The server listens on loopback only, requires the token on every `/api`
route (`Authorization: Bearer`, or `?token=` for the WebSocket and the
EventSource, which cannot set headers), answers only to the hosts
`127.0.0.1`, `localhost` and `[::1]`, and refuses a request that changes
something when its `Origin` is not its own.

## The state database

`<data-dir>/workbench.chloedb`, opened once by the server (one writer).

```sql
CREATE TABLE agents (
  id KEYWORD PRIMARY KEY,      -- 'a' + 8 hex
  name KEYWORD,                -- unique among running agents; [a-z0-9-], at most 32
  repo KEYWORD,                -- absolute path of the repository's main checkout
  cwd KEYWORD,                 -- where the session runs (a worktree or the repo)
  branch KEYWORD,
  task TEXT,                   -- the first prompt
  parent KEYWORD,              -- the agent that spawned it, or NULL for you
  depth INT,
  state KEYWORD,               -- starting | working | waiting | idle | exited
  state_since TIMESTAMP,
  waiting_for TEXT,            -- the notification's message while waiting
  last_event TEXT,             -- one line: what it did last
  session_id KEYWORD,          -- Claude Code's session id (set with --session-id)
  pid INT,
  exit_code INT,
  model KEYWORD,
  created TIMESTAMP,
  ended TIMESTAMP
);
CREATE TABLE events (
  id KEYWORD PRIMARY KEY,      -- agent id + ':' + seq, zero-padded
  seq INT,                     -- global, increasing
  ts TIMESTAMP,                -- milliseconds
  agent KEYWORD,
  kind KEYWORD,                -- a hook's event name, or spawned | input | exited
  tool KEYWORD,                -- the tool's name, for tool events
  summary TEXT,                -- one line for people
  data TEXT                    -- the hook's JSON, trimmed to 16 KB
);
```

State from events: `spawned` → starting; `SessionStart`,
`UserPromptSubmit`, `PreToolUse`, `PostToolUse` → working; `Notification` →
waiting (`waiting_for` = its message); `Stop` → idle (its turn is over, it
waits for a prompt); process exit or `SessionEnd` → exited. A waiting agent
that gets input or a tool event goes back to working. The attention queue is
the agents waiting or idle, oldest `state_since` first.

## How a session is started

1. A worktree, unless `worktree: false`: `git -C <repo> worktree add
   <data-dir>/worktrees/<repo-name>-<hash8>/<name> -b wb/<name> <base>`
   (`base` defaults to the repo's current `HEAD`). `repo` must be a git
   checkout; a worktree passed as `repo` is read back to its main checkout.
2. A settings file `<data-dir>/sessions/<id>/settings.json` holding hooks
   for `SessionStart`, `UserPromptSubmit`, `PreToolUse`, `PostToolUse`,
   `Notification`, `Stop`, `SubagentStop` and `SessionEnd`, each running
   `<this exe> hook` with a timeout of 5 s. It adds to the user's own
   settings and hooks (ChloeCtx's keep working); it never edits theirs.
3. An MCP config `<data-dir>/sessions/<id>/mcp.json` naming the server
   `workbench`: `<this exe> mcp`.
4. The process: `<claude-bin> --session-id <uuid> --settings <file>
   --mcp-config <file> [--model M] [<task>]` in a PTY of 120 x 32, cwd the
   worktree, with `CHLOEDB_WORKBENCH_URL`, `CHLOEDB_WORKBENCH_TOKEN` and
   `CHLOEDB_WORKBENCH_AGENT` in its environment (the hook and MCP
   subcommands read them).

The supervisor keeps the last 256 KiB of each terminal's output in memory
and replays it to a WebSocket that attaches. Agents do not outlive the
server in this build: stopping the server ends them (SIGHUP, then SIGKILL
after 5 s); at start, agents left `working` by a crash are marked `exited`.

## HTTP API

All JSON. Errors are `{"error": "...", "error_code": "..."}` with a fitting
status (400 bad_request, 401 unauthenticated, 404 not_found, 409 conflict,
429 limit_exceeded).

| route | body | answer |
|---|---|---|
| `GET /api/health` | | `{ok, version, agents_running, max_agents}` (no token needed) |
| `GET /api/agents?all=1` | | `{agents: [Agent]}`, running ones first; `all=1` adds exited ones |
| `POST /api/agents` | `{repo, task?, name?, branch?, base?, worktree?: true, model?, parent?}` | `{agent: Agent}` (201); 429 past `--max-agents`, 409 past `--max-depth` or a name in use |
| `GET /api/agents/:id` | | `{agent: Agent}` |
| `GET /api/agents/:id/events?after=<seq>&limit=200` | | `{events: [Event]}`, oldest first |
| `POST /api/agents/:id/input` | `{text, enter?: true}` | `{ok}`; writes to the terminal (and `\r` when `enter`) |
| `POST /api/agents/:id/resize` | `{cols, rows}` | `{ok}` |
| `POST /api/agents/:id/stop` | | `{ok}`; SIGHUP, then SIGKILL after 5 s |
| `GET /api/attention` | | `{agents: [Agent]}`: waiting or idle, oldest first |
| `GET /api/wait?agent=<id>&until=idle\|waiting\|exited\|any&timeout_s=30` | | `{agent, reached: bool}` once the state is reached (`any`: any new event), or at the timeout |
| `POST /api/hook` | the hook's stdin JSON, plus header `x-workbench-agent` | `{ok}` |
| `GET /api/stream` | | `text/event-stream`: `event: agent` with an Agent on every change, `event: event` with each new Event; a first `event: snapshot` with `{agents}` |
| `GET /api/agents/:id/pty` | WebSocket | server to client: binary frames of terminal output (the scrollback first); client to server: binary or text frames of keystrokes, and a text frame `{"resize":[cols,rows]}` |

`Agent` is the row as JSON (times as ISO 8601 text) plus `running: bool`
and `children: [id]`. `Event` is the row as JSON without `data` unless
`?data=1`.

## The hook subcommand

`chloedb-workbench hook` reads the hook's JSON from stdin, posts it to
`$CHLOEDB_WORKBENCH_URL/api/hook` with the token and agent headers, waits at
most 2 s, prints nothing and exits 0 in every case. Without the variables it
exits 0 at once.

## The MCP subcommand

`chloedb-workbench mcp`: MCP over stdio (protocol 2025-06-18, JSON-RPC
lines), a client of the HTTP API with the variables above. Its caller is
`$CHLOEDB_WORKBENCH_AGENT`, which becomes `parent` of what it spawns.

| tool | arguments | does |
|---|---|---|
| `spawn_agent` | `task`, `name?`, `worktree?` (true), `model?`, `base?` | starts an agent on the caller's repo; returns its id and name |
| `list_agents` | `all?` | the agents this caller started (all of them with `all`) |
| `read_agent` | `id`, `after?` | its state and its events after a seq, as lines |
| `wait_agent` | `id`, `until?` (`idle`), `timeout_s?` (300, at most 1800, polling `/api/wait` in 30 s steps) | waits, then answers its state and its last events |
| `send_to_agent` | `id`, `text` | types the text and Enter into its terminal |
| `stop_agent` | `id` | stops it |

Its instructions at `initialize` say that spawned agents are real sessions
the user watches, cost tokens, and should get a task that stands alone.

## The UI

One page, no build step: `ui/index.html`, `ui/app.js` (an ES module),
`ui/style.css`. xterm.js and its fit add-on from a pinned CDN URL
(`https://cdn.jsdelivr.net/npm/@xterm/xterm@5.5.0/...`,
`@xterm/addon-fit@0.10.0`). Light and dark themes from the system.

- **Top bar**: Workbench, the count of running agents, a **New agent**
  button (`n`).
- **Attention rail** (left): agents waiting or idle, oldest first, each
  with its name, why (`waiting_for`, or "turn finished"), and how long; a
  click zooms it.
- **Grid**: a tile per running agent (exited ones under a fold): name,
  state chip (starting, working, waiting for you, idle, exited), branch,
  task (two lines), last event, time in state; waiting tiles are outlined
  in the warning colour. Each tile holds a live, small, read-only terminal
  preview, or its last event when there are many tiles.
- **Zoom** (click, Enter or `z`): one agent's full terminal, interactive,
  with its events beside it; Esc goes back to the grid.
- **New agent** dialog: repo path (remembered), name, task, own worktree
  (ticked), model.
- **Prefix keys** as tmux's: `Ctrl-b` then `n` new, `z` zoom, arrows move
  between tiles, `x` stop (asks first), `?` help.
- State changes come from `/api/stream`; nothing polls.
- The token comes once in the URL (`/?token=...`), is kept in
  `sessionStorage`, and the address bar drops it.

## Decision trail (second build)

Every line an agent wrote can answer **who** decided it (which agent, started
by whom, on whose prompt), **when** (the turn, the time, the commit),
**where** (repository, worktree, branch, file and line), **what it knew**
(the memory ChloeCtx had put in its context, and what became of each record
since) and **why** (the decisions distilled from that turn). The surprise:
**Rewind**, which plays the agent's own terminal back at the moment it wrote
the line, from the recording Workbench keeps of every session.

It is built on `chloectx blame --json` (docs/chloectx/receipts.md), ChloeCtx's
decision notes and Workbench's own rows. When `chloectx` is not installed or
has no record of the line, the card still answers from Workbench's rows (the
agent, its task, the time from git) and says what it could not find.

### Backend

- **Terminal recording.** The supervisor appends every chunk of a terminal's
  output to `<data-dir>/sessions/<id>/term.cast`, asciicast v2 (a header
  line `{"version": 2, "width", "height", "timestamp"}`, then
  `[seconds_since_start, "o", "text"]` lines; resizes as `"r"` events
  `"COLSxROWS"`). Capped at 64 MiB per session (then it stops recording and
  says so in an event). Kept after the agent exits.
- `agents` gains `base` (the commit the worktree started from) and
  `started_ms`.
- `GET /api/agents/:id/recording?from_ms=&to_ms=` answers
  `{width, height, started_ms, events: [[ms_since_start, "o"|"r", data]]}`
  for that window (milliseconds since the session started), at most 8 MiB,
  plus `{before: "<the screen's bytes up to from_ms, as one string>"}`
  trimmed to the last 256 KiB, so a player can start mid-session with the
  right screen.
- `GET /api/agents/:id/diff` answers `{base, files: [{path, status,
  hunks: [{old_start, new_start, lines: [{kind: "+"|"-"|" ", text,
  new_line}]}]}]}`: what the agent changed from `base`, committed or not
  (`git diff <base>` in its worktree, untracked files included as added).
- `GET /api/why?agent=<id>&path=<rel>&line=<n>` (or `repo=` instead of
  `agent=` for a line in the main checkout): the decision card as JSON:

  ```
  {
    "line": {"path", "line", "text"},
    "where": {"repo", "worktree", "branch", "commit": {sha, date, subject} | null},
    "who": {
      "agent": Agent | null,             -- the Workbench agent whose session made the edit
      "lineage": [Agent...],             -- its parents, nearest first, up to you
      "prompt": {"text", "ts", "by": "you" | "<parent agent's name>"} | null
    },
    "when": {"edit_ts", "turn", "session"} | null,
    "knew": [{"kind", "id", "text", "score", "decision", "now": "current"|"superseded by …"|"deleted", "untrusted": bool}],
    "why": [{"kind": "decision"|"preference"|"fact", "text", "ts"}],   -- notes of that turn
    "worth_a_look": ["..."],
    "rewind": {"agent", "from_ms", "to_ms"} | null,   -- 20 s before the edit's tool call to 3 s after
    "found": "chloectx" | "git" | "workbench",         -- where the answer came from
    "missing": ["what could not be found, in a sentence each"]
  }
  ```

  It runs `chloectx blame <path>:<line> --json` in the agent's worktree (the
  `--chloectx-bin` flag, default `chloectx`), maps the blame's session id to
  the Workbench agent whose `session_id` it is, and finds the edit's tool
  call among the agent's events (by time and file) to place the rewind.
- `GET /api/decisions?repo=<path>&limit=200`: the repo's decision notes,
  newest first: `{decisions: [{id, text, ts, session, agent: Agent|null,
  superseded_by: id|null, files: [path]}]}`, from `chloectx sql --json
  "SELECT id, ts, session, text, tag, files, links FROM memory WHERE kind =
  'decision' ORDER BY ts DESC LIMIT n"` run in the repo
  (`chloectx sql --json` prints `{"text", "columns": [...], "rows":
  [[...]]}`).
- `POST /api/ask {repo, question}`: `chloectx ask --json` in the repo,
  answered as it prints it, plus the Workbench agents its records' sessions
  map to; and the answer for a person (`src/ask.rs`): `summary` (one
  sentence), `memory` (the records that share the question's words, at most
  five notes and the transcripts, each `{id, kind, what, who, agent, text,
  ts, ts_ms, files, transcript}`, phrased without "The agent X said:"),
  `code` (the top five `search::quick` hits with `path, symbol, line,
  snippet`), `history` (`{answer, commits}` from Trace), `terms`,
  `missing` and `took_ms` (an integer). No model is called.
- All four read only, token as the other routes. Tests use a fake
  `chloectx` script that prints canned JSON.

### UI

- **Changes** tab in the zoom view, beside the terminal: the agent's diff,
  file by file. Hovering an added line shows a **Why?** button; `Alt`-click
  or `y` on the focused line opens the card.
- **Decision card** (a side sheet): a headline sentence ("fix-total wrote
  this at 7:41 PM, on your prompt, after recalling 3 notes"), then Who
  (the lineage as a chain of chips: you → lead → add-validate, each
  clickable to zoom that agent), When, Where, Why (decision notes, quoted),
  What it knew (records with a warning chip when superseded or untrusted),
  Worth a look, and **Rewind**.
- **Rewind**: an xterm player that starts from the screen as it was at
  `from_ms` and plays to the edit at 1x, 2x or 4x, with a scrubber and a
  marker where the edit happened; the moment of the edit flashes the line
  in the Changes view beside it.
- **Decisions** screen (top bar, `d`): every decision of the repo on a time
  axis, one lane per agent (and one for decisions with no agent: other
  sessions), superseded ones struck through and linked to what replaced
  them; a click opens the card for the decision with the lines that came
  from its turn. An **Ask why** box above it asks ChloeCtx in a sentence
  ("why do we round half up?") and answers with the records and their
  agents.

## Levels (third build)

The user's ask: Workbench must be significantly more than terminals in a
grid: more abstraction, with the detail there when needed, from ChloeCtx.
The answer is **semantic zoom**. Everything can be read at five levels,
and the terminal is the last of them:

| level | for the fleet | for one agent |
|---|---|---|
| Intent | **Mission** (goal, outcomes, one sentence per agent), **Away** (the digest) | its outcome and its sentence |
| Plan | **Decide** (questions with precedent, rules), **Flow** (where time went, coach) | its questions |
| Story | | **Story** (chapters per turn, grouped steps, what it knew, déjà vu) |
| Changes | **Map** (territory, collisions, ◆ decisions), **Preview** (the running app) | its diff with Why? (second build) |
| Terminal | the grid (first build) | its terminal |

`[` and `]` move one level down or up for what is in focus; the top bar
shows the level. Concept screens: `/mnt/project-files/workbench/concepts/`
(`1-mission.png` … `7-away.png`, with their HTML). The real screens take
their look (layout, density, the ChloeCtx ◆ chips), in the existing
Workbench theme, light and dark.

Nothing here calls a model on its own, except where marked *optional*
(off unless `--narrate-model` is set). Everything is derived from
Workbench's rows, the hook payloads, the session transcripts (read from
`transcript_path` in the hook payloads) and `chloectx` run in the repo.

### New tables

```sql
CREATE TABLE goals (
  id KEYWORD PRIMARY KEY,  -- 'g' + 8 hex
  repo KEYWORD, text TEXT, state KEYWORD,   -- active | done | dropped
  lead KEYWORD,            -- the agent planning it, if any
  created TIMESTAMP, done TIMESTAMP
);
CREATE TABLE outcomes (
  id KEYWORD PRIMARY KEY,  -- 'o' + 8 hex
  goal KEYWORD, title TEXT, detail TEXT,
  state KEYWORD,           -- next | active | needs_you | done | dropped
  agent KEYWORD,           -- who works on it (NULL while next)
  ord INT, after KEYWORD,  -- shown order; an outcome it waits for
  progress INT,            -- 0..100, set by the agent or derived
  created TIMESTAMP, updated TIMESTAMP
);
CREATE TABLE questions (
  id KEYWORD PRIMARY KEY,  -- 'q' + 8 hex
  agent KEYWORD, outcome KEYWORD, repo KEYWORD,
  kind KEYWORD,            -- ask (from ask_human) | permission (a Notification asking to use a tool)
  text TEXT, context TEXT,
  options TEXT,            -- JSON [{label, detail?}]
  recommended INT,
  precedent TEXT,          -- JSON: what `chloectx ask --json` found when it was asked
  state KEYWORD,           -- open | answered | expired
  answer TEXT, answered_by KEYWORD,  -- 'you' | 'rule:<id>' | 'terminal'
  rule KEYWORD,            -- the rule made from the answer, if any
  created TIMESTAMP, answered TIMESTAMP
);
CREATE TABLE rules (
  id KEYWORD PRIMARY KEY,  -- 'r' + 8 hex
  repo KEYWORD, text TEXT,
  question KEYWORD,        -- where it came from
  ctx_id KEYWORD,          -- the ChloeCtx record id (`chloectx remember decision`)
  active BOOL, created TIMESTAMP
);
CREATE TABLE deliveries (  -- what each agent has been told (rules, steers, its outcome)
  id KEYWORD PRIMARY KEY,  -- agent + ':' + item id (a rule's, a steer's, or 'assign:' + outcome)
  agent KEYWORD, item KEYWORD,
  via KEYWORD,             -- the hook event that carried it, or 'typed'
  ts TIMESTAMP
);
CREATE TABLE seen (        -- the levels' own key/value state: last_seen
  id KEYWORD PRIMARY KEY,  -- `key` is a reserved word of ChloeDB's SQL
  value TEXT, ts TIMESTAMP
);
CREATE TABLE marks (       -- small key/value state: last_seen, preview urls, allow-lists
  "key" KEYWORD PRIMARY KEY, value TEXT, ts TIMESTAMP   -- quoted: KEY is reserved
);
```

`agents` gains `goal`, `outcome`, `narration` (one sentence), `narration_ts`
and `preview_url`.

### Narration (one live sentence per agent)

After each `PostToolUse`, `Stop` and `UserPromptSubmit`, the server reads
the tail of the session's transcript (the `transcript_path` of the payload;
read at most the last 256 KiB) and sets `narration` to the first sentence
of the latest assistant text block (at most 160 characters, Markdown
stripped). When the latest thing is a tool call with no text after it, it
is a sentence made from the call ("Running `cargo test -p harbor`",
"Editing src/ledger/post.rs", "Reading 4 files in src/refunds"). A waiting
agent's sentence is its question. *Optional:* with `--narrate-model M`,
each finished turn also gets a chapter title and a two-line summary from
`claude -p --model M` (one call per turn, never more than one in flight
per agent), cached in the event's data.

### Goals and outcomes (Mission)

- `POST /api/goals {repo, text, plan?: true}` makes a goal; with `plan`, it
  spawns a lead agent (name `lead`, or `lead-2`…) whose task is the goal
  plus instructions to break it into 3 to 8 outcomes with `add_outcome`,
  start an agent per outcome with `spawn_agent(outcome=…)`, ask the user
  through `ask_human` rather than in its terminal, and mark outcomes done.
- `GET /api/goals?repo=` → `{goals: [Goal]}`; `GET /api/goals/:id` →
  `{goal, outcomes: [Outcome], agents: [Agent], open_questions: n,
  learned: [Record], heads_up: [HeadsUp], pulse: [[minute_ms, count]]}`
  where `learned` is today's ChloeCtx decision, preference, fact and
  error-fix records of the repo (`chloectx sql --json`), `heads_up` the
  collisions from the map, and `pulse` the events per minute for the last
  60 minutes.
- `POST /api/outcomes {goal, title, detail?, after?}`, `PATCH
  /api/outcomes/:id {title?, state?, progress?, agent?}`.
- An outcome's state follows its agent unless set: agent working →
  `active`, waiting or with an open question → `needs_you`, exited with code
  0 after marking it → `done`.

### Questions and rules (Decide)

- MCP tool `ask_human {question, options: [{label, detail?}], recommended?,
  context?}`: posts `POST /api/questions`, then waits (polling `/api/wait`
  style, `GET /api/questions/:id/wait?timeout_s=30`, up to 30 minutes) and
  returns the answer text. The agent's state is `waiting` meanwhile, with
  `waiting_for` = the question.
- A `Notification` that asks permission for a tool also makes a question,
  `kind: permission`. Answering in the terminal instead marks it
  `answered_by: terminal`. Its options and how an answer is typed are in
  "Sixth build: Safe answering" (they were a fixed Yes / Yes, and don't
  ask again / No, typed as `1`, `2` or `3`).
- On creation the server runs `chloectx ask --json -- <question>` in the
  repo and stores what it found as `precedent` (records with their agents,
  as `/api/ask` answers). When a precedent record is a Workbench rule or a
  decision whose words match an option's label, that option is marked
  `matches_precedent`.
- `GET /api/questions?repo=&state=` → `{questions: [Question]}`.
- `POST /api/questions/:id/answer {option?: n, text?, rule?: "text"}`:
  answers it (the agent's `ask_human` returns), and with `rule`, runs
  `chloectx remember decision "<rule>"` in the repo and adds a `rules` row.
- `GET /api/rules?repo=`, `PATCH /api/rules/:id {active}`.
- **Delivery.** The `hook` subcommand now prints what the server answers
  to `POST /api/hook` (as its `output`) when it is a `UserPromptSubmit`,
  `SessionStart` or `PostToolUse`: `{"hookSpecificOutput":
  {"hookEventName": E, "additionalContext": "..."}}` holding active rules
  of the repo and steers this agent has not been told yet (recorded in
  `deliveries`), at most 9,000 characters (Claude Code moves more than
  10,000 to a file). It still never fails the hook: any error prints
  nothing.
- `POST /api/agents/:id/steer {text}`: an idle agent gets it typed with
  Enter; a working one gets it as additional context at its next
  `PostToolUse` ("The user adds: …").

### Story (one agent)

`GET /api/agents/:id/story` →

```
{
  "agent": Agent, "intent": "its outcome's title, or its task",
  "chapters": [{
    "turn": n, "prompt": {"text", "by"}, "started_ms", "ended_ms",
    "title",            -- the optional model title, or the prompt's first words
    "summary",          -- the turn's last assistant text, first two sentences
    "state": "done" | "now",
    "steps": [{"kind": "read"|"search"|"edit"|"run"|"memory"|"agents"|"web"|"other",
               "label": "Read 12 files", "detail": "src/ledger/*, …",
               "count", "started_ms", "ms", "failed": n, "events": [seq...]}]
  }],
  "knew": [{"id", "kind", "text", "score", "turn", "deja_vu": bool}],
  "deja_vu": [{"text", "record", "when", "shown_ms"}],
  "tokens": {"input", "output", "cache_read"}       -- summed from the transcript's usage
}
```

Steps group consecutive tool events of one kind within a turn (Read, Grep,
Glob, LS → read/search; Edit, MultiEdit, Write, NotebookEdit → edit; Bash →
run, grouped by the command's first two words; `mcp__chloectx__*` → memory;
`mcp__workbench__*` → agents; WebFetch, WebSearch → web). `knew` comes from
`chloectx receipts --session <session_id> --json` (add `--json` to it if it
is missing). A recalled record of kind `fix` (ChloeCtx's error → fix), or
one recalled right after a failed tool call, is a **déjà vu**.

### Map

`GET /api/map?repo=` (`src/map.rs`) →

```
{
  "repo": "/abs/path/of/the/main/checkout",
  "tree": Node,
  "agents": [{"agent": Agent, "files": [{"path", "added", "removed", "status"}], "reading": [path]}],
  "collisions": [
    {"path", "agents": [id, id...], "kind": "both_changed", "detail"},
    {"path", "agents": [changer, reader], "kind": "heading_into", "changed_by", "read_by", "detail"}
  ],
  "pins": [{"path", "line": n|null, "record": {"id", "kind", "text", "ts"}, "agent": Agent|null}],
  "missing": ["what could not be found, in a sentence each"]
}
Node = {"name", "path", "kind": "dir", "lines", "files", "children": [Node], "collapsed"?: true}
     | {"name", "path", "kind": "file", "lines"}
     | {"name": "…", "path": "<dir>/…", "kind": "more", "lines", "files"}   -- small files folded
```

- **tree**: `git ls-files` of the main checkout with line counts (binary
  files count 0; files over 4 MiB, and all files once 256 MiB were read,
  are estimated at 40 bytes a line), reused for 30 s while HEAD and the
  index stay the same. The root is depth 0; folders to depth 3 are opened,
  deeper ones are `collapsed` with their size. In a folder, files under
  5% of its lines (and under 50 lines), and files past its 16 biggest,
  fold into one `more` entry, unless an agent changed or read them or a
  pin names them. Folders come first, biggest first, then files.
- **agents**: the repository's agents that run, or ended in the last 24
  hours, oldest first. `files` is the diff from `base` (as `/diff`,
  untracked files as added); agents that share a folder keep only the
  files their own edit calls named. `reading` is the files their `Read`
  calls named in the last 10 minutes, relative to the checkout.
- **collisions**: `both_changed` for every file in two or more agents'
  diffs; `heading_into` for every file one agent changed that another,
  which did not change it, read in the last 10 minutes. `map::collisions(sup,
  repo) -> Vec<Json>` answers the same list for Mission's `heads_up`.
- **pins**: `chloectx sql --json "SELECT id, kind, text, ts, files, ctx,
  session, tag FROM memory WHERE kind = 'decision' OR kind = 'preference'
  OR kind = 'fact' ORDER BY ts DESC LIMIT 1000"` in the main checkout. A
  pin per file a record names, when the file is tracked or in an agent's
  diff; records tagged `superseded` are left out. ChloeCtx's records carry
  no line, so `line` is the one a file entry names (`path:42`,
  `path#L42`), else the first line of the file in the main checkout that
  holds, as a word, one of the identifiers of the record's `Anchor:` line
  (`idents=` in `ctx`), else null.

### Preview

- `PUT /api/agents/:id/preview {url}` → `{agent: Agent}` (`src/preview.rs`):
  `url` is `http://127.0.0.1:<port>` or `http://localhost:<port>`,
  optionally with a path, never Workbench's own port; it is kept without
  a trailing `/` in `agents.preview_url`, and an empty or null `url`
  clears it. 400 for anything else. The agent's row goes out on the stream.
- MCP tool `set_preview {url}` (`src/mcp.rs`): the same for the calling
  agent.
- `GET|HEAD|POST /preview/:agent/<path>?<query>` proxies to
  `<preview_url>/<path>?<query>` (`/preview/:agent` redirects to
  `/preview/:agent/`). Auth: `?token=` on a GET answers `303` to the same
  address without it and sets `wb_preview=<token>; Path=/preview/;
  HttpOnly; SameSite=Strict`; after that the cookie (or a Bearer header)
  is the token. A wrong or missing token is 401, an agent with no preview
  404, an app that does not answer 502 (small HTML pages, for the frame).
  The guard checks the host, and the origin of a POST, as on every route;
  `/api` routes never take the cookie.
- What goes to the app: `Accept`, `Accept-Language`, `Content-Type`,
  `User-Agent`, conditional and range headers, `Authorization` unless it
  is a Bearer token, its own cookies (never `wb_preview`), `Origin` and
  `Referer` rewritten to the app's, and `Accept-Encoding: identity`.
  Redirects are not followed.
- What comes back: hop-by-hop headers and `Content-Length` dropped,
  `X-Frame-Options: SAMEORIGIN`, `Location` to the app (absolute or
  `/path`) rewritten to `/preview/:agent/…`, `Set-Cookie` without
  `Domain` and with its `Path` under `/preview/:agent/`. An HTML answer
  (`text/html`, not encoded) gets `<script src="/preview-overlay.js">
  </script>` before its last `</body>` (else `</html>`, else at the end),
  and its absolute-path links (`src`, `href`, `action`, `formaction`,
  `poster` values that start with one `/`) are rewritten to
  `/preview/:agent/…`. URLs that scripts build (`fetch("/x")`, a bundler's
  module imports) are not: such an app runs its dev server under a base
  path and sets the preview URL with it (`vite --base
  /preview/<agent>/` and `http://127.0.0.1:5173/preview/<agent>`), and
  then links that are already under `/preview/:agent/` are left as they
  are.
- `ui/preview-overlay.js` (compiled in, `/preview-overlay.js`, from disk
  under `CHLOEDB_WORKBENCH_UI_DIR`): in a frame, it outlines the element
  under the pointer, and a click (with its pointer and mouse events)
  never reaches the page but posts to the parent, at Workbench's origin:
  `{type: "wb-pick", selector, text (≤ 1000 chars), html (outerHTML,
  ≤ 2048 bytes), rect: {x, y, width, height}, url}`. It takes messages
  from its parent only: `{type: "wb-pick-mode", on}` turns picking off
  and on (on by default in a frame, off on its own); `{type: "wb-who",
  id, texts?: [string], selector?, max?: 300, watch?: bool}` is answered
  with `{type: "wb-who", id, items: [{selector, text, classes, rect}]}`,
  the visible elements with text of their own (under `selector`; with
  `texts`, those whose text holds one of them or is held by one, so the
  parent can pass an agent's added lines); `classes` are the element's
  and its parent's (at most 12), which the Preview screen matches against
  the class names in an agent's added lines. With `watch`, the same
  answer comes again, under the same id, after the page scrolls, resizes
  or changes (debounced), until the next `wb-who`. On load it posts
  `{type: "wb-ready", url}`, and the parent asks again.
- `POST /api/preview/route {agent, pick}` →

  ```
  {"owner": Agent, "files": [path], "related": [{"id", "kind", "text", "ts", "files"}],
   "matched": [{"path", "line", "text"}], "score": n, "message": "...", "missing": [...]}
  ```

  `agent` is the preview's agent. The pick's needles are its text's
  lines of 3 characters or more and the class names and id of its HTML's
  first tag and of its selector. Each agent of the repository on the map
  scores 3 for each text needle and 1 for each class or id (as a whole
  word) on each added line of its diff; the best score is the owner
  (ties to the preview's own agent), else the preview's own agent with
  no files. `files` are the owner's matching files, best first; `related`
  ChloeCtx's decision, preference and fact records that name them (at
  most 20); `message` is the text to `/steer` the owner with (the
  selector, the text and the HTML).
- Security: the previewed page is served from Workbench's own origin
  (the overlay needs its parent, and the cookie its path), so its scripts
  could reach the Workbench page around it. Preview only an app you trust
  as you trust the agents' code; Workbench refuses any target but
  loopback, so it cannot be turned into a proxy to elsewhere.

### Flow

`GET /api/flow?repo=&minutes=60` (`src/flow.rs`; `minutes` 1 to 10080) →

```
{
  "repo": "/abs/path",
  "window": {"from_ms", "to_ms", "minutes"},
  "totals": {"model_ms", "tools_ms", "tests_ms", "waiting_ms", "redo_ms", "idle_ms"},
  "agents": [{"agent": Agent, "model_ms", "tools_ms", "tests_ms", "waiting_ms", "redo_ms", "idle_ms",
              "segments": [[start_ms, end_ms, "model"|"tools"|"tests"|"waiting"|"redo"|"idle"]],
              "tokens": {"input", "output", "cache_read"}}],
  "tokens": {"input", "output", "cache_read"},
  "suggestions": [{"id", "kind", "title", "detail", "saves": "saves ~4 min/h" | null, "saves_ms": n | null,
                   "action": {"kind", ...} | null, "applied": bool, "from"?: "chloectx"}],
  "report": {...} | null,
  "missing": [...]
}
```

- **Segments**, from each agent's events (the agents of the repository
  that ran during the window). Every event ends the stretch before it and
  starts the one it implies: `spawned`/`SessionStart` idle (or model in a
  turn), `UserPromptSubmit` model, `PreToolUse` tools (tests when an open
  Bash call runs a test runner: `cargo test|nextest`, `pytest`, `python
  -m pytest`, `npm|yarn|pnpm|bun test`, `npm run test*`, `go test`,
  `jest`, `vitest`, `mvn test|verify`, `gradle test`, `make test|check`,
  `rspec`, `phpunit`, `ctest`, `tox`, `mix test`, `dotnet test`, `swift
  test`, …, after `cd …&&` and `VAR=x`), `PostToolUse` back to tools while
  other calls are open, else model; `Notification` waiting (not an idle
  notice of a finished turn) until the next `input`, tool event or prompt;
  `Stop` idle; `exited`/`SessionEnd` nothing. Calls overlap: a PostToolUse
  ends the call of its `tool_use_id`, else the newest open one with its
  tool and input. Neighbours of one kind are joined, and segments are
  clipped to the window.
- **Failures**: a call whose PostToolUse (or `PostToolUseFailure`) says
  it failed (`tool_response.is_error` or `.error`), whose transcript
  `tool_result` of its `tool_use_id` has `is_error: true`, or that is
  still open when the turn ends (unless the user was asked about it: then
  it was refused). Its error is the hook's or the transcript's text, else
  "`<tool>` failed: `<command>`"; the key is its line that says error
  (else its first), numbers masked. **Redo**: the tool and test time of a
  failure whose key an earlier failure in the window had, of any agent.
- **Waiting** is Notification time only for now; open questions join it
  once Decide is merged.
- **tokens**: each `transcript_path` the agent's hook payloads named
  (`.jsonl` files, their last 64 MiB), the `usage` of assistant messages
  timed in the window, once per message id: `input_tokens`,
  `output_tokens`, `cache_read_input_tokens`.
- **Suggestions**, in this order:
  - `allow`, id `allow:Bash(<prefix>:*)`: a Bash call approved at a
    permission prompt (a Notification about permission while it was open,
    then its PostToolUse) 3 or more times with one prefix (the first two
    words, three after `npm run`, `python -m` and the like; never `rm`,
    `sudo`, `curl`, `git push`, shells and the like). `action: {kind:
    "allow", pattern}`; `saves` is the waiting it caused, per hour.
  - `fix`, id `fix:<hash8 of the key>`: one error 3 or more times.
    `action: {kind: "fix", error}`; `saves` is its redo time.
  - `capsule`, id `capsule:<agent>`: files one agent read 4 or more times.
    `action: {kind: "capsule", agent, files}` (it says how; nothing to
    apply).
  - `sequence`, id `sequence:<path>`: agents' edit calls on one file
    switched between agents twice or more. `action: {kind: "sequence",
    path, agents}` (nothing to apply).
  - the report's own, id `report:<i>`, `kind` as the report's, `title` its
    text, `saves_ms` its seconds, `action: null`, `from: "chloectx"`.
- `report` is `chloectx report --json --days 1` run in the main checkout,
  as it prints it, or null (and `missing` says why).
- `POST /api/flow/apply {id, repo, minutes?, text?}`:
  - `allow:…` adds the pattern to the mark `allow:<repo>` (a JSON list),
    and the supervisor writes the repository's list into
    `permissions.allow` of the settings file of every agent it starts from
    then on. → `{ok, id, applied: {kind, pattern}, allowed: [pattern],
    note}`.
  - `fix:…` needs `text`, the fix; it finds the suggestion in the window
    (`minutes`, default 60) and runs `chloectx remember fact "When this
    error comes back: “<error>”, the fix is: <text>"` in the repository.
    → `{ok, id, applied: {kind, error, text}, chloectx: "<its output>"}`;
    409 when ChloeCtx fails.
  - Anything else is 400 (it says how; nothing to apply).
  - Applied suggestions are kept in marks (`flow:<repo>:<id>`) and come
    back with `applied: true`.

#### Deviations (Map, Preview, Flow)

- `marks`' column `key` is a reserved word of ChloeDB's SQL, so it is
  quoted: `"key" KEYWORD PRIMARY KEY` (and `"key"` in every statement).
  `StateDb::mark`/`set_mark` and `Supervisor::mark`/`set_mark` wrap it.
- Map adds `repo`, `missing`, and `kind`/`files`/`collapsed` on tree
  nodes; `heading_into` adds `changed_by` and `read_by`; agent files add
  `status`.
- A preview URL may carry a path (a dev server under a base path).
- `POST /api/preview/route` adds `matched`, `score`, `message` and
  `missing`; `related` records are `{id, kind, text, ts, files}`.
- The overlay also takes `wb-pick-mode`, posts `wb-ready`, and `wb-who`
  takes `texts`, `selector`, `max` and `watch`; its items carry `classes`.
- Flow adds `repo`, `window.minutes`, per-agent `tokens`, `missing`, and
  on each suggestion `kind`, `saves_ms`, `applied` (and `from` for the
  report's); `apply` takes `repo` (and `text` for a fix).
- A failure's error comes from the transcript when the hooks do not say
  (Claude Code runs PostToolUse after successful calls only).

### Away

- `POST /api/seen` sets `last_seen` (the UI calls it when the page is
  visible and when it is hidden).
- `GET /api/digest?since=<ms>` (default `last_seen`) →
  `{since_ms, until_ms, tiles: {done, decided, waiting, learned},
  story: [{ts, agent: Agent|null, text, chips: [{kind, text}]}],
  questions: [Question], rules: [Rule], replay: [{agent, from_ms, to_ms,
  label}]}`: outcomes done, questions answered (by you, a rule or the
  terminal), questions still open, ChloeCtx records learned, and a story
  line per agent from its narrations and chapters; `replay` is the moments
  of decisions (each answered question and each decision record with a
  rewind), for the Rewind player to play one after another.
- The UI opens on Away instead of Mission when `last_seen` is more than 30
  minutes old and something happened since.

### The levels' backend as built (Mission, Decide, Story, Away)

Modules: `src/levels.rs` (the routes, what follows each hook, delivery,
steer), `src/goals.rs`, `src/questions.rs`, `src/narrate.rs` (transcripts
and sentences), `src/story.rs`, `src/digest.rs`. `server.rs` merges
`levels::routes()` under its guard. Rows' times are ISO 8601 text, as
`Agent`'s; numbers in milliseconds are named `*_ms`.

`Agent` gains `goal`, `outcome`, `narration` and `narration_ts` (ISO).
`POST /api/agents` takes `goal?` and `outcome?`: an outcome's agent works
toward its goal and the outcome becomes `active` (409 when the outcome
already has a running agent, 400 when there is no such outcome); an agent
started by an agent with a goal works toward that goal too. New event
kinds: `question` (data `{question}`), `answered` (`{question, by}`) and
`steer` (`{id, text}`); the stream carries them as any event, and no new
SSE event types are added (a page refetches `/api/questions` on them).

```
Goal     {id, repo, text, state, lead, created, done,
          outcomes_total, outcomes_done, needs_you, progress}   -- not counting dropped ones
Outcome  {id, goal, title, detail, state, agent, agent_name, agent_state,
          ord, after, progress, created, updated, updated_ms}
Question {id, agent, agent_name, outcome, repo, kind, text, context,
          options: [{label, detail?, matches_precedent: bool}], recommended,
          precedent: {text, ids, agents: {record id: Agent}, missing,
                      rules: [{id, text, option}]} | null,   -- null until chloectx answered
          state, answer, answered_by, rule, created, created_ms, answered, answered_ms}
Rule     {id, repo, text, question, ctx_id, active, created, created_ms}
Record   {id, kind: "decision"|"preference"|"fact"|"fix", text, ts, ts_ms,
          session, agent: Agent|null, files: [path]}
```

| route | body | answer |
|---|---|---|
| `POST /api/goals` | `{repo, text, plan?, model?, worktree?}` | 201 `{goal: Goal, lead: Agent\|null}` |
| `GET /api/goals?repo=` | | `{goals: [Goal]}`, newest first |
| `GET /api/goals/:id` | | `{goal, outcomes, agents, open_questions, learned: [Record], heads_up: [], pulse: [[minute_ms, count]] (60), missing}` |
| `PATCH /api/goals/:id` | `{text?, state?}` | `{goal}` |
| `GET /api/outcomes?goal=` | | `{outcomes: [Outcome]}`, in order |
| `POST /api/outcomes` | `{goal?, title, detail?, after?}` | 201 `{outcome}`; `goal` defaults to the calling agent's (`x-workbench-agent`) |
| `PATCH /api/outcomes/:id` | `{title?, detail?, state?, progress?, agent?}` | `{outcome}` |
| `POST /api/questions` | `{question, options?: [{label, detail?} \| "label"], recommended?, context?}`, header `x-workbench-agent` (or `agent` in the body) | 201 `{question}`, already answered when a rule answers it |
| `GET /api/questions?repo=&state=&agent=` | | `{questions: [Question]}`, newest first |
| `GET /api/questions/:id` | | `{question}` |
| `GET /api/questions/:id/wait?timeout_s=30` | | `{question, answered: bool}` once it is not open, or at the timeout |
| `POST /api/questions/:id/answer` | `{option?: index from 0, text?, rule?}` | `{question, rule: Rule\|null, missing}`; 409 when not open |
| `GET /api/rules?repo=` | | `{rules: [Rule]}`, oldest first |
| `POST /api/rules` | `{repo, text}` | 201 `{rule, missing}` |
| `PATCH /api/rules/:id` | `{active?, text?}` | `{rule}` |
| `POST /api/agents/:id/steer` | `{text}` | `{ok, steer: {id, text}, delivered: "typed"\|"queued"}`; 409 when not running |
| `GET /api/agents/:id/story` | | as above, plus `missing` |
| `POST /api/seen` | `{ts_ms?}` | `{ok, last_seen_ms, previous_ms}` |
| `GET /api/digest?since=&repo=` | | see below |
| `POST /api/hook` | as before | `{ok, output: {hookSpecificOutput: {hookEventName, additionalContext}} \| null}` |

Story: `chapters[].steps[]` also has `running` (a call not finished yet);
`knew[]` is `{id, kind, text, score, decision, turn, receipt, shown_ms,
deja_vu}`, each record once, where it was first shown; `deja_vu[]` is
`{text, record, when: ISO of the record's own time | null, session,
agent: {id, name} | null, shown_ms}` (`when` and `session` from one
`chloectx sql --json` on the déjà vus' ids); `tokens` sums the `usage` of
each assistant message once (Claude Code writes a line per content block,
each with the message's usage) over every transcript the agent's hooks
named, read from their last 64 MiB.

Digest: `{since_ms, until_ms, last_seen_ms, away_ms, tiles: {done, decided,
waiting, learned}, decided_by: {you, rule, terminal}, done: [Outcome],
story: [{ts, ts_ms, agent: Agent, text, chips: [{kind: "edits"|"done"|
"decided"|"rule"|"waiting", text, question?, by?}]}], questions: [open
ones, then those answered since], rules: [made since], learned: [Record],
replay: [{agent, from_ms, to_ms, at_ms, label}], happened: bool, missing}`.
`since` defaults to `last_seen`, else the last 24 hours; `learned` asks
`chloectx sql --json` in each repository that saw work (at most 5).

MCP tools added: `ask_human {question, options?, recommended?, context?}`
(answers "The user answered: …", or "A rule the user set (r…) answers it:
…", or after 30 minutes that there is no answer yet), `add_outcome {title,
detail?, after?}` (the caller's goal), `update_outcome {id, progress?,
state?, title?, detail?}` (answers the goal's outcomes, a line each), and
`spawn_agent` takes `outcome`.

ChloeCtx: `chloectx receipts --json` prints `{text, receipts: [{id, ts
(seconds), session, turn, source, decision, shown, rows: [{id, kind, tag,
rank, score, decision, text}] (the rows it showed, their text as it is
now), tokens, txn, query}]}`.

#### Deviations

1. `POST /api/hook` answers `{ok, output}`, and the hook prints `output`
   alone (never the `ok`), only for an answer of 200 within its 2 s.
2. `last_seen` is kept in a table of the levels' own, `seen (id, value,
   ts)`, not in `marks`; its key column is `id` because `key` is a
   reserved word of ChloeDB's SQL (`marks` will need the same).
   `deliveries` gains `via`.
3. Narration at `UserPromptSubmit` is "Starting on: <the prompt's first
   sentence>", since the transcript is written after the hook fires; at
   `Stop` it is the first sentence of `last_assistant_message` when the
   payload has one; at `PostToolUse` it is the transcript's tail, else a
   sentence from the hook's own tool call. It is made on a thread of its
   own, one at a time per agent (the newest hook wins), and a waiting
   agent keeps its question as its sentence.
4. An outcome's agent is told its outcome (and to use `update_outcome` and
   `ask_human`) as additional context at its `SessionStart`, not in its
   task. Rules and the outcome are told again after `/clear` or a
   compaction (`SessionStart` with source `clear` or `compact`).
5. Steers are events of kind `steer`. One for an idle agent is typed; any
   other is told at the next `SessionStart`, `UserPromptSubmit` or
   `PostToolUse` as "The user adds: …", and those still untold when a
   turn ends (`Stop`) are typed in.
6. An outcome's state: `done` and `dropped` stay once set; no agent →
   `next`; its agent waiting or with an open question → `needs_you`; its
   agent exited → `done` when it exited with code 0 at progress 100, else
   `needs_you`; otherwise (working, idle, starting) `active`. It is brought
   up to date when outcomes are read, and written back when it changed.
7. A permission prompt is a Notification with `notification_type`
   `permission_prompt` (or, without a type, the word "permission"); its
   question's `context` is the agent's last `PreToolUse` summary ("Bash:
   cargo test"); there is one open per agent, it records no `question`
   event (its Notification stands for it), and any other hook of the agent
   before your answer marks it `answered_by: terminal`.
8. A rule answers a question by itself (`answered_by: rule:<id>`) when an
   agent of the repository asks again, in the same words, the question the
   rule was made from, and that answer is one of the new options (or it
   has none).
9. `matches_precedent`: every word of the option's label (three letters or
   more, the commonest left out) is in an active rule of the repository or
   in one line of what `chloectx ask --json` printed (it gives the records'
   text as one block and their ids, not each record's kind).
10. `learned` of a goal is the last 24 hours, not the calendar day.
11. Déjà vu: a fix is an `error` record tagged `fix`; "right after a failed
    tool call" is within 120 s after it; a failed call is a `PreToolUse`
    with no `PostToolUse` before the turn ended (Claude Code fires none
    then), a `PostToolUseFailure`, or a response with `is_error`.
12. `heads_up` is always `[]` here; the map fills it (`map::collisions`,
    a TODO in `goals::detail`).
13. `--narrate-model` (the optional chapter titles and summaries by a
    model) is not built; titles are the prompt's first eight words and
    summaries the turn's last assistant text, first two sentences.
14. Extra routes: `PATCH /api/goals/:id`, `GET /api/outcomes`, `GET
    /api/questions/:id`, `POST /api/rules`.

### UI

- `ui/app.js` keeps the shell (top bar, stream, token, terminal grid and
  zoom, decision trail). The new screens are ES modules in
  `ui/levels/`: `mission.js`, `story.js`, `decide.js`, `map.js`,
  `preview.js`, `flow.js`, `away.js`, and `common.js` (shared bits:
  agent chip, ◆ ChloeCtx chip, time, a small chart helper). Each exports
  `mount(root, ctx)` → `{update(state), unmount()}`, where `ctx` has
  `api(path, opts)`, `state` (agents, events from the stream), `go(hash)`,
  `openWhy(args)`, `openRewind(args)`, `zoom(agentId)`. The server
  compiles them in and serves `/levels/<name>.js` (and
  `/preview-overlay.js`), from disk under `CHLOEDB_WORKBENCH_UI_DIR`.
- A left rail: Mission, Decide (with the count of open questions), Map,
  Story, Preview, Flow, Away, and Terminals (the grid). Routes are hashes:
  `#/mission`, `#/decide`, `#/map`, `#/story/<agent>`, `#/preview/<agent>`,
  `#/flow`, `#/away`, `#/grid`, `#/zoom/<agent>`.
- Mission is the default screen once a goal exists; before that, the grid
  with a "Set a goal" box above it. Opened without a route after more than
  30 minutes away (`/api/digest`'s `away_ms`) when something happened, it
  opens Away; leaving the page (hidden or closed) marks it seen
  (`POST /api/seen`).
- `ui/dev/mock-server.mjs` serves fixtures for every new route (a story of
  a payments repo, as in the concepts), so each screen can be built and
  screenshotted without a model.
- `ui/dev/seed-real.mjs --start` runs the real server instead, with the
  tests' fake `claude` and `ui/dev/fake-chloectx.mjs` (which answers
  `ask`, `sql`, `receipts`, `remember`, `blame`, `report` and `wiring` in
  chloectx's formats, from a JSON file), and seeds it through the HTTP
  API: a repository, a goal with its outcomes, agents that write, read,
  fail and ask, questions with a precedent, a rule, steers, a collision
  and a preview. Re-runnable; the header says how.

## ChloeCtx is required

Most of what the levels show comes from ChloeCtx, so every session
Workbench starts is wired to it, without touching the user's settings:

- `chloectx wiring --project <session cwd> --exe <abs path>` (hidden;
  `chloectx/src/install.rs`, `wiring()`) prints `{version, exe, wired:
  "user"|"project"|null, settings: {hooks, permissions: {allow:
  ["mcp__chloectx"]}, env: {CHLOECTX_DISTILL_MODEL}}, mcp: {chloectx:
  {type, command, args: ["mcp"]}}, store, model: {ok, path, fix}}`.
  `settings` and `mcp` are `{}` when the folder is already wired by the
  user's settings or its project's, so a session is never recorded twice
  (a worktree has no `settings.local.json` of its own, so it gets them).
- At spawn (`supervisor.rs`), before a worktree is made, `wiring` is run
  for the repo; a failure answers 409 `chloectx_missing` with the fix.
  Then it is run for the session's cwd and merged (`src/ctxwire.rs`) into
  the session's `settings.json` (hooks appended per event, allow joined,
  env added) and `mcp.json` (server `chloectx` beside `workbench`).
- `GET /api/chloectx`: `{required, ok, bin, version, wired, model,
  problem, fix}`. The UI (`ui/levels/setup.js`) checks it at start and,
  when not ok, shows a sheet with the problem and the fix
  (`./install.sh --workbench`), "Check again", and a bar while unfixed.
- `--chloectx-bin` resolves a bare name on PATH, else `~/.local/bin/`.
- `./install.sh --workbench` builds and installs both programs, fetches
  the model and runs `chloectx wiring`; Claude Code's settings are left
  alone unless `--user-too` (or `--project`) asks.
- `--no-chloectx` (hidden): no wiring, no setup sheet. The tests pass it
  unless a test asks for ChloeCtx (`"+chloectx"` in `Server::start`).

## Fourth build: what only ChloeDB makes possible

The user's ask: use cases 1 to 11 of the list "unique to ChloeDB", each
with UI where it makes sense, in new screens or in the existing ones. Five
parts, each built by one agent that owns the files named here. Shared
files take only additive, one-place edits (a line in `server.rs`'s route
merges and `LEVELS`, a table const in `db.rs`, tool entries and match arms
in `mcp.rs`, a link in `index.html`'s `#lvnav`, `SCREENS` and `LEVEL_OF`
in `app.js`, tests appended at the end of `tests/workbench.rs` under the
part's own heading, fixtures as new files in `ui/dev/fixtures/`).

Everything degrades when a helper is missing (`chloectx`, `chloedb-crawl`,
Node, Playwright): the UI says what is missing and how to get it, the API
answers `{"available": false, "why": ...}`, and the tests do not need it.
No model is called without the user starting it.

| # | use case | part | backend | MCP | UI |
|---|---|---|---|---|---|
| 1 | code search for agents | Search | `src/search.rs`: a per-repo index of the worktree's files in the state DB (`code_files`, `code_chunks`: path, symbol, lines, text with a code analyzer), reindexed from `git ls-files` and the agents' edits (PostToolUse Edit/Write); `GET /api/search/code?repo&q&agent&tokens` answers chunks and, with `tokens`, a `PACK n TOKENS` text | `search_code` | **Library** screen (new, level Plan): Code tab; and `/` in the top bar opens a quick search over code, docs, decisions |
| 10 | docs on tap | Search | `src/library.rs`: the repo's dependencies (Cargo.toml, package.json, pyproject, go.mod) to doc sites; fetched by `chloedb-crawl` when it is on PATH, else a small built-in fetcher (same host, depth 1, 200 pages); pages as rows `doc_pages`; `POST /api/library/crawl`, `GET /api/library`, `GET /api/search/docs` | `search_docs` (answers with URLs to cite) | Library screen: Docs tab (each library, pages, crawl state, which agents cited what) |
| 2 | simulated users | Preview lab | `src/swarm.rs`: runs `chloedb-swarm`'s persona runner (Node, Playwright) against `/preview/:agent/`, a persona and a task; findings as rows `swarm_runs`, `swarm_findings`; a finding becomes a Decide question or a steer to the owning agent | `test_with_persona` | Preview: "Test with a persona" panel (persona, task, live steps, findings with Send to agent / Ask me) |
| 3 | runtime blame | Preview lab | `src/runtime.rs`: `POST /v1/logs` (OTLP/HTTP JSON) and the preview proxy's 5xx and the overlay's `window.onerror` as rows `runtime_errors`; each error is joined (ASOF JOIN on time, and the file in its stack) to the agent's last edit before it; `GET /api/runtime/errors` | `runtime_errors` | Preview: an errors strip; each error says "probably <agent>'s edit to <file> at 7:12" and opens the Why card for that line |
| 4 | fleet alerts as SQL | Health | `src/alerts.rs`: rules as rows `alert_rules` (a SQL over `events`, `agents`, `test_runs`, a threshold, a window) checked every 30 s; firings group into `incidents` (same agent, repo or file within 10 min) with a board (the firings, the agents' sentences, the events around them); built-in rules: a test re-run failing 5 times, tokens per hour above a limit, two agents editing one file, an agent idle while asked; actions: notify, ask, pause the agent | `list_incidents` | **Health** screen (new, level Plan): Incidents (board per incident), Rules (edit the SQL, test it now); a count on the nav |
| 5 | flaky test radar | Health | `src/testruns.rs`: every test run an agent makes (Bash PostToolUse of cargo test, npm test, pytest, go test, jest/vitest; parsed per test) as rows `test_runs` (test, agent, commit, passed, ms); p50/p99 with `approx_percentile`, pass rate, flakiness (passed and failed on one commit); `GET /api/tests` | `test_history` | Health screen: Tests tab (radar: flaky, slow, newly failing) and a chip on Story's steps that ran tests |
| 7 | blast radius | Ship | `src/blast.rs`: from an agent's diff (functions changed) to the tests that cover them (test files that name them, `test_runs`), the decisions about those lines (`chloectx blame`, the ChloeCtx graph), and the other agents touching the same files or callers; `GET /api/agents/:id/blast` | `blast_radius` | **Ship** screen (new, level Changes, per agent): a radial graph and lists; the Changes tab gets a "Blast radius" summary |
| 11 | capsule with every PR | Ship | `src/capsule.rs`: `chloectx pack` of the agent's decisions (its session, since its start) into `<session dir>/capsule`; a PR body section that links it and says how to ask it (`chloectx mount`); `POST /api/agents/:id/capsule`, `GET /api/agents/:id/pr` (title, body with the decisions, blast radius, tests) | `prepare_pr` | Ship screen: the PR draft and its capsule, "Ask the capsule" box |
| 8 | time travel | Time and guard | `src/timetravel.rs`: snapshots of the state DB every 10 min and at each goal change (`link_from`, kept a day), with the commit of each worktree at that time; `GET /api/time?at=ts` answers Mission's and Map's data as of then | | a time scrubber on Mission and Map ("what did the fleet know at 7:12") |
| 9 | guarded actions | Time and guard | `src/guarded.rs`: `change_requests` (a migration, a deploy, a push to main, any command named in the repo's guarded list); an agent's key can only propose; the user approves in Decide and Workbench runs it in the agent's worktree, keeping its output; PreToolUse denies the guarded commands with a pointer to `propose_change` | `propose_change` | Decide: "Change requests" section (command, why, diff, risk, Approve and run / Decline) |
| 6 | model routing that learns | Time and guard | `src/routing.rs`: per finished task, features (words of the task, files, repo, model, tokens, minutes, redo: reopened or steered, outcome done) as rows `task_features`; a suggestion for a new task from the nearest past tasks (BM25 over their task text) weighted by outcome and cost; `GET /api/route?repo&task` | (spawn_agent's `model` defaults to the suggestion when left out) | the New agent dialog: "Suggested: Haiku, 9 of 11 like this finished, $0.40 avg" with the alternatives |

New screens join `SCREENS` and the nav: Library (`ui/levels/library.js`),
Health (`ui/levels/health.js`), Ship (`ui/levels/ship.js`). Additions to
existing screens are separate modules that the screen imports and mounts
in one place: `ui/levels/preview-lab.js`, `ui/levels/timebar.js`,
`ui/levels/change-requests.js`, `ui/levels/route-hint.js`.

Each part first builds its UI on fixtures in the levels mock
(`ui/dev/levels-mock.mjs`) and screenshots it dark and light at 1440x900
into `/mnt/project-files/workbench/fourth/` (`<n>-<name>-{dark,light}.png`),
then the backend, then the UI against the real API.

### Health as built (rows 4 and 5)

**Capture.** `testruns::after_hook` runs after each hook is recorded
(`server.rs`'s `hook`, one line): a Bash `PostToolUse` or
`PostToolUseFailure` whose command `flow::is_test` takes for a test run is
parsed on a thread; an Edit, Write, MultiEdit or NotebookEdit becomes a row
of `file_edits`; each hook's `transcript_path` is remembered for the
tokens. Claude Code fires no `PostToolUse` for a failed Bash call, and
Workbench's sessions do not register `PostToolUseFailure`, so at `Stop`
the turn's test calls that had none are read from the transcript's
`tool_result` (with `is_error`) instead. Parsers: cargo test and nextest
(`test x ... ok|FAILED|ignored`, `<0.012s>` from `--report-time`, the
`---- x stdout ----` sections as the error), pytest (`-v` lines, `PASSED
[ 50%]`, `--durations`, the short summary, the `___ name ___` sections),
jest and vitest (✓ ✕ ↓ lines under their file and describe blocks, `●`
sections), go test (`--- PASS: TestX (0.01s)` as `pkg › TestX`). A command
none parses is one row named by the command, passed when it exited 0. A
test's `ms` is its own when the output gives it, else the run's (PreToolUse
to Post), and `own_ms` says which.

**Tables** (in `db.rs`): `test_runs` (one row per test per run: `run`,
`repo`, `agent`, `"commit"` (quoted: `commit` is a reserved word), `tree`
(a hash of `git diff HEAD` and the untracked files, `clean` without them),
`runner`, `command`, `test`, `outcome` passed|failed|skipped, `passed`,
`ms`, `own_ms`, `run_ms`, `exit_code`, `error`, `use_id`, `ts`, `seq` (the
PreToolUse's event, which Story's steps carry)); `file_edits` (`repo`,
`agent`, `file` relative to the agent's checkout, so worktrees compare,
`tool`, `ts`, `seq`); `token_use` (one row per assistant message of a
transcript, keyed by its id: `input` (with cache writes), `output`,
`cache_read`, `ts`; read incrementally before each evaluation, the last
16 MiB the first time); `alert_rules`; `alert_firings` (`rule`, `entity`,
`agent`, `file`, `value`, `summary`, `state` firing|silenced|resolved,
`started`, `last_seen`, `ended`, `misses`, `agents`, `incident`);
`incidents` (`title`, `severity`, `state` open|resolved, `agents`, `files`,
`rules`, `actions` as JSON, `needs_you`).

**Rules.** A rule is one SELECT (or WITH … SELECT, no `;`) whose rows name
`entity` and `v`, and may name `agent`, `file`, `repo`, `detail`; `:since`
(now minus the window), `:now` and `:repo` are bound when the SQL names
them. A row whose `v` passes `cmp` (`> >= < <= = !=`) and `threshold` is a
firing keyed by rule, entity and agent; one not seen in 2 runs in a row is
resolved. The rules run every 30 s, and on `POST /api/alerts/check`.
Built-ins (switched off, not deleted): `test-refail` (a test failing 5
times in an hour for one agent; ask), `tokens-hour` (input and output
tokens over 400k in an hour; notify), `same-file` (2 agents editing a path
in 30 min; notify), `idle-asked` (an agent idle or waiting while its
question has been open 10 min; notify). A firing joins an open incident of
its repository that still fires or changed in the last 10 min and shares
an agent or a file with it (one that names neither joins one of the same
rule), else opens one; an incident resolves when none of its firings
fires. Actions run once per incident and rule: **notify** sets
`needs_you`, and app.js lists those incidents in Needs you after the
agents (a click opens the board); **ask** writes a question of kind
`alert` for the agent (Pause it / Tell it to stop and report / Let it carry
on), and its answer is carried out (Esc, a steer, nothing; other words are
steered as they are); **pause** sends Esc to the agent's terminal. You
resolving an incident silences its firings until they stop, so it does not
open again at once, and expires its open questions.

**Endpoints.** `GET /api/incidents?state=open|resolved|all&repo=`
(`incidents`, `counts`, `agents`, `eval_every_s`, `last_eval_ms`);
`GET /api/incidents/:id` (`incident`, `board`: `firings`, `sentences` (each
agent's narration), `events` (the agents' events from 6 min before it to
its end, an alert's as kind `fired`), `tests` (the tests its test rules
named, with their last 20 runs), `error`, `related` (ChloeCtx's decisions,
preferences and facts naming its files, by `chloectx sql --json`, asked at
most once a minute), `window`, `missing`); `POST /api/incidents/:id/resolve`;
`POST /api/incidents/:id/act {kind: notify|ask|pause, agent?}`;
`GET|POST /api/alerts/rules`, `PATCH|DELETE /api/alerts/rules/:id` (a
built-in answers 409 to DELETE; SQL that does not run is refused with 400);
`POST /api/alerts/test {sql, cmp, threshold, window_s, repo?}` (`ok`,
`columns`, `rows` (200 at most), `count`, `ms`, `would_fire`, `params`,
`note`, `error`); `POST /api/alerts/check`. `GET /api/tests?repo=&days=7`
(`totals`, `tests` (per test: runs, pass rate, `p50_ms`, `p99_ms`, `flaky`
and why, `newly_failing`, `slow`, `last`, the last 20 runs), `commands`,
`agents`, `percentiles`, `missing`); `GET /api/tests/history?repo=&test=`;
`GET /api/tests/runs?agent=` (Story's chip). Without `repo`, the
repository most agents work in.

**Verdicts.** Flaky: passed and failed on one commit with the same tree,
or two flips within a day with no new commit. Newly failing: its last run
failed and one before it passed. Slow: p99 at least 5 s, or at least 1 s
and three times its p50.

**MCP.** `list_incidents {state?}` and `test_history {test?, days?}`
answer text, for the caller's repository.

**UI.** `ui/levels/health.js` (`#/health`, level 1, a count of open
incidents on the nav as `lvn-health`): Incidents (open and resolved; the
board: what fires with a meter against the threshold, what the agents say,
the error, the tests in it, Done about it, the events around it, what
ChloeCtx knows of its files; Pause, Ask me in Decide, Resolve), Rules (the
SQL, the tables it may read, comparison, threshold, window, severity,
action; ⌘↵ tests it now and highlights the rows it would fire on), Tests
(a radar of p50 against failure rate, the flaky, newly failing and slow
lists, every test with its runs, the recent commands). `1` `2` `3` switch
tabs. `ui/levels/test-chip.js`, mounted on Story's `run` steps: "41 passed ·
2 failed · 1 flaky" for the runs of that step, opening Health's Tests.
Fixtures: `ui/dev/fixtures/health.mjs`.

**Deviations.** p50 and p99 come from `approx_percentile(ms, 0.5)` and
`(ms, 0.99)` over `test_runs` grouped by test; when that form is not there
(an older ChloeDB) they are computed in Rust from the rows (nearest rank),
and `percentiles` says `rust`. Incidents group by a shared agent or file,
not by repository alone (that grouped every firing of a busy repository
into one). `tokens-hour` reads tokens from transcripts, so an agent
without one counts none. Firings in a repository Workbench has no agent in
carry the rule's repository, or none. Workbench's sessions do not register
`PostToolUseFailure`; adding it to `supervisor::settings_json` would let a
failed run be recorded as it ends rather than at `Stop`.

### Ship as built (use cases 7 and 11)

**Endpoints.**

- `GET /api/agents/:id/blast` (`src/blast.rs`) answers
  `{agent, repo, base, head, branch, totals, risk, files, functions, tests,
  decisions, related_files, agents, sources, missing}`:
  - `functions`: each function the diff against the agent's base touches,
    `{id: "path#name", name, kind, lang, path, line, status:
    changed|added|deleted, added, removed, callers, callers_total, tests,
    decisions, agents}`. They are found by a symbol finder per language
    (Rust, JS/TS, Python, Go, Java/Kotlin/C#, C/C++, Ruby, PHP, Swift; hand
    written, no regex crate). It runs over the new text for added lines
    and the base's text (`git show base:path`) for removed ones. Test
    files are not counted as changed functions. Callers come from
    `git grep -w` (8 per function, at most 40 functions searched).
  - `tests`: test files that name a changed function (3 points per name)
    or import its file (2), ranked, the top 20. Each is
    `{path, score, covers, why, runs}`. `runs` comes from the `test_runs`
    table (Health's) when it exists: total, passed, failed, flaky (passed
    and failed on one commit), p50_ms, last_passed. When there is no such
    table, `runs` is null, `sources.test_runs` is false and `missing`
    says so.
  - `decisions`: `{id, kind, text, ts, session, agent, files, via,
    functions, superseded, superseded_by, line, entity}`, where `via` is
    any of `blame`, `file` and `graph`:
    - `blame`: `chloectx blame <path>:<first added line> --json
      --no-replay` for up to 8 functions.
    - `file`: the repository's decision, preference and fact records that
      name a changed file (`map::file_records`).
    - `graph`: `chloectx rpc '{"op":"view","what":"graph"}'`, meaning
      notes that touch a changed file, one hop over the entities they are
      about, and `supersedes` links. The other files those notes touch are
      `related_files`.
  - `agents`: the other agents whose worktree diffs change the same files
    (`same_file`), change a file that calls a changed function
    (`calls_changed`), or are reading a changed file (`reading`), from
    `map::territory`.
  - `risk`: `{level: low|medium|high, score, line, reasons}`:
    - Points: 2 per untested function (at most 6); 3 per function deleted
      but still called; 3, 2 and 1 for the three kinds of other agent; 1
      for a flaky test and 2 for a failing one; 1 per superseded decision;
      1 for more than 10 callers.
    - Levels: low below 3, medium below 13, high from 13.
    - `line` is one sentence, for example "Medium risk: 6 functions
      changed with 5 callers; rateFor has no test that names it and
      cart-ui changes src/cart/total.ts too."
- `POST /api/agents/:id/capsule` (`src/capsule.rs`) moves an old capsule
  aside, then runs `chloectx pack <session dir>/capsule --since-days
  <since the agent started, plus a minute> --title "<name>: <task's first
  sentence>" --json` in the worktree. It answers
  `{available, capsule: {dir, title, records, by_kind, bytes, sha256,
  created, since, pack_ms, scope, mount, claude}}`, or
  `{available: false, why}`. `GET /api/agents/:id/capsule` answers the
  stored one, if its `manifest.json` is still there.
- `POST /api/agents/:id/capsule/ask {question}` answers
  `{available, via: capsule|repo, question, text, ids, agents, ms,
  missing}`:
  - With a capsule, it runs `chloectx mount <dir>` over stdio (MCP) and
    calls its `capsule_ask` tool, with no model. The ids come from the
    answer's `(capsule …, records: …)` trailer and `[id]` marks.
  - Without one, it runs `chloectx ask --json` in the worktree, and
    `missing` says that the repository's memory answered.
- `GET /api/agents/:id/pr` answers `{agent, title, branch, base_branch,
  base, body, body_file, gh, push, sections: {summary, commits, decisions,
  blast, tests}, capsule, missing}`:
  - `title`: see "The pull request says what was done" below.
  - `body` (Markdown) has these sections: Summary, Decisions behind it
    (`- text (whose)`, in plain words, never an id nor a fact; the agent's own session's records from
    `chloectx sql` first, then those about its lines), Blast radius,
    Tests run (the agent's Bash test commands and their outcome, from its
    events), and Ask why (the `claude mcp add … -- chloectx mount <dir>`
    command).
  - It saves the body to `<session dir>/pr.md`.
  - `gh` is `gh pr create --base … --head … --title … --body-file …`.
    Workbench never pushes or opens the pull request.

**Tables.** None new. The capsule is kept as the mark `capsule:<agent>`.

**MCP.** There are two tools:
- `blast_radius {id?}` answers the risk line and the lists, as text.
- `prepare_pr {id?, capsule?}` packs the capsule unless `capsule` is
  false, then drafts the pull request. It answers the title, the body
  file, the push and `gh` commands, and the body.

Both default to the calling agent.

**UI.**

- **Ship** (`ui/levels/ship.js`; `#/ship/<agent>`; level Changes; nav link
  Ship) has two tabs:
  - **Blast radius**:
    - A risk banner with its reasons.
    - A radial SVG graph with the change at the center and rings of
      functions, tests, decisions and agents. Hovering a node dims the
      rest, and clicking a function opens the Why card.
    - An "If this merges" card.
    - Four lists: functions, tests, decisions with ◆ chips, and other
      agents.
  - **Pull request**:
    - An editable title and the body, as a preview or as Markdown.
    - Copy body, Copy gh command and Copy title, with the push and gh
      commands.
    - The Capsule card: records, size, kinds and the mount command, with
      Pack and Pack again.
    - "Ask the capsule".
    - Tests it ran.
- **Changes tab**: `ui/levels/blast-summary.js` is mounted once above the
  diff (`showBlastSummary` in app.js). It shows one row: the risk level,
  the counts of functions, tests and untested functions, a ◆ decisions
  chip, the other agents, and Ship →. Below it is the risk line. It reads
  only once it is visible, and keeps an answer for 20 s.
- **Fixtures**: `ui/dev/fixtures/ship.mjs`, with the same shapes as the
  real API.

**Deviations.**

- `chloectx pack` has no session filter. The capsule is the repository's
  memory since the agent started (`--since-days`), and its `scope` says
  so. The PR's decisions are filtered to the agent's sessions.
- `chloectx ask` takes no capsule path, so "Ask the capsule" drives
  `chloectx mount` instead.
- Changed functions come from a finder over both sides of the diff
  rather than git's hunk headers, which name the enclosing function of
  the context and miss new ones.
- `test_runs` is read only if Health created it.

### Time and guard as built (rows 8, 9 and 6)

**Tables** (`src/db.rs`, `TIME_GUARD_TABLES`): `time_snapshots` (id, ts,
reason `timer`|`goal`|`manual`, detail, txn, heads: JSON), `change_requests`
(id, agent, repo, cwd, branch, command, why, rule, risk, risk_why, diff,
precedent, state `open`|`running`|`done`|`failed`|`declined`, output,
exit_code, ms, decided_by, reason, told `typed`|`queued`, created, decided),
`task_features` (id = the agent's, repo, name, task, files, model, input,
output, cache_read, cost_usd, minutes, redo, done, outcome, exit_code,
ended). A repository's guarded list is the mark `guarded:<repo>`.
`StateDb::checkpoint(dest, link_from)` and `StateDb::open_read_only(dir)`
are new.

**Time travel** (`src/timetravel.rs`). A thread looks at goals' and
outcomes' states every 5 s: a change is a snapshot (`goal`, detail such as
"goal set: …", "outcome done: …", "outcome needs you: …"); every 10 minutes
a snapshot (`timer`) when events were written since the last one, the
first one 10 minutes after the server starts; `POST /api/time/snapshot` takes
one by hand (201 `{snapshot}`). A snapshot is ChloeDB's checkpoint in the
directory layout at `<data-dir>/snapshots/<id>/`, hard-linking the
unchanged segments of the one before; snapshots older than a day and
folders no row names are removed (at start and every 5 minutes). Its row's
`heads` holds, per agent on the map then: branch, HEAD, `tree` (the
worktree's uncommitted state as a commit on HEAD that no ref names, written
through a copy of its index with `git add -A`/`write-tree`/`commit-tree
--no-gpg-sign`, so the agent's own index is untouched; git keeps such
objects until a prune), its diff from its base and what it had read.
- `GET /api/time?repo=` → `{available, repo, now_ms, from_ms, keep_ms,
  every_ms, snapshots: [{id, ts_ms, reason, detail}], events: [[minute_ms,
  count]]}` (the repository's agents' events per minute, last day).
- `GET /api/time?at=<ms|s|ISO>&repo=` opens the nearest snapshot read-only
  (by absolute distance; kept open for the next request) →
  `{available, at_ms, repo, snapshot, distance_ms, heads: [{agent, name,
  branch, head, tree, dirty, files: n}], agents: [Agent as then], mission:
  {goals, detail (the active goal's), details: {goal id: detail}, questions,
  rules}, map, missing}`; `{available: false, why}` before any snapshot.
  Mission's detail has Mission's shape (`goals::detail`); its `pulse` comes
  from the live events up to `at` (events are only added), `learned` from
  ChloeCtx filtered to `ts_ms <= at`. `map` is `map::map_of` over the work
  the snapshot kept, with pins of records made by then; the tree is the
  repository's files now.
- Refactors, minimal: `goals.rs` and `questions.rs` make `Goal::of`,
  `Outcome::of`, `Question::of`, `Rule::of` and their column lists
  `pub(crate)` (read from a snapshot's tables); `map::map` is split into
  `map` and `map_of(sup, main, work, until_ms)`.

**Guarded actions** (`src/guarded.rs`).
- Agents' keys: each agent's hooks and MCP server get a token of their own
  (`guarded::agent_token`, in `supervisor.rs`'s spawn); the server's guard
  takes it on every route, but approve, decline and `PUT /api/guarded`
  answer 403 to anything but the user's token.
- The guarded list, `GET /api/guarded?repo=` → `{repo, default_branch,
  rules: [{id, kind: builtin|prefix, label, detail, on}]}`; `PUT
  /api/guarded {repo, rules}` keeps the built-ins' `on` and the prefix
  rules (a command whose first words are the label's). Built-ins:
  `migrations` (prisma migrate deploy/reset/dev, prisma db push, sqlx
  migrate run/revert, diesel migration run/redo, alembic upgrade/downgrade,
  rails/rake db:migrate, knex migrate:latest, flyway migrate, manage.py
  migrate, goose up, dbmate up, sequelize db:migrate, typeorm
  migration:run, drizzle-kit push/migrate, liquibase update, a package
  script whose name holds "migrat"), `deploy` (a `deploy*` script,
  kubectl apply/delete, terraform/tofu apply, terraform destroy, fly
  deploy, helm upgrade/install, npm/pnpm/yarn/cargo publish, twine upload,
  docker push, serverless/sls/cdk/wrangler/firebase deploy, vercel or
  netlify with --prod, gcloud … deploy, a `deploy`/`release` package
  script), `push-default` (a git push whose refspec names the default
  branch, a `+` refspec, --force*, --mirror, --all, --prune, or a plain
  push from the default branch), `rm-outside` (a recursive rm of a path
  outside the agent's worktree: `~`, `$VAR`, or resolved outside it;
  temporary folders are allowed). Commands are split as a shell splits
  them (quotes, `&&`, `||`, `;`, `|`), `sudo`/`env`/`VAR=1`/`npx`/`pnpm
  exec` are looked past, `cd` is followed, and `bash -c`/`eval` are read.
- PreToolUse (in the server's hook handler, after `levels::after_hook`):
  a guarded Bash call answers `{"hookSpecificOutput": {"hookEventName":
  "PreToolUse", "permissionDecision": "deny", "permissionDecisionReason":
  "… propose_change …"}}` and records a `guarded` event.
- `POST /api/changes {command, why, risk?}` (from an agent: its key's, or
  the `x-workbench-agent` header) → 201 `{request}`: its rule, the risk
  (high for a push to the default branch, a deploy, an rm outside; medium
  for a migration or a prefix rule, high when the diff adds DROP TABLE,
  DROP COLUMN, TRUNCATE or DELETE FROM or the command may lose data; the
  agent's own word only raises it) and why, the agent's diff (12 files, 60
  lines each, migrations first); ChloeCtx's precedent (`trail::ask`) comes
  in later. `GET /api/changes?repo=&state=` (newest first), `GET
  /api/changes/:id`.
- `POST /api/changes/:id/approve` (user only; 409 unless open): runs `bash
  -c <command>` in the agent's worktree, stdin closed, `CI=1`, at most 10
  minutes, the last 64 KiB of output kept; answers when it ends or after
  8 s (`running`); the agent is told through `levels::steer` (typed when
  idle, else at its next hook), and `told` says which. `POST
  /api/changes/:id/decline {reason}` (user only) tells it why. Events
  `change_request`, `change_approved`, `change_declined`.
- MCP `propose_change {command, why, risk}`.

**Model routing** (`src/routing.rs`). A thread on the stream of changes
records an agent's `task_features` when it exits, or ends a turn (Stop)
with its outcome done; at start, agents that ended before without a row.
Model: haiku, sonnet, opus (by name), another name as given, `default`
when none. Tokens from its transcripts (`narrate::tokens`); cost at Haiku
5.5 $0.10/$0.50, Sonnet 5.5 $2/$10, Opus 5.5 $4/$20 per million input and
output tokens (cache reads $0.01, $0.10, $0.20), `default` and other names
priced as Opus 5.5. `redo`: prompts after the first plus steers. `done`:
its outcome is done; without an outcome, its last turn ended (a Stop after
its last prompt or notification).
- `GET /api/route?repo=&task=` → `{available, similar, apply, suggestion:
  {model, n, done, rate, avg_cost_usd, avg_minutes, avg_tokens}, why,
  alternatives: [same], examples: [{agent, name, task, model, done,
  cost_usd, minutes, ended, score}], default, missing}`. Similar: the
  repository's past tasks ranked by `bm25(task, :words, syntax => 'plain',
  min => 2)` (1 when the task has under 3 content words), within 30% of
  the best score. Per model, the rate of finishing with Laplace's rule; the
  suggestion is the cheapest model within 0.1 of the best rate. Under 3
  words: `similar: 0`, no suggestion. `apply` is `similar >= 5`.
- MCP `spawn_agent` without `model` asks `/api/route` and takes the
  suggestion when `apply` (and it is not `default`).

**UI**: `ui/levels/timebar.js` wraps Mission's and Map's `mount` (one line
each); in the past it remounts the screen with an `api` that answers
`/api/goals`, `/api/goals/:id`, `/api/questions`, `/api/rules`,
`/api/outcomes`, `/api/map`, `/api/agents[/:id]` from `/api/time?at=` and
refuses changes. `ui/levels/change-requests.js` is a section of Decide (one
import, one mount); `ui/levels/route-hint.js` attaches to the New agent
dialog from `app.js` (one line). Fixtures: `ui/dev/fixtures/timeguard.mjs`.

**Deviations and limits.** The screens still count "ago" and ETAs from the
real now while showing the past. The map's tree is today's files. A
snapshot's worktree commits are not kept by a ref. Precedents need
ChloeCtx; without it a request has none. `rm-outside` lets a recursive rm
of a temporary folder through.

### Search as built (rows 1 and 10)

**Tables** (`db.rs`, `SEARCH_TABLES`):
- `code_files` holds one row per file and tree: `tree` is `main` for the main checkout, or an agent's id for the files that agent changed in its worktree. The other columns are `sig` (size:mtime), `chunks`, `lines`, and `deleted`. A file deleted in an agent's tree hides the main checkout's copy.
- `code_chunks` holds `path`, `lang`, `symbol`, `kind`, `start_line`, `end_line`, `text`, `terms` and `head`. The last three are TEXT with the `simple` analyzer.
- `search_log` records which agent searched (`by`), with kind, q, hits, tokens and cites.
- `doc_libs` holds one row per library of a repository. Its `lib` is the API id, such as `crates-serde`, `npm-react` or `site-<host><path>`.
- `doc_pages` holds `url`, `title`, `text` (the english analyzer), `words`, `depth` and `fetched`.
- `doc_cites` records which agent's answer cited which page, and for what query.

**Code search** (`src/search.rs`):
- Chunks:
  - A chunk is one definition, with its doc comments and attributes. Definitions are found per line for Rust, Python, TS/JS, Go and Markdown headings. Methods get chunks of their own.
  - Leading lines form `head`. Chunks longer than 80 lines are cut at 60, and short neighbours are joined.
  - Files without definitions are cut into 60-line windows.
- Ranking:
  - There is no code analyzer, so identifiers are split in Rust. `terms` holds the parts of each compound identifier plus the parts joined (`cartTotal` → cart total carttotal; `cart_total` → carttotal). The query gets the same expansion.
  - The rank is `bm25(text) + bm25(terms) + 2*bm25(head)`, where `head` is the symbol plus the path's parts.
- Freshness:
  - A full pass runs `git ls-files --cached --others --exclude-standard` and chunks again only the files whose signature changed.
  - It runs on the first search of a repository (the search waits up to 20 s), in the background on a search once the last pass is a minute old, and on `POST /api/search/reindex`.
  - Each agent with a worktree that is running, or that ended in the last 24 h, gets its `git diff --name-only <base>` and untracked files as its tree. The trees of agents gone longer are dropped.
  - A `PostToolUse` of Edit, MultiEdit, Write or NotebookEdit reindexes its file in the agent's tree. A watcher thread on the stream of changes catches up 250 ms after it, and each search catches up too (mark `search:seq`). The hook path is not touched.
- Limits: 512 KiB a file, 20,000 files, 96 MiB a pass.
- Routes:
  - `GET /api/search/code?repo&q&agent&lang&limit&tokens&by` answers `{repo, q, agent, hits:[{id, tree, path, lang, symbol, kind, start_line, end_line, text, score, edited_by, overlaid_by}], pack, index, took_ms, missing}`.
    - With `agent`, the search sees that agent's tree laid over main.
    - With `tokens`, `pack` is `{budget, used, text, rows:[{path, start_line, end_line, symbol, decision, tokens}]}` from `PACK n TOKENS OF text WITH RECEIPT`. Each chunk sits under a `// path:start-end kind symbol` line.
    - With `by`, the search is logged as that agent's.
    - Without `repo`, the agent's repository is used.
  - `GET /api/search/index?repo` answers `{index:{state, files, chunks, langs, indexed_ms, took_ms, head, progress, overlays, error}, recent, searches, agents}`.
  - `POST /api/search/reindex {repo}` answers `{index, changed, removed, took_ms}`.
  - `GET /api/search/quick?repo&q` answers `{code, docs, decisions}`. Decisions are ChloeCtx's from `trail::decisions`, scored by the words they share with the query. Without ChloeCtx, decisions is empty and `missing` says why.

**Docs** (`src/library.rs`):
- Manifests:
  - Up to 12 are read: `Cargo.toml` (versions from the nearest `Cargo.lock`; path, git and workspace dependencies skipped), `package.json` (dependencies and peers; version and homepage from `node_modules`), `pyproject.toml` (PEP 621 or Poetry), `requirements.txt` and `go.mod` (direct requirements).
  - Up to 60 libraries are kept.
- Doc sites:
  - Crates map to `https://docs.rs/<name>/<version>/`, Go modules to `https://pkg.go.dev/<module>@<v>`.
  - Well-known npm and PyPI packages map to their own sites (a small table). Other packages map to their homepage, else their registry page.
  - `POST /api/library/crawl {url}` adds any site.
- Crawl:
  - It fetches the first page, then the pages it links to under the same host and folder (depth 1, at most 200 pages, html or text), as robots.txt allows. A group naming `chloedb-workbench` wins over `*`, and the longest rule wins.
  - The folder is that of the page the redirects led to. `dir/` and `dir/index.html` count as one page.
  - Crawls run one at a time on a thread, with progress kept in memory.
  - A crawl the server did not finish reads as `failed`, with a reason.
- Routes:
  - `GET /api/library?repo` answers `{libraries:[{id, name, version, ecosystem, manifest, url, state, pages, error, crawled_ms, cited, cited_by, progress}], crawler:{kind, available, via, note, why?, chloedb_crawl}, citations, manifests, missing}`.
  - `GET /api/library/:id/pages?repo` answers `{library, pages:[{url, title, words, depth, fetched_ms, cited, cited_by:[{agent, ts_ms, q}]}]}`, cited pages first.
  - `POST /api/library/crawl {repo, library | libraries | url}` answers 202 `{library | libraries, started}`. Without curl it answers `{available: false, why}`.
  - `GET /api/search/docs?repo&q&library&limit&tokens&by` answers `{hits:[{url, title, library:{id, name}, text (excerpt), score}], pack, missing}`.
    - The rank is `bm25(text) + 2*bm25(title)`.
    - With `tokens`, `PACK … OF text EXCERPT …` packs the text.
    - With `by`, the first 3 hits are recorded in `doc_cites` as cited by that agent, and the search is logged.

**MCP:**
- `search_code {query, tokens?, limit?, lang?}` answers `path:start-end kind symbol` lines with the chunks' first lines, or the PACK text. It searches as the caller (its worktree) and logs as the caller.
- `search_docs {query, library?, tokens?}` answers `title <url> (library)` with the passage and "Cite the URLs you rely on", or the PACK text. The pages answered are recorded as cited by the caller.

**UI:**
- Library (`ui/levels/library.js`, level Plan, `#/library[/code|docs]`, a nav link):
  - The Code tab has search, "as seen by" (main checkout or an agent's worktree), PACK with a budget, language chips, hits with edit and overlay chips, Why?, the Index card with Reindex, "Kept fresh" (edits reindexed) and "What agents searched".
  - The Docs tab has the libraries with their crawl state, Crawl, "Crawl the rest", adding a site, docs search over all docs or one library, the library's pages, and "Cited by agents".
- Quick search (`ui/levels/quicksearch.js`) is the top bar's Search button, and `/` anywhere but in a field, in a terminal, or on Library, whose own `/` focuses its box. It covers code, docs and decisions; Enter opens a hit and Shift-Enter opens Why.
- The fixture `ui/dev/fixtures/library.mjs` keeps the real shapes.

**Deviations:**
- The crawl does not use `chloedb-crawl`, which crawls into a chloedb-server cluster of its own. The built-in fetcher keeps the pages in the state database. It runs `curl` (`CHLOEDB_WORKBENCH_CURL` overrides the path), because this crate's ureq is built without TLS and curl follows the machine's proxy settings.
- Without curl, the crawler reports `available: false` with the reason, and search over pages already kept still works.
- There are no embeddings (chloedb is built without models), so search is BM25 only.
- Docs search ranks pages, not passages. The excerpt is the stretch with most of the query's words.

### Preview lab as built (use cases 2 and 3)

Simulated users (`src/swarm.rs`) and runtime blame (`src/runtime.rs`),
shown in Preview by `ui/levels/preview-lab.js`, which `preview.js` mounts
in one place.

#### Tables

| table | what |
|---|---|
| `swarm_runs` | a persona run: `agent` (whose preview), `repo`, `persona` (JSON: id, name, role, traits, device), `task`, `model`, `adapter` (`claude -p`, `api`, or the runner's word), `started_by` (`you` or the calling agent's id), `state` (`starting`, `running`, `done`, `failed`, `stopped`), `outcome` (`done`, `gave_up`, `steps`, `time`, `error`, `stopped`), `answer`, `steps` (JSON, at most 200), `step_count`, `max_steps`, `error`, `cost_usd`, `created`, `ended` |
| `swarm_findings` | what a run found: `run`, `agent`, `repo`, `kind` (bug, confusing, accessibility, slow, copy), `severity` (3 blocker, 2 major, 1 minor), `title`, `detail`, `step`, `url`, `state` (`open`, `sent`, `asked`, `dismissed`), `question` (the Decide question made from it), `steer` (the steer that sent it) |
| `runtime_errors` | one error of an agent's app, repeats counted: `agent` (whose app), `repo`, `cwd` (the worktree the error's top frame of the repository lies in, else the agent's), `source` (`overlay`, `console`, `proxy`, `otlp`, `api`), `level`, `message`, `stack`, `file` and `line`, `col` (the top frame in the repository, relative to `cwd`), `url`, `status`, `service`, `fingerprint` (agent, source, message with numbers masked, file), `hits`, `first_ms`, `last_ms`, `state` (`open`, `resolved`) |
| `runtime_edits` | the agents' edits, from the `PostToolUse` events of the edit tools (kept up to date at each read, after the mark `runtime:edits_seq`): `seq`, `ts_ms`, `agent`, `repo`, `cwd`, `file` (relative), `line` (where the edit's new text is in the file now), `tool` |

A report that matches an open error's fingerprint within 15 minutes adds
a hit to it.

#### The culprit (ASOF JOIN)

Each open error is put down to an edit, when one fits, at each read of
the list:

```sql
SELECT e.id, d.id, d.agent, d.file, d.line, d.ts_ms, e.first_ms
FROM runtime_errors e ASOF JOIN runtime_edits d
  MATCH_CONDITION (e.first_ms >= d.ts_ms)
  ON e.cwd = d.cwd AND e.file = d.file
WHERE e.state = 'open' [AND e.repo = :repo]
```

`how: "stack"`: the last edit, before the error first happened, to the
file its stack names, in the worktree that file is in (so an error of the
`checkout` agent's page whose stack is in the `api` agent's worktree is
put down to `api`). Errors without one get the same join `ON e.cwd =
d.cwd` (`how: "time"`: the last edit in that worktree, when at most an
hour before). If the join fails, the same lookup runs over the rows in
order and `missing` says so. An absolute path in a stack is matched to
the agents' worktrees of the repository and its main checkout; a URL's
path (with `/preview/<agent>/`, the preview URL's path and Vite's `/@fs/`
taken off) is a file of the reporting agent's worktree when it is there;
`node_modules` is never the repository's.

#### Endpoints

| route | what |
|---|---|
| `GET /api/runtime/errors?agent=&repo=&state=open\|resolved\|all&limit=50` | `{errors, total, missing, otlp: {endpoint, env}}`; each error with `agent_name`, `frames` (`fn`, `file`, `line`, `col`, `in_repo`, `raw`), `count`, `first`/`last` (ISO), and `culprit` (`agent` as the agents list has it, `agent_id`, `file`, `line`, `edit_line`, `edit_ms`, `before_ms`, `how`, `event`) or null |
| `POST /api/runtime/errors` | a report from anything with the token: `{agent (or x-workbench-agent), message, stack?, source?, level?, url?, status?, service?, filename?, lineno?, colno?, ts_ms?}`; 201 `{error}` |
| `POST /api/runtime/errors/:id/resolve` | `{error}`, its state `resolved` |
| `POST /preview/:agent/__wb/runtime` | the overlay's reports (`kind`: `error`, `unhandledrejection`, `console`; `message`, `stack`, `filename`, `lineno`, `colno`, `url`, `ts_ms`), on the preview's cookie (or the token), at most 64 KiB |
| `POST /v1/logs` | OTLP/HTTP logs in JSON, with the token as a Bearer; protobuf gets 415. Records of severity ERROR or above (`severityNumber` ≥ 17 or the text), or with `exception.*` attributes, become errors (`source: otlp`): `exception.type: exception.message` or the body is the message, `exception.stacktrace` (or a body with frames) the stack, `url.path`/`http.target`/`http.route` the URL. The agent is the header `x-workbench-agent`, else the resource's `workbench.agent`, else its `service.name` when that is an agent's id or name. Answers `{}`, or `{partialSuccess: {rejectedLogRecords, errorMessage}}` |
| the preview proxy | an answer of 500 or more through `/preview/:agent/` is recorded (`source: proxy`): `"<status> <METHOD> /<path>: <the body's first line>"`, the body as the stack when it holds frames; the answer goes on unchanged |
| `GET /api/swarm/personas` | `{personas, available, why, adapter, models, default_model}` |
| `GET /api/swarm/runs?agent=&limit=` | `{runs}`, newest first, without steps |
| `POST /api/swarm/runs` | `{agent, persona (an id) or persona_text (words, at most 500 bytes), task, model? (default haiku), max_steps? (3 to 60, default 25)}`; the caller is `x-workbench-agent`'s agent or `you`. 201 `{run, available: true}`; 200 `{available: false, why, run: null}` when no runner can start here; 409 without a preview or with a run of that preview playing; 429 past two playing |
| `GET /api/swarm/runs/:id` | `{run (with steps), findings}` |
| `POST /api/swarm/runs/:id/stop` | the runner gets SIGTERM; the run ends `stopped` |
| `POST /api/swarm/findings/:id/send` | `{text?}`: a steer to the preview's agent (what the persona ran into, the task, the step and URL, and what you add); the finding is `sent` |
| `POST /api/swarm/findings/:id/ask` | a Decide question (kind `ask`, options "Send it to <agent>" and "Not a problem", recommended by severity); its answer sends the finding (with the words you add) or dismisses it; expired, the finding is open again |
| `POST /api/swarm/findings/:id/dismiss` | the finding is `dismissed` |

#### Pointing a dev server's logs at Workbench

Each session already has `CHLOEDB_WORKBENCH_URL`, `_TOKEN` and `_AGENT`.
A dev server with an OpenTelemetry SDK started in it sends its error logs
with:

```bash
OTEL_EXPORTER_OTLP_ENDPOINT=$CHLOEDB_WORKBENCH_URL \
OTEL_EXPORTER_OTLP_PROTOCOL=http/json \
OTEL_EXPORTER_OTLP_HEADERS="Authorization=Bearer $CHLOEDB_WORKBENCH_TOKEN,x-workbench-agent=$CHLOEDB_WORKBENCH_AGENT" \
npm run dev
```

(`OTEL_EXPORTER_OTLP_LOGS_ENDPOINT=$CHLOEDB_WORKBENCH_URL/v1/logs` when
only logs should go there.) The list's `otlp` field and the empty state
of the strip say the same.

#### The persona runner

`chloedb-swarm/src/workbench.mjs` (Node 20, Playwright and Chromium from
wherever the machine has them): Workbench runs `node workbench.mjs`
(`CHLOEDB_WORKBENCH_NODE` names the node; the script is
`CHLOEDB_WORKBENCH_SWARM`, a file or a chloedb-swarm checkout, else
`chloedb-swarm/src/workbench.mjs` beside an ancestor of the binary or of
the source tree; `CHLOEDB_WORKBENCH_SWARM_CMD` replaces the whole command,
which the tests use). `--check --claude BIN` answers `{ok, why, adapter}`
(kept a minute); a run gets the job on stdin (`url` of the preview,
Workbench's token, which it trades once for the preview's cookie as the
user's browser does, `persona`, `task`, `model`, `max_steps`,
`max_seconds` 900, `claude_bin`, `workdir` under the data dir's `swarm/`)
and answers lines of JSON: `start`, `step` (`step`, `action`, `input`
with the element's `name`, `thought`, `ok`, `error`, `url`, `events`,
`ms`), `finding`, `done` (`outcome`, `answer`, `cost_usd`), `error`. The
runner's environment has no `CHLOEDB_WORKBENCH_TOKEN` or `_AGENT`. The
model is `claude -p` on the user's login (stateless per step:
`--output-format json --json-schema <one action> --tools "" --strict-mcp-config
--settings '{"disableAllHooks":true}' --no-session-persistence`), or the
Anthropic API when `CHLOEDB_SWARM_ANTHROPIC_KEY` or `ANTHROPIC_API_KEY` is
set and the SDK is installed. Findings: the page's uncaught exceptions
and 5xx answers the browser saw (one per message), then one review call
over the steps, and a blocker when the persona gave up. A run starts only
from the user's click or an agent's `test_with_persona`.

#### MCP tools

- `test_with_persona {task, persona?, model?, wait? (true), timeout_s? (600)}`:
  starts a run on the caller's preview and waits, answering the outcome,
  the answer and the findings; says why when no runner can start.
- `runtime_errors {all?, limit?}`: the open errors of the caller's app
  (`all`: of its repository), each with its count, place, stack frames of
  the repository and "probably <agent>'s edit to <file> at <time>".

#### UI

In Preview: a "Test with a persona" button in the browser's chrome opens
a drawer over the side panel (personas as cards and "Someone else" in
words, the task, suggestions, the model, Start; the earlier runs; a run's
live view with its progress, Stop, findings with Send to <agent> / Ask me
/ Dismiss, its steps); when no runner can start, the drawer says why
instead of Start. Under the page, a strip of the open errors, one line
when closed (the count, the newest, its culprit); open, each error with
its source, count and culprit, and the chosen one's frames and a card:
"Why? file:line" (`ctx.openWhy`), Rewind (`ctx.openRewind` around the
edit), Send to <agent> (a steer with the error and its frames), Resolved.

#### Deviations

- The overlay reports to `/preview/:agent/__wb/runtime`, not to `/api`:
  the previewed page has the preview's cookie, never the token.
- The culprit's join is on the worktree too (`cwd`), so the same file
  edited in two worktrees is told apart.
- `tests/workbench.rs`'s MCP test checks that the tool list starts with
  the earlier tools, so new tools can follow.
- The levels mock serves the story "shop" when the page's URL has
  `story=shop` (`ui/dev/fixtures/previewlab.mjs`); other stories are as
  before.
- `ui/levels/common.js`: the `useStyle("common-helpers", …)` template is
  closed (the branch's fix 7ccb756b).
- Not done: the overlay is injected at the end of `<body>`, so what a page
  throws before it loads (a script that fails as it is parsed) is not
  reported; injecting a first, small listener in `<head>` would need a
  second change to the proxy. A `fetch("/api/…")` with an absolute path
  from the previewed page goes to Workbench, not to the app, as before.

### Trace (as built)

Trace answers a question like "when and why did we change the cart total
rounding from floor to half-up, and how does it work" with the evidence:
the commits and edits that made the change, the decisions, questions,
rules and goals behind them, the state of the fleet then, and the code as
it is now. No model reads the question or writes the answer; one is called
only when the user presses Explain.

**Backend** (`src/trace.rs`, routes merged in `server.rs`):

- `GET /api/trace?q&repo|agent[&x&y&topic&subject&in&kinds]` answers
  `{question, repo, reading, answer, timeline, now, links, missing, ms}` (400 without `q`).
  The optional parameters replace what the reading found (the chips the
  user edited); `kinds` is a comma list of `when,why,how,who`.
- `POST /api/trace/explain` takes the same fields as JSON, traces again
  and runs `chloectx ask --deep --json -- <question and the evidence,
  tagged E1..>` (5 minutes at most), answering `{available, answer, text,
  ids, confidence, cost_usd, ms, evidence, missing}`, or `{available: false, why}`
  when ChloeCtx is not installed or fails. The question tells the model to
  name only the listed commits, and `missing` names any commit the answer
  names that the timeline does not show.
- `GET /api/trace/examples?repo` answers `{examples, files}`: questions to
  try made of the repository's history (its most-changed files, not tests
  or lock files, and a recent commit's subject).
- The answer's history line ("x changed 3 times: …") leaves out commits
  whose lines about the subject only import or re-export it.
- `GET /api/trace/commit?repo|agent&sha` answers one commit's diff
  (`git show -U0`, parsed); the sha must be hex.

**Reading** (`read`): quoted strings, `code`, identifiers (snake, camel,
dotted, ALLCAPS of 4 or more), file names, "in api/" (a path), and the
change: "from x to y", "x to y" after a change verb, "x -> y" and "x → y".
"change ROUND to half_up" makes the identifier a subject, with no x. The
words before the change, without stop words, are the topic. The kind is
when, why, how or who from the question's words; with none, all of them.
The reading is returned as chips `{key, label, text}`.

**Needles**: at most 4 strings to look for (y, x, subjects, quotes). They
are matched case-insensitively with `-`, `_` and spaces dropped, so
"half-up" finds `round_half_up` and `HalfUp`. A weak value (a number or
fewer than 3 characters) is kept with the word before it and must come
within 40 characters after that word ("retries … 5").

**When**: `git log --all -G <ERE>` per needle (the ERE spells each letter
`[aA]` and joins parts with `[-_ ]?`), then `git show -U0` of each commit,
classed `changed` (x removed and y added), `added`, `added_x`, `removed` or
`touched`; with both x and y, a commit that touches neither side's lines
is dropped. At most 12 commits, oldest first. A commit belongs to an agent
when it is on the agent's branch and not in its base
(`git merge-base --is-ancestor`), else to the agent whose Bash
`git commit` event was within 120 s of it. Agents' uncommitted edits come
from PostToolUse events of Edit, MultiEdit, Write and NotebookEdit. An edit
whose added line a commit of the same agent also adds is folded into that
commit as `written` (when it was written, by which tool).

**Why**: ChloeCtx's `ask --json`, then `sql --json` over `memory`
(`kind IN ('decision','preference','fact')`, `MATCH(body, ...)`; memory
indexes `body`, not `text`) and `blame <path:line> --json` of the first
line of code now; Decide's questions (with the answer and who gave it) and
rules; and the goal and outcome of each agent involved. Without
`chloectx`, `missing` says so and decisions are left out.

**Timeline items** `{id, kind, ts, ts_ms, tag, title, agent, ..., links}`
with ids `c-<short>`, `w-<agent>-<seq>`, `d-<ctx id>`, `q-<id>`, `r-<id>`,
`g-<goal>-<agent>`, sorted by time and tagged E1, E2, ... Each item's
`links` are `{story: {agent, at_ms}, rewind: {agent, from_ms, to_ms,
at_ms}, why: {agent|repo, path, line}, diff: {sha}, time_ms}`.

**Answer**: one sentence assembled from the key item, for example "The
cart total rounding went from floor to half-up {when} in Dana Okafor's
commit f00093d “...”", with a clause for the answered question, rule or
decision behind it. `{when}` is filled by the UI in the viewer's time;
`plain` has it in UTC. `evidence` counts the items by kind.

**How it works now**: when the Search part's table `code_chunks` exists,
BM25 over the main checkout's chunks with `PACK 1200 TOKENS OF text`;
else `git grep` in passes (y alone first, then all needles) over the main
checkout, then the involved agents' worktrees, each hit shown with the
definition around it (its doc comment included; a hit in a doc comment is
about the definition below). `doc_pages`, when it exists, adds up to 3 doc
pages.

**MCP**: tool `trace {question, repo?}` calls `GET /api/trace` (as the
calling agent when `repo` is left out) and answers the evidence as text.

**UI** (`ui/levels/trace.js`, level 1, nav link after Decide): the screen
reads `#/trace?q=...` (and `ctx.params.q`) and keeps the question in the
hash. The reading is chips the user can remove or edit (x red, y green),
with toggles for when, why, how and who; the answer card with "Explain ·
calls a model", whose `[En]` cites scroll to and flash the card; the
timeline of evidence cards (Diff inline, Why for the line, Story, Rewind,
Mission then and Map then); and a right column with the code now and the
state then. The fixtures are `ui/dev/fixtures/trace.mjs`.

#### Deviations (Trace)

- The Why card for a line of the main checkout (not an agent's worktree)
  is drawn inside Trace from `/api/why?repo=`, since `ctx.openWhy` takes
  an agent's line only.
- Story links go to `#/story/<agent>`; Story has no anchor for a turn yet.
- "Mission then" and "Map then" link to `#/mission?at=<ms>` and
  `#/map?at=<ms>`, shown only when `GET /api/time` answers (a 404 hides
  them) and the moment is within a day (snapshots are kept a day); the
  time bar has to read `?at=` for them to land on that moment.
- `app.js` keeps a screen's `?query` in the hash (`openLevel(..., {query})`)
  and remounts the screen when it changes.
- Quick search (`ui/levels/quicksearch.js`) is not changed: it should hand a
  question to Trace with `ctx.go('#/trace?q=' + encodeURIComponent(text))`,
  for input that starts with when, why, how or who, or from a "Trace this"
  row.

## Fifth build: Hotspots

The user asked: can we see the pieces the agents keep correcting, redo, or
break? Workbench already records each of those as a row: every edit an agent
made (`file_edits`), every test it ran (`test_runs`), the runtime errors of
its app (`runtime_errors`), the user's steers and prompts (`events`), the
questions it asked (`questions`) and the incidents Health opened
(`incidents`); git holds the reverts. A hotspot is a file (and, where the
edits' lines fall in a definition, its function) where those pile up. It is
worth seeing for two reasons: the user learns where agents struggle (and
what is missing: a rule, a decision, a test), and the next agent to edit
the file can be told its history before it repeats it.

Contract:

- **Score** (`src/hotspots.rs`): per repository over a window (14 days by
  default), each file's points are the sum of what was counted, with
  weights: a re-edit (an agent's edit of a file after its first) 1, each
  agent after the first 2, a revert 3 (a git commit whose subject starts
  with "Revert" touching the file, or an Edit whose old and new text are
  an earlier edit's of the file the other way round), a test that failed
  after an edit of the file 1.5 (the edit the agent made last before the
  failure, within 30 minutes: an `ASOF JOIN`), a flaky test among those
  3 (passed and failed on the same commit and tree), a runtime error put
  down to the file 2.5, a correction 3 (a steer, or a prompt typed to the
  agent within 30 minutes of its edit that corrects it: "no", "don't",
  "stop", "again", "instead"...), a question to the user about the file
  1.5 (asked within 30 minutes of an edit of it, or naming it) and an
  incident naming it 2. A file of 3 points or more is a hotspot: hot from
  15, warm from 8, mild below. Each comes with counted reasons and the ids
  of their evidence, edits per day, the function most of its edits fell in,
  the tests that failed after its edits, what the user's rules and
  ChloeCtx's decisions, preferences and facts say of it (◆), and a
  suggestion: make the user's correction a rule when nothing names the
  file, run the test that keeps failing first, or route the work to a
  stronger model than the agents that kept redoing it (all on haiku or
  sonnet).
- **Rows**: SQL over the tables (`GROUP BY file, agent`, `date_bin('1 day',
  ts)` for the churn, `COUNT(*) FILTER (...)` and `HAVING` for flaky tests,
  `ASOF JOIN ... MATCH_CONDITION (t.ts >= e.ts) ON t.agent = e.agent` from
  failures, corrections and questions to the edit before them, with the
  same lookup over the rows in order when a join cannot run), written to
  the table `hotspots`, made again every minute and on demand.
- **Told once**: an agent's PreToolUse of Edit, Write, MultiEdit or
  NotebookEdit on a file scoring 8 or more gets its history and what to do
  first as `additionalContext`, once per session and file.
- **Seen**: a heat layer on Map, a Hotspots tab in Health, a badge on the
  Mission card of an agent working on one; agents ask the MCP tool
  `hotspots`.

### Hotspots as built

**Table** `hotspots` (`src/hotspots.rs`, `TABLE`; made by `StateDb::open`):
`id` (`h` + a hash of repository and file, the same whatever the window),
`repo`, `file`, `symbol`, `score`, `level`, the counts (`edits`,
`re_edits`, `agents`, `reverts`, `test_fails`, `flaky`, `errors`,
`steers`, `questions`, `incidents`), `first_at`, `last_at`, `days`,
`data` (the hotspot's JSON), `evidence` (its timeline's JSON) and
`updated`. A refresh deletes the repository's rows and writes them again;
the mark `hotspots:<repo>` keeps its totals, its time and what was
missing. A pinned test and a routed model are the marks `hotpin:<id>` and
`hotroute:<id>`, applied when a hotspot is read, so they hold across
refreshes. The timer (`hotspots::start`) scores every repository with a
running agent, or one that ended within the window, every 60 s; a `GET` of
the default window older than 30 s scores again first.

**Endpoints**:

- `GET /api/hotspots?repo=|agent=&days=&since=&limit=&file=`: `{repo,
  window: {from_ms, to_ms, days}, updated_ms, every_s, hotspots, totals:
  {hotspots, hot, files, edits, reverts, test_fails, errors, steers},
  agents, ctx: {available, records | why}, missing}`. Each hotspot: `{id,
  repo, file, symbol, symbol_kind, line, score, level, counts, reasons:
  [{kind, n, text, ids, points}], agents, first_ms, last_ms, first, last,
  churn: [[day_ms, n]], symbols: [{symbol, kind, line, edits}], tests:
  [{test, runs, failed, flaky, command}], records: [{id, kind, text, ts,
  via: chloectx | rule}], suggestion: {kind: rule | test | model, text,
  rule? | test?, command? | model?, from?} | null, summary,
  last_correction, window_days, pinned_test, routed_model, context}`
  (`context` is what the next agent is told). The default window reads the
  table; another (`days` 1 to 90, or `since` in ms or ISO 8601) is scored
  live and not stored.
- `GET /api/hotspots/:id`: `{hotspot, evidence: [{id, kind (edit, revert,
  test_fail, error, steer, prompt, question, incident, record), ts_ms, ts,
  agent, text, detail, link}], agents, missing}`, oldest first, at most 80;
  `link` is `{story: {agent, at_ms}}`, `{test, run}`, `{error}`,
  `{question}`, `{incident}`, `{commit}` or `{record}`.
- `GET /api/hotspots/agents?repo=`: the running agents that edited a
  hotspot of 8 or more in the last hour, each with its hottest.
- `POST /api/hotspots/refresh {repo | agent}`: `{ok, repo, hotspots, ms}`.
- `POST /api/hotspots/:id/act {kind: pin_test, test} | {kind: route,
  model} | {kind: clear}`: `{hotspot}`. "Add rule" is the existing `POST
  /api/rules`.

**Hook** (`hotspots::check`, after `guarded::check` in the hook handler):
the PreToolUse of an edit tool on a file of the agent's checkout whose row
scores `TELL_SCORE` (8) or more answers `{"hookSpecificOutput":
{"hookEventName": "PreToolUse", "additionalContext": ...}}` and records the
row `deliveries` `<agent>:hot:<session_id>:<file>`, so a session is told
once per file (a new session id, after /clear, is told again); the agent's
events get a `hotspot` event. The text: the file and its score, a line per
reason, the user's last correction, the ◆ rules and records, and "Before
you edit": the pinned or suggested test to run first and again after, the
model the user routed it to (ask the user whether to hand it over), and to
ask the user (ask_human) rather than change a rule or decision back and
forth.

**MCP tool** `hotspots {repo?, file?}` (the last of the list): the ten
hottest with their summaries and suggestions, or one file's context and
tests.

**UI** (`ui/levels/hotspots.js`, wrapping three screens in one place each):
`withHeat` on Map (a "Heat" toggle in its legend, key `h`, kept in
`localStorage`; files and folders tinted by score, a score badge on hot
files, the reasons added to the map's hover card, and a side card of the
hotspots that opens Health); `withHotspots` on Health (a Hotspots tab, key
4, `#/health/hotspots`: totals, a 7/14/30 day window and "Score again",
filters, the ranked list with reason chips, ◆ count, agents and a churn
sparkline, and a board per hotspot: why it is hot with points, the evidence
timeline with links to Story, Tests, the incident's board, Decide, Preview
and Trace, the churn, "Do something about it" (Add rule prefilled with the
correction, Pin a test, Route to a stronger model), what ChloeCtx knows,
functions edited and what the next agent is told); `withHotBadges` on
Mission (a badge on the card of an agent working on a hotspot). Fixtures:
`ui/dev/fixtures/hotspots.mjs` (the demo repo "shop", `?story=hot`) and
`ui/dev/hotspots-context.html` (the terminal as the agent sees the
context).

#### Deviations (Hotspots)

- "Route to a stronger model" does not change how agents are spawned
  (`spawn_agent` and routing.rs are not changed): agents about to edit the
  file are told the user wants that model for it and to ask before going
  on. Making `routing::suggest` read `hotroute:<id>` for tasks naming the
  file is a small follow-up in routing.rs.
- A failing test is put down to the one file the agent edited last before
  it (the `ASOF JOIN`), not to every file it edited since the test last
  passed.
- A correction typed in the terminal is told from other prompts by its
  words; a steer always counts.
- Rewinds of Claude Code's own history (Esc Esc) leave no hook, so an edit
  undone that way is not seen; an Edit that puts text back is.
- The symbol of an edit comes from `runtime_edits`' line (the first line of
  the new text found in the file when the hook came), placed in the code
  index's chunks of the agent's tree, else main's, else the file chunked
  as the index would.

## Fifth build: Autocomplete

The questions behind it: do we have contextual autocomplete everywhere we
type something, and if code is the unit of edit, autocomplete and syntax
of an IDE, what is the equivalent for an IDE that works through agents?
The answer here: the unit is the task (an intent given to an agent). The
box completes references into the project's memory, and its syntax check
is an intent check.

### The contract

- `GET /api/complete?field=<kind>&q=<text>&cursor=<n>&repo=&agent=&user=`
  answers ranked suggestions `{kind, label, insert, detail, ref, replace:
  {start, end}}` for the token at the cursor (offsets in UTF-16 units, as
  a browser's `selectionStart`). The sources are @agents (the agents
  table); files and symbols (the files under the repository, and
  `code_chunks`); ◆ decisions, preferences and facts (ChloeCtx through
  `chloectx sql --json`, which degrades when ChloeCtx is missing) and the
  user's rules; earlier tasks and goals by BM25, with how they ended;
  tests (`test_runs`); SQL tables and columns; personas; doc pages.
  `field` picks the sources and their order: task, goal, steer, answer,
  trace, quick, why, persona, change, search, sql.
- `POST /api/intent/check {text, repo, field, agent?}` answers marks
  `{start, end, severity, kind, message, fix?, ref?}`:
  - `no_done_check`: a task or goal that does not say how it is done;
  - `contradicts`: against a ChloeCtx decision or a rule, with the record
    linked;
  - `guarded`: a guarded command or path (guarded.rs's rules);
  - `overlap`: with a running agent's task or files;
  - `hotspot`: only when a `hotspots` table exists;
  - `unknown_ref` and `superseded`: for @names and ◆ids.

  The checks are cheap and deterministic, with no model.
- UI: `attachComplete(input, {field, ctx, ...})` gives every box a list
  (arrows, Tab or Enter, Esc, chips), squiggles with a hover card and a
  one-click fix. A question in the quick search goes to Trace first.

### The bar

The user raised it: "The autocomplete should be amazing, every time I
write a prompt or search or write anything at all, I want the
autocomplete to be so contextualized and personalized that it blows away
the user's imagination." The part is held to it in this order:

1. **Before the first keystroke.** An empty box proposes the three likeliest
   things to write, given where the user is and what is happening:
   - a new task: a flaky or failing test from `test_runs`, the top
     hotspot, an open runtime error, or the next step of a goal's outcome
     that is not done;
   - a steer: what the agent is doing now, and the correction this user
     made last time in a similar spot;
   - Trace and `/`: questions about what changed today.
2. **A ghost for the whole sentence**, not just the token. Tab takes a
   word; Shift+Tab or → takes it all. It is built from what the screen
   knows: the focused agent, its files, the failing test, the ◆ rule that
   applies, the user's own past tasks. It ends with a done check made
   from the tests that cover the files. The deterministic version comes
   first and is instant.
3. **Personal.** Every suggestion shown, accepted and ignored is a row.
   Ranking uses those rows, frecency, and the user's own words (BM25 over
   their past tasks and steers, and ChloeCtx's record of what they typed).
   Two users, or one user over time, see different orders.
4. **Context, not just matching.** Each item says why it is offered ("cart
   edited it 4 min ago", "◆ decided Nov 9"). @agent shows what the agent
   is doing now.
5. **Optionally, a model.** After about 700 ms of pause, a fast model may
   offer a better ghost. It counts only if it arrives before the next
   keystroke. It is off without the claude binary, and is never called in
   tests. The UI shows where a suggestion came from.

The deterministic path stays under about 30 ms, measured.

### Autocomplete as built

**Code**:
- `src/complete.rs`: the routes, the rows, the token completion and the
  intent checks.
- `src/complete/foresee.rs`: proposals, the ghost, "because" lines,
  habits, the model ghost.
- `ui/levels/complete.js`: the UI.
- `ui/dev/fixtures/complete.mjs` and `ui/dev/complete.html?demo=task|sql|quick|steer`:
  fixtures and a dev page in the real shapes.

**Tables** (created in `StateDb::open`):
- `complete_items (id, repo, kind, pfx KEYWORD[], label, ins, detail,
  refj, body TEXT, weight, grp)`: one row per thing to complete; `repo =
  '*'` for SQL words, tables, columns and personas.
  - Prefixes go through `PREFIX(pfx, :p)`, with `fuzzy => 'auto'` as the
    fallback when fewer than four rows come back.
  - Records near a text go through BM25 over `body`, which uses the default
    stemming analyzer ("floats" meets "float").
  - Rows are refreshed from their sources by signature: the first time
    before answering, then in the background once 3 s old. ChloeCtx is
    asked again after 60 s.
  - Kinds: file, symbol, test, doc, rule, decision, preference, fact,
    prompt (ChloeCtx's `kind = 'prompt'` records: what the user typed into
    sessions), table, column, keyword, function, persona.
- `complete_events (id, "user", repo, field, agent, kind, label, ctx, text,
  accepted BOOL, rank, source, ts)`: every list shown, the row picked (and
  its rank), the rows passed over, ghosts taken, and what the user wrote
  in the end (`kind = 'typed'`, `text` searchable by BM25). `ctx` is the
  agent's step at that moment.

**Routes**:
- `GET /api/complete?field&q&cursor&repo&agent&user&force&limit` answers
  `{field, repo, user, mode, token, suggestions, ghost, context, took_ms,
  missing}`:
  - `mode` is `propose` for an empty box, `complete` otherwise;
  - a suggestion adds `because`, `picked` (how often this user took it),
    `sub` and `source` (proposals), and `go` (a hash to open instead);
  - `ghost` is `{text, parts: [{kind: word|rule|done|continued, text}],
    because, source: "chloedb"}` and comes only when the cursor is at the
    end;
  - `context` is `{agent, id, now}` on a steer or an answer.
- `POST /api/intent/check {text, repo?, field?, agent?}` answers `{repo,
  field, marks, refs, took_ms, missing}`.
- `POST /api/complete/events {user, field, repo?, agent?, shown: [{kind,
  label, rank, source}], accepted?, typed?}`.
- `GET /api/complete/stats?user&repo` answers the rank of what the user
  picked, early against late: `{shown, accepted, typed, acceptance, all,
  first_half, second_half}`, each half with `{n, mrr, top1}`.
- `POST /api/complete/ghost {text, field, repo?, agent?, user?, ghost?}`
  answers `{available, cached, took_ms, ghost: {text, source: "model",
  model: "haiku", because}}`. It answers `{available: false, why}` when
  the claude binary is not found.

**What reaches the bar**:
1. **Proposals.** Each comes with its because.
   - task and goal: tests failing or flaky in the last day ("Fix
     totals::coupon: flaky, failed 1 of 3 runs in the last 4 min; done when
     `cargo test coupon` passes 5 times in a row."), open runtime errors,
     the top hotspot (with a covering done check), the next outcome of an
     unfinished goal that no agent has, and the user's own recent task
     again;
   - steer: the test the agent broke since its edit; the ◆ rule or
     decision about the files it is editing; "run `<covering command>` and
     show me"; and what this user said in a spot like this (BM25 of their
     typed steers against the agent's task, current step and files: "you
     said this to hooks yesterday, when it was at …"). The header shows
     "ledger · working · now: Edit src/refunds/service.rs · 1 min ago";
   - trace, quick, why: "Why did cart change money.py today?" (file_edits
     since local midnight), today's commits on every branch, and "When did
     X start failing?". In the quick search they go to Trace.

   Proposals are cached per repository, field and agent for 3 s. The
   user's own rows are read on each request.
2. **The ghost.** It comes from, in order:
   - (a) a text that begins as the typed one: the user's own `typed` rows,
     the box's proposals, the agents' tasks, ChloeCtx prompts. The rest of
     it is the ghost, because "you wrote this 2 days ago";
   - (b) otherwise: the rest of the word being typed (from the top
     suggestion), then ", per ◆<id>" for the rule or decision that applies
     (sharing the files or two stemmed words, and not one the text goes
     against), then "; done when `<cmd>` passes." when the text has no
     done check.

   `<cmd>` comes from the tests that cover the files the text names (or,
   on a steer, the agent's files):
   - pytest `tests/test_money.py -q`, `npx vitest run x.test.tsx` or
     `npx jest`, `go test ./dir`, `cargo test --test t`, `cargo test stem`,
     rspec;
   - else a recorded test run named like the file;
   - else the most-run command.

   `no_done_check`'s fix uses the same command. Tab takes a word (when no
   list is open), Shift+Tab or → takes all of it, and Esc drops it. The
   ghost is drawn in the squiggles' mirror, the rule part in ◆ purple, the
   done part in green, with a small "Tab ChloeDB" (or "Tab haiku") tag.
3. **Personal.**
   - The box keeps a user id in `localStorage` (`wb.complete.user`), or
     takes `ctx.user`.
   - It posts what it showed on accept (with the rank), proposals typed
     over as passed over, ghosts taken, and the final text on `change`
     (blur) or Enter.
   - Ranking adds `12 * (1 - e^-w)` for the suggestion and `3 * (1 -
     e^-w/4)` for its kind. `w` sums the picks with a two-week half life,
     at half weight from other fields. A suggestion never picked loses 0.7
     per time it was passed over, up to six.
   - The user's own earlier texts come back as `mine` rows (BM25 over their
     `typed` rows of that field), ChloeCtx prompts as `prompt` rows.
4. **Because.** Each suggestion says why:
   - agent: "now: Edit api/src/cart/total.rs · 1 min ago" (its last tool
     call), or exited;
   - file and symbol: "cart edited it 4 min ago";
   - decision: "◆ decided Nov 9 by cart";
   - rule: "your rule since Nov 13; every agent is told it";
   - test: "failed 2 min ago", "flaky: failed 1 of 3 runs", or "passed …";
   - task: "like what you are typing: fix-floor's task, 2 days ago";
   - proposals and `mine` rows: their own.

   A row the user picked shows ★n.
5. **The model ghost.**
   - After a 700 ms pause at the end of the text, the UI posts
     `/api/complete/ghost`.
   - The server runs `claude -p --model haiku --output-format json --tools
     "" --strict-mcp-config --settings {"disableAllHooks":true}
     --no-session-persistence --system-prompt …`. These are the flags
     chloedb-swarm uses, so no hook or session of the user's records it.
   - The prompt is a pack of the box's context: the agent, its task, step
     and files; the covering done check; up to four ◆ records near the
     text; the failing test; the user's last three texts of the field; the
     deterministic ghost.
   - The answer is cached by text and reused while the user types what it
     said.
   - The UI shows it only if the text has not changed since the request,
     tags it "haiku", and turns the model off for the page once the server
     says it is not available.

**Measured.** Latency is warm, the median of 9, as `took_ms` (server) and
HTTP round trip. The demo repository is seed-real.mjs's harbor (5 agents,
test runs, ChloeCtx records), on this 4-CPU container, while another
agent's build was running:

| box | server | HTTP |
|---|---|---|
| empty task box (proposals) | 3.2 ms | 4.6 ms |
| `Make ref` (files, symbols) | 3.9 ms | 4.9 ms |
| `Ask @le` | 3.6 ms | 4.5 ms |
| empty steer | 3.1 ms | 4.0 ms |
| quick `why did we chan` | 3.7 ms | 4.5 ms |
| SQL `a.na` | 3.4 ms | 4.0 ms |

The slowest of the 9 runs of any box was 5.7 ms; `/api/intent/check` took
8.5 ms. A first request for a repository refreshes its rows before
answering, which includes `chloectx sql`. The integration test's warm
requests take under 10 ms.

**Learning.** That it learns is measured by the integration test
`an_empty_box_proposes_a_ghost_finishes_the_sentence_and_each_user_is_learned`:
- two users type the same "Look at mon"; ana picks monitor.py three times
  and ben money.py;
- after that their lists differ for the same text: ana's first file is
  monitor.py and ben's is money.py;
- the picked row shows `picked: 3`;
- `/api/complete/stats` reports a second-half top-1 of 1.0 and MRR at
  least the first half's;
- ana's own sentence comes back to her as the ghost and as a `mine` row,
  and not to ben;
- ana's steer comes back on cart's empty steer box, and ben's has none.

There is no measurement on real users yet: the stats endpoint is how one
would be read.

**MCP**: `check_intent {text, field?, repo?}` (appended last) posts
`/api/intent/check` as the calling agent and answers the marks as text,
so a lead checks a task before it hands it on.

**UI wiring**: one import and call in each place:
- the task box and Ask why: `app.js`;
- Health's rule SQL: `health.js`;
- answers: `decide.js` and `away.js`;
- steer: `story.js`, `preview.js` and `flow.js`;
- the goal: `mission.js`;
- the Trace question: `trace.js`;
- quick search: `quicksearch.js`, whose "Trace: …" row is the Trace
  hand-off the Trace part asked for;
- the persona task: `preview-lab.js`;
- the capsule's question: `ship.js`;
- a change request's notes: `change-requests.js`;
- code search: `library.js`.

**Tests**:
- In `tests/workbench.rs`, under "Fifth build: Autocomplete":
  - `autocomplete_ranks_agents_files_symbols_records_tasks_tests_and_sql`;
  - `intent_checks_fire_on_a_crafted_task_and_stay_quiet_on_a_good_one`;
  - `autocomplete_and_checks_without_chloectx_say_what_is_missing`;
  - `an_empty_box_proposes_a_ghost_finishes_the_sentence_and_each_user_is_learned`.
- Unit tests in `complete.rs` (tokens and UTF-16 offsets, choices and
  negation, PREFIX over KEYWORD[], BM25 over body, hotspots read when the
  table exists) and in `foresee.rs`.
- The fake claude has a new case: `-p` prints one JSON result, which is
  the model ghost.

#### What does not reach the bar yet

- The deterministic ghost is a template: the word, ", per ◆id", and the
  done clause. It does not write new middle clauses from the context; the
  model ghost is the path to that.
- The ghost's done check covers files a text names by path or file name.
  A symbol named alone does not lead to its file yet.
- Personalization is per browser id, not per signed-in person: Workbench
  has no accounts. Two people on one browser share habits. ChloeCtx
  prompts are per machine, not per user.
- The model ghost is off in the fixtures (the dev page shows it only for a
  text that says "model"). It has run only against the fake claude here.
  Its latency with a real Haiku (seconds) means it lands mostly on real
  pauses.
- Proposals for a goal reuse the task ones. The answer, persona and change
  boxes propose nothing yet: their list starts at the first keystroke.
- The hotspot proposal and check read a `hotspots` table by whatever
  columns it has (path or file; score; reason or why). They were tested on
  a table made in a unit test, not on the hotspots part's own table.

#### Deviations

- `contradicts` finds candidate records by BM25 over `complete_items.body`,
  then reads exclusive choices (rounding modes, floats against cents, case,
  sync and async, tabs and spaces, naming styles, REST/GraphQL/gRPC, ids,
  time zones) with negation, or "never/don't/avoid/instead of X". It is
  not a general contradiction detector.
- `guarded` marks are `info`: the agent is not stopped, it proposes the
  command and waits for approval.
- The user id is a parameter (`user`), not a header.

## Sixth build: D, read without an agent, and the missing pieces

Lee and Maya each opened Workbench to look before changing anything. Every
reading screen answered "repo is required". The Map of a ten-file
repository showed one folded tile that did nothing when clicked. A
restarted agent vanished from the Map. Story had no way to change the
agent it showed, the Rules panel never opened, and a goal could only be
planned by a lead. This part fixes each of those.

### 1. A repository without an agent (done)

- `src/repos.rs` (new) adds a `repos` table (path, name, added, opened)
  and four routes. `GET /api/repos` lists the repositories Workbench
  knows: those opened in a picker and those an agent worked in. Each comes
  with its branch, its agents and when it was last used.
  `POST /api/repos {path}` takes any folder inside a checkout, resolves it
  to the checkout's main folder and starts indexing it for code search
  (`search::open_repo`). It answers 201 the first time and 200 after that.
  A relative path, a missing folder or a folder outside git gets a 400
  that says which of the three it is. `DELETE /api/repos?path=` forgets
  a repository. `GET /api/repos/dirs?q=` completes a folder path: folders
  under what was typed, git checkouts first, hidden folders only when the
  typed name starts with a dot, at most 40.
- `ui/levels/repos.js` (new) is one picker that every screen shares. It
  completes folders with Tab, the arrow keys and Enter, lists the known
  repositories and keeps the end of a long path in view. A repository
  chosen there becomes the one that Search, Map, Trace, Library and the
  time bar read. It is kept per browser and announced with a `wb-repo`
  event. `repoSwitch` is the small "⌂ shop ▾" button that changes it from
  a screen's header.
- `app.js`'s `pickRepo` now goes, in order: the zoomed agent, then the
  repository opened in a picker, then the selected agent. `knownRepos`
  includes the server's list.
- Search, Map, Library and Trace show the picker where they used to show
  "repo is required". Search runs the query as soon as a repository is
  opened. The time bar says "open one" and makes no request.
- Opening `#/library/code` no longer sets an agent focus called "code".
  That focus was the cause of "There is no agent code".

Left out: Health's Tests and Hotspots panels (part B's files) now get a
repository through `pickRepo`, but still show their own message when
there is none, not the picker.

### 2. Map folds (done)

- `map.rs` folds a folder's small files only when, together, they hold at
  most 20% of that folder's lines (`FOLD_MAX_SHARE`). A small repository
  is therefore drawn file by file. A fold now carries its files
  (`items`, at most 400).
- A folded tile reads "… n small · open" and works by click or keyboard.
  Opening it gives those files tiles of their own and lists them beside
  the map, largest first, with the agents that changed each one. Picking
  a file opens its usual card. "Fold them again" closes the list.

### 3. Who works where (done)

The cause: an agent started again under the same name reuses its
worktree, so the exited agent and the new one share a folder. The Map
filtered the shared diff by each agent's Edit and Write events. cents had
written its file with a Bash heredoc, so no Edit event named it and the
file belonged to no one. Now each agent claims the files it edited
(events and `file_edits` rows). Any file left unclaimed in a shared
folder goes to that folder's newest agent: a running one first, then the
most recently created. Test:
`the_map_gives_a_worktree_started_again_to_its_live_agent`.

Left out: a file that an agent edited and then reverted no longer shows.
The Map reads diffs, not history.

### 4. Story's agent picker (done)

A "Story of" row of chips sits at the top of Story, in every state. It
lists running agents, newest first, then the agents that ended in the
last day. With no agent chosen, the page says to pick one above. An
unknown id says the same instead of only "not found".

### 5. Rules (done)

- `ui/levels/rules.js` (new) is the panel, and Decide's "Rules (n)" button
  now opens it: it scrolls to the panel, focuses it and flashes it.
- Add: `POST /api/rules {repo, text}`. That route already existed but no
  screen called it.
- Edit adds the new words as a rule and deletes the old rule, keeping
  whether it was on or off. This way agents are told the new words, and
  ChloeCtx keeps the old ones as a past decision. The rule being edited
  spans the panel's width.
- Delete asks once inline. `DELETE /api/rules/:id` (new, in `levels.rs`)
  removes the row and gives 404 for a rule that does not exist. Test:
  `rules_are_added_and_deleted_without_a_question`.

Left out: deleting a rule does not remove ChloeCtx's copy, because
chloectx has no command to forget a record. The panel says so ("ChloeCtx
keeps its copy … as a past decision").

### 6. Goals: three ways to staff one (done)

`POST /api/goals` now staffs a goal in one of three ways:

| who does the work | body | what starts |
|---|---|---|
| a lead plans it | `plan: true`, `model`, `effort` | the lead, as before, with its model and effort |
| I know the outcomes | `outcomes: ["title — how to tell it is done", {title, detail, task}]` | one agent per outcome (at most 12), named from its title, its task the title plus the goal; no planner |
| agents already running | `agents: [ids]` | nothing: each agent becomes an outcome of the goal (its task is the title) and is told so at its next step |

- `effort` is one of `low`, `medium`, `high`, `xhigh` or `max`, and is
  passed to Claude Code as `--effort`. Any other value is a 400, as is
  `plan` together with `outcomes` or `agents`.
- An outcome whose agent cannot start is listed in `missing`, and the
  outcome stays in the goal.
- On Mission, "Set a goal" has three tabs that match the table, a model
  box with the usual names, an effort select, and a button that says what
  will happen ("Start 3 agents", "Set the goal for 1 agent").
  "+ New goal" sets another goal while one is open.
- `SpawnRequest` gained `effort`.
- Test: `a_goal_takes_known_outcomes_or_running_agents_and_its_leads_effort`.
  The fake claude prints `EFFORT:`. That is a new case; the existing cases
  are unchanged.

Left out: an agent's effort is not stored on its row, so a restarted
agent does not keep it.

### 7. The guide (done)

GUIDE.md (new) covers what each screen is for and when to open it, the
first ten minutes, the keyboard and the words Workbench uses. The README
links to it at the top. Workbench serves it at `/guide.md` (compiled in,
or read beside `ui/` when `CHLOEDB_WORKBENCH_UI_DIR` is set; no token,
since it is not under `/api`). `#/guide` (`ui/levels/guide.js`, the "?"
at the bottom of the left rail, and a link in the keyboard help) renders
it with a table of contents. Its links to screens stay inside the app.

## Sixth build: Ship and Trace tell the truth

The testers caught Ship and Trace saying things the repository did not
support. Ship counted another agent's commits as this branch's work. It
called a branch already merged in a collision and gave one diff two
risks. Its pull request was titled with Workbench's own brief to a lead.
Trace and the Why card credited a prompt typed after the commit. Each item
below names what changed, the test that holds it, and what is left out.

### 1. Ship knows the branch it lands on

- `blast::landing` (src/blast.rs) decides where a branch lands: the main
  checkout's branch when that is not the agent's own, else `main` or
  `master`. The fork is the merge base with that branch.
- Each other agent's branch is compared with this one (deduplicated by
  branch, the newest agent kept):
  - Its merge base lies past the fork: it is **under** this branch. It is
    stacked when its tip is on this branch's first-parent line, else merged
    in.
  - Its merge base is this branch's HEAD: it is **above**, stacked on this
    branch.
- The change is counted from the top-most commit shared with a branch under
  it (`base_kind: "stack"`), else from the agent's recorded base when that
  lies past the fork, else from the fork. `/api/agents/:id/blast`,
  `/api/agents/:id/diff`, `/pr` and the Changes tab all use that base.
- Ship says "This branch is on top of freeship (wb/freeship): 1 commit of
  its work, not counted as this change". It says "has paybutton's branch
  wb/paybutton merged in" for a merge.
- An agent whose branch is under this one is not a collision. An agent
  stacked above has its files counted from this branch's HEAD, so it does
  not collide with this branch's own commits.
- When two branches change the same file and both have committed it, Ship
  runs `git merge-tree --write-tree` on the two tips. The agent then
  carries `merges_clean`, with "merges cleanly" or "conflicts" and the
  files. A clean merge scores 1 instead of 4.
- Tests: `ship_counts_a_stacked_branch_from_the_branch_under_it_and_ships_the_stack_together`
  and `ship_says_when_two_branches_in_one_file_merge_cleanly`.

### 2. One risk per diff

- The blast radius is computed once per set of facts and cached for 10
  minutes. The facts are the diff, HEAD and base, the tips under it, every
  other agent's tip and state, and the counts of `test_runs` and
  `test_summaries`. Blast radius, Pull request and the Changes tab's
  summary read the same answer, and `computed` says when it was made.
  The Changes summary keeps its copy 5 s instead of 20 s.
- Callers in test files are counted apart, as `test_callers`. A function
  called only by tests reads "no caller outside tests and 12 in tests",
  never "0 callers".
- Python `from … import` lines are not calls, and a method counts only
  where it is called (`.name` or `name(`).
- Test functions a change adds or edits are listed too, so "Only tests
  changed" is said rather than an empty green.
- Unit test: `blast::tests::counts_callers_in_tests_and_a_clean_merge_is_a_note`.
- **Deviation:** callers come from `git grep` over the agent's worktree, not
  from `code_chunks`/`identifiers`. That index is the main checkout's, so
  it does not hold the branch's new functions or their new callers.

### 3. Ship together

- `GET /api/agents/:id/pr/stack` drafts one pull request for the whole
  stack: every agent's branch under or above this one, bottom first.
- The draft names the top branch onto the landing branch. It has one part
  per branch: title, risk, summary and decisions. It is saved as
  `pr-stack.md` next to `pr.md`, with the `gh pr create` command for it.
- On the Pull request tab, a stack of two or more branches shows its chain
  in merge order and a "Ship together" button.
- A single branch's draft targets the branch it is stacked on (`base_branch`)
  and says so in its body.

### 4. The pull request says what was done

- The title is taken from the first of these that exists:
  1. the outcome the agent worked on;
  2. the goal it leads;
  3. its only commit;
  4. its task: the sentence that says what to do ("Make Pay show a
     confirmation with the total"), not the one that describes the bug
     nor "Done when …";
  5. its last commit;
  6. its name.

  What the agent said in the chat is never used, for the title or the
  summary (it names URLs, commit ids and "I haven't committed").

  Workbench's own brief to a lead is never used. `title_from` says which
  one it was, and the tab shows it.
- The summary is built from the commits' subjects and from what changed:
  per file, the functions it adds, changes and removes, and the tests by
  name. Cut text ends at a word, never inside a URL, a number or a code
  span.
- Decisions are only those of the branch's agents' sessions: the agent, a
  restart in the same worktree, and the children whose work is in the
  branch. A superseded decision names what replaced it ("superseded by
  ◆id: text"), or says ChloeCtx names no successor.
- "Tests it ran" names tests with their last result, fixed and flaky:
  - Count-only runs (`test_summaries`) are listed as commands with their
    counts.
  - Legacy rows that stored a command as a test are skipped.
  - A test that failed and then passed shows ✓ and "fixed in this work".
  - The merged-tree run (a merge commit containing this branch's HEAD) has
    its own row.
  - When no row exists, the terminal's test commands are shown with the
    counts their output printed, and the card says so.

### 5. The capsule is this branch's

- `pack` sets `CHLOECTX_PACK_SESSIONS` to the sessions of the branch's
  agents (the same set as the decisions).
- When ChloeCtx's manifest names the sessions it packed, the scope reads
  "2 sessions of this branch's agents (csv-export, freeship), since …".
- A chloectx that does not filter still packs everything, and the scope
  says so: "this chloectx cannot pack by session". The filter itself is
  ChloeCtx's change (part F).

### 6. Trace answers with the commit

- Edits to files outside the checkout (absolute paths, `../`) are not
  evidence.
- The headline's key change is a commit when one qualifies:
  - for "from x to y", the commit that changed x to y;
  - else the newest commit that changed or added it.

  An edit is the key only when no commit qualifies. An uncommitted edit
  after it is added: "Since then cents has an uncommitted edit to …".
- A question, rule or decision is cited only when it is the same agent's
  and about the subject.
- "Between x and y" reads as a change either way.
- The answer carries `history`, every change in order. The card shows it in
  one line ("Changed 3 times: rounded down (Nov 1) → half-even (Nov 5) →
  half-up (Nov 9)"), each step opening its card. `history_plain` is the same
  line for agents.
- Unit tests: `trace::tests::answers_with_the_commit_and_the_whole_history`
  and `reads_between_x_and_y_as_a_change_either_way`.
- **Deviation:** "between" keeps possessives off ("banker's" reads as
  "banker"), as the tokenizer does everywhere.

### 7. The Why card is read at the commit

- `/api/why` takes `commit=<sha>` (4 to 64 hex characters, else 400). Trace
  passes it for a line of a commit card, so the line is read with
  `git show <sha>:path` and blamed with `git blame <sha>`, and `at` says
  which commit.
- ChloeCtx's blame is used only when the current file still has the line.
- An edit counts only when it was made before the line's commit, and only
  then is its prompt credited. Workbench's events after the commit are
  dropped. A prompt typed after the commit is never "on your prompt".
- When no edit is on record, the notes come from the agent's session
  between its prompt and the commit (`why_from: "session"`). The card
  says so, rather than "not available".
- The header says "wrote this" only with an edit on record. Else it says
  "This line came in with b72f105 Nov 9, “…”, on rounding-doc's branch. No
  recorded edit shows who typed it." The footnotes say the same.
- Tests: `why_reads_a_line_as_the_commit_it_was_clicked_from_has_it` and
  `why_credits_no_prompt_after_the_lines_commit`.

### 8. One Why? button

- The diff has one Why? button. It sits on the added line under the
  pointer, else on the cursor's line, so a page of 91 added lines shows
  one. `y` and Alt-click work as before.

## Sixth build: E

Agent lifecycle, Preview, read-only agents and grounded autocomplete. The
tests are in `tests/workbench.rs`
(`restart_starts_an_exited_agent_again_in_its_worktree_and_session`,
`read_only_agents_and_path_guards_refuse_changes`,
`an_agents_servers_are_listed_and_stopped_with_it`,
`a_done_check_is_its_own_clause_and_a_ghost_names_only_what_exists`). The
unit tests of `guarded.rs`, `complete.rs`, `complete/foresee.rs` and
`preview/servers.rs` cover the rest.

### 1. Restart

`POST /api/agents/:id/restart {prompt?}` starts an exited agent again. It
keeps the agent's:

- name, task, worktree and branch;
- model, goal and read-only flag;
- outcome, unless another agent took it since;
- parent, if that one still runs.

When the old session's transcript is on disk, Claude Code gets
`--session-id <new> --resume <old> --fork-session`. It continues the
conversation under a new session id, so the two agents' events stay
apart.

- If the old session ended mid-turn (its last hook was not Stop), the
  first prompt says "Workbench restarted this session after it ended.
  Carry on with your task where you left off."
- Otherwise the agent waits at the prompt.
- Without a transcript, the task is the first prompt again.

The answer is 409 when the agent runs, or when a running agent has its
name in its repository.

The new agent carries `restarted_from` and `resumed` (the session it
continues), kept in the mark `agent-extra:<id>`. Its `spawned` event says
"restart of <id>, continuing its session".

In the UI:

- An exited agent's zoom has Restart where Stop was.
- When the new agent appears, the old agent's zoom, Preview and Ship move
  to it (`followLive`, by `restarted_from`).
- A level opened without an agent picks the live agent of the name
  (`liveId`).
- The task text is the old agent's.

### 2. Agents' servers

A server is a process that listens on a TCP port and belongs to an agent.
It belongs to the agent when either:

- its environment has `CHLOEDB_WORKBENCH_AGENT=<id>`, so a server started
  with `setsid` or `nohup` is found too; or
- it descends from the agent's process.

On Linux they are read from `/proc` (`net/tcp`, `net/tcp6`, the sockets in
`fd/`, `environ` and `stat`); elsewhere from `lsof` and `ps`.

The routes:

- `GET /api/servers?agent=` lists them: agent, command, ports, since, cwd,
  and whether it serves the agent's preview.
- `POST /api/servers/:pid/stop` stops one the scan finds: SIGTERM to its
  process group, then SIGKILL after 3 s.
- `POST /api/agents/:id/stop {servers}` stops them with the agent.
- `PUT /api/servers/policy {on_stop}` sets the policy: `stop`, `keep`, or
  empty.

Without the `servers` flag, the policy decides on Stop; an empty policy
stops them. An agent that ends by itself stops its servers only under
`stop`. Each stop is a `servers_stopped` event.

The UI lists them in the zoom's facts and in Preview's side panel. The
stop dialog asks once ("Stop them too"), with Remember.

### 3. Preview overlay

Overlay off now sends `{type: "wb-pick-mode", on: false}` to the page: on
toggle, on load and on `wb-ready`. The overlay script stops taking clicks,
so every click and key reaches the app, and a pick that arrives while the
overlay is off is ignored. The button says "overlay off: the page is
yours".

The stray "null" under the frame came from the runtime strip's
`replaceChildren(..., null)`, which writes the text "null". It is now
filtered out.

### 4. Read-only agents and path guards

`POST /api/agents {read_only: true}` is the New agent dialog's Read-only.

- Edit, Write, MultiEdit and NotebookEdit go into the session's
  `permissions.deny`, and the PreToolUse hook refuses them too.
- Bash runs only commands that read. Each simple command must be on the
  read list: `cat`, `rg`, `ls`, `git log|diff|show|status|…`, `find`
  without `-delete` or `-exec`, `sed` without `-i`, and so on.
- A command with `$(…)`, backticks or a redirect to a file is denied.
- Anything else is denied with a reason that points to `propose_change`.
- An agent that a read-only agent starts is read-only too.

The intent check marks a task that says it changes nothing ("read-only",
"don't change any files", …). The mark offers "Start it read-only", which
ticks the box.

The guarded list (`PUT /api/guarded`) takes rules of kind `path` beside
`prefix`. A pattern is read as `.gitignore` reads it: `money.py` matches
anywhere, and `migrations/**` and `*.lock` work as there. With a path rule:

- Edit-family tools on a matching file are denied.
- A Bash command that names a matching file, and does not only read, is
  denied.

The Decide screen's guarded list has "Add a path".

### 5. Grounded autocomplete

The model's context now has the repository's files (up to 60) and the four
code chunks nearest the text (`code_chunks`, BM25). Its prompt says to
name only what the context shows.

Every ghost the model writes is checked before it is offered:

- each file name must be a tracked file;
- each identifier (`snake_case`, `camelCase`, `name()`) must be in the
  code or the context;
- each other word of four letters or more must be in one of: the code
  (`git grep -w -i`, with its forms), the text, the context, the memory's
  records, or the language of tasks.

A ghost with any other name is dropped: the answer has `ghost: null`,
`dropped: {text, names}` and a reason. So it never becomes the Tab
default. A checked ghost carries `checked: true`, and the page offers a
model ghost only then.

### 6. "Add a done check"

Where the clause goes:

- at the end of a complete sentence, after a space;
- when the text ends mid-sentence, after the last complete sentence;
- when no sentence is complete, nowhere: the card says to finish the
  sentence first and offers no fix.

What it says:

- A task that changes code gets "Done when `<test command>` passes.", or
  "Done when a test shows it works."
- A task that asks for an answer and changes nothing (or is read-only)
  gets "Done when it answers: <what it asks>." and no test.

### 7. The repository box

The New agent dialog's repository box is a one-row textarea that grows and
wraps, so the whole path shows. It completes from the recent repositories
and from part D's `GET /api/repos/dirs?q=`, git repositories first. Enter
or Tab takes a suggestion, and a folder opens its children.

### Left out

- Restart is in the UI and the API, not an MCP tool. Nothing restarts
  every agent at once after a server restart.
- Servers are tested on Linux only; CI does not run the macOS path (`lsof`
  and `ps`). A server is not found if its process left the agent's tree
  and dropped `CHLOEDB_WORKBENCH_AGENT` from its environment.
- The read-only Bash check is a list of commands, so a read command it
  does not know is refused (the agent can propose it).
- Path guards see a file in Bash only when the command names it as a word.
  A script that writes the file is not caught.
- The grounding check is lexical. A word that is in the code passes in
  any sense, and words under four letters pass. The deterministic ghost,
  built from the records, is not checked.
- The repository box needs part D's `/api/repos/dirs`. Without it (a 404)
  it completes from the recent repositories only. For the screenshots, the
  test harness answered that route from the real scratch folder.

## Sixth build: ChloeCtx remembers the right things

Five simulated users found that ChloeCtx kept the wrong things, or kept the
right things under the wrong name: a one-off instruction became a standing
rule, a lead's brief became "User preference", an agent's guess became a
fact, one worktree's reading of the code was given to another, and a ruling
was half superseded by nothing. The fixes are in ChloeCtx (`chloectx/`),
plus what Workbench tells it about each session (`src/ctxwire.rs`).

1. **One-off instructions stay with their task.** The distiller says, for
   each rule, whether it is standing or for the task, and the daemon
   decides with `capture::standing_rule`. A rule stays a `preference` only
   when the user said so, in a sentence about the same thing: "always",
   "never", "from now on", "by default", "every time", a convention or a
   policy. An answer to the assistant's question asking for a rule also
   counts (the end of its previous message goes to the distiller). Any other
   instruction becomes a `note` tagged `task`, worded "For cents's task only
   (the user said, not a standing rule): …". The store's filter keeps a
   `task` note out of every other session's search, recall, briefing,
   consolidation and capsule (`Filter::conds`). Workbench's Map, Ship and
   Trace read only decisions, preferences and facts, so they no longer show
   it. Rules added in Workbench's Rules panel are `chloectx remember`
   records, as before.
2. **Who said it.** Every record can carry a `By:` line: `user`,
   `workbench`, `agent:<name>` or `assistant[:<name>]` (`record::Source`).
   - Preference cues ("User preference: …") come only from a person's
     prompt. An agent's or Workbench's prompt is stored with its source and
     gives no cue.
   - The distiller is told when the request was written by another agent or
     by Workbench. Each item says whether the requester stated it or the
     assistant found it. `distill::attribute` then makes:
     - an agent's or Workbench's rule a task note;
     - an agent's or Workbench's decision or fact "The agent lead said: …";
     - what a named Workbench agent concluded "The agent cents said: …".
   - `memory_remember` (the agent's tool) records the assistant as its
     author. `chloectx remember` (a person, or Workbench for one) records
     the user.
   - `memory_ask` heads its notes "Notes from earlier sessions", not "from
     earlier conversations with the user". It shows "(the agent lead
     said)" after the kind, and the role of an excerpt is the prompt's
     author ("agent lead", "Workbench's brief", "user").
   - The hook reads `CHLOECTX_AGENT_NAME`, `CHLOECTX_PROMPT_SOURCE` (who
     wrote the session's first prompt, its task) and `CHLOECTX_PROMPT_LOG`.
     That is a JSON-lines file of `{ts, text, source}` that Workbench
     appends to before it types a prompt that is not the user's. The newest
     line of the last hour whose text starts the prompt wins
     (`hook::prompt_source`). A prompt nothing names is the user's.
   - `ctxwire::Who` builds those three variables. Its source is
     `agent:<parent name>` for a spawned agent, `workbench` for a goal's
     lead and `user` otherwise. `ctxwire::wire_as` merges them into the
     session's settings and notes a task that is not the user's.
     `ctxwire::note_prompt` adds a line.
3. **Branches.**
   - When a session's hooks first name its worktree, and whenever its
     branch changes, the daemon stores an event (tag `worktree`, an
     `Anchor:` line with branch and commit). Every record therefore has a
     branch: its own anchor's, else its session's at the time.
   - A file the agent read or changed (Read, Edit, MultiEdit, Write,
     NotebookEdit) anchors the call's records: commit, branch and content
     hash.
   - `Checker::label_record` labels a record from another branch. Git gives
     the merge base of that branch with this worktree's HEAD. A record
     written after that commit's time describes work this worktree does not
     have, so it is labelled "[from branch wb/paybutton, not merged here: …
     not a description of this worktree's code]". The label is left off
     when the record's anchored files hash the same here.
   - Every place a record is shown uses it: recall, the knowledge block,
     file notes, the briefing and the query engine. The briefing moves such
     notes to its "may be out of date or from another branch" section.
     The ranker's prompt says never to present them as the current code
     (Maya's "to_cents still uses banker's rounding" reading came from
     paybutton's Read of money.py).
   - Branch-merged records (the branch committed after the record and is
     an ancestor of HEAD) and records older than the fork are shown as
     before.
4. **Superseded only by a successor.** `Store::supersede(old, new)` marks
   `old` superseded only when `new` is stored, and adds a `superseded_by`
   link to it, so every view can show what replaced it. The distiller's
   replacements count only after the new note is stored, links to the old
   one and shares at least 30% of its words (`daemon::successor_of`). A
   replacement dropped as a near-copy, or one about the other half of a
   ruling, leaves the old note live. `remember --supersedes` refuses an id
   that does not exist and supersedes after storing. Consolidation
   supersedes with the merged note.
5. **Decisions visible where Workbench looks.**
   - A person's prompt line that states a decision ("My decision: …",
     "Decision: …", "We decided …") is stored at once as a `decision` by
     the user ("The user decided: …", tag `stated`). It does not wait for
     the distiller, and the distiller sees it as already noted.
   - An agent's write to a memory file outside the repository (a path
     with a `memory` folder, `.md`) is kept as a note that names the file
     (tag `memory-file`, `Lives: <path>`).
   - The briefing tells agents to keep decisions with `memory_remember`,
     not in memory files of their own.
   - `remember` answers with the store's path ("Stored decision as … in
     ChloeCtx (…/memory.strata), where Workbench and every later session
     see it").

For Ship's capsule (part C): `chloectx pack` keeps only the records of the
sessions in `--sessions a,b` or `CHLOECTX_PACK_SESSIONS`, and the entities
they link to. It names them in the manifest as `sessions`, which a capsule
of every session leaves out.

Also: `chloectx --help` names `wiring` in its list of commands. Its test
failed at the start commit.

### Integration left for the merge

Workbench's `spawn` and `Supervisor::input` belong to other parts, so
these two calls are not made yet. Until they are, `Who`, `wire_as` and
`note_prompt` are marked `allow(dead_code)`, and Workbench's sessions are
recorded as before: every prompt the user's, but with the other fixes.

- In `spawn`, replace `ctxwire::wire(...)` with
  `ctxwire::wire_as(..., &who)`, where `who` is
  `ctxwire::Who::new(&name, &session_dir, parent_name.as_deref(), req.goal.is_some() && req.outcome.is_none() && req.parent.is_none(), task.as_deref())`
  and `parent_name` is the parent agent's name.
- In `Supervisor::input`, before the text is written, when `by` names
  another agent, call
  `ctxwire::note_prompt(&ctxwire::prompt_log(&data_dir.join("sessions").join(id)), text, &format!("agent:{}", by_name))`.

### Left out

- Task scope is the session. A restarted agent with a new session id does
  not see its earlier task notes; Workbench's Restart (part E) continues
  the session when it can.
- `standing_rule` reads words, not meaning. "We use semicolons in CSV
  exports" with no standing word becomes a task note. A rule the user meant
  as standing can be added in the Rules panel, or said with "always".
- Records written before this build have no `By:` line and are taken as
  the user's. Old cue preferences from agents' briefs stay until consolidated
  or forgotten.
- The branch check needs git and the record's branch to still exist. A
  record from a deleted branch is shown without a branch label.
- Workbench's own views (Ship, Map, Trace, capsule) show the record text,
  which now carries the attribution. They do not yet read the `By:` line
  or `superseded_by` themselves (parts C and D).

## Sixth build: Safe answering

The testers' prompts (Maya's 11-, 12-, 15-, 17-; Dev's 07-, 43-, 46-,
51-; Sam's 22-; Priya's 13-, 15-) showed that a permission card could
grant more than its button said. The card had fixed options (Yes /
Always / No) and typed their numbers, `1`, `2` or `3`. Claude Code's
prompt has three or four options, and their order varies: with four,
`3` is "Yes, and switch to auto mode", so the card's "No" put the agent
in auto mode. A card whose prompt had gone typed its digit into the text
prompt as a message. Workbench now reads what the terminal shows and
types only into the prompt it read.

### What the terminal shows (src/screen.rs)

Each agent's terminal output also feeds a small VT emulator (`Screen`,
the agent's size, in `supervisor::Term`). It handles UTF-8 and wide
characters, the cursor and erase sequences Claude Code's renderer
(Ink) uses, scroll regions, the alternate screen, DECCKM (application
cursor keys), and the lines the terminal itself wraps. `screen::read`
says what the screen waits on:

- `Menu`: a prompt with its kind (`permission`, `trust`, or `dialog` for
  any other menu, such as "Do you want to create …?"), header (the
  command or the file, with the dashed rule skipped), question, options
  (`{n, label}`, wrapped labels joined, a hint after " · " cut), the
  selected option and footer. A menu is the last thing on the screen
  apart from its footer (at most three lines below it). Numbered options
  run 1..n. An unnumbered one, the trust prompt, needs an "Enter to
  confirm" or "Esc" footer. A `❯` line above the text prompt is not a
  menu: it is something the agent printed.
- `Input { interrupted }`: Claude Code's text prompt, `❯` between two
  rules, with "Interrupted · What should Claude do instead?" above it or
  not.
- `Busy`: a spinner line ("✻ Word… (26s · …)").
- `Unknown`: anything else.

A menu's signature (kind, header, question and labels, with blanks and
borders taken out) names "the same prompt". Its unit tests are built
from the testers' screens: Maya's four options with option 2 wrapped
over three lines, Dev's three options with auto mode second, the trust
prompt, the idle and interrupted text prompts, a spinner, and a menu
printed in the output above the text prompt.

### The items

1. **The prompt's own options.** A permission question's `options` are
   `[{label, n, no, grants?, matches_precedent}]` as the prompt lists
   them. They are attached when the Notification arrives if the prompt
   is already drawn, or by the screen watcher once it is. They are empty
   until then: the card says "Reading the prompt's options…" and offers
   its terminal. Answering `{option: i}` chooses that label: Workbench
   finds it on the screen, moves the cursor with the arrow keys (in the
   prompt's cursor mode), checks after each key that the selection moved
   and resends a lost key, and presses Enter only with the cursor on
   that label. It then waits for the prompt to go. `{cancel: true}`
   presses Esc. An optional `label` must match the option at `option`,
   or nothing is typed. "No" is the prompt's own No ("No", "No, and tell
   Claude what to do differently"), or Esc when the prompt has none.
   `grants` marks an option that grants more than the one call ("don't
   ask again", "auto mode", "during this session"). The card shows it
   dashed and says what it grants. Needs-you shows only the plain first
   option and the No, plus "N more…", which opens Decide. Digit keys in
   Decide map to the prompt's own numbers. The answer is recorded as an
   `input` event, "chose “X” in its prompt".
2. **The whole command, and the diff.** The question's new `detail`
   (JSON, a new column; `ALTER TABLE … ADD COLUMN IF NOT EXISTS detail`)
   comes from the PreToolUse hook that asked:
   - Bash: the whole command (up to 64 KB) and its description.
   - Edit, MultiEdit, Write and NotebookEdit: the file, its path relative
     to the hook's cwd, and a line diff (LCS, three lines of context,
     "@@ line N" when the old text is found on disk, at most 600 lines).
   - Any other tool: its input.

   Decide shows it monospaced and scrollable, the diff with added and
   removed lines coloured. The context line (`Bash: <first line>`,
   `Edit: <rel path>`) stays for the lists.
3. **The trust prompt.** It is read from the screen ("Accessing
   workspace", "No, exit / Yes, I trust this folder"). In a worktree
   Workbench made (under its data dir's `worktrees/`), Workbench answers
   "Yes, I trust this folder" by the same safe typing, once per prompt,
   and says so in the agent's events ("Workbench trusted <cwd> for it: a
   worktree Workbench made"). Elsewhere it is a Needs-you question,
   "Trust this folder?", whose options are the prompt's own.
4. **A stale card.** Before typing, Workbench reads the screen again: if
   no menu shows, or a menu with another signature, it types nothing,
   closes the question (`state: expired`, `answered_by: screen`, the
   answer saying why) and answers 409 `prompt_gone`: "Nothing was typed:
   the prompt is no longer on X's screen: it was interrupted (Esc) and
   waits at its text prompt, where a key would be a new message. The
   question is closed." The screen watcher also closes a question whose
   prompt has left the screen for 1.5 s (answered or cancelled in the
   terminal, interrupted, or working again), or that another prompt
   replaced. It closes the very question it saw, not the agent's newest.
   The UI shows the reason and reloads.
5. **Keys.** In a zoomed terminal every key goes to the agent: no
   single-key shortcut fires there or in a text field. Typing while the
   focus has left the terminal (on the page, not in a field) goes to the
   terminal. Esc goes to the agent too (Claude Code interrupts with it).
   For a working or waiting agent a note says so, at most every 8 s:
   "Esc went to X: Claude Code stops what it is doing. To leave the
   terminal, press Ctrl-Esc." Ctrl-Esc leaves the zoom from anywhere
   (also Ctrl-b z). The hint and the help say Ctrl-Esc.
6. **A steer to an interrupted agent.** `POST /api/agents/:id/steer`
   reads the screen:
   - At a menu: nothing is typed, the steer is queued, and `message` says
     to answer the prompt first.
   - At its text prompt, or idle: it is typed in as the next instruction
     ("Typed into X's prompt as its next instruction. It had been
     interrupted and was waiting for what to do instead.").
   - Otherwise it is queued for the next tool call, as before.

   The answer carries `message`. Story shows it (green when typed, amber
   when queued, red on an error, with a 15 s timeout). The watcher also
   types queued steers when an agent sits at its text prompt.
7. **Needs you, from the screen.** A menu the screen shows for 600 ms
   with no open question becomes one. Claude Code sends no Notification
   for the trust prompt, "Do you want to create …?" and dialogs, and
   sends some late. Its event is "waits on a prompt: …", and the agent
   is `waiting`. An agent whose screen shows its text prompt for 2.5 s
   while it is `working`, `starting` or `waiting` (with no open
   ask_human) becomes `idle`: "interrupted: waits at its prompt for what
   to do instead", or "waits at its prompt".
8. **Agent-to-agent requests.** For `mcp__workbench__*` tools, `detail`
   names the other agent and its state, and the card's title says the
   action: "Stop freeship (idle)", "Type into web (waiting for you):
   “…”", "Wait for X", "Start an agent named X: …". With an outcome,
   its state is shown too.
9. **Permissions in a worktree.** An agent started in its own worktree
   gets these in `permissions.allow` (`flow::with_worktree_defaults`):
   - `Edit(//<worktree>/**)`, which in Claude Code also covers Write,
     MultiEdit and NotebookEdit.
   - `Read(//<worktree>/**)`, which also covers Grep and Glob.
   - Thirty test runners (`cargo test`, `pytest`, `npm test`, `go test`,
     …).
   - Local git: status, diff, log, show, add, commit, stash, branch,
     checkout, switch, restore, rebase, merge, rev-parse, blame, grep,
     ls-files, cherry-pick, mv, rm. Not push, not reset.
   - `mcp__workbench` and `mcp__chloectx`.

   Guarded commands are still denied by the PreToolUse hook
   (src/guarded.rs). Anything outside the worktree still asks. New agent
   has "Ask before edits" (`ask_edits: true` on `POST /api/agents`), off
   by default. When it is on, or outside a worktree, none of this is
   added. The help line under it says what is allowed.

### When the prompt cannot be read

A Notification whose prompt never shows as a menu (a layout this reader
does not know) leaves the options empty. Answering it then types only
what means the same in every Claude Code prompt: option 0 as `1` and
Enter (the plain "Yes"), or `cancel` as Esc. It does so only while the
screen shows neither the text prompt nor a spinner. Anything else is 409
`not_typed`, with a pointer to the terminal.

### API changes

- Questions have `detail` (see 2). A permission question's `options` are
  the prompt's own, and `[]` until they are read.
- `POST /api/questions/:id/answer` for a permission question takes
  `{option, label?, rule?}` or `{cancel: true}`. 409 `prompt_gone` means
  the prompt left the screen, nothing was typed, and the question is
  closed. 409 `not_typed` means nothing was typed and the question stays
  open.
- `POST /api/agents/:id/steer` answers `message` too.
- `POST /api/agents` takes `ask_edits`.

### Tests

The fake claude draws the testers' prompts with a `menu` function:
numbered or not, a `❯` cursor moved by the arrow keys in either cursor
mode, Enter, Esc and digits, and the Notification hook after the first
drawing (`PERM3`, `PERM4`, `CREATE` with no Notification, `TRUST`). After
No or Esc it shows the interrupted text prompt and runs no Stop hook, as
Claude Code does. New cases:

- No chooses the prompt's own No, and a steer reaches the interrupted
  agent (typed into its prompt).
- A stale card types nothing and closes, saying why.
- The trust prompt is answered in Workbench's worktrees and asked
  elsewhere.
- A prompt with no Notification still needs you.
- A card shows the whole command (a heredoc), the diff and another agent
  by name.
- Agents in their worktrees edit, test and commit without asking, and
  "Ask before edits" turns that off.

The existing `permission_prompts_are_questions_and_steers_reach_the_agent`
was changed in two lines: it asserted the fixed labels and the typed `1`,
which is the bug. It now draws a three-option prompt and expects that
prompt's labels and the choice of "Yes" on its screen.

### Left out

- **Pre-trusting a folder.** Claude Code has no supported way to trust a
  folder other than its own prompt: the trust is kept in
  `~/.claude.json`, which Workbench must not edit. Workbench answers the
  prompt from the screen instead.
- **A steer into a text prompt that holds a draft.** It is typed after
  whatever is there: Workbench does not clear what someone has started
  typing in the terminal.
- **`mcp__workbench` is allowed whole in a worktree,** stop_agent
  included, as the brief lists it. "Ask before edits" leaves it out with
  the rest.
- **Only `Edit(…)` and `Read(…)` rules are written.** In Claude Code they
  cover Write, MultiEdit and NotebookEdit, and Grep and Glob. Rules for
  those tools by name are not added.
- **Prompts not tested.** The LSP install popup, the cost and model
  dialogs, and plan-mode prompts are read as `dialog` menus when they
  match the menu rules. None of them is in the fake, so they are
  untested.
- **Terminal reflow on resize.** The emulator does not reflow. A line
  the terminal wrapped keeps the width it wrapped at, and a narrower
  terminal keeps the text past its edge until the program redraws
  (Claude Code does on a resize), so zooming a terminal does not change
  the prompt Workbench reads. The signature ignores blanks, so Claude
  Code's own rewrapping does not change it either.

## Sixth build: B Honest numbers

Every number that Health, Away, Hotspots and the model hint show is now
checked against the rows it comes from, and each says what it counts. The
causes and fixes, item by item:

1. **Every test of every run** (`src/testruns.rs`). Health said 0 flaky
   for `test_promo_window` because `pytest -q` prints no test names. The
   run became one made-up "test" named after its command. Now:
   - Each session's environment has
     `PYTEST_ADDOPTS=--junitxml='<session dir>/pytest-junit.xml' -o junit_family=xunit1`.
     It is appended to the user's own value, and left out when
     `pytest.ini`, `pyproject.toml`, `setup.cfg` or `tox.ini` mention junit.
   - A report counts for a call only when both are true:
     - It was written inside the call: from 50 ms before its PreToolUse to
       5 s after its PostToolUse.
     - No other call has read it already. A call that starts right after
       another must not read the first one's report.
   - A `--junitxml` path in the command is read too.
   - Test names: xunit1 names tests by their file
     (`test_coupons.py::test_x`). An xunit2 dotted class name is matched
     against the checkout.
   - The console parsers read `pytest -v` and `-rA` lines. They also read
     every runner's summary counts (cargo, pytest, jest, vitest, go).
   - A command that names no test is only a row of the new table
     `test_summaries`. That table has one row per test command:
     - counts and exit code;
     - `complete`;
     - `source`: junit, console, summary or exit;
     - `ms` and `wait_ms`.
   - `test_runs` holds named tests only. Old rows with `runner = 'other'`
     are left out of the statistics.
   - Flaky keeps its definition: passed and failed on the same commit, or
     flipped twice within a day on one commit.
   - A test is `gone` ("not run any more") when its command ran again
     without it. A gone test is not counted as newly failing.
   - `./pytest`, `-m pytest` and `.../py.test` now count as test commands
     (`is_test_command`). `flow::is_test` is left as it is.
2. **Waits and fixes.**
   - Claude Code fires PreToolUse before the permission prompt, so the
     prompt's wait is inside the Pre-to-Post span. The brief assumed the
     reverse.
   - A run's time is the first of these that is known:
     - the runner's own total;
     - else the span minus the time its permission questions were open,
       when each was answered in Workbench;
     - else unknown. A prompt answered in the terminal says nothing about
       when the run began.
   - A test gets the run's time only when it was the run's only test, or
     when the runner gave a total. `slow` needs a test's own times.
   - "Fixed in this session": a failure followed, for the same agent, by a
     pass after the code changed (`fixed_in_session`, public for Ship). It
     is shown, and left out of the pass rate.
   - Recent test commands say how long a command waited, as in "waited
     4 s for you".
3. **Stable since and Run it N times.**
   - Each test has `stable`:
     - the commit and time of the first pass after its last failure;
     - the runs since then;
     - the failures before;
     - for a flaky test, how likely that many passes in a row would be if
       nothing had changed: (1-f)^n.
   - The history has "Run it N times" (5 to 50).
     `POST /api/tests/rerun {test, n}` runs the one test:
     - pytest by node id;
     - cargo with `--exact`;
     - go with `-run ^X$`.
   - The rerun happens in the checkout of the agent that ran the test
     last, with these settings:
     - no shell, `CI=1`, no `PYTEST_ADDOPTS`;
     - no bytecode written;
     - 5 minutes per run at most.
   - Each run is recorded like an agent's, and marked as Workbench's
     (`by_workbench`).
   - `GET /api/tests/rerun/:id` follows the rerun. The page shows its
     progress, then a result such as "10 of 10 passed on c4617594".
   - For a runner where Workbench cannot run a single test, the page
     offers "Ask <agent> to run it 10 times" (a steer).
4. **Test with a persona** is a card at the foot of Tests.
   - It has a button per agent with a preview URL. The button opens
     Preview with the persona drawer open. A `sessionStorage` flag,
     `wb.persona`, is read once by `ui/levels/preview-lab.js`.
   - The persona prompts (`chloedb-swarm/src/workbench.mjs`) now say:
     - do the task's main action once, and keep going past problems;
     - before calling a number wrong, check it against every amount the
       page shows.
   - The review now says:
     - a wrong number is a bug only after that check;
     - severity 3 only when the app stopped the task;
     - "Pay was never pressed" when the main action was not done.
5. **Away** (`src/digest.rs`, `ui/levels/away.js`).
   - Since: the `since` parameter, else when you last looked, else when
     the first agent started. A first visit used to count from 24 h ago.
   - `mode` says which, and "Welcome back" shows only for `away` and
     `stepped`:
     - `first`: since the first agent started;
     - `away`: since you last looked;
     - `stepped`: since "I'm stepping away";
     - `here`: the page's heartbeat came 90 s or more after `since`. The
       page posts `POST /api/here` once a minute while it is shown and
       used;
     - `since`: from the `since` parameter.
   - "Replay the morning", "afternoon", "evening" or "night" comes from
     the middle hour of the span. It says "the day" over 10 h, and "the
     decisions" over a day.
   - The "need you" tile is the top bar's count: running agents waiting or
     idle. The digest's `needs_you` is the same list.
   - "Who waited on you" shows, per agent:
     - how long its questions were open, by kind;
     - its idle stretches, from Stop to the next prompt.
     Both are clipped to the window.
   - An answer in a terminal counts as yours. Only rules decide "without
     you".
   - "I'm stepping away" (`POST /api/away/leave`) marks the moment and
     lists:
     - who needs you now;
     - who asked you in the last 15 min;
     - who is idle.
     It answers nothing.
6. **Hotspots** (`src/hotspots.rs`, `ui/levels/hotspots.js`).
   - The header's totals are the sums of every scored file's counts
     (`Totals::of`), with the hotspots by level, as in "2 hotspots, 1 warm
     · 1 mild". The list and the header can no longer disagree.
   - `git log --all`, without merges, seeds the score:
     - each commit after the first: 1.5;
     - each fix commit: 1.5. A fix commit's subject has fix, bug, broken,
       wrong, regression, hotfix or correct.
   - Commits join the churn per day.
   - A function gets an edit only inside its own span.
     - The code index's chunk starts at what is attached above the
       definition (imports, comments).
     - The span now starts at the line that defines the name
       (`def_line`): a whole-word match after def, fn, class and similar
       words.
   - Each piece of evidence names its branch:
     - the agent's branch;
     - or, for a commit, HEAD's branch when HEAD reaches it, else the ref
       git names.
   - A permission you refused counts as a correction of the file it named.
7. **Model routing** (`src/routing.rs`, `ui/levels/route-hint.js`).
   - With 1 to 4 similar past tasks there is no suggestion. The hint says
     only "Not enough history: N similar past tasks …. The model is your
     choice." The `enough` field says which case applies.
   - A cost of $0.00, meaning none was recorded, is not shown.

Left out, and why:
- No per-test pass is inferred from a summary line without names: a
  count is shown as a count.
- `flow::is_test`, which Flow uses for test time, is unchanged. The wider
  match is in `testruns::is_test_command` only, so Flow's numbers do not
  move here.
- Part A may change how permissions are answered. `permission_wait` counts
  only questions answered in Workbench, and says "unknown" otherwise, so
  it stays true whatever A changes.
- Run it N times keeps its progress in memory. A restart forgets the
  progress, not the recorded runs.

## Sixth build: Review tells the truth

Simulated users (Maya, Lee, Sam, Priya, Dev) reviewed and merged branches
with Review (`ui/goals/review.js`, `src/review.rs`). What changed:

- **One source for decisions.** `decisions_of` in `src/review.rs` is the
  list for Review, Ship and the PR: the ChloeCtx records of the branch's
  sessions (no facts, no superseded entries), the active rules that name a
  changed file, and the user's own decisions ("My decision: …", signed
  "yours", never struck through). Each item has `by` (you, agent), `why`
  (replaces, yours, new rule, rule) and `confirmed` (from **Still
  right**). A decision that replaces another is never routine; routine
  ones open from "Decisions N".
- **The headline is what changed** ("Changes cartTotal(); …"), not the
  chat reply cut at its first period.
- **Merge-ability is computed one way**, on the worktree including its
  uncommitted changes (`worktree_commit`: a temporary index, `add -A`,
  `write-tree`, `commit-tree`), in the list and on the branch page;
  `merge.main.with_uncommitted` says when it was. Exceptions skip branches
  already merged.
- **Merged.** When the last merge stands and nothing changed since, the
  branch page answers `state: "Merged"`, `merged {commit, onto}` and
  `merged_commits`, and shows them with Undo; `/api/repos` counts the
  agent `finished` and adds `merged {id: {commit, onto, ts}}`, which the
  top bar ("1 agent · 1 done") and Continue ("X was merged into main")
  read.
- **One pull request for several branches**: `POST /api/review/pr
  {agents}` → `{title, body, body_file, branches, head, onto, make, gh,
  missing}`. The title joins the parts' titles with "; ", the body says
  "N changes, merged in this order: …" and has each part's summary,
  decisions and tests; `make` builds `wb/together-<names>` from the onto
  branch with them merged, and `gh` opens it. Nothing is run. An empty
  list, or agents of different repositories, is 400.
- **Tests check**: a failing test named in an active rule (a known flaky
  test) does not hold the merge and is listed as ignored; the failing
  output opens under "Why it fails".
- **UI**: the embedded Ship page no longer overlaps Review; one merge
  action per state (inspector on desktop, the bar on a phone); the gate
  (Still right / Change it) at the top; Enter in Merge anyway and
  Ctrl/Cmd-Enter in Send back submit, with a confirmation; Tell stays on
  ready agents; the preview's URL box is focused only when setting it;
  no "null" text where a part is missing.
- Test: `review_drafts_from_what_changed_counts_every_decision_and_says_merged`.

Left out: a preview of main after a merge, and the Changes tab's diff
wrapping on a phone (both done in the next section).

## Sixth build: the last leftovers

From the simulated users' second run (Priya, Maya, Dev, Lee):

- **The Changes tab wraps on a phone** as Review's key change does: no
  sideways scroll inside the diff; Why? sits at the line's right edge.
  The tab's body keeps most of the screen (a question card above it no
  longer squeezes the diff to a few lines): the page scrolls to it.
- **The task once.** The agent page's header holds the whole task, folded
  to two lines with **Show the whole task**; Progress's Intent is a
  one-line summary, and a narration that only repeats the task
  ("spawned: …") says "Starting…"; a chapter whose prompt is the task says
  "you: the task" rather than quoting it again.
- **A batch is done when every part is merged.** A goal's JSON has
  `parts {total, merged, left, done, words}` ("3 parts, 1 merged, 2 left",
  "3 parts, 3 merged"); an outcome has `merged` (Review's merge mark, not
  undone) and a merged one is `done`; a goal whose parts are all merged is
  written `done`. Since you left lists the project's goals with their parts
  and says "… is done (3 parts, 3 merged)"; an outcome's agent page says it
  under Intent.
- **Preview main.** `GET /api/repos/main-preview?repo=` → `{repo,
  project, id, href, branch, url, by, found, merged}` and `PUT
  /api/repos/main-preview {repo, url}` (empty clears). Main has its own id
  in the preview proxy, `main-<12 hex of the path's FNV-1a>`, served at
  `/preview/<id>/` with the same token, cookie, overlay and loopback-only
  URL check as an agent's; its URL is kept in the mark
  `preview-main:<repo>`, set by you or found: a process listening with the
  main checkout as its working directory (`by: "found"`). The page is
  `#/p/<project>/main`, reached from the project page's inspector and from
  every agent's Preview tab (primary once that agent was merged).
- **One count of decisions.** `GET /api/agents/:id/blast` adds `decided
  {count, matter, routine}` from `review::decisions_of`; the Changes tab's
  chip and Ship's tile read it.
- **What a decision replaced says whose it was** by the timeline's rules
  (`trail::said_by_you`: the note's words, its tag `stated`, its ctx `By:
  user`, the user's steers to its agent): `capsule::records` reads `tag`,
  `ctx` and `session`, and `replaces.who` is "yours" or "<agent>'s".
- **The headline and the key change are the source.** Tests and checks
  (`review::helper_path`: the test paths, `*_check.*`, `*.spec.*`,
  `*.test.*`, `check_*`, `checks/`) come after the files a user sees;
  lockfiles and generated files last.
- **Search** puts a hit that defines a word of the query first, by its
  symbol or by a line of its text (`def fmt`, `fn fmt`, `function fmt`, a
  page's own script), above the tests that call it; it reads three times
  the limit before cutting.
- **Trace says when two reasons conflict**: `conflict {text, kind,
  earlier, later}` when a later commit removes a line an earlier one added
  (or brings back one it removed) and the earlier message gave a reason
  (`because`, `after`, `for`, `ticket`, `#41`, `(finance asked)`, …): both
  messages quoted, in one line under the timeline. No model.
- **The Coach's "allow them in .claude/settings.json"** (ChloeCtx's
  report) carries `harness {href, label, allow}`: **Make this a harness
  change** opens `#/harness/edit/permissions` (also `#/harness/permissions`)
  with the prefixes approved more than once in the window prefilled in the
  draft, unsaved. When Workbench's own Allow covers the advice, the link
  sits beside that Allow.
- Trace orders commits made in the same second by their ancestry, so the
  later of two is the one that descends from the other.
- Tests: `a_batch_is_done_when_its_parts_merge_and_main_has_its_own_preview`,
  `review_names_the_fix_before_its_check_search_the_definition_and_trace_the_conflict`,
  `a_replaced_decision_that_was_yours_says_yours`, and unit tests in
  goals, preview, blast, trail, review, search, trace and flow.
