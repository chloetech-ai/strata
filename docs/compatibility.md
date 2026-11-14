# Compatibility

What this code promises about the files it reads and writes, the protocol
its servers speak to each other, and the names it was known by before; and
what it does not promise yet. It describes the code as it is on `main`
(November 2026, every crate and package at version 0.3.0). The
[changelog](../CHANGELOG.md) says what changed.

ChloeDB is pre-1.0: the statements below are the current behaviour, which
the code enforces, not a support commitment with a time limit.

## The database file

A database is one file (or, with `directory`, a directory with a `CURRENT`
file naming its manifest). Its format has two layers.

**Format version.** The file's meta slots carry a format version
(`FORMAT_VERSION` in `chloedb/src/storage/pager.rs`, now 3):

| format | what it is |
|---|---|
| 1 | a database holding only the table it was created with |
| 2 | a database of several tables |
| 3 | format 2 with the list of features the database uses; the last format number: later changes add features instead |

This code opens formats 1 to 3 and refuses a higher number
("unsupported format version").

**Features.** A format-3 manifest lists the features it uses,
`{required: [...], optional: [...]}` (`chloedb/src/storage/manifest.rs`).
A reader refuses a database that requires a feature it does not know, and
names the feature; it ignores optional features and keys it does not know.
This code knows: `added_fields`, `bool`, `cluster_by`, `cold_segments`,
`compact_keys`, `default`, `derive`, `derive_keyword`, `derive_text`, `edge_table`,
`expire_by`, `field_ids`, `functions`, `json`, `json_text`, `media`,
`multi_vector`, `not_null`, `packed_positions`, `rank_profiles`, `rabitq`,
`retain`, `semantic_id`, `seq_no`, `shard_by`, `sources`, `sparse_embed`,
`substring:1`, `timestamp`, `vector_partition` and `zstd` (the list `FEATURES` in
`manifest.rs` is the authority), and `analyzer:<name>:<version>` for each
analyzer but `english` (`Analyzer::feature` in `chloedb/src/text/analyzer.rs`).

0.3.0 added `analyzer:<name>:1` for its new analyzers (`simple`'s, from
before, is unchanged), `substring:1` (a substring index), `sparse_embed` (a
SPARSE field a model fills) and `derive_keyword` (a KEYWORD field derived
from a text). Each is written only by a database that uses it: a 0.2.x build
refuses such a database by name and opens every other database 0.3.0
wrote.

**What it writes.** Every database this code creates is format 3 with
feature `field_ids` (each field named by an id rather than its position).
Other features are added as a database starts using them (a BOOL or
TIMESTAMP field, a NOT NULL field or a DEFAULT, sequence numbers with
`--seq-no`, a rank profile, ...). So a database stays readable by an older
build exactly as long as it uses only features that build knows, and an
older build that meets a newer feature refuses the file by name rather than
misreading it. Starting to use a feature cannot be undone.

**Rollups.** A rollup made by 0.3.3 or later keys its rows by number,
where one made by 0.3.1 or 0.3.2 keys them by text. 0.3.3 reads and
refreshes both. 0.3.2 reads the new kind but cannot refresh it, so its
definition carries the option `needs_chloedb_0_3_3`, which 0.3.2 refuses
by name ("no option needs_chloedb_0_3_3"): a refresh by 0.3.2 fails with
that message rather than leaving the rollup's newest buckets stale. Open a
database whose rollups 0.3.3 made with 0.3.3 or later.
A rollup made by 0.3.4 or later that counts a field's values (`COUNT(f)`,
or `AVG(f)`, which keeps `count_f` so that its mean skips NULLs) has a
column 0.3.3 does not write; its definition carries `needs_chloedb_0_3_4`,
which 0.3.3 refuses by name in the same way. 0.3.4 refreshes a rollup made
before with `AVG(f)` as it was made, without `count_f`.

**Databases from before field ids.** A database written before field ids
(format 1, 2, or 3 without `field_ids`) no longer opens for use. `upgrade()`
(`chloedb.upgrade(path)` in Python, `upgrade(path)` in JavaScript;
`chloedb/src/upgrade.rs`) copies it
into a new database of this format in its place and keeps the old files
beside it as `<path>.before-field-ids`. It refuses a database that numbers
its writes (replication) or keeps stream offsets (`_sources`), since the
copy could not continue their numbers: such a database is rebuilt from its
source or from a replica instead.

**Not promised.** There is no downgrade: a file a newer build changed may
not open in an older one. There is no stated period for which an old
format stays readable by new builds; formats 1 to 3 and the `upgrade()`
path are what this code has, and a change to that will be in the
changelog.

## The packages and the engine

