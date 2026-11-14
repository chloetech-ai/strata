# Optimize the harness

A design for a new goal of Workbench, next to Pick up, Review, Find out
why, Work and New: **Optimize the harness** (key `H`). Nothing in this
document is built yet. It is the contract for a first build, as
[SPEC.md](SPEC.md) is for the builds before it. The mocks are in
`ui/dev/harness.html` (serve `chloedb-workbench/ui` with any static
server, for example `python3 -m http.server -d chloedb-workbench/ui`, and
open `/dev/harness.html`; ES modules do not load from `file://`), drawn in style direction C, Instrument. The screenshots are listed at the
end.

In one paragraph: Workbench already keeps every session as rows in ChloeDB:
each tool call, permission prompt, test run, steer, answer, token count and
ChloeCtx recall. This goal reads those rows to find where the harness, not
the model or the task, cost the user time or quality. It then proposes
changes to the harness, each with its evidence. It plays the same sessions
again, from the same commit, under the old and the new harness, and keeps
only the changes that win on sessions they were not proposed from. Around
that loop it gives the harness an IDE: a structured editor for each part,
autocomplete and lint grounded in what agents did, the impact of an edit
before it is saved, what the model saw at any turn, evals as rows,
versions, and promote and roll back.

## Why

A harness is everything around the model that decides what an agent can
do and what it knows. For Claude Code that means CLAUDE.md, permission
rules, hooks, MCP servers, skills, subagents, the model and effort, and the
memory ChloeCtx gives it. Today it is a dozen text files in four places.
Nobody can say whether a change to it helped. The user edits a rule, feels
that the next session went better, and moves on. The cost of a bad harness
is real but scattered:

- **Permission storms.** A rule allows `Bash(python3 -m pytest:*)`, but
  agents type `PYTHONPATH=. python3 -m pytest`. The rule never matches, and
  the user approves the same command 22 times in a week. Flow's coach
  already counts these for one agent; nobody counts them across sessions,
  or across versions of the rule.
- **Memory missed at the turn it mattered.** ChloeCtx had "Shipping is not
  taxed", but at freeship's turn 2 the record scored 0.58, under the gate's
  0.60, and was left out. Two tests failed after it. The receipt proves it:
  replayed on its snapshot, the same query gives the same 0.58.
- **Redo loops.** cents ran `test_money.py` six times on one float error.
  ChloeCtx had the fix from totals, three days earlier.
- **Context nobody uses.** playwright's 18 tool definitions cost 6.1k
  tokens every turn of every agent of shop, and no agent called one in 14
  days.
- **Slow hooks.** ChloeCtx's UserPromptSubmit hook takes 1.9 s at p95,
  because the daemon was asleep and its embedder loads cold.

Each of these can be measured from rows Workbench already has. Each can be
fixed by a change to the harness. And whether the fix worked can be
measured too, by playing the sessions again. That is the goal: the harness
gets better from the work itself, and the user decides what is kept.

The user asked for it in these words: "make it more advanced than just
debug, make it an optimize feature that can take a look at the current
session or past using ChloeDB, and reproduce the steps using a new version
and improve the performance... self improving / auto learn / optimize
harness. Also, harness is a very text driven setup with configs and
scripts, can we make this more of an IDE?"

## The harness: what it is made of

A **part** is one piece of the harness that one editor edits. The harness
of a repository is the set of its parts at one version.

| part | what it holds | where it lives today | how Workbench applies a version |
|---|---|---|---|
| `instructions` | CLAUDE.md files, Workbench's rules (`rules` rows), ChloeCtx's distilled rules | `~/.claude/CLAUDE.md`, `<repo>/CLAUDE.md`, `rules`, the ChloeCtx store | the repository's CLAUDE.md through Review (a change to the repository); rules as the SessionStart context Workbench already gives (`levels.rs`) |
| `prompts` | the system prompt Workbench appends, the brief of a spawned agent, the steer and answer templates | `supervisor.rs`, `mcp.rs` | `--append-system-prompt` and the first prompt, per session |
| `permissions` | allow, ask and deny rules, the default mode, additional directories | `~/.claude/settings.json`, `<repo>/.claude/settings*.json`, `flow::worktree_allow`, `guarded.rs` | the per-session `--settings` file Workbench writes for every agent (`settings_json`, `flow::with_allowed`) |
| `hooks` | each event's commands, matchers and timeouts | the same settings files, and Workbench's own and ChloeCtx's | the same per-session `--settings` file |
| `tools` | MCP servers and their tools, skills, subagents | `~/.claude.json`, `.mcp.json`, `.claude/skills`, `.claude/agents` | `--mcp-config` with `--strict-mcp-config`, and the list of skills and subagents in the worktree |
| `routing` | which model and effort for which kind of task | `routing.rs` (learned from `task_features`) | `--model` and `--effort` at spawn |
| `memory` | ChloeCtx's recall gate, pack budget, briefing size, knowledge thresholds, pins | ChloeCtx's defaults (`Gate`, `PackParams`, `KnowledgeParams`) | `CHLOECTX_*` settings in the session's environment, through `ctxwire::wire_as` |

The last column is the important one. **A harness version is applied per
session, by Workbench, without touching the user's files.** Workbench
already writes its own `--settings`, `--mcp-config` and environment for
every agent it starts. A version changes what those contain. The user's
`~/.claude/settings.json`, `~/.claude.json` and the repository's
`.mcp.json` are never edited by this feature (AGENTS.md says they are only
edited by `chloectx install` and the user). Only `instructions` that live
in the repository (its CLAUDE.md) and skills or subagents in `.claude/`
are files of the repository. A change to those goes through Review as a
diff, like any other change.

So two agents can run two versions side by side, and a version is
something to compare, not a state of the machine.

## Concepts

- **Version.** An immutable set of parts, named like `1.4` and identified
  by a hash of its parts' canonical text (`a3f9c21`). The same harness has
  the same hash. A version has one parent (or two, after a merge of
  branches). It can be `draft` (being edited), `candidate` (proposed by an
  optimize run), `current` (what new agents of the repository get) or
  `retired`.
- **Branch.** A line of drafts, like `1.4-hooks`, for an experiment that is
  not ready.
- **Finding.** Something in the sessions where the harness cost time or
  quality: a permission storm, a missed memory, a redo loop, a correction
  by the user, unused context, a slow hook. It has a kind, the sessions it
  was seen in, what it cost (minutes, tokens, failed checks) and an
  evidence level.
- **Suggestion.** A change to one or more parts, made to fix findings. It
  has a diff, the findings it answers, its expected impact, whether it
  widens permissions, and once tested, a verdict.
- **Evidence level.** How a number was obtained. The screens show it next
  to every number that matters, and nothing is shown without one.

  | level | how | cost | example |
  |---|---|---|---|
  | `counted` | exact, from rows, by evaluating the new harness against what happened | none | "−18 of 22 prompts": the 22 commands are rows, and the new rule is matched against each |
  | `recomputed` | exact, by replaying ChloeCtx's receipts on their snapshots with new parameters | seconds, no model | "+5 records in 4 sessions": `receipt::replay` with the pin |
  | `replayed` | estimated, by playing sessions again N times under each version | dollars | "8 of 10 sessions pass, interval…" |
  | `judged` | a model's opinion, against a rubric | cents | "the persona could pay" |

  Counted and recomputed evidence needs no model and no replay. It is the
  cheapest, the fastest and the most trustworthy, so the loop tries it
  first. Many harness changes (permissions, hooks' time, unused tools,
  recall settings) can be measured exactly this way.
