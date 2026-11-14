# Receipts and `chloectx blame`

ChloeCtx puts records from earlier work into an agent's context: on every
prompt, after a compaction, and when the agent calls `memory_search`. Each
of those recalls now writes a **receipt**, and the store is **kept as a
snapshot** after a recall that showed something, so any recall can be
replayed later on exactly the data it read. `chloectx blame` starts from a
line of code and walks back to the edit that wrote it, the prompt behind
the edit, and the receipts of what memory had put in the agent's context
before it.

```text
$ chloectx blame src/payments/retry.rs:88
line        sleep(backoff_delay(attempt));
commit      3f9a2c1b0e 2026-11-10 14:05 UTC · Retry with the new backoff
edit        2026-11-10 14:02 UTC · session 7f3a91c2 turn 4 · 7f3a91c2-…:212 (the newest recorded edit whose new text holds the line)
prompt      use the new backoff policy for payment retries
before it   3 tool calls in the turn:
              Read src/payments/retry.rs (120 lines)
              $ rg backoff src/payments
              Read src/payments/policy.rs (40 lines)

recalled in the edit's turn: receipt rc-19a6… · prompt · Inject · store commit 48211
    #  kind        record                      score    cos   tok  decision
    k  decision    1c2e…:31                    0.812  0.812    35  knowledge  ← superseded since, by 1c2e…:58
    2  preference  1c2e…:12                    0.640  0.700    15  take
    3  tool_result 88aa…:90                    0.520  0.610   310  excerpt  ← untrusted source: the web

replay of rc-19a6…:
  on the snapshot kept at commit 48211 (auto-48211): identical: the same rows, scores and decisions
  on today's store (commit 49007): different
      1c2e…:31 (decision, knowledge) is no longer returned
      1c2e…:58 (decision) is new: knowledge

worth a look:
  - 1c2e…:31 (decision) was in context before the edit and is superseded since, by 1c2e…:58
  - 88aa…:90 (tool_result) was in context before the edit and is untrusted source: the web
  - the same request today recalls different records
```

(The values above are made up to show the layout.)

## What a receipt holds

| field | what |
|---|---|
| `id`, `ts`, `session`, `turn`, `source` | which recall: `prompt`, `compact` or `search` |
| `snapshot` | the store's commit the recall read (`txn`, and `seq_no` in a store that numbers its writes), the newest record's number (recency is measured from it) and the record count |
| `inputs` | everything else the result depends on: the query, the session's live window, the records already shown to the session, and the ranking, gate, packing and knowledge parameters as they were |
| `decision`, `gate`, `evidence` | the gate's score and what it decided (`Inject`, `Brief`, `Skip`) |
| `rows` | each candidate (up to 40): its rank, final score, raw BM25 and cosine, importance, distance from the newest record, the tokens it took or would have taken, a hash of its text, and the decision |
| `text`, `text_sha`, `tokens` | the context the agent was given, as given |

