# ChloeDB Workbench

Workbench is a local app that runs many Claude Code sessions side by side.
Each one gets a real terminal (a pseudo-terminal), and most get their own git
worktree on a branch `wb/<name>`. Every session's state and events are rows
of an embedded ChloeDB database. Think of it as tmux with a friendlier UI and
a database as its source of truth. [SPEC.md](SPEC.md) is the contract that
this build follows.

**Using it rather than building on it? Read [GUIDE.md](GUIDE.md)**: what
each screen is for and when to open it, a first ten minutes, and the
keyboard. In the app, **Guide** (the **?** at the bottom of the left rail)
opens it. This README is the reference: installing, settings and the API.

## Install and run it

Workbench needs ChloeCtx: most of what it shows above the terminal (who
decided what and why, what each agent knew, rules, precedent, the digest)
is ChloeCtx's memory. One command, from the repository root, installs both:

```bash
./install.sh --workbench     # builds chloectx and chloedb-workbench into ~/.local/bin,
                             # downloads ChloeCtx's 129 MB model, checks both
~/.local/bin/chloedb-workbench serve
```

It does not change your Claude Code settings. Each session Workbench
starts gets ChloeCtx's hooks and its `chloectx` MCP server (the `memory_*`
tools) in its own settings and MCP config, from `chloectx wiring`; when
your settings or the project's already wire ChloeCtx (`./install.sh`,
`chloectx install`), nothing is added, so a session is recorded once. If
ChloeCtx is missing or too old, `serve` still starts, the UI shows what to
run, and starting an agent is refused (`chloectx_missing`) until it is
fixed. `--no-chloectx` (hidden) turns this off; the tests use it.

What ChloeCtx keeps, and what it shows you as a rule, follows who said it:

- An instruction you give one agent ("don't touch money.py or cart.py")
  is kept for that agent's task only. It becomes a standing rule, shown on
  Ship and Map and given to later agents, only when you say so: "always",
  "never", "from now on", "by default", or an answer to a question that
  asked for a rule. The Rules panel adds one directly.
- What a lead tells the agents it starts, a goal's brief and what agents
  conclude are labelled as theirs ("The agent lead said: …"), never as your
  preference or as fact. Workbench tells ChloeCtx who wrote each prompt.
- "My decision: …" (or "We decided …") typed to an agent, a steer included,
  is kept at once as your decision, in ChloeCtx's store for the repository,
  where Decisions, Ship and Trace read it. `chloectx remember` says where
  that store is.
- Worktrees share one memory but not their code. A record from another
  agent's branch that is not merged into this one is labelled "from branch
  wb/paybutton, not merged here" when it is shown to an agent, and a note
  is marked superseded only when a named note replaced it.

From a checkout, for development:

```bash
cd chloedb-workbench
cargo build --release
./target/release/chloedb-workbench serve --chloectx-bin ../chloectx/target/release/chloectx
```

It prints a URL that carries a token (`http://127.0.0.1:7740/?token=...`).
It does not open a browser for you. Open the URL: Start asks what you want
to do. The commands that take you somewhere:

```bash
chloedb-workbench serve ~/src/shop      # opens that project; the URL goes to it
chloedb-workbench new ~/src/new-thing   # makes the folder (git init, a first commit)
                                        # and prints the page that asks what to build
chloedb-workbench open ~/src/shop       # the running server's URL for that project:
                                        # Continue, or --review, --debug "why …", --continue
```

`open` and `new` talk to the server already running (the same `--addr`
and `--data-dir`); they start nothing themselves.

### The pages

Every page answers one question, and the goal comes first:

- **Start** (`#/`): start a new project, continue one, review and ship,
  or find out why something went wrong, with your projects below and
  what needs you at the side. On a first run it offers only: open a
  project you have, or start a new one.
- **New project** (`#/new`): four steps, the folder, what to build, who
  builds it (one agent, or a lead that plans the parts), and the plan,
  which you read before anything runs.
- **Continue** (`#/p/<project>`): since you left, what needs you, what is
  ready and what is still working. **Understand** asks about the project
  and shows who is changing which file; **Start a change** says what
  should change and how you will know it is done.
- **The agent page** (`#/a/<agent>`): the agent, the question it asks, and
  its tabs: Progress, Changes, Preview, Terminal, Events. On a phone the
  terminal is read-only until **Type**.