- **Case.** A session made reproducible: its starting commit, the user's
  prompts and answers, and its **checks**, which say whether a run of it
  worked. A case is a row; "Make this a test" makes one from any session.
- **Suite.** A named set of cases, like `shop core`. A suite has a train
  part and a held-out part.
- **Run.** One play of one case under one version. An **optimize run** is
  many runs: every case of a suite, N times, under A (the current version)
  and B (the candidate).
- **pass^k.** A case passes at k only when all k runs pass (from τ-bench).
  pass³ is the default. One lucky run does not count.
- **Noise floor.** How much a metric moves between runs of the same
  version on the same case (A against A). A difference smaller than that
  is noise, whatever its sign.

## Screens

Each screen below has a mock. They share the toolbar, a header with the
goal's tabs (Overview, Optimize, Editor, What the model sees, Evals,
Versions) and the inspector on the right. The inspector holds the thing to
decide, and its dock holds the action, with its key.

1. **Overview** (`#/harness`, shot 01). The week's scorecard as six gauges,
   live from the rows: sessions done by their checks, interventions a
   session, permission prompts, wall time, tokens, memory missed. Below
   them, where the harness cost you, worst first: one row per finding, with
   its sessions, its cost and its evidence level. Below that, the agents
   grouped by the version they ran. The inspector holds the suggestions,
   each a card with its diff, why, its counted or recomputed impact and its
   verdict once tested. It also holds the auto-learn mode and the dock
   action "Test 3 changes on 10 sessions".
2. **Optimize run** (`#/harness/run/7`, shot 02). The loop as a stepper
   (Find, Propose, Reproduce, Score, Keep). Four gauges: runs, money spent
   against the cap, time, and the noise floor. The sessions table, one row a
   session, with its set (train or held out) and a cell per run of A and of
   B (pass, fail, running, queued, skipped). Scores so far, with "too early"
   where they are. The inspector shows the replay in progress as a log
   (the played prompt, the recall it got, its tool calls), the sandbox's
   guarantees, and what happened that was not in the log.
3. **Results** (`#/harness/run/7/results`, shot 03). The scorecard of A
   against B, with paired intervals and a verdict on each metric: better,
   inside the noise, or worse. The **divergence** of one session's runs:
   A's and B's turns as two tracks on one time axis, a line at the first
   turn where they differ, what differed in the context at that turn (the
   pinned record, given in B, left out in A, and everything else the same to
   the token) and what each did with it (the two hunks of `cart.py`). The
   inspector holds each change as a checkbox, measured alone, "Honest
   about" (what the numbers do not say), and "Make 1.5 from 2 changes".
4. **Editor: Permissions** (`#/harness/edit/permissions`, shot 04). Rules as
   a table: the decision, the pattern, where it comes from, its hits in 14
   days and lint flags. A new rule being typed, with autocomplete from the
   commands agents ran (22 prompts for `PYTHONPATH=. python3 -m pytest`).
   The raw JSON beside it, in sync, with the lint as squiggles. The inspector
   holds the lint and the counted impact: 22 prompts become 4, no deny
   changes, and it widens, which needs the user.
5. **Editor: Hooks** (`#/harness/edit/hooks`, shot 05). One turn as a
   waterfall: each hook on the time axis, with the waits it causes. Every
   hook with its p50 and p95, its runs, its failures and the tokens it adds.
   The inspector shows the slow one with its last runs.
6. **Editor: Memory** (`#/harness/edit/memory`, shot 06). ChloeCtx's recall
   settings as scales with the current value and the old one marked, the
   pins, and a live preview of one turn's recall (freeship turn 2) under
   the edit. Below that, every recall of the week recomputed on its
   snapshot: how many records change, and which misses are fixed.
7. **Editor: Tools and routing** (`#/harness/edit/tools`, shot 07). MCP
   servers, skills and subagents, each with its tools, its tokens a turn
   and its calls in 14 days; playwright, unused, is flagged. The routing
   table: kinds of tasks, the model and effort for each, how often they
   finished and their cost, from `task_features`.
8. **What the model sees** (`#/harness/sees`, shot 08). The context of one
   turn, put together part by part: Claude Code's prompt, the tool
   definitions, CLAUDE.md, Workbench's rules, ChloeCtx's briefing, the task,
   the recall, and the conversation so far. Each part has its tokens, its
   share and its source, as a stacked bar under 1.4 and under the
   candidate. Each part says whether it was measured or estimated. Below
   that is the text of any part, as a diff between versions.
9. **Evals** (`#/harness/evals`, shot 09). Suites as tabs, cases as rows:
   where each was made from, its checks, its pass³ on each version, its
   cost. Then the runs of the suite. The inspector is "Make this a test"
   for a session: where it starts, what it plays, the proposed checks to
   tick, and its set.
10. **Versions** (`#/harness/versions`, shot 10). The history as a graph:
    current, candidate, drafts and branches, each with who made it, its
    suite result and the agents that ran it. Below that, the diff of two
    versions by part, each with its impact and evidence. The inspector holds
    Promote, Roll back and Branch.
11. **Where it plugs in** (`#/`, shot 11). Home with the sixth goal, the
    harness chip on Continue, the agents table with the harness each ran,
    and Find out why's Fix ("Pin the record to cart.py", "Make it a rule")
    leading into the harness as a draft.

On the phone (shots 20 to 23) the goal is reduced to what is done away
from a desk. The first screen is the suggestions to approve, after the
run's three main gauges: success, prompts, held-out regressions. Each
suggestion has its evidence and impact, with Keep, See the evidence and Not
now, and a widening one says so at the top. The second screen is the
scorecard as gauges, two to a row, with the findings one a row. Editing,
the divergence and evals are desk work and are not on the phone.

## The optimize loop

```
 Find ──► Propose ──► Reproduce ──► Score ──► Keep
  ▲          │  counted / recomputed: skip Reproduce   │
  └──────────┴──────────── next run ◄──────────────────┘
```

### 1. Find

Detectors are SQL over the tables Workbench has, run per repository over a
window (7 days by default) and written to `harness_findings`. Each detector
says what a finding cost, in the unit it can measure.