A row's decision is one of `knowledge` (a decision, rule or fact shown on its
own text, ahead of the gate), `take`, `excerpt` (taken as a snippet),
`listed` (named by id under "Further candidates", or a `memory_search`
result), or `skip: ` with the reason: `shown before`, `over budget`,
`per-turn cap`, `record cap`, `uncertain`, `same question`, `shown as
knowledge`, `the gate held back`, `below the limit`. The words are those of
ChloeDB's `PACK` receipts, so a ChloeCtx receipt and a `PACK ... WITH
RECEIPT` answer read alike, and tokens are counted as `PACK` counts them
(`chloedb::query::pack::count_tokens`).

Receipts live in their own ChloeDB file beside the store,
`receipts.strata` (table `receipts`). The store's file and its format are
unchanged. A receipt is written once the hook has answered, so it adds no
write to the prompt's critical path. The file is flushed every 256
receipts and when the daemon stops, so its write-ahead log (replayed when
a daemon starts) stays small, and receipts older than 90 days, or past the
newest 20,000, are dropped when the daemon starts.

## Kept snapshots

After a recall that showed records, once the hook has answered and before
anything else is written, the daemon keeps a snapshot of the store, at most
once an hour (`min_interval_secs`, 3600 by default):
`snapshots/auto-<commit>/` in the store's directory, a checkpoint in
ChloeDB's directory layout. It is not taken when the store has not changed
since the last one.

What a snapshot costs:

- **Time.** The checkpoint flushes the store's memtable, then reads every
  segment whole and checks its checksums, and checks the copy: a few
  passes over the store, whether or not a segment is linked. Tenths of a
  second for a store of tens of MB; seconds for one of a GB. This runs
  after the hook has answered; when it runs, the prompt's own record is
  stored after it, and `chloectx stop` waits for both.
- **Space.** Each snapshot hard-links the segment files of the one before
  it that no merge has replaced since, and writes the rest. The live store
  is a single file, which a directory snapshot cannot share bytes with, so
  the first snapshot is a full copy, and every segment a merge writes is
  copied again by the next snapshot while older ones keep its inputs. The
  store's smallest merge tier spans up to 8,000 live rows, so each
  snapshot's flush makes a small segment that is merged again and again
  within it, and each snapshot keeps one of those outputs. With no budget,
  a simulation of the merge policy at 50 turns a day, 60 records a turn
  and hourly snapshots over an 8-hour day (the fourth audit, not a
  measurement) gave snapshots of about 3 times the store after a week
  (143 MB beside 46.5 MB) and 4 times after 30 days (1.16 GB beside
  279 MB); taken after every turn (`min_interval_secs: 0`), about 10
  times after a week. On a file system where hard links fail (some
  network and FUSE mounts, exFAT), every snapshot is a full copy, and
  `chloectx doctor` says so.

So the automatic snapshots are kept to a budget: at most the store's size
(`budget_store_ratio`, 1.0; at least 32 MB; `max_bytes` sets one in bytes,
and 0 for both sets none), counting each file once however many snapshots
link it, and counting named snapshots too, which are never dropped for it.
Past it, the oldest automatic ones go, never the newest. Automatic
snapshots older than 3 days are thinned to the last of each day
(`daily_after_days`; 0 keeps them as taken). Those older than `keep_days`
(30), or past the newest `max_auto` (2,000), are dropped too. All of this
runs when the daemon starts and before each new snapshot, whatever the
settings say now. Anything under `snapshots/` that no snapshot owns (a
checkpoint a stopped daemon left half written) is removed when the daemon
starts. `chloectx stats` shows the bytes of the store, the receipts and the
snapshots, and the budget (`receipts`, `snapshots`, `total_bytes`), and
`chloectx doctor` says them in one line, `disk`.

A receipt replays exactly on the snapshot kept at its commit: the replay
runs the same retrieval code (`receipt::retrieve`, which the recall itself
runs) with the receipt's inputs, and must give the same rows, scores and
decisions. When no snapshot was kept at that commit (a recall that showed
nothing, or one inside `min_interval_secs`), it replays on the nearest
later one and says so. With hourly snapshots that is most receipts: about
one in six replays exactly at 6 recalls an hour, and the rest on the next
snapshot, whose store holds the records written since; after 3 days only
the last snapshot of each day is left, and after the budget fills, fewer. It is also replayed on today's store, which shows
what has changed since: a record superseded, deleted or outranked.

`chloectx snapshot keep NAME` keeps one by name, until `chloectx snapshot
drop NAME`; `chloectx snapshot list` lists them.

Settings, in the store's `config.json`:

```json
{"receipts": {"enabled": true,
              "snapshots": {"after_recall": true, "min_interval_secs": 3600, "keep_days": 30, "max_auto": 2000},
              "retention": {"keep_days": 90, "max_receipts": 20000}}}
```

`chloectx receipts off` turns receipts and automatic snapshots off for the
project's store (it writes `receipts.enabled` in its `config.json`), and
`chloectx receipts off --purge` also removes the receipts and automatic
snapshots kept so far; `chloectx receipts on` turns them back on. Without
`receipts.enabled` in `config.json`, `CHLOECTX_RECEIPTS=0` in Claude Code's
environment turns them off: each hook sends it to the daemon, whichever
process started the daemon. With receipts off, `memory_forget` and
`chloectx redact` still scrub the receipts kept before.

## Commands

```bash
chloectx blame src/retry.rs:88       # or a whole file: chloectx blame src/retry.rs
chloectx blame src/retry.rs:88 --json --no-replay
chloectx receipts [--session S] [--limit N]
chloectx receipts off [--purge] | on
chloectx receipt rc-19a6… [--text] [--no-replay] [--json]
chloectx snapshot keep before-release | list | drop before-release
```

`blame path:line` reads the line from a regular file inside the project,
after links and `..` are resolved, of at most 8 MiB; any other path is
refused, so neither the command center's Blame tab nor the daemon's socket
can be used to read a file elsewhere.

Every `memory_search` answer ends with its receipt's id, so an agent (or
the person reading its transcript) can ask why a record ranked where it did.

## In the command center

ChloeCtx's page in ChloeDB's command center (`chloedb-server --app
chloectx=URL`, then Apps → ChloeCtx) lists every space it can read; a
Claude Code store of the machine the app runs on opens with three more
tabs (Receipts, Blame, Snapshots and capsules) beside Overview, Ask, Memories, Sessions, Graph and Connect
(`#/app/chloectx/store/<store>/receipts`, `…/blame/<path:line>`):

