# ChloeCtx as an app of a ChloeDB cluster

`chloectx app` runs ChloeCtx beside a ChloeDB cluster, as the crawler and Sheets do, and the command center shows it under Apps. Its memory is not a file on one laptop: it lives in the cluster's tables, so any number of agents and people can write to it and ask it for context.

Its first page lists every memory it can read, as *spaces* to pick from, and a space opens on what it knows now:

- **Team spaces.** Memory that any agent writes notes and conversations to and asks for context before a task, within a token budget: a table of the cluster each. An answer is found by search alone (BM25 and vectors, no model call), and it says what went in, why, and what was left out.
- **Claude Code projects.** What Claude Code sessions remembered: a project `chloectx sync` copied into the cluster (table `chloectx_memory`), and, with `--local-stores`, the stores of the machine the app runs on, where Receipts, Blame and capsules are too ([Receipts and blame](receipts.md#in-the-command-center)). A store and its synced copy show as one project.
- **Seen before**, a page of its own: the cluster's firing alerts, each beside the past incidents it looks like and the fix that worked. One button hands the alert to the cluster's on-call agent with those incidents as its context.

![A team space's Ask tab: the question "Who approves a refund of 1,200 EUR?", answered first by the current refund decision with the 500 EUR version it replaced struck through, then the notes that also bear on it, the past conversations folded, and the context's size](../assets/img/chloectx-app-context.png)

*The demo's team space, asked "Who approves a refund of 1,200 EUR?". The answer leads with the current decision (over 1000 EUR or 900 GBP, a second approver from finance) and what it replaced, then what else bears on it; the scores, what was left out and the context as the agent gets it are below. 14.4 ms, no model call.*

## Start it

A chloedb-server with `chloectx` installed beside it (in the same directory, as an install puts them) starts it by itself, with a token and a key of its own ([apps the server starts](../chloedb/server.md#apps-the-server-starts)); its key may make and write `ctx_*` tables and read `chloectx_memory`, so the on-call hand-off of Seen before, which needs an admin key, is refused there. Started by hand:

```bash
cd chloectx && cargo build --release
./target/release/chloectx fetch-model                # the embedding model, 129 MB, once
./target/release/chloectx app --server http://127.0.0.1:7700 --port 7760 --token "$APP_TOKEN"
# and the coordinator shows it:
CHLOEDB_APP_TOKEN="$APP_TOKEN" chloedb-server --shards ... --app chloectx=http://127.0.0.1:7760
```

| option | default | what |
|---|---|---|
| `--server` (`CHLOEDB_URL`) | `http://127.0.0.1:7700` | the chloedb-server (a coordinator, or a server on its own) the spaces live in |
| `--key` (`CHLOEDB_KEY`) | none | its key for the app's reads and writes: an admin key, since a new space is a new table and the on-call agent takes admin calls |
| `--token` (`CHLOECTX_APP_TOKEN`) | none | the token every caller must send (`Authorization: Bearer`); chloedb-server sends its `CHLOEDB_APP_TOKEN` |
| `--insecure` | off | serve any caller with no token on an address other than loopback; without it, `chloectx app` refuses to start with `--host 0.0.0.0` (or any non-loopback address) and no token, since whoever reached the port could read and write every space and start model calls billed to its key |
| `--host`, `--port` | `127.0.0.1`, `7760` | where it listens |
| `--embedder` (`CHLOECTX_EMBEDDER`) | `static` | `hash:<dim>` for tests without the model |

Model calls (writing a conversation's notes down, drafting an incident from a postmortem) go to Anthropic's API when `ANTHROPIC_API_KEY` is set (model `CHLOECTX_APP_MODEL`, by default Haiku), else to the `claude` CLI when it is on the PATH (`CHLOECTX_DISTILL_MODEL`). `CHLOECTX_APP_MODEL=off` turns them off. Everything else, asking included, works without a model. At most four conversations are distilled at once and 256 wait; past that an exchange is stored without notes (its answer says `skipped`, and `/v1/info` counts it under `distill.skipped`). The app serves 64 requests at once and answers one more with 503.

The app refuses requests that carry an `Origin` header: a browser reaches it only through chloedb-server, which checks the signed-in key and forwards with the app's token and who called (`x-chloedb-scope`, and `x-chloedb-tables` for a key limited to tables). The app's manifest says `"checks_callers": true`, and it refuses what the caller's key may not do itself: reading a space or asking it needs read on its table (`ctx_<space>`), adding or retiring notes and exchanges write there, making a space admin there (it makes the table), the incidents are the space `incidents`'s (`ctx_incidents`), and what is not a space's (building apps, handing an alert to the on-call agent, this machine's stores) needs an admin key of every table to change and a key of every table to read. So a write key records notes, and a key limited to `ctx_ops_*=write` writes the spaces `ops_*` and nothing else (`GET /v1/spaces` lists the spaces it reads). A request with none of those headers (a program calling the app itself with its token) may do anything, as before.

### On Kubernetes

A cluster the [operator](../chloedb/server.md#on-kubernetes) runs gets ChloeCtx from its `StrataCluster`, with nothing to start by hand:

```bash
kubectl -n strata patch strata demo --type merge -p '{"spec": {"chloectx": {}}}'
```

The operator runs `chloectx app` from the cluster's own image (it carries it, as it carries the crawler and the spreadsheets) as a Deployment of one pod, `<cluster>-chloectx`, behind a Service of that name on port 7760, and restarts the coordinators so that they list it: **Apps → ChloeCtx** appears in the command center. It reads and writes with the cluster's admin token (`tokenSecret`), which its API requires too, so the coordinators send it that token and nothing else reaches it (a cluster without a `tokenSecret` is open to whoever reaches it, and the operator starts its ChloeCtx with `--insecure` to match), and it calls the model with the cluster's `secretEnv` (`ANTHROPIC_API_KEY`; without one, everything but writing a conversation's notes down and drafting incidents works). The embedding model is not in the image, which every server pulls: an init container downloads it from Hugging Face (125 MB) into the pod's scratch volume each time the pod starts. The spaces are tables of the cluster, so a pod started again (a new image, a node replaced) has them all; the upgrader moves it with the servers, since it runs their image (from the image of 2026-11-10 on: an older one has no `chloectx`).

| `spec.chloectx` | default | |
|---|---|---|
| `paused` | `false` | no pod for now (the coordinators still list it, as an app that does not answer; the spaces stay) |
| `embedder` | `static` | `hash:<dim>` for a cluster that cannot reach Hugging Face: no download, vectors from hashed words. Notes keep the vectors they were written with, so after a change earlier ones match by their words only |
| `args`, `resources` | | more arguments of `chloectx app`, and its container's resources (50m CPU and 128 MiB asked, 1 GiB at most; it uses about 40 MiB) |

Removing `spec.chloectx` deletes its Deployment and Service and restarts the coordinators without it; the spaces stay in `ctx_spaces` and `ctx_<space>`.

## Where the memory is

Each space is a table of the cluster, `ctx_<space>`, with ChloeCtx's own records ([the data model](index.md#the-data-model)): notes (`decision`, `preference` for rules, `fact`, `todo`, `note`), messages (`prompt`, `answer`), entities, and links between them. The spaces themselves are rows of `ctx_spaces`. A table per space keeps BM25's statistics per space, so one team's words do not weigh on another's ranking, and SQL reads a space like any table:

```sql
SELECT kind, text, ts FROM ctx_billing WHERE kind IN ('decision', 'preference') AND tag != 'superseded' ORDER BY ts DESC LIMIT 20
```

The app keeps nothing of its own: a restart, or another copy of it, finds every space in the cluster. It numbers the records it writes, so one app writes to a cluster's spaces at a time.

Notes are never overwritten. A note that `replaces` another marks the older one `superseded` and links them both ways (`supersedes`, `superseded_by`); a context shows the current version with what it changed, and the history keeps every version.

## The API

JSON over HTTP; through chloedb-server, the same paths under `/v1/apps/chloectx`.

| call | body | answer |
|---|---|---|
| `GET /v1/spaces`, `POST /v1/spaces` | `{name, title?, description?, kind?}` | the spaces with their counts (`by_kind`, every record; `live_by_kind`, the notes not replaced, retired or done); the new space (one that exists is left as it is); `kind` is `notes` (the default) or `incidents`, a team's own incidents for Seen before |
| `DELETE /v1/spaces/:s` | | the space, deleted by an admin of its table: its table `ctx_<space>` dropped and its row of `ctx_spaces` removed, `{deleted, table, kind}`; it cannot be undone |
| `POST /v1/spaces/:s/notes` | `{kind, text, replaces?, entities?, session?, when?}` | the note (`kind`: `decision`, `rule`, `fact`, `todo` or `note`); `when` (or `ts`) is when it happened, for a note written down after the fact: seconds since 1970, or ISO 8601 text (`2026-09-14`, `2026-09-14T03:10:00Z`); by default, now |
| `GET /v1/spaces/:s/notes?kind=&limit=&all=1` | | the notes, newest first; the current ones unless `all=1` |
| `GET /v1/spaces/:s/notes/:id` | | the note, the versions it replaced and those that replaced it |
| `DELETE /v1/spaces/:s/notes/:id` | | the note, retired: kept, tagged `retired` (not `superseded`: nothing replaced it), and in no context again |
| `POST /v1/spaces/:s/exchanges` | `{user, assistant, distill?}` | the two messages; with a model, their decisions, rules and facts follow as notes in a few seconds (`GET /v1/info` counts them) |
| `POST /v1/spaces/:s/ask` (or `GET ?q=&budget=`) | `{question, budget?}` | `{context, tokens, ms, items, left_out}`; `budget` is 200 to 50,000 tokens (4,000 left out), and one outside that is refused |
| `GET /v1/incidents?space=`, `POST /v1/incidents` | `{title, symptom, cause, fix, alert?, replaces?, when?, space?}` | the incidents of every incidents space the key reads (or of `space`), each with its space and the fixes it replaced; the new one, in `incidents` or the team's `space`, dated `when` (the incident's own date) |
| `GET /v1/incidents/firing?space=` | | the cluster's firing and pending alerts, each with its closest incidents and the context the on-call agent would get |
| `GET /v1/incidents/match?q=&alert=&space=` | | the incidents closest to a description |
| `POST /v1/incidents/match` | `{lines \| text, space?, alert?, n?}` | the incidents closest to log lines pasted from an outage (redacted as notes are, at most 4,000 characters) |
| `POST /v1/incidents/draft` | `{text}` or `{run}` | incidents a model drafted from a postmortem or an on-call run's report; nothing is kept |
| `POST /v1/incidents/handoff` | `{alert}` | the on-call agent's run, started with the closest incidents |
| `POST /v1/spaces/:s/apps/suggest` | `{n?}` (2 or 3) | apps a model suggests from the space's notes: `{apps:[{title, pitch, why, kinds, description}]}`; nothing is built |
| `GET /v1/spaces/:s/apps/estimate` | | what a build costs: the median of the app engine's past first builds, its model and what is left of its cap |
| `POST /v1/spaces/:s/apps/items` | `{kinds?, exclude?}` | the notes a build would copy |
| `POST /v1/spaces/:s/apps` | `{title, description, kinds?, exclude?, confirm: true}` | the build, started (409 without `confirm`, 402 when the engine's cap is spent) |
| `GET /v1/spaces/:s/apps` | | the space's builds: `building`, `loading`, `ready` or `failed`, with their cost and rows loaded |
| `POST /v1/spaces/:s/apps/:id/open` | | a one-time link into the app engine's studio, at the built app |

An answer of `ask` has, for each record in the context, its `section` (`rule`, `note`, `timeline` for a question about the whole history, `reply` and `excerpt` for past conversations), its `tokens` (about four characters each), its scores (`bm25` for the question's words, `cos` for its meaning) and, for a note that replaced another, `replaced`: what it changed. `left_out` names what the searches found and the context leaves out, and why: `superseded` (a later note replaced it), `retired`, `uncertain`, or `budget`. The context is built as ChloeCtx's own `memory_ask` builds it for Claude Code ([how](index.md)), with its sizes scaled to the budget.

### A space as an MCP server

`POST /v1/spaces/:s/mcp` speaks the Model Context Protocol (streamable HTTP, JSON answers; a `GET` gets 405) with three tools on that space: `ask` (`question`, `budget`: the context, as `ask` above, with the ids of its notes), `remember` (`kind`, `text`, `replaces?`, `entities?`: a note, session `mcp`) and `notes` (`kind?`, `limit?`: the current ones with their ids). A space that does not exist is 404, not a new space. To connect Claude Code, through chloedb-server:

```bash
claude mcp add --transport http ctx-billing https://chloedb.example.com/v1/apps/chloectx/v1/spaces/billing/mcp \
  --header "Authorization: Bearer $KEY"
```

A space's page in the command center has a **Connect Claude Code** card that makes the key and shows this line with it filled in: a key limited to the space's table, `ctx_billing=write`, which may read and write that space and nothing else (below).

### Keys limited to tables

chloedb-server lets a key limited to tables call the app, because the app's manifest says `"limited_keys": true`: it tells the app the key's grants (`x-chloedb-tables`), and the app, which writes with a key of its own, holds the caller to them: a space is its table `ctx_<space>` (read to read and ask it, MCP included, write to add, replace or retire notes and hand over exchanges), the incidents are `ctx_incidents` (read, write to record or draft), the Claude Code projects read `chloectx_memory`. Creating a space and building apps from one take an unlimited admin's key, and no limited key reads the machine's local stores (Receipts, Blame). `GET /v1/spaces` lists the spaces the key may read. A caller with no such header (an admin's key, the server's token) is not limited.

## Spaces and what each shows

`#/app/chloectx` lists the spaces, grouped (team spaces; Claude Code projects) with what each holds, and a filter. A space's page (`#/app/chloectx/space/<name>`, `…/project/<owner/name>`, `…/store/<store>`) has a switcher to another space and these tabs:

- **Overview**: how many rules, decisions, facts and open to-dos it holds now, its sessions and records, the rules (which go into every context), the latest decisions and facts, the open to-dos, *what changed* (notes that replaced an earlier one, with the old text struck through) and the latest sessions.
- **Ask**: a question, answered as an agent would be: the note that answers it first, the rules that apply, what else bears on it and what past conversations said, then the scores behind each (BM25, cosine, section, tokens), what was left out and why, and the context as the agent gets it. A link carries the question (`…/ask/<question>`). A synced project, which has no store to build a context from, answers with its records ranked by BM25.
- **Memories**: every note, by kind, current or with its history; a note opens on the versions it replaced and those that replaced it. A team space's notes can be replaced or retired here.
- **Sessions**: each session titled by its first prompt, read as the conversation it recorded: turn by turn, the prompt, the tool calls (folded), the answer and the notes written down from the turn. *Every record* searches all of them (BM25) and filters by kind. A record opens in a drawer with its whole text (up to 4,000 characters), its context, files, links and id.
- **Graph**: how its memory is connected: each note (a circle, by kind; a replaced one hollow), the subjects it is about (entities, larger the more notes name them), the files it touched, the session it came from (off by default) and the note it replaced (a dashed link). A click lights up a node's neighbours and lists them, with its history, its session, *Ask about it* or *Blame it*; a search finds a subject, file or note; the link carries the selected node (`…/graph/e:refund`).
- **Connect**: how to use the space. A team space: the `claude mcp add` line with a key limited to its table, the HTTP calls and the SQL that read it, and apps built from it. A Claude Code project: its MCP tools, its CLI, and how to sync it here.
- **Receipts**, **Blame**, **Snapshots and capsules**: a store of this machine only ([Receipts and blame](receipts.md#in-the-command-center)).

The older links (`#/app/chloectx/inspector/<project>`, `…/receipts/<store>`, `…/blame/<store>`) open the same space's tab.

Until a project is synced, the projects group offers a setup instead: a button (for an admin) makes the table `chloectx_memory` and a key that may write only it, and shows the `chloectx sync setup` command with the key filled in; `sync` needs ChloeCtx 0.2.0 or later. A project is its git repository, so every checkout shares one memory ([keep the memory on a ChloeDB server](setup.md#keep-the-memory-on-a-chloedb-server)).

What the tabs read, beside the space calls above:

| call | answer |
|---|---|
| `GET /v1/spaces/:s/overview` | `{records, sessions, first, last, by_kind, live, rules, decisions, facts, todos, changes, recent_sessions}`: `live` counts the current notes by kind, each list holds the newest 12, `changes` the notes that replaced another with `was` (the older texts) |
| `GET /v1/spaces/:s/sessions` | `{sessions: [{session, records, first, last, turns, title}], by_kind}`: the latest first; `title` is the first prompt's first line, `turns` its prompts |
| `GET /v1/spaces/:s/records?session=&kind=&q=&limit=&before=` | `{records, more}`: a session's in order, else the latest first (`before`, a `seq`, pages back), or the best matches for `q` (BM25); at most 500 |
| `GET /v1/spaces/:s/graph` | `{nodes, edges}`: nodes `{id, type, label}` of type `note` (with `kind`, `text`, `tag`, `current`, `ts`, `session`), `entity`, `file` (`f:<path>`) and `session` (`s:<id>`, its title); edges `{s, t, label}`: `about`, `touches`, `from` (the session) and `supersedes`; the newest 1,500 notes |
| `GET /v1/memory` | `{table, exists, projects: [{project, records, sessions, last}], create}` (`create`: the table's `CREATE TABLE`) |
| `GET /v1/memory/overview?project=`, `/v1/memory/notes?project=&kind=&all=1`, `/v1/memory/graph?project=` | a synced project's overview, notes and graph, as a space's |
| `GET /v1/memory/sessions?project=`, `/v1/memory/records?project=&…` | its sessions and records, as a space's |

A store of this machine answers the same calls under `/v1/local/:store/` ([the list](receipts.md#in-the-command-center)).

## A demo to show it

`chloectx/demo/seed.py` plays a scripted Claude Code history into ChloeCtx, so every page has something to show: twenty sessions over four weeks on *harbor*, a made-up order and payments service (refunds, webhooks, rate limits, migrations, a double-refund incident, a release process), with corrections along the way (a refund limit raised twice, a currency added, a rate limit changed for partners, a debug print that logged a key). It runs the real hooks and daemon, each turn at its date in the story (`CHLOECTX_CLOCK_OFFSET`), with a scripted distiller (`demo/scripted_distill.py`) in place of the model, so it makes no model call and takes about 15 seconds:

```bash
python3 chloectx/demo/seed.py --dir ~/chloectx-demo                 # a Claude Code store, with receipts and snapshots
CHLOECTX_HOME=~/chloectx-demo/home chloectx app --server http://127.0.0.1:7700 --local-stores
python3 chloectx/demo/seed.py --dir ~/chloectx-demo --no-local \
  --server http://127.0.0.1:7700 --space harbor --sync               # the same story as a team space, and synced
```

`--space` writes the story into a new team space through the API (`--space-reuse` adds to one that exists), `--sync` copies the store into the cluster's `chloectx_memory` as the project `acme/harbor`, and `--key-env` names the variable holding the server's key. All of it is made up: no transcript of anyone's sessions is in it.

## Seen before

![Seen before in the command center: four firing alerts of a local cluster, each with the past incidents it looks like; the disk alert's closest incident was recorded for the same alert](../assets/img/chloectx-app-seen-before.png)

*Seen before on a two-shard cluster in the cloud container (the alerts are real: its disk was over 90% full, and a test load raised the p99). "Disk full within 12 hours" finds the incident recorded for the same alert first, and the p99 alert the merge incident, whose first fix (a restart) was replaced by a better one.*

An incident is a note in the space `incidents`, or in a team's own space of kind `incidents` (`POST /v1/spaces {name, kind: "incidents"}`): a title, the symptom, the cause, the fix, and the alert rule it fired, if one did. Seen before reads every incidents space the signed-in key may read, so a team that keeps its own sees its incidents beside the shared ones, and log lines pasted from an outage find the incidents that look like them as an alert does. For each firing alert, it searches the incidents with what the alert says (its name, metric, description and the copies it fires on). The incidents recorded for the same rule come first; another must be close in meaning (cosine 0.3, or 0.2 when it shares a word) and at least half as close as the closest. A match shows the fixes it replaced struck through, so an agent does not repeat one that was given up.

**Hand to the on-call agent** starts the cluster's on-call agent (`POST /v1/admin/oncall`) on the alert with the closest incidents and the incident space's rules in its question; the server needs a Claude key for it. When the run ends, **keep what it found** drafts an incident from its report for a person to check and record. A postmortem pasted into **Record an incident** is drafted the same way.

## Create an app from a space

A space's page has **Create an app from this context**. **Suggest apps** asks the model for apps the space's notes could become (one call, on at most 150 notes); each says why, and which kinds of notes it reads. **Build this...** opens the build: its name, the request the app engine gets (editable), the kinds of notes, and every note that goes in, each with a tick to leave it out. Before anything is spent it shows what a build costs, the median of the engine's past first builds (`sa_requests`), and what is left of the engine's cap; the build starts only on **Build it**, which sends `confirm: true`.

The cluster's app engine (the app `apps`, [chloedb-apps](../../chloedb-apps)) builds the app from the request and leaves out its example rows. When it is done, ChloeCtx maps the picked notes, 30 at a time, onto the new app's tables with its model and inserts them through the engine; a row that names a note it was not given, or a table the app does not have, is dropped. The app gets copies of the picked notes and nothing else: no exchanges, no notes of other spaces, and later notes do not follow. Builds are rows of `ctx_builds`, and a restart follows those still building. **Open in the studio** asks the engine for a one-time link to the app.

## Tests

`chloectx/tests/app_cluster.rs` runs the app against two shard servers and a coordinator of this build, with a stub of Anthropic's API: spaces, notes replaced and retired, contexts within a budget, an exchange distilled, incidents matched to an alert rule that fires, a handoff refused without a key, an app suggested and built from a space against a stub of the app engine (refused without `confirm`), the inspector's overview, sessions and records of a space and of a local store, a capsule cut, and a restart. CI runs it in its server smoke test:

```bash
cd chloedb-server && cargo build --profile ci
cd ../chloectx && CHLOECTX_NEED_SERVER=1 cargo test --profile ci --test app_cluster
```

`CHLOECTX_UI_DIR=chloectx/ui` serves the page from disk, without a rebuild.