| kind | from | cost |
|---|---|---|
| permission storm | `events` of kind `permission` grouped by the command's simple form (`flow::simple_commands` strips `VAR=` prefixes), joined to what the user answered | prompts × the median time to answer |
| missed memory | ChloeCtx receipts whose skipped rows include a record later cited in a correction, or a record whose words match the failing test's file (a `decision` pinned or naming the file) | the turns and time from that turn to the fix |
| redo loop | `test_runs` with the same failing test and the same error signature 3 or more times in one session, where ChloeCtx's `fixes` had a match | the time of the loop |
| corrected by you | steers and answers (`events`, `questions`) whose text says the same thing to 2 or more agents | the count, and the time each agent waited |
| context unused | tool definitions never called (`events.tool` against the session's tool list) and briefing records never cited (receipts against the transcript) | tokens × turns |
| slow hook | hook timings (a new `hook_runs` row per run, see the data model) over a p95 threshold | the wait, summed |
| wrong model | `task_features` where a cheaper model finished similar tasks as often (`routing.rs` already ranks them) | dollars |
| denied then allowed | a deny rule the user overrode by hand, often | the count |

A finding links the rows it was made from. Every number on the Overview
opens the rows behind it.

### 2. Propose

Two proposers, used in this order:

- **Rules.** For findings with a known fix, a template makes the
  suggestion: an allow rule for a command's prefix as agents type it (and
  the narrower alternative, `pythonpath = .` in `pytest.ini`, as a change
  to the repository); a pin for a decision that names a file; leaving out a
  server nobody called; a longer timeout or a warm-up for a slow hook; the
  routing table's cheaper model. These are cheap, and most have counted or
  recomputed evidence before any replay.