The Python wheel, the JavaScript addon and the Java library each compile
the engine (`chloedb/`) into themselves from the same commit. A package
version therefore opens what the engine of its commit opens (above), no
more: a wheel built from an older commit refuses a file that uses a feature
added since, by name. The Java jar checks that its native library is from
the same checkout (`Native.ABI`) and refuses a mismatched pair.

The HTTP clients (`chloedb.connect` in Python, `chloedb/client` in
JavaScript) contain no engine: they speak the server's `/v1` HTTP JSON API
and work with any server that has the routes they call. A client newer than
its server may send an option the server does not know, and an older
server may ignore it rather than refuse it (one from before Arrow answers
ignores `"format": "arrow"`): the clients detect and fall back where their
documentation says so (Arrow answers, scrolls), and otherwise assume a
server of their own commit.

**Not promised.** A package does not yet expose the engine commit or the
format version it was built with, and nothing checks that a client and a
server are of the same release. Every crate and package carries the
version of the last release (0.3.0), and keeps it between releases: name a
build from a commit after a release by its commit.

## A cluster of mixed versions

Servers of a cluster (coordinators, shards, replicas, followers) talk over
gRPC with a numbered shard protocol (`chloedb-server/src/grpc.rs`:
`MIN_PROTOCOL` 1, `PROTOCOL` 21; [the list of what each adds](chloedb/server.md)).

- When a coordinator or a peer connects it calls `Hello` with the range it
  speaks (1 to 20) and the server answers its own range, the plan version
  it reads (plan IR version 1) and the plan features it has. The two use
  the highest protocol both speak; if the ranges do not meet the connection
  is refused, naming both ranges. A coordinator asks again every minute and
  after a copy fails, since it may have restarted on another version.
- A coordinator keeps each shard copy's protocol, speaks the oldest any of
  them speaks, and uses a newer call only where every server it needs
  speaks it. Where one does not, it refuses
  that request by name ("a coordinator refuses a scroll while a shard
  speaks an older one") or falls back (rows instead of Arrow columns; a
  read sent to the primary when a replica is older than protocol 5) rather
  than sending a call the shard would misread.
- A server refuses a protocol newer than its own, and a plan of a newer
  version or one that needs a feature it lacks, by name.
  0.3.0 added the plan feature `terms_syntax`, which only a search with
  query expansion (`bm25(.., expand => n)`) uses: a 0.2.x shard refuses
  such a search by name, so upgrade the shards before using it.
  0.3.1 added the plan features `asof_join` (`ASOF JOIN`), `date_bin` (a
  `GROUP BY` of `date_bin` or `date_trunc` run on the columns) and
  `approx_percentiles` (`approx_percentile`): a 0.3.0 shard refuses a
  query that uses one by name, so upgrade the shards first here too.
  0.3.2 moved the shard protocol to 21 (write receipts' counts, strict
  writes, `insert_if_absent`) and added the plan feature `full_join`
  (`FULL JOIN`): a coordinator counts and writes strictly only
  through shards of 21, and a 0.3.1 shard refuses a full join by name, so
  upgrade the shards before the coordinator.

So a rolling upgrade works shard by shard: while versions are mixed, the
cluster serves what the oldest server involved speaks, and the newer
features turn on as the last server is upgraded (some, such as slot maps,
only once the coordinator restarts after that). A replica is sent copies
of its primary's rows and files, so the file-format rules above apply
between them too: a replica older than its primary refuses a feature the
primary started using. Upgrading the replicas of a shard before its
primary avoids that.

**Not promised.** There is no stated lowest version a cluster can be
upgraded from in one step, and mixed-version clusters are tested for the
protocol fallbacks above, not as a matrix of releases.

## Old names

ChloeDB was called Strata, and ChloeCtx was called Engram, until November
2026. The old names below are still accepted, and are promised "for a
release": they will be removed in a release that the changelog names in
advance.

| what | new name | old name, still accepted |
|---|---|---|
| programs | `chloedb-server`, `chloedb-crawl`, `chloedb-sheets` | `strata-server`, `strata-crawl`, `strata-sheets`, in the image and the release archives |
| container image | `ghcr.io/chloetech-ai/chloedb-server` | `ghcr.io/chloetech-ai/strata-server` |
| settings | `CHLOEDB_*` | `STRATA_*` |
| the server an app writes to | `chloedb-crawl --server URL`, `chloedb-sheets --server URL` (0.3.2) | `--strata URL` |
| models folder | `~/.local/share/chloedb/models` (0.3.2) | `~/.local/share/strata/models`, used where it exists and the new folder does not |
| HTTP headers and gRPC metadata | `x-chloedb-*` | `x-strata-*` |
| Python package | `import chloedb`, `ChloeDBError` | `import strata`, `StrataError` (with a `DeprecationWarning`) |
| JavaScript errors | `ChloeDBError` | `StrataError` |
| JDBC URLs | `jdbc:chloedb:` | `jdbc:strata:` |
| ChloeCtx program | `chloectx` | `engram`, a link to it |
| ChloeCtx settings | `CHLOECTX_*` | `ENGRAM_*` |
| repository | `chloetech-ai/chloedb` | `chloetech-ai/strata` and `rkonow/engram`, which GitHub redirects |
| website | https://chloetech-ai.github.io/chloedb/ | the old address does not redirect |