- **Review and ship** (`#/p/<project>/ship`) and **Debug** (`#/debug`):
  below.
- **Needs you** (`#/needs`, or `.` for the drawer): every question,
  permission prompt, trust prompt and alert that asks you, across
  projects, in one card each. Filter by project, agent and kind; sort by
  the oldest or by the most work an answer unblocks. `x` selects cards
  that run the identical command and `⇧` and a digit answers them alike,
  after a check. An option that grants more than one call ("don't ask
  again") opens a check first: the exact rule, where it is written, what
  else it would have allowed, and a narrower choice.
- **More** (`#/more`): all terminals, where the time went, the decisions
  timeline, Library, Rules and grants, Settings, Guide, Keys, the setup
  check, and on a phone Notifications.

The top bar has the path you are on, Search (`/`), one count of what needs
you, the agents (`a`, then a digit), the notifications bell and More. On a
phone a tab bar has Start, Needs you, Projects and More. The old addresses
(`#/mission`, `#/ship`, `#/trace`, `#<agent>` and the others) still work:
they go to the page that replaced them. **More › Keys** (or `?`) lists the
keys; in a terminal every key goes to the agent, and Ctrl-b first reaches
Workbench (Ctrl-b Esc leaves it).

To read a repository without any agent, open it from Start (**Open a
project you have**, with folder completion): Workbench indexes it for
Search, Understand and Debug. The settings:

| flag | env | default |
|---|---|---|
| `--addr` | `CHLOEDB_WORKBENCH_ADDR` | `127.0.0.1:7740` (loopback only) |
| `--data-dir` | `CHLOEDB_WORKBENCH_HOME` | `~/.local/share/chloedb-workbench` |
| `--claude-bin` | `CHLOEDB_WORKBENCH_CLAUDE` | `claude` |
| `--max-agents` | | 8 running at once |
| `--max-depth` | | 2: an agent you start may start agents, and those may not |
| `--chloectx-bin` | `CHLOEDB_WORKBENCH_CHLOECTX` | `chloectx`, else `~/.local/bin/chloectx` |
| `--token` | `CHLOEDB_WORKBENCH_TOKEN` | random |

The data dir holds:

- `workbench.chloedb`: the tables `agents` and `events`, and the levels' (`goals`, `outcomes`, `questions`, `rules`, `deliveries`, `seen`)
- `sessions/<id>/`: each session's `settings.json` (its hooks) and `mcp.json`
- `worktrees/<repo>-<hash>/<name>/`: the worktrees

Workbench never edits your own Claude Code settings. Each session's hooks
(Workbench's and ChloeCtx's) are added to yours.

When `CHLOEDB_WORKBENCH_UI_DIR=chloedb-workbench/ui` is set, the server
reads the UI from disk instead of the copy built into the binary, so UI
changes show up without a rebuild.

## What it serves

Every `/api` route except `/api/health` needs the token, sent as
`Authorization: Bearer <token>`. The stream and the terminal WebSocket can
take it as `?token=` instead. The server answers only to the hosts
`127.0.0.1`, `localhost` and `[::1]`. It refuses a request from another
origin when that request would change something.

- `GET /api/health`
- `GET /api/agents?all=1`, `POST /api/agents`, `GET /api/agents/:id`
  (`POST /api/agents` takes `permissions: {allow, ask, deny}`, Claude
  Code's rules, written into that session's own settings; a rule Claude
  Code would not read is refused)
- `GET /api/agents/:id/events?after=&limit=&data=1`
- `POST /api/agents/:id/input|resize|stop` (stop takes `{servers: true|false}`:
  whether the dev servers it started stop with it)
- `POST /api/agents/:id/restart {prompt?}`: an exited agent again, with
  its name, task and worktree, continuing its Claude Code session
- `GET /api/servers?agent=`, `POST /api/servers/:pid/stop`,
  `PUT /api/servers/policy {on_stop: "stop"|"keep"|""}`: the servers
  agents started (processes that listen on a port)
- `GET /api/attention`: the agents that are waiting for you or idle
- `GET /api/repos`, `POST /api/repos {path, trust?, init?}`, `DELETE /api/repos?path=`:
  the repositories Workbench knows without an agent (opened in a picker,
  or worked in by an agent); opening one indexes it for code search. Each
  has a short name (`project`), its agents counted by group (needs you,
  working, ready, finished) and whether it is trusted: with `trust: true`
  Workbench answers Claude Code's "trust this folder?" for it.
  With `init: true` it makes a new project (the folder, `git
  init`, a first commit). `GET /api/repos/dirs?q=` completes a folder
  path, git checkouts first
- `GET /api/needs`: everything that needs you, as one list and one count:
  questions and permission prompts (with Claude Code's numbered options),
  trust prompts, agents waiting on a prompt not read yet, and alerts that
  ask you, oldest first
- `GET /api/wait?agent=&until=idle|waiting|exited|any&timeout_s=`
- `POST /api/hook`: what `chloedb-workbench hook` posts
- `GET /api/stream`: server-sent events (`snapshot`, `agent`, `event`)
- `GET /api/agents/:id/pty`: a WebSocket with the terminal's output (its
  last 256 KiB first), which also takes keystrokes and
  `{"resize":[cols,rows]}`

`chloedb-workbench hook` is the program each session's hooks run.
`chloedb-workbench mcp` is the MCP server (`workbench`) that each session
gets. Its tools are `spawn_agent`, `list_agents`, `read_agent`,
`wait_agent`, `send_to_agent` and `stop_agent`, which let an agent start
other agents and steer them while you watch.

## The decision trail

Workbench records every session's terminal to
`sessions/<id>/term.cast` (asciicast v2, at most 64 MiB a session; past
that it stops and an event of kind `recording` says so). It keeps each
agent's `base`, the commit its worktree started from, and `started_ms`,
the zero of its recording. On top of those, and of ChloeCtx when it is
installed, these routes answer for any line an agent wrote:

- `GET /api/agents/:id/recording?from_ms=&to_ms=`: the window's events
  (`[ms, "o"|"r", data]`), the terminal's size at `from_ms`, and `before`,
  the screen's last 256 KiB before it, so a player can start mid-session
- `GET /api/agents/:id/diff`: what the agent's branch changed, committed or
  not, with untracked files as added: counted from where it forked from the
  branch it lands on, or from the top of another agent's branch it is
  stacked on or merged in (`landing` says which, as Ship does)
- `GET /api/why?agent=<id>&path=<rel>&line=<n>` (or `repo=`): the decision
  card. It runs `chloectx blame <path>:<line> --json` in the worktree, maps
  the edit's session to its agent, walks the agent's parents, finds the
  prompt and who gave it (you, or the agent that spawned it or typed it with
  `send_to_agent`), and places Rewind 20 s before the edit's tool call. When
  ChloeCtx is missing or has no record, it answers from git blame and
  Workbench's events; `found` says where the answer came from and `missing`
  what could not be found. With `commit=<sha>` (a line clicked on a commit,
  in Trace) it reads and blames the line as that commit has it, and credits
  no edit or prompt that came after the line's commit
- `GET /api/decisions?repo=&limit=`: the repository's decision notes, through
  `chloectx sql --json`
- `POST /api/ask {repo, question}`: `chloectx ask --json`, plus the agent of
  each record whose session is one of Workbench's

Ship (`#/ship/<agent>`) is where you decide a merge. It counts only the
branch's own change: when a branch was rebased onto another agent's branch,
or merged one in, it says "on top of freeship" and leaves those commits out,
and an agent whose branch is already in this one is not a collision. When
two branches change the same file and both have committed, it runs
`git merge-tree` and says whether they merge cleanly. The risk is computed
once for the same facts, so the Changes tab, Blast radius and Pull request
say the same thing. The pull request draft takes its title and summary
from the outcome, the agent's commits and what it said when it finished
(never Workbench's own brief), lists only this branch's decisions and the
tests it ran by name with their last result, and packs the capsule from
this branch's agents' sessions only. When branches stack, "Ship together"
drafts one pull request for the whole stack, in merge order.

Review and ship (`#/p/<project>/ship`) has three steps: what is ready,
check it, and merge or send back.

- **What is ready** lists the ready branches with their tests by name,
  whether each merges cleanly (into main and with the others) and the
  order to merge them in, with the reason.
- **Check it** shows one branch's blast radius:
  - a one-line verdict, then three rings: functions changed, callers
    reached and things users see;
  - only the exceptions;
  - then the pieces of work, riskiest first, with the mechanical changes
    folded.
- **Merge** merges into main's checkout with `--no-ff`. First a sheet says
  exactly what happens, and you can undo the merge while main has not
  moved on and nothing is pushed. Otherwise Undo gives the `git revert -m
  1` command.
- Merge waits for you in two cases:
  - when a decision replaces an earlier one, until you say it is still
    right;
  - when tests fail or the branch conflicts, until you type the branch's
    name.
- **Send back** resumes the agent with your note.
- **Open a pull request instead** opens the draft.

Debug (`#/debug`) follows one bad turn through four steps:

1. **Session and turn**: the session's turns, the ones that stand out, and
   **This went wrong** on each.
2. **What it knew**: what Workbench reminded it of, and the note it had but
   was not reminded of, with why ("58% match; notes under 60% are left
   out").
3. **Where it went wrong**: the turn's blast radius, worst first, and its
   diff.
4. **Fix**:
   - always remind agents editing a file of that note;
   - remind agents of more of your notes, which is a project-wide setting
     you can undo;
   - make it a rule.

   Sending the agent back is separate.

Debug has other ways in:

- `#/debug/code` asks Trace why the code on main is like this.
- `#/debug/failing` is Health, starting with what fails.
- `#/debug/records` lists what agents remember about the project:
  - who said each note (you or which agent) and on which branch;
  - **Correct**, **Always remind**, **Make it a rule** and **Forget**.

`--chloectx-bin` (`CHLOEDB_WORKBENCH_CHLOECTX`, default `chloectx`) names the
ChloeCtx program. The tests use a fake one that prints canned JSON.

## Map, Preview and Flow

Three of the levels in [SPEC.md](SPEC.md) ("Levels (third build)") have
their backend here. Each reads Workbench's rows, git and, when it is
installed, ChloeCtx:

- `GET /api/map?repo=`: the repository as territory. You get the tracked
  files sized by lines (folders to depth 3; in a big folder, small files
  fold into "…", which lists them so the map opens it),
  each agent's changes since `base`, and the files it read in the last 10
  minutes. It also lists collisions: `both_changed` when two agents
  changed one file, `heading_into` when one agent changed a file another
  just read. Pins are ChloeCtx's decisions, preferences and facts on the
  files they name, at a line when the record's anchor points at one.
- `PUT /api/agents/:id/preview {url}` (or the MCP tool `set_preview`, for
  an agent that started a dev server) takes `http://127.0.0.1:<port>` or
  `http://localhost:<port>` only. `/preview/<agent>/` then shows that app
  through Workbench with an overlay: hover outlines an element, and a
  click sends it to the Workbench page instead of the app.
  `POST /api/preview/route` finds the agent whose diff made the picked
  element. With the overlay turned off ("overlay off: the page is yours")
  every click and key goes to the app itself. Open the preview once with `?token=`; a cookie scoped to
  `/preview/` carries the token after that. Absolute links in HTML are
  rewritten to stay in the preview. An app that builds URLs in scripts
  should run under the base path `/preview/<agent>/` and set its preview
  URL with that path. The previewed page runs in Workbench's origin, so
  preview only apps you trust as you trust the agents' code.
- An agent in a worktree Workbench made edits and reads files there, runs
  the tests, uses git locally (not push) and Workbench's and ChloeCtx's
  tools without asking (`permissions.allow` in its session's settings).
  Guarded commands and anything outside its worktree still ask. Tick "Ask
  before edits" in New agent (`ask_edits: true`) to have it ask for each.
- In a zoomed terminal every key goes to the agent, and typing goes there
  even when the focus has left it. Ctrl-Esc leaves it (Esc is the agent's:
  Claude Code interrupts with it).
- `GET /api/flow?repo=&minutes=60`: where the last hour went, per agent,
  as model, tools, tests, waiting on you, redo (a call that failed with an
  error seen before) and idle. It adds the tokens from the sessions'
  transcripts and suggestions from a coach. `POST /api/flow/apply {id,
  repo}` applies one. An "allow" adds a Bash prefix you approved 3 or more
  times to `permissions.allow` for agents started from then on, in that
  repository only. A "fix" (with `text`) saves the fix for a repeated
  error through `chloectx remember`. `chloectx report --json --days 1`
  comes along as `report`.

The state database also has a `marks` table (small key/value rows: the
allow-lists, applied suggestions) and `agents.preview_url`.

## Restart, servers and read-only agents

- **Restart.** An agent that ended (it crashed, you stopped it, the server
  restarted) has Restart where Stop was, in its zoom. It comes back with the
  same name, task and worktree. When its session's transcript is still
  there, Claude Code continues that session (`--resume <old> --fork-session`);
  otherwise it starts on its task again. A view of the old agent (the zoom,
  Preview, Ship) moves to the new one.
- **Servers.** A dev server an agent starts in the background is listed
  under Servers, in the agent's zoom and in Preview: its port, command and
  age, with Stop. Stopping the agent asks whether its servers stop too
  (tick Remember to stop asking).
- **Read-only.** Tick Read-only in New agent, or take "Start it read-only"
  when the task says not to change anything. The agent gets no Edit, Write,
  MultiEdit or NotebookEdit, and Bash runs only commands that read (`cat`,
  `rg`, `git log`, `ls`…). Anything else is refused and can come to you as
  a change request. An agent a read-only agent starts is read-only too.
- **Guarded paths.** On the Decide screen, the guarded list takes paths
  beside commands (`money.py`, `migrations/**`, read as `.gitignore` reads
  them). Agents may read those files but not edit them, nor change them
  from Bash.
- **The repository box** in New agent completes paths as you type, git
  repositories first, and wraps so the whole path shows.

## The levels: goals, questions, story, away

The third build reads the fleet at higher levels than terminals. The
backend keeps new tables (`goals`, `outcomes`, `questions`, `rules`,
`deliveries`, `seen`) and gives each agent a `goal`, an `outcome` and a
`narration`: one live sentence of what it does, made after each
`PostToolUse`, `Stop` and `UserPromptSubmit` from the tail of its session's
transcript (at most its last 256 KiB, off the request path). Nothing calls
a model.

- **Goals**: `POST /api/goals {repo, text, plan: true}` makes a goal and
  starts a lead agent that breaks it into outcomes (`add_outcome`), starts
  an agent per outcome (`spawn_agent` with `outcome`) and marks them done
  (`update_outcome`). An outcome's state follows its agent's.
  Without a planner: `outcomes: ["…", {title, detail, task}]` starts an
  agent per outcome, and `agents: [ids]` makes agents already running
  outcomes of the goal; `model` and `effort` (`low` to `max`, Claude
  Code's `--effort`) are the lead's or those agents'.
  `plan: "draft"` returns the lead's plan (its parts, each with how you
  will know it is done, the parts it waits for and its model) and starts
  nothing: New project shows it before you start it.
  `GET /api/goals/:id` adds the goal's agents, open questions, what
  ChloeCtx learned in the last day (`chloectx sql --json`) and its pulse.
- **Questions and rules**: the MCP tool `ask_human` asks you in Workbench
  and waits for the answer; a permission prompt becomes a question too.
  Workbench reads the prompt off the agent's screen: its card shows the
  prompt's own options (3 or 4 of them, "switch to auto mode" marked as
  granting more) and the whole command, or the edit's diff. Answering
  moves the cursor to the option you chose, checks it there, and presses
  Enter, only while that same prompt is on the screen; "No" always
  chooses the prompt's No (or Esc). A prompt that left the screen (Esc in
  the terminal, say) types nothing and closes its card, saying why. A
  prompt no Notification announced (the trust prompt, "Do you want to
  create …?", a dialog) is a question too. Each question carries what
  `chloectx ask --json` found about it. An answer can become a rule
  (`chloectx remember decision`), which every agent of the repository is
  told, and which answers the same question when it is asked again.
  `POST /api/rules {repo, text}` adds one without a question,
  `PATCH /api/rules/:id` turns one on or off, `DELETE /api/rules/:id`
  removes it.
- **Delivery**: `chloedb-workbench hook` prints what the server answers for
  `SessionStart`, `UserPromptSubmit` and `PostToolUse` as
  `hookSpecificOutput.additionalContext`: rules, steers
  (`POST /api/agents/:id/steer`) and an agent's outcome, each told once.
  It still never fails a hook. A steer to an agent its screen shows at its
  text prompt (idle, or interrupted with Esc) is typed in; the answer's
  `message` says what became of it, and Story shows it.
- **Story**: `GET /api/agents/:id/story` reads an agent's turns as
  chapters of grouped steps, with its tokens (from the transcript's
  `usage`) and what ChloeCtx put in its context (`chloectx receipts
  --json`), déjà vus marked.
- **Away**: `POST /api/seen` and `GET /api/digest` tell what happened since
  you last looked: outcomes done, questions decided and open, what was
  learned, a line per agent, how long each agent waited on you, and the
  moments of decisions to replay. The first time, it counts from when your
  first agent started. While the page is open and used, it posts
  `POST /api/here` once a minute, so time at your desk is not called
  "away". "I'm stepping away" (`POST /api/away/leave`) marks the moment
  and lists the agents likely to need you while you are gone; it answers
  nothing for you.
- **Rules**: More › Rules suggests a rule when you approved the same kind
  of command again and again, and always the narrowest one its approvals
  support (`Bash(python3 -m pytest:*)`, never `Bash(python3:*)`). Each
  suggestion lists every command it would have allowed in the last 7 or
  30 days, risky ones first (migrations, databases, network, deletes,
  root, writes outside the worktree), with the prompts and the waiting it
  would have saved. Nothing is written until you confirm the exact rule;
  it goes to the session settings of agents started afterwards in that
  repository, never to `~/.claude` or the project's `.claude` files. The
  same page lists every grant (Workbench's, the "don't ask again" lines
  Claude Code wrote, session-wide answers) with who, when, where and how
  often it was used, and Revoke. A question option that grants more than
  one call opens a check first: the rule, the file, what else it would
  allow, and a narrower choice.
- **Notifications**: the bell in the top bar. Per project: every question,
  only an agent blocked for over N minutes (2 by default), or only alerts.
  Quiet (25 minutes) holds everything but critical alerts, and a digest
  sums up what happened when it ends or when you come back after 5
  minutes away. Desktop notifications start only after you turn on
  "Notify me"; they show the command and Claude Code's options, answer
  nothing, and a click opens the question's card to answer with its digit
  and Enter.

SPEC.md, "The levels' backend as built", has the JSON shapes;
JOURNEYS.md, "Rules and notifications", has the routes of the last two.

## What Health's numbers count

- **Tests by name.** Workbench records every test of every run the agents
  make when the runner names them: cargo test, jest, vitest, go test -v,
  pytest with `-v` or `-rA`, and pytest's junit report. Each session gets
  `PYTEST_ADDOPTS=--junitxml=<session dir>/pytest-junit.xml`, added to your
  own value; it is left out when the repository's pytest config already
  writes a junit report. A command that prints only a summary ("12 passed")
  or only an exit code is counted as a command and named as such; no test
  is made up from it.
- **Flaky** means passed and failed on the same commit. A failure the same
  agent fixed in its session (it failed, the agent changed the code, it
  passed) is not counted against the pass rate and is shown as "fixed in
  this session".
- **Test time** is the runner's own. The time a command waited for your
  permission is never counted as test time.
- **Run it N times**, in a test's history, has Workbench run that one test
  N times in the checkout of the agent that last ran it (pytest, cargo and
  go tests; no shell, 5 minutes a run at most) and shows how many passed.
  "Stable since <commit>: N runs, 0 failures" says how long it has passed.
- **Test with a persona**, at the foot of Tests, opens Preview's persona
  test on an agent's running app.
- **Hotspots** count from Workbench's rows and from `git log` (every
  commit, fix commits named as such), and name the branch on each piece of
  evidence.
- **Model routing**, in the New agent dialog, suggests a model only from 5
  similar past tasks in the repository; with fewer it says there is not
  enough history.

## Tests

```bash
cargo test --profile ci
```

The tests use a fake `claude`, a shell script that runs the hooks of the
settings file it is given, so they call no model.

## Not done yet

- Agents do not outlive the server. Stopping it ends them, and on the next
  start the agents left behind are marked `exited`. Restart brings each
  back in its session, one at a time; nothing restarts them all.
- Worktrees and `wb/*` branches are kept when an agent ends. A new agent
  with the same name picks up its worktree again. Nothing removes them.
- Claude Code's trust prompt is answered for you in a worktree Workbench
  made (the agent's events say so); elsewhere it is a question in Needs
  you ("Trust this folder?"). Its other first-run questions (the theme)
  appear in the terminal: answer them there.
- The preview proxies GET, HEAD and POST but not WebSockets, so a dev
  server's hot reload does not reach the page through it.
- Not in this build: the board, capsules on handoff,
  change requests for shell actions, a desktop shell (Tauri) and team mode.