- **A reflective proposer**, after GEPA. A model (`claude -p`, no tools,
  as ChloeCtx's distiller runs) is given the traces of the failing and
  slow train sessions, condensed: the turn, what the model saw, what it
  did, the check that failed, the user's correction. It is also given the
  parts of the harness and asked for changes to them in a structured diff,
  each with the findings it answers. It never sees the held-out sessions.
  Its proposals are new candidates on a **Pareto front**: a candidate is
  kept for the next round when it is the best on at least one train
  session, not only on the average, so a change that fixes one kind of
  session is not lost to one that is average on all. This is GEPA's
  selection, and what keeps the front diverse.

Every suggestion is checked before it is tested:

- **Overfit lint.** The suggestion is rejected if it names a string only
  one session had: a test name, a file path only one task touched, an
  identifier from one diff, or the text of one prompt. Instructions must
  be about the kind of work, not the instance. "Shipping is not taxed" is a
  decision of the user, and is pinned as one; "in test_total_shown, use
  subtotal()" is the answer to one case, and is rejected.
- **Support.** A suggestion must answer findings in 2 or more train
  sessions, unless the evidence is counted.
- **Permission lattice.** The suggestion's permissions are compared with
  the current version's, over every command and path agents used in the
  window and the rule's own patterns. If any of them goes from deny or ask
  to allow, the suggestion **widens**, and it is marked so everywhere.
  See Safety.
- **Size.** Instructions are limited to a token budget. A suggestion that
  adds tokens to every turn says how many.

### 3. Reproduce

See [Reproduce and replay](#reproduce-and-replay). A suggestion whose
evidence is counted or recomputed and whose effect is entirely in that
measure skips this step. A permission rule's only effect is on prompts,
and its prompts are counted. A change that can change what the model does
is replayed: instructions, prompts, memory that adds or removes records,
tools, routing.

### 4. Score

Each run is scored by its case's checks and the scorecard's metrics.
Then A and B are compared case by case, with the statistics in
[The scorecard](#the-scorecard).

### 5. Keep

A change is kept only when, measured alone (each change of a candidate is
also run on and off, so a winning change does not carry a useless one):

1. no case that passed under A fails under B, on train or held out;
2. no metric is worse beyond its noise floor;
3. at least one metric is better beyond its noise floor, on train **and**
   not worse on held out;
4. it does not widen permissions, or the user approves the widening when
   it is kept, not only when it was tested.

What is kept becomes a candidate version (`1.5-rc`). Promoting it makes it
current for new agents; agents already running keep the version they
started with, and record it.

## The scorecard

One table, the same for the week on the Overview and for A against B on
the results. Success is the metric to improve. Held-out regressions and
widening are gates. The rest are guardrails.

| metric | definition | from | better | role |
|---|---|---|---|---|
| **Success** | sessions whose checks pass, pass³ for replays | checks on the run's final tree; for live sessions, the outcome and the last `test_runs` | higher | primary |
| **Held-out regressions** | held-out cases that pass under A and fail under B | runs | 0 | gate: any is a reject |
| **Widens permissions** | any command or path that goes to allow | the permission lattice | no | gate: needs the user |
| Interventions | prompts + answers + steers + corrections, a session | `events`, `questions` | lower | guardrail |
| Permission prompts | a session | `events` | lower | guardrail |
| Wall time | median, from the task to the checks passing | `agents`, `events` | lower | guardrail |
| Turns | a session | transcript | lower | guardrail |
| Tokens | input + output, a session | `token_use` | lower | guardrail |
| Cost | dollars a session | `task_features.cost_usd` | lower | guardrail |
| Tests passing | at the end of the session, of those that ran | `test_runs` | higher | guardrail |
| Memory missed | records left out at a turn and needed after it | receipts, findings | lower | guardrail |

Each metric on the results gets:

- **Δ**, B − A, as the median of the per-case differences (paired by
  case: each case is its own control, which removes most of the variance
  between tasks);
- **an interval**, 90% by paired bootstrap over cases (10,000 resamples);
  for success, an exact interval on the paired counts (McNemar's
  discordant pairs);
- **a verdict**: *better* when the interval excludes 0 in the good
  direction and |Δ| exceeds the noise floor; *worse* when it excludes 0 in
  the bad direction; otherwise *inside the noise*, said in those words.

The **noise floor** comes from the N runs of A on each case: the median of
the per-case spread of A against A. It is measured on every optimize run,
because it changes with the model and with Claude Code. A result smaller
than the floor is not reported as a win, however good the interval looks.

When the result is inside the noise and the change is promising, the
results offer "Run 5 more a session" with its price. The interval is
recomputed with the new runs; there is no peeking penalty to pay, because
the decision rule is fixed before (a sequential test with an alpha spent
per look would be stricter; the first build keeps the fixed rule and says
so).

## Reproduce and replay

To play a session again is to start an agent like the original: same
repository and commit, same task, same answers. Only the harness differs.

### Where it starts

- **The commit.** `agents.base` is where its worktree started. If the
  session began on uncommitted changes, the `time_snapshots` row before it
  holds them as a tree (`heads[].tree`), a commit no branch names. The
  replay checks that tree out.
- **The worktree.** A new git worktree in `<data-dir>/replays/<run>/`,
  cloned from the repository **without a remote**, so nothing can be
  pushed, and with the hooks of the version under test.
- **Memory.** A copy of ChloeCtx's store for the repository as of the
  session's start (ChloeCtx's snapshots, `chloectx/src/snapshots.rs`, by
  the receipt's txn), opened by a separate daemon. A replay never writes
  the user's store. Records made after the session started are not in the
  copy, so the replay does not know what the user learned later. That is
  the leak that would make every replay look better than the original.
- **Services.** The session's dev server, if it had one, on a free port
  (Workbench's `preview.rs` already allocates them).

### Who plays the user

The user's side is scripted from the log, in order:

- **Prompts and steers** are played at the same points: the first prompt
  at the start, each later one when the agent stops (Stop), as the user
  sent them.
- **Answers** to `ask_human` and questions are matched to the original
  question by meaning: BM25 over the question's text, plus the options,
  with a threshold. The original answer is given.
- **Permission prompts** are answered as the user answered the same
  command (its simple form) in the original, or, if it never came up, as
  the user answered such commands in the last 30 days. If there is no
  precedent, the answer is *no*.
- **Something not in the log** (a question or command the original never
  had) is answered by precedent, counted as an intervention, and listed on
  the run's page under "Not in the log". It is never answered by a model
  pretending to be the user. That is a judged level, and a source of
  flattering results.

### The model and the tools

| | live | recorded |
|---|---|---|
| **model** | `claude` runs for real, under the version's model and effort | not offered: a replay with a recorded model replays nothing |
| **tools** | tool calls run in the sandbox | a **cassette**: each tool call of the original, keyed by tool and input, played back while the replay's calls match the original's |

The model is always live, because the model's behaviour under the new
harness is the thing measured. Tools are live by default. A cassette makes
a run cheaper and faster (no test suite, no server), and it is exact while
the run follows the original. At the first call that differs, the run
leaves the cassette and goes live for the rest. The run's page says from
which turn ("none matched the recording after turn 2").

Recall is a special case of recorded: ChloeCtx's `retrieve` is
deterministic, so a recall under the same parameters on the same snapshot
gives the same answer. A change to recall parameters alone is
**recomputed**, without a model.

### Non-determinism

- **N runs** per version per case, 3 by default; pass³ counts a case only
  when all 3 pass.
- **Temperature and seeds** cannot be fixed in Claude Code. The design
  does not pretend they can; the variance is measured instead.
- **The noise floor** (A against A) is measured on every optimize run.
- **Flaky checks.** A check is flaky when it fails on a commit where it
  also passes (`test_runs` already records that: `test_promo_window`
  fails 2 of 15 on the same tree). A flaky check counts as passed only on
  3 of 3, and the results say so.
- **Interleaving.** Runs of A and B alternate in time, so a slow hour for
  the API or the machine does not land on one version.

### Cost

Each optimize run has a cap (`$8.00` by default) and a cap per run
(`$0.60`). A run over its cap is stopped and counted as failed. A session
whose original cost more than the cap (checkout-v2, 186 turns, about $9.40
a run) is skipped. Its first turns can be a case of their own: "Make this a
test" can take turns 1 to 20, with checks on the state at turn 20. Before
starting, the run's page shows its estimate, from the originals'
`task_features.cost_usd` × N × 2 versions.

## Safety

Every replay runs in a sandbox, and every rule below is checked in code,
not by convention.

- **No pushes.** The replay's repository has no remote. `git push` is
  denied by the guarded rules (`guarded.rs`) as for any agent, and has
  nowhere to go.
- **No writes outside the worktree.** Edit and Write are allowed only in
  the replay's worktree (`flow::worktree_allow`, the same rules as a
  normal agent). The sandbox does not use the user's home settings: the
  replay gets `--settings` from the version under test, and `--strict-mcp-
  config` with the version's servers.
- **Memory is a copy.** See above. A replay's distiller is off.
- **A budget**, per optimize run and per run, enforced from `token_use` as
  the run goes.
- **Permissions are never widened without the user.** The permission
  lattice compares two versions over every command, path and tool seen in
  the window. A version that allows anything the current version asks about
  or denies **widens**. It is marked in the editor, on the suggestion, on
  the version and on the diff.
  - Testing it in replays needs the user's approval ("You approved trying
    it at 07:29"), because a replay runs the commands for real.
  - Keeping it needs the user's approval again, as a separate decision.
  - Auto-learn never applies it, in any mode.
  - Narrowing (taking a tool away, adding a deny, asking where it allowed)
    is not widening and can be applied within limits.
- **Hooks are code.** A suggestion never adds a hook command that is not
  already in the user's harness or Workbench's or ChloeCtx's own. It may
  change a hook's timeout, matcher or order, or leave one out.
- **Secrets.** The lint flags a token, key or password in any part, and an
  `env` that passes one through to every agent.
- **The user's files are not touched.** See "The harness: what it is made
  of". A change to the repository's CLAUDE.md or `.claude/` is a diff in
  Review.

## Generalization

The risk with optimizing a harness on the user's own sessions is that it
learns the sessions, not the work: an instruction that says "in
cart.py, shipping is untaxed" fixes freeship and nothing else. The design
guards against it in five ways.

1. **Held out by session.** One case in three goes to the held-out set,
   chosen by session, never by turn, so turns of a held-out session never
   inform a proposal. The proposer never sees held-out traces. The
   held-out set is reported separately, and any held-out regression rejects
   the change.
2. **The overfit lint**, in Propose: no string only one session had.
3. **Support**: a change answers findings in 2 or more sessions.
4. **Rotation.** The held-out set changes when it has been used for 5
   decisions, so it does not become a train set by selection. Cases made by
   hand (like `no-push-ever`) stay in every set.
5. **Decisions are the user's.** A rule the user stated ("Customers win:
   everything rounds half up") is a decision, kept in ChloeCtx with its
   author. A pin makes it reach the turn that needs it. The optimizer does
   not write facts about the code into instructions. It changes *when* and
   *how* the user's own knowledge is given, and *what* the agent may do.

## Auto-learn modes

| mode | what runs | what is applied | default |
|---|---|---|---|
| **Suggest** | Find and Propose, after each session ends; counted and recomputed evidence | nothing: suggestions wait on the Overview and the phone | yes |
| **Auto, within limits** | as Suggest, then a replay run when 3 suggestions wait, within a daily budget | changes that are counted or recomputed, narrow (never widen), win, and stay within limits: memory parameters within a band of the default, a tool left out, a hook's timeout. They go to a candidate, and are promoted after the next suite run passes. They roll back on their own if the next day's sessions are worse beyond the noise. | no |
| **Nightly** | the full loop at 02:00 on the window's sessions, within its budget | as Suggest: the morning has results to keep, not suggestions to test | no |

In every mode, the user can see each change, why, its evidence, and roll
it back in one action. A change the user dismissed is not proposed again
unless its evidence grows by half.

## The IDE

The second half of the user's request: the harness as something to build
and inspect, not a pile of text.

### One editor per part, and the raw text

Each part has a structured editor, shown in the mocks:

- a **permission builder**: rules as rows, each with its hits and lint;
- a **hook timeline**: hooks on a turn's time axis, with their latencies;
- a **tool palette**: servers, skills and subagents, with what each costs
  and its use;
- a **routing table**: kinds of tasks, with model and effort;
- **recall settings** as scales with a live preview.

**Two-way sync with the text.** A part's canonical form is its row in
`harness_parts`, as JSON. The structured editor and the text editor both
edit it. The text editor shows the part as the file it becomes
(`settings.json`'s `permissions`, a CLAUDE.md, a hook script's command
line), and parses it back on each change. Unknown keys and comments are
kept: a part keeps its text verbatim, and a structured edit is applied to
the text as a minimal edit (a JSON patch on a parsed tree that keeps
positions), so the user's formatting survives. If the text does not parse,
the structured editor shows the last good state and the error, and does not
overwrite the text.

### Autocomplete grounded in history

Autocomplete (the fifth build's `complete_items`) gets kinds for the
harness:

- **commands** agents ran, by simple form, with how often they were
  prompted for them and what the user answered ("asked 22 times, allowed
  22");
- **paths** agents read and wrote;
- **tools** and MCP tools they called;
- **matchers** for hooks, from the tools that ran;
- **decisions** in ChloeCtx, for pins;
- **models** with their success on similar tasks.

Each completion says where it came from and its impact if taken ("−18 of
22 prompts").

### Lint

| rule | example |
|---|---|
| unreachable | `Bash(python3 -m pytest:*)` never matched in 14 days; agents type `PYTHONPATH=. python3 -m pytest` |
| conflict | an allow and a deny that match the same command; which wins, and from which file |
| shadowed | a rule another rule already covers |
| wider than needed | `Bash(python3:*)` where agents only ran `python3 -m pytest` |
| widens | the rule allows something the current version asks about |
| slow hook | p95 over 1 s on a hook that blocks the prompt |
| secret | a token or key in a part |
| unused | a tool, server, skill or subagent with no calls in 14 days, and its tokens |
| too long | instructions over their token budget |
| overfit | an instruction naming something only one session had |

### Impact preview

Every edit shows what it would have changed in the window, before it is
saved, at the best evidence level it can get:

- **counted** for permissions (prompts saved, denies changed, by agent);
- **counted** for tools left out (tokens a turn, calls that would fail);
- **recomputed** for recall (records added or dropped at each turn of the
  week, and which misses that fixes);
- **counted** for hooks' timeouts (runs that would have timed out).

What cannot be counted (an instruction's wording) says "needs a replay"
and offers one.

### What the model sees

The context at any turn of any session, as parts with tokens. Parts that
come from Workbench or ChloeCtx are **measured**: their receipts hold the
text the hooks returned, counted with the model's tokenizer (the count
tokens API). The conversation is **from the transcript**. Claude Code's own
prompt is **estimated** as what is left of the turn's input tokens in its
usage. The screen says which is which, and compares two versions at the
same turn.

### Versions, diffs and branches

Every save makes a draft version; the history is a graph. Each agent
records the version it ran (`agents.harness`), so any session can be
filtered or compared by version. The diff of two versions is shown by part,
each with its impact and evidence. Branches are for experiments, and merge
by part.

### Evals as rows

- **Make this a test**, from any session, is one action. It proposes
  checks from what happened: `fail→pass` for tests the session made pass
  (once the user has read a test an agent wrote), `pass→pass` for tests
  that passed at its start (after SWE-bench's FAIL_TO_PASS and
  PASS_TO_PASS), a persona check if the session had a preview (the swarm's
  runner, judged), limits ("at most 2 permission prompts"), and safety
  ("no git push ran").
- **Suites** group cases; each case has a set (train, held out).
- **Runs of the suite** on each version are kept, so the history of a
  case across versions is a row.
- **Compare** any two versions on a suite.

### The divergence view

For two runs of one case, the turns of each as tracks, the first turn
where they differ, and the difference in what the model saw there
(context diff, part by part) and in what it did (tool calls, diff hunks).
It is how a result is explained, not just reported.

### Promote and roll back

Promote makes a version current for new agents of the repository. Roll
back makes the previous current again; it is one action, from the
Versions page or a notification, and is recorded with its reason. Agents
running keep their version; "Restart under 1.5" is offered for those that
are idle.

## Where it plugs in

- **Find out why → Fix.** Debug's "Make it a rule" and "Pin the record"
  stop being one-off actions: they make a harness draft with the change,
  open it in the editor with its impact on the last 7 days, and offer to
  test it on the sessions that missed the same record.
- **Home and Continue.** The sixth goal, Optimize (`H`), shows the count of
  suggestions waiting. Continue shows a chip with "harness 1.4 · 3
  suggestions".
- **Agents.** Every agent records its version; the agents table has a
  harness column, and one that runs an older version says so.
- **Spawn.** `spawn_agent` and the New dialog use the current version;
  `harness` can name another, for an experiment.
- **The phone.** Suggestions to approve, the scorecard, and the results
  of a run. It never edits.

## Data model

New tables, in Workbench's ChloeDB (`db.rs`), with the conventions of the
existing ones (`KEYWORD` ids, `TIMESTAMP` times, JSON as `TEXT`).

```sql
-- One version of one repository's harness.
CREATE TABLE harness_versions (
  id KEYWORD PRIMARY KEY,      -- hash of the parts' canonical text, 7 hex shown
  repo KEYWORD,
  name KEYWORD,                -- "1.4", "1.5-rc", "1.4-hooks"
  parent KEYWORD, parent2 KEYWORD,
  branch KEYWORD,
  state KEYWORD,               -- draft | candidate | current | retired
  made_by KEYWORD,             -- you | optimize:<run> | fix:<agent>
  note TEXT,
  claude_version KEYWORD,      -- Claude Code's version when it was made current
  widens BOOL,
  approved_by KEYWORD, approved TIMESTAMP,
  created TIMESTAMP, promoted TIMESTAMP, retired TIMESTAMP
);

-- The parts of a version (a part unchanged between versions is shared by hash).
CREATE TABLE harness_parts (
  id KEYWORD PRIMARY KEY,      -- hash of kind + text
  kind KEYWORD,                -- instructions | prompts | permissions | hooks | tools | routing | memory
  text TEXT OPTIONS(indexed = false),   -- the canonical text, verbatim
  model TEXT OPTIONS(indexed = false),  -- the parsed form, as JSON
  tokens INT                   -- what it adds to every turn, when it does
);
CREATE TABLE harness_version_parts (id KEYWORD PRIMARY KEY, version KEYWORD, kind KEYWORD, part KEYWORD);

-- Where the harness cost time or quality.
CREATE TABLE harness_findings (
  id KEYWORD PRIMARY KEY, repo KEYWORD, kind KEYWORD, title TEXT, detail TEXT,
  sessions TEXT, agents TEXT,  -- JSON lists
  cost_s FLOAT, cost_tokens INT, cost_checks INT,
  level KEYWORD,               -- counted | recomputed | replayed | judged
  rows TEXT,                   -- the ids of the rows it was made from
  version KEYWORD,             -- the harness it was seen under
  state KEYWORD,               -- open | answered | dismissed
  first TIMESTAMP, last TIMESTAMP
);

-- A proposed change.
CREATE TABLE suggestions (
  id KEYWORD PRIMARY KEY, repo KEYWORD, base KEYWORD,  -- the version it changes
  title TEXT, why TEXT, diff TEXT,                     -- a structured diff by part
  kinds KEYWORD[], findings TEXT,
  proposer KEYWORD,            -- rule:<name> | reflect:<run>
  widens BOOL, overfit TEXT,   -- the lint's verdicts
  impact TEXT, level KEYWORD,  -- the expected impact and its evidence
  verdict KEYWORD,             -- untested | won | noise | lost
  run KEYWORD,
  state KEYWORD,               -- waiting | kept | dismissed | applied
  decided_by KEYWORD, decided TIMESTAMP, created TIMESTAMP
);

-- Evals.
CREATE TABLE eval_suites (id KEYWORD PRIMARY KEY, repo KEYWORD, name KEYWORD, note TEXT, created TIMESTAMP);
CREATE TABLE eval_cases (
  id KEYWORD PRIMARY KEY, suite KEYWORD, repo KEYWORD, name KEYWORD,
  agent KEYWORD,               -- the session it was made from, if any
  commit KEYWORD, tree KEYWORD,          -- where it starts
  turns_from INT, turns_to INT,
  script TEXT,                 -- prompts, steers and answers, in order, as JSON
  checks TEXT,                 -- JSON: [{kind: fail_to_pass|pass_to_pass|sql|http|persona|limit|never, ...}]
  "set" KEYWORD,               -- train | held_out
  cassette KEYWORD,            -- the recording of its tool calls, if kept
  cost_usd FLOAT, made_by KEYWORD, created TIMESTAMP
);

-- An optimize run (or a suite run): many runs of cases under versions.
CREATE TABLE optimize_runs (
  id KEYWORD PRIMARY KEY, repo KEYWORD, suite KEYWORD,
  a KEYWORD, b KEYWORD,        -- the versions
  n INT, cap_usd FLOAT, spent_usd FLOAT,
  state KEYWORD,               -- queued | running | scoring | done | stopped
  why KEYWORD,                 -- you | nightly | auto | promote
  scorecard TEXT,              -- the results table, as JSON
  noise TEXT,                  -- the A-against-A floor per metric
  started TIMESTAMP, ended TIMESTAMP
);
CREATE TABLE eval_runs (
  id KEYWORD PRIMARY KEY, run KEYWORD, "case" KEYWORD, version KEYWORD, k INT,
  agent KEYWORD,               -- the replay's agent row (agents, with harness = version)
  state KEYWORD, passed BOOL,
  checks TEXT,                 -- each check's result
  turns INT, prompts INT, interventions INT, tokens INT, cost_usd FLOAT, wall_ms INT,
  left_cassette_at INT,        -- the turn it went live, if it had a cassette
  not_in_log TEXT,             -- moments answered by precedent
  started TIMESTAMP, ended TIMESTAMP
);

-- A run's turns, for the divergence and What the model sees.
CREATE TABLE run_turns (
  id KEYWORD PRIMARY KEY, agent KEYWORD, version KEYWORD, turn INT,
  summary TEXT,
  parts TEXT,                  -- JSON: [{part, source, tokens, level}]
  context_hash KEYWORD,        -- equal hashes mean the same context, to the token
  receipt KEYWORD,             -- ChloeCtx's receipt for the turn's recall
  tool_calls INT, edits TEXT, tests TEXT, ts TIMESTAMP
);

-- Tool calls of an original session, keyed for play back.
CREATE TABLE cassette_calls (
  id KEYWORD PRIMARY KEY, cassette KEYWORD, seq INT, tool KEYWORD,
  input_key KEYWORD,           -- hash of the tool's normalized input
  input TEXT OPTIONS(indexed = false), output TEXT OPTIONS(indexed = false),
  ms INT
);

-- Each hook's run, for the hook timeline and the slow-hook finding.
CREATE TABLE hook_runs (
  id KEYWORD PRIMARY KEY, agent KEYWORD, version KEYWORD, event KEYWORD,
  source KEYWORD,              -- workbench | chloectx | yours:<file>
  command TEXT, ms INT, exit_code INT, tokens_added INT, ts TIMESTAMP
);
```

And one column on an existing table:

```sql
ALTER TABLE agents ADD COLUMN IF NOT EXISTS harness KEYWORD;  -- the version it ran
```

Replays are agents too, rows of `agents` with a `replay` parent, so every
view that works on agents (Story, Trace, Rewind, What the model sees)
works on a replay without new code.

## HTTP API

Same conventions as SPEC.md: JSON, errors as `{error, error_code}`.

| route | body | answer |
|---|---|---|
| `GET /api/harness?repo=` | | `{current, candidate, drafts, scorecard, findings, suggestions, mode}` for the Overview |
| `GET /api/harness/versions?repo=` | | `{versions: [Version]}` with parents, for the graph |
| `GET /api/harness/versions/:id` | | `{version, parts: [Part]}` |
| `POST /api/harness/versions` | `{repo, parent, name?, branch?, parts: {kind: text}}` | `{version}` (201): a draft |
| `GET /api/harness/diff?a=&b=` | | `{parts: [{kind, diff, impact, level, widens}]}` |
| `POST /api/harness/versions/:id/promote` | `{approve_widening?: true}` | `{version}`; 409 `widens` without the approval |
| `POST /api/harness/versions/:id/rollback` | `{reason?}` | `{version}`: the previous current |
| `POST /api/harness/lint` | `{repo, kind, text}` | `{flags: [{rule, range, message, fix?}]}` |
| `POST /api/harness/impact` | `{repo, base, kind, text, window_days?}` | `{level, impact, rows}`: counted or recomputed, or `{level: "needs_replay"}` |
| `GET /api/harness/complete?repo=&kind=&q=` | | completions with source and impact (through `complete.rs`) |
| `GET /api/harness/sees?agent=&turn=&version=` | | `{parts: [{part, source, tokens, level, text?}], total, window}` |
| `GET /api/harness/findings?repo=&days=` | | `{findings}` |
| `GET /api/harness/suggestions?repo=` | | `{suggestions}` |
| `POST /api/harness/suggestions/:id` | `{action: keep\|dismiss\|test}` | `{suggestion}`; `keep` on a widening one needs `approve_widening` |
| `POST /api/harness/runs` | `{repo, suite?, a, b, n?, cap_usd?, cases?}` | `{run}` (201), with its estimate; 409 `widens` without the approval to test |
| `GET /api/harness/runs/:id` | | `{run, cases: [{case, set, a: [Run], b: [Run]}], scores}` |
| `POST /api/harness/runs/:id/stop` | | `{run}` |
| `GET /api/harness/runs/:id/divergence?case=&a_k=&b_k=` | | `{a: [Turn], b: [Turn], first, context_diff, edits}` |
| `GET /api/harness/suites?repo=` | | `{suites}` |
| `POST /api/harness/cases` | `{agent, suite, turns_from?, turns_to?, checks?, set?}` | `{case}` (201); without `checks`, the proposed ones |
| `POST /api/harness/mode` | `{repo, mode: suggest\|auto\|nightly, budget_usd?}` | `{mode}` |

The stream (`/api/stream`) gets `event: harness` with run progress and new
suggestions.

MCP: `spawn_agent` takes `harness`. No tool lets an agent change the
harness. An agent can call `propose_change` on the repository's CLAUDE.md
as it can on any file, and that goes through Review.

## What it reuses

| need | already in Workbench or ChloeCtx |
|---|---|
| per-session settings, hooks, MCP config, model, effort | `supervisor.rs` (`settings_json`), `flow::with_allowed`, `flow::with_worktree_defaults`, `guarded::read_only_settings`, `ctxwire::wire_as` |
| commands in their simple form, allow prefixes, storms | `flow.rs` (`simple_commands`, `allow_prefix`, the coach's suggestions) |
| the starting tree of a session | `agents.base`, `time_snapshots.heads` (`timetravel.rs`) |
| tests, flakes, fail→pass | `test_runs`, `testruns.rs` |
| recall counterfactuals | ChloeCtx's receipts: `retrieve` and `replay` on a snapshot (`chloectx/src/receipt.rs`) |
| a fix seen before | ChloeCtx's `fixes.rs` (error signatures) |
| model routing | `routing.rs`, `task_features` |
| autocomplete | `complete.rs`, `complete_items`, `complete_events` |
| a persona checking a preview | the persona runner (`swarm.rs`) and chloedb-swarm's checks and judge |
| what a session did, turn by turn | `events`, `trail.rs`, `story.rs`, the recording (`recording.rs`) |
| sandboxed worktrees, ports | `git.rs`, `preview.rs` |
| guarded commands | `guarded.rs` |

New code is mostly the version store, the detectors (SQL), the lattice,
the replayer (a supervisor of agents that plays a script into their
terminals and hooks), the scorer, and the screens.

## MVP and phases

**MVP: counted before replayed.** Three steps, each useful alone:

1. **Versions and findings.** Record the harness each agent ran
   (`harness_versions`, `harness_parts`, `agents.harness`), made from what
   Workbench writes today. Run the detectors whose evidence is counted
   (permission storms, unused tools, slow hooks) and recomputed (missed
   memory, through receipts). Show the Overview with the scorecard and
   findings. No model, no replay.
2. **The permissions and memory editors with impact.** The permission
   builder with grounded autocomplete, lint, the lattice and counted
   impact. The memory editor with recomputed impact. Saving makes a draft,
   and promoting it changes what Workbench writes for new agents. This
   alone fixes the storm and the missed memory, with exact evidence.
3. **Make this a test, and replay A against B.** Cases from sessions
   (fail→pass, pass→pass), the sandbox, the scripted user, N=3 live runs,
   the paired scorecard with the noise floor, and held-out cases. Suggest
   mode only.

**Phase 2.** The hook timeline (`hook_runs`), What the model sees, the
tools and routing editors, cassettes, the divergence view, versions as a
graph with branches.

**Phase 3.** The reflective proposer with the Pareto front, the overfit
lint on its output, auto within limits, nightly, the phone's approvals.

**Phase 4.** Suites shared across repositories (a user's harness has
parts that are not about one repository), instructions, and Claude Code's
own updates as a version change to test.

## Built (H): steps 1 and 2

Steps 1 and 2 of the MVP are built, with no model and no replay. The
server side is `src/harness.rs`, `src/harness/perms.rs` (the rules, lint,
impact and widening), `src/harness/memory.rs` (receipts, misses, the
recompute and the preview) and `src/harness/stats.rs` (the no-difference
statistics). The pages are `ui/goals/harness/*.js`, at `#/harness` in R1's
router. Tests are the `harness_*` tests in `tests/workbench.rs` and the
unit tests of each module.

**Versions.**
- At spawn (`harness::at_spawn`, one call in `supervisor.rs`), Workbench
  applies the current version's edits to the session's own `--settings`
  and `--mcp-config`. `<worktree>` becomes the full path of the session's
  worktree.
- It then records the version the agent ran on `agents.harness`, in
  `harness_versions` and `harness_parts`. The version is made from the
  normalized parts: permissions, hooks, env, tools, brief, memory and
  `claude --version`. The session's own paths are placeholders, so two
  agents of one config share one version.
- `1.0` is Workbench's own harness. Saving makes a draft (one per
  repository). Promote makes it current for new agents, and Roll back is
  one step back.
- A draft that widens what agents may do is refused with 409 `widens`
  until `approve_widening` names every pattern. The page asks with one
  checkbox per pattern.
- A secret in any part is refused (400 `secret`). An unchanged save is
  refused (409 `unchanged`).
- Only the user's token changes the harness: an agent's key gets 403.

**Findings** (`GET /api/harness`, `/findings`). Each finding has its
evidence level.
- Permission storms, with each session's permission mode. A storm the
  current rules already allow is shown as handled and suggests nothing.
- Auto mode's blocks and escalations.
- Unused skills, subagents and MCP servers, with what they cost.
- Slow hooks: p50 and p95, from the opt-in timing.
- Redo loops, from `test_runs`.
- Your corrections: the same words typed to two or more agents, and
  Hotspots' corrected files.
- Missed memory, from ChloeCtx's full receipts (`chloectx receipt ID --json
  --no-replay`). A miss is a decision, rule or fact that was left out and
  names a file the agent then edited, followed by a failing test.

The Overview shows:
- the week's live scorecard: done by checks, interventions, prompts, wall
  time, tokens and missed memory, plus the modes line;
- the findings, worst first;
- agents by version, with each one's mode;
- the current version against the one before;
- suggestions, which go into the draft: a storm's rule, or a missed record
  as a pin.

**Editors.**
- **Permissions:**
  - the rules with their 7-day hits;
  - autocomplete from the commands agents ran (`/api/harness/complete`);
  - lint: unreachable, conflicting, shadowed, wider than needed, widens,
    secrets;
  - the counted impact against the current rules, with a prompt the
    current rules already allow counted as `already`, not as the change's;
  - Structured, Split and Text views, with the JSON and the table kept in
    sync both ways through `/api/harness/lint`.
- **Memory:**
  - the gate, the knowledge threshold, the budgets and pins;
  - a live preview of one turn's recall under the draft;
  - every recall of the week recomputed: exact on decisions, and estimated
    on which records a changed decision packs;
  - lint of the misses.

**Market adjustments.**
1. *Auto mode*, the default for Pro, Max and Team since Aug 14, 2026:
   - Every call carries the session's `permission_mode` from the hook's
     input.
   - A `PermissionDenied` event counts as auto mode blocking a call.
   - A permission prompt in a session in auto mode counts as an
     escalation.
   - The scorecard says which mode each session ran in, and storms say it
     too.
   - Workbench does not wire the `PermissionDenied` hook itself, so blocks
     are counted only where that event exists.
2. *What the model sees*:
   - Counted, from Debug's turn context (R2's `GET
     /api/agents/:id/turns/:n/context`): the turn's size, the prompt and
     ChloeCtx's records.
   - Injected, from `/api/harness/sees`: what Workbench put in (rules,
     steers, pins).
   - Estimated, and labelled so: everything Claude Code assembles itself
     (system prompt, tools, conversation, CLAUDE.md, skills).
   - `OTEL_LOG_RAW_API_BODIES` is reported only as set or not set. Its
     bodies are not read yet, because `runtime.rs` keeps only errors of the
     OTLP export; storing `claude_code.api_request_body` there is the
     integration point. Workbench never turns the export on.
3. *No detectable difference* is a result in its own right:
   - Both the impact and the comparison of two versions say it.
   - A comparison gives the effect that its session counts could detect: a
     two-proportion test or Poisson rates at 95% confidence and 80% power.
   - Fewer than 3 sessions a side is "too few".

**Reused, not duplicated.**
- P's routes:
  - `GET /api/rules/suggested` is linked from the Overview. A rule
    accepted there lands in `allow:<repo>`, which the baseline reads as
    "Flow".
  - `grantSheet` comes from `ui/goals/rules-suggest.js` for every allow
    added in the editor.
- R2's routes:
  - the turn context;
  - `/api/repos/:p/threshold`, which Promote sets when a version changes
    the knowledge threshold.
- R1's router (`case "harness"`), `GOAL` map, Start (a line under the four
  goals) and More (an entry).

**Left out, and why.**
- The gate and the budgets reach new sessions as `CHLOECTX_RECALL` and are
  recomputed here. ChloeCtx reads them from its store's `config.json`, so
  they are not yet applied per session.
- The knowledge threshold is per project, through R2's API.
- Pins arrive at session start (a SessionStart hook), not as the agent
  touches the file.
- Hook timing is opt-in per version. It uses `date +%s%N`, so it works on
  Linux and not on macOS.
- The tools, routing and hooks editors, replay, evals, the proposer and
  auto-learn beyond "Suggest" are not built. They belong to step 3 and
  later.

**Dev.**
- `node ui/dev/harness-seed.mjs` starts a server with the fake `claude` and
  `ui/dev/harness-fake-chloectx.mjs`, then seeds a week:
  - storms;
  - an agent in auto mode;
  - a redo loop;
  - a correction;
  - a missed record;
  - 1.1 promoted with its widening named;
  - a 1.2 draft.
- It prints `/?token=dev#/harness`.
- `ui/dev/harness-live.html` mounts the pages without the shell.
- Screenshots: `H-*.png` in the project's `workbench/rewrite/` folder.

## Risks

- **Replay fidelity.** A session depends on things outside the commit: a
  service, the network, the time, data in a local database. A replay that
  diverges for those reasons measures noise. Mitigations: the A/A floor
  shows it; cassettes pin tool outputs; a case says what it needs; cases
  that are unstable under A against A are flagged and left out of scoring.
- **Cost.** Replays are real model runs. shop core, 12 cases at 3 runs
  each, costs about $4.60 a version in the mocks, so A against B is about
  $9. Mitigations: counted and recomputed evidence
  first; caps; cassettes; partial cases (turns 1 to 20); skip what is
  over the cap.
- **Overfitting to the user's sessions.** Mitigations in Generalization.
  Even so, a small repository has few sessions. Below 6 cases the results
  say "too few to tell", and only counted and recomputed changes are
  proposed.
- **Too few runs.** Three runs a case is little. Intervals will often
  include zero. The design says "inside the noise" plainly and prices more
  runs; it does not round a tie into a win.
- **Widening by composition.** Two narrow-looking rules can allow together
  what neither does alone. The lattice is computed on the whole version,
  not on each rule.
- **The scripted user is not the user.** The original answers fit the
  original run. When B asks something new, the precedent may be wrong.
  Mitigations: such moments are counted as interventions, listed, and never
  answered by a model.
- **Goodhart.** If prompts are the metric, a harness that allows everything
  wins. The gates (widening, held-out regressions) and success as the
  primary metric are what stop it.
- **Claude Code changes under it.** Its own prompt, tools and permission
  syntax change between versions. Each version records Claude Code's
  version, and a Claude Code update is itself a change to test against the
  suite.
- **Privacy.** Cases contain the user's prompts and code. They stay in
  Workbench's local database like the sessions they come from. A suite is
  never uploaded, and the reflective proposer sends condensed traces to the
  model the user already uses, as the distiller does.

## Prior work

- **DSPy** (Khattab et al., 2023) treats prompts as parameters of a
  program, compiled against a metric; **MIPROv2** (Opsahl-Ong et al., 2024)
  searches instructions and examples with Bayesian optimization.
- **GEPA** (Agrawal et al., 2025, "Reflective Prompt Evolution Can
  Outperform Reinforcement Learning") reflects on traces in natural
  language to propose prompt changes and keeps a Pareto front of
  candidates. The reflective proposer and its selection follow it.
- **TextGrad** (Yuksekgonul et al., 2024) and **OPRO** (Yang et al., 2023):
  a model's feedback as the gradient of a text, and a model as the
  optimizer.
- **Reflexion** (Shinn et al., 2023), **Voyager** (Wang et al., 2023) and
  **ACE**, Agentic Context Engineering (Zhang et al., 2025): agents that
  improve from their own trajectories by keeping lessons, a skill library,
  or an evolving playbook. ChloeCtx is that memory; this goal tunes how it
  is given.
- **ADAS** (Hu et al., 2024) and the **Darwin Gödel Machine** (Zhang et
  al., 2025) search over agent designs, the latter by editing a coding
  agent's own harness and scoring it on SWE-bench. Their lesson is that
  harness changes are worth searching, and their risk is the one named
  under Generalization.
- **SWE-bench** (Jimenez et al., 2024): FAIL_TO_PASS and PASS_TO_PASS
  checks. **τ-bench** (Yao et al., 2024): pass^k, for consistency over
  luck.
- **Eval-driven development** (Hamel Husain, "Your AI product needs
  evals", 2024; Shreya Shankar et al., "Who Validates the Validators?",
  2024): look at traces, write checks from failures, run them on every
  change.
- **Trace-based evals**: LangSmith, Braintrust, Arize Phoenix and the
  OpenTelemetry GenAI conventions turn production traces into datasets.
  "Make this a test" is the same move, with a session's commit and its
  user's answers, so the case can be played again and not just scored.

## Mocks

`ui/dev/harness.html` and `ui/dev/harness/`: plain modules, no build, any
static server. They use the style of direction C, Instrument: Geist and Geist
Mono (in `harness/fonts/`), flat panes on hairlines, a frosted toolbar and
inspector, small-caps mono labels, numbers in mono columns, gauges as tick
scales, key hints on actions, one indigo accent. `harness/style.css` is
the direction's stylesheet; `harness/harness.css` adds this goal's
pieces.

| file | what |
|---|---|
| `harness/app.js` | the router, `?t=dark`, and the mock's clicks |
| `harness/dom.js` | the helpers that make HTML (labels, buttons, gauges, ticks, cells) |
| `harness/fixtures.js` | the shop's versions, findings, suggestions, sessions and scorecard |
| `harness/overview.js` | Overview, the phone's suggestions and scorecard, and Where it plugs in |
| `harness/run.js` | the optimize run, and its results |
| `harness/editor.js` | the editors: permissions, hooks, memory, tools and routing |
| `harness/sees.js` | What the model sees |
| `harness/evals.js` | Evals |
| `harness/versions.js` | Versions |
| `harness/shoot.cjs` | the screenshots (`NODE_PATH=$(npm root -g) node chloedb-workbench/ui/dev/harness/shoot.cjs OUT/`) |

Screenshots, at 2x: desktop at 1440 wide in light and dark (01 to 11),
the phone at 390x844 in light (20 to 23).

| shot | route |
|---|---|
| 01-overview | `#/harness` |
| 02-optimize-run | `#/harness/run/7` |
| 03-results | `#/harness/run/7/results` |
| 04-edit-permissions | `#/harness/edit/permissions` |
| 05-edit-hooks | `#/harness/edit/hooks` |
| 06-edit-memory | `#/harness/edit/memory` |
| 07-edit-tools | `#/harness/edit/tools` |
| 08-what-the-model-sees | `#/harness/sees` |
| 09-evals | `#/harness/evals` |
| 10-versions | `#/harness/versions` |
| 11-plugs-in | `#/` |
| 20-suggestions-phone, 21-suggestions-more-phone | `#/harness/approve` |
| 22-scorecard-phone, 23-scorecard-findings-phone | `#/harness` |

The content is one morning of the shop: cents fixing `money.py`'s rounding
(half up, in cents), freeship missing "Shipping is not taxed" at turn 2,
flakehunt and the flaky `test_promo_window`, paybutton and the Pay button
that does nothing, and the 22 prompts for one test command.