For the server in particular: when both a `CHLOEDB_*` setting and its
`STRATA_*` name are set, the new one wins; an answer carries its headers
under both names (`x-chloedb-took-ms` and `x-strata-took-ms`); the browser
UI moves what it saved under `strata.*` to `chloedb.*` the first time it
loads; the MCP server introduces itself as `chloedb`, and the OpenSearch
API's cluster is named `chloedb`. The Kubernetes operator's pods start the
programs by their old names, with `STRATA_*` settings, which every image
understands.

The metrics were renamed outright, from `strata_*` to `chloedb_*`: a
dashboard over servers from before and after the rename can match
`{__name__=~"(chloedb|strata)_.*"}`. So were the alert messages, which begin `[chloedb FIRING ...]`
rather than `[strata FIRING ...]`.

Some names have not been renamed yet and keep the old spelling: the file
format (`STRATADB` magic, ChloeCtx's `memory.strata` and `engram.sock`), the
gRPC services, the Kubernetes operator's resources (the `StrataCluster`
kind, the `strata.chloetech.ai` API group, the `strata-system` namespace),
and the app engine in `chloedb-apps/`. The default models directory is
`~/.local/share/chloedb/models` from 0.3.2 (`~/.local/share/strata/models`
before); a machine that has the old folder and no `~/.local/share/chloedb`
keeps using it, as a machine that already has ChloeCtx's old folder,
`~/.local/share/engram`, does.

## Experimental features

These parts of chloedb-server work and are documented, but carry no
compatibility promise yet: their routes, settings and what they store may
change in any release, including a patch release, with no upgrade path
promised. Each answer of their routes carries the header
`x-chloedb-experimental: <feature>`, `GET /v1/catalog` marks their routes
(`"experimental": "<feature>"`, and the features under `experimental`), and
the command center marks their pages.

| feature | routes | what it is |
|---|---|---|
| `cloud` | `/v1/admin/cloud/*`, `/v1/admin/aws/*`, `/v1/admin/digitalocean/*` | [clusters made on AWS or DigitalOcean](chloedb/server.md#in-the-cloud) from the command center |
| `hub` | `/v1/admin/hub/*`, `/c/<name>/` | [the hub](chloedb/server.md#many-clusters-from-one-command-center): a book of other clusters, and calls forwarded to them |
| `autoscale` | `/v1/admin/kube/autoscale/*`, `/v1/admin/autoscale/*`, the `StrataCluster`'s `spec.autoscale` | [the operator's autoscaler](chloedb/server.md#autoscaling) |
| `oncall` | `/v1/admin/oncall/*` | [the on-call agent](chloedb/server.md#the-on-call-agent) |

The list is `EXPERIMENTAL` in `chloedb-server/src/catalog.rs`. A feature
leaves it when its shape is settled; the changelog says so.

## Demos

These apps show what ChloeDB can do and are not part of its releases or
their promises:

- `chloedb-netops`, network operations on a simulated carrier network;
- `chloedb-swarm`, simulated users, an internal test instrument;
- `examples/feature-store`, a shop ranked on features ChloeDB keeps;
- `examples/parley`, a social network of LLM agents.

They are on a release train of their own. CI's required check, `CI result`,
does not test them, and a change only under them runs nothing there; the
`Demos` workflow (`.github/workflows/demos.yml`) tests them against a
chloedb-server of the same commit, on a pull request that touches one and
every night. A fault in a demo does not hold a ChloeDB release, and fixing
one does not wait for the engine's tests. An app whose manifest says
`"maturity": "demo"` is marked as a demo in the command center.

## Not promised yet

- Semantic versioning, or any version bump rule.
- A support period for releases; fixes go to `main` and the next release
  ([SECURITY.md](../SECURITY.md)).
- That a release tag is never rebuilt: the release workflows replace the
  assets of an existing tag (`--clobber`). Check a download against the
  release's `SHA256SUMS` (Python, JavaScript) or `.sha256` (server).
- Stability of anything not described above: the Rust crate's API, the
  command center's routes, the operator's resource beyond its documented
  fields.