- **Receipts**: the store's newest receipts (when, which recall, what it
  was for, records and tokens shown, the gate's decision, the session and
  turn). Open one for its candidates (rank, score with a bar,
  BM25, cosine, tokens, decision), the query and the gate, the text the
  agent was shown, and the replay on the kept snapshot and on today's
  store. The kept snapshots are under **Snapshots and capsules**: keep one
  by name, drop a named one, or cut a capsule (pick the kinds of record)
  with the `claude mcp add` line that mounts it.
- **Blame**: type `path:line`. It shows the line, its commit, the edit, the
  prompt and the tool calls before it, the notes from the turn, each
  receipt of what memory had recalled before the edit (with what has become
  of each record since), the replay, and a "worth a look" list. Each
  receipt links to its page under Receipts.

They read the Claude Code stores of the machine `chloectx app` runs on, so
they are off unless it is started with `--local-stores` (or
`CHLOECTX_APP_LOCAL=1`): a store is a transcript of its sessions, and every
user of the command center could read it. Without it the tabs say how to
turn it on. A store whose daemon is stopped is started when read. The API
behind them, under the app's prefix (`/v1/apps/chloectx`):

| request | answer |
|---|---|
| `GET /v1/local` | this machine's stores |
| `GET /v1/local/:store/receipts?session=&limit=` | the newest receipts |
| `GET /v1/local/:store/receipts/:id?replay=0` | one receipt, replayed unless `replay=0` |
| `GET /v1/local/:store/blame?target=path:line` | blame, as `chloectx blame --json` |
| `GET`, `POST {name}` `/v1/local/:store/snapshots`, `DELETE /v1/local/:store/snapshots/:name` | the kept snapshots; keep one; drop one |
| `GET`, `POST {title?, kinds?, since_days?}` `/v1/local/:store/capsules` | the capsules cut from the store, newest first; one cut, its manifest and folder |
| `GET /v1/local/:store/overview`, `/sessions`, `/records`, `/notes[/:id]`, `/graph`, `POST /v1/local/:store/ask` | what the other tabs read, as a team space's calls of the same names answer ([the app](app.md#spaces-and-what-each-shows)) |
| `GET /v1/local/:store/stats` | `chloectx stats --json` |

## How blame finds the edit

Blame reads the line from the file and looks among the stored tool calls
(`Edit`, `MultiEdit`, `Write`, `NotebookEdit`) of that file for the newest
that added it: the line is a whole line of the edit's new text and not of
the text it replaced (an edit that only kept the line around its change
did not write it). A relative path is taken from where `chloectx blame`
runs; in a request to the daemon, from the project. When none does (the line was written outside a
recorded session, or changed by hand since), it takes the newest edit of
the file before the line's commit (from `git blame`), and says which it
did. Stored edits keep up to 1,100 characters of their new text, so a line
deep inside a long `Write` may only be found this second way.

The edit's turn gives the prompt, the tool calls before the edit and the
notes distilled from the turn. The receipts of that turn, and of up to
three earlier turns of the session that showed records, give what memory
had put in context. Each record shown is checked against the store as it
is now: deleted, superseded (and by what), or from an untrusted source (a
tool's output; the web for `WebFetch` and `WebSearch`).

## Forgetting and redaction

A receipt and a snapshot hold copies of what was recalled. So `memory_forget`
(and the `forget` request) scrubs the text of the receipts that showed the
forgotten record (their rows keep ids and scores, and lose the record's
text hash), removes a forgotten prompt from the query of the receipts it
was asked as, rewrites the store's segments that held the record without
drops every automatic snapshot, and rewrites the store's segments that
held the record without it, with no snapshot taken until that is done, so
no snapshot taken afterwards holds its bytes. Only the segments that held
it are rewritten (those whose live rows fell with the delete, and those a
flush or a merge wrote since that still hold it, marked deleted), not every
segment with an older delete, and then the free pages of the store's file,
where the old segments were, are written with zeros
(`Database::zero_free_pages`); pages an open snapshot still reads are left,
and said. `memory_forget` answers before the rewrite, which runs after the
answer, and `chloectx forget` waits for it. Named snapshots are kept and
listed, for you to drop.
`chloectx redact` redacts the receipts as it does the records, and drops
the automatic snapshots too. Replays of the receipts before then can no
longer be exact.
