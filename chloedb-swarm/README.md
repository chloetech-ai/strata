# chloedb-swarm: simulated users for ChloeDB

> **A demo.** Swarm, an internal test instrument, shows what ChloeDB can do; it is not part of ChloeDB's releases or their promises. It is tested on a train of its own (the `Demos` workflow, `.github/workflows/demos.yml`), outside CI's required check, so a fault in it does not hold a ChloeDB release, and the command center marks it as a demo ([compatibility](../docs/compatibility.md#demos)).

Swarm plays a set of people (personas) doing tasks with ChloeDB, many at
once, and reports what worked and what did not. Each person is a model
told who they are and what they want; it uses ChloeDB the way that person
would, through one of two surfaces:

- `mcp`: the server's MCP tools (`POST /v1/mcp`), as a developer's coding
  agent does;
- `browser`: the command center in Chromium, through Playwright, reading the
  page as its accessibility tree and acting on its elements by reference.

Every session runs against a sandbox of its own: a fresh `chloedb-server` on
a free port, with a random root token, the task's fixture loaded and an API
key of the task's scope (`read`, `write` or `admin`). Nothing a session does
reaches any other server. At each step the person writes a short note of
what they are doing (`note`, kept as the step's `thought`), which makes a failure readable
afterwards. When they finish (or give up, or run out of steps or time), the
task's checks read the sandbox and the answer to decide success, and the
person answers a short exit interview.

Then a stronger model reviews each session, naming the problems it shows
(bug, error message, confusing UI, missing feature, docs, performance), each
with the step it happened at and a severity, and telling them apart from the
simulated person's own mistakes (`agent_mistake`). It names the strengths
too: what in the product helped the person (an error that said what to do, a
page where they looked for it). Two more calls group the problems of the whole
run into findings, and its strengths likewise: one problem that many sessions
hit is one finding, with how many sessions and which personas hit it.

## Run it

```bash
cd chloedb-server && cargo build --release      # the sandboxes run this build (or CHLOEDB_SERVER_BIN)
cd ../chloedb-swarm && npm ci
export ANTHROPIC_API_KEY=...                    # or CHLOEDB_SWARM_ANTHROPIC_KEY
node bin/swarm.mjs run scenarios/starter.json --limit 4 --budget 2
```

The browser surface needs Playwright and Chromium
(`npm i -g playwright && npx playwright install chromium`; or
`CHLOEDB_SWARM_CHROMIUM=/path/to/chromium`).

A run writes `swarm-runs/runs/<id>/` (`run.json`, `sessions.json`,
`steps.jsonl`, `findings.json`, `report.md`) and a screenshot of each browser
session's last page. `node bin/swarm.mjs report swarm-runs <id>` writes the
report again, and `node bin/swarm.mjs regroup <id>` (with `--out DIR` or
`--server URL --token T`) groups a finished run's findings again from the
reviews it kept, without playing or reviewing a session.

In the browser, the person reads the page as its accessibility tree, cut at
16,000 characters; an open dialog comes first, whole, and `look` with a
`part` (an element's reference) reads one table, list or section whole.

| flag | what |
|---|---|
| `--budget USD` | the run's cap on model spend (default the set's `budget_usd`, or 25): three quarters for the people, a quarter for the reviews; a spent cap stops new sessions and the run ends as `budget` |
| `--concurrency N` | sessions at once (default the set's, or 4) |
| `--limit N`, `--persona a,b`, `--task x,y`, `--seed 1,2` | play part of the set |
| `--server URL --token T` | store the run in a chloedb-server's tables (`swarm_runs`, `swarm_sessions`, `swarm_steps`, `swarm_findings`) instead of a folder |
| `--keep` | keep each session's sandbox database |

## In the command center

```bash
node bin/swarm.mjs serve --local swarm-home     # a server of its own, with the Swarm app on it
node bin/swarm.mjs serve --server URL --token T # store runs in a server you run (start it with --app swarm=http://127.0.0.1:7900)
```

`serve --local DIR` starts a chloedb-server on `127.0.0.1:7700` with the
Swarm app on `127.0.0.1:7900`, and prints the command center's address. The
Swarm page starts a run of a scenario set (with a budget and a session
limit), shows each run as it plays (progress, success rate, cost, the
findings with their evidence, success by persona and task), and each
session's steps with the person's notes, the errors the browser saw, the
answer, the checks, the exit interview and the review.

The runs are rows of the server's tables, so SQL reads them too:

```sql
SELECT persona, task, AVG(CAST(success AS INT)) FROM swarm_sessions GROUP BY persona, task
```

## A scenario set

`scenarios/starter.json` is the example: five personas, four tasks, three
seeds, 60 sessions. `scenarios/new-features.json` has the same personas on
features that came later: context that fits a token budget (`PACK`), the
busiest day (`date_trunc`), a key that reads one table, and a rollup. A set is:

```jsonc
{
  "name": "starter", "seeds": 3,
  "models": ["haiku", "haiku", "sonnet"],   // the model of each seed, in turn
  "budget_usd": 25, "concurrency": 4,
  "personas": [{ "id": "analyst", "name": "Tomás Rivera", "role": "a data analyst",
                 "traits": ["Knows SQL well.", "Has never used a search database."], "device": "desktop" }],
  "tasks": [{
    "id": "weekly-counts", "title": "Orders by status this week", "surface": "mcp", "scope": "read",
    "prompt": "How many orders of the last 7 days are in each status?",
    "personas": ["analyst", "pm"],             // optional: who plays it (default everyone)
    "fixture": { "tables": [{ "name": "orders", "create": "CREATE TABLE orders (...)",
                              "generate": { "count": 300, "fields": { "id": { "seq": "o-" },
                                "status": { "pick": ["paid", "shipped"], "weights": [2, 3] },
                                "created": { "daysAgo": [0, 30] } } } }] },
    "checks": [{ "name": "counts", "kind": "answer_matches_sql",
                 "sql": "SELECT status, COUNT(*) AS n FROM orders WHERE created >= '{{days_ago:7}}' GROUP BY status",
                 "columns": ["status", "n"] }]
  }]
}
```

A fixture's rows are given (`rows`) or drawn from the seed (`generate`: `seq`,
`pick` with `weights`, `int`, `float` with `digits`, `daysAgo`, `const`), so a
seed draws the same rows every time. A browser task may name an `entry` (the
hash to open, such as `#/tables`), and a persona a `device` (`desktop` or
`phone`). The checks:

| kind | passes when |
|---|---|
| `sql` | the query's first row has the `expect`ed values, or it returns at least `min_rows` |
| `http` | a request to the sandbox (`path`, `method`, `body`) answers a body for which the JavaScript expression `test` holds |
| `answer_includes` | the answer names `all` of the values, or `at_least` N of `any` (ids as whole words, numbers as numbers) |
| `answer_matches_sql` | every value of the `columns` of every row the query returns is in the answer |

`{{now}}` and `{{days_ago:N}}` in a check's SQL become times. A session succeeds
when the person said `done` and every check passed.

## Tests

```bash
npm test
```

No test calls the API: a scripted model stands in for it. The integration
tests start chloedb-servers (skipped without a build, unless
`CHLOEDB_SWARM_NEED_SERVER` is set, as CI does) and Chromium (skipped
without Playwright).
