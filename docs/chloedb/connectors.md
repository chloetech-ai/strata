# Connectors

A connector brings another system's data into a table of `chloedb-server`, and keeps it coming: it reads the system on a schedule (a pull) or takes what the system sends (a push), turns each record into a row by a map, and upserts the rows by the table's key, so a record read twice is the same row. Connectors are configured as rows of a table, each writes with a key limited to its own target, and the command center's **Data > Connectors** page shows each one's health.

| kind | pull or push | what it brings in | default table |
|---|---|---|---|
| `prometheus` | pull | PromQL queries over each new step (`query_range`), joined by labels into one row per subject and minute: the rollups an incident needs, the full series staying in Prometheus (or Thanos, Mimir, VictoriaMetrics) | `prometheus_rollups` |
| `jira` | pull | issues matching a JQL query updated since the last run (Jira Cloud's `/rest/api/3/search/jql`, or Data Center's `/rest/api/2/search`) | `jira_issues` |
| `servicenow` | pull | records of a table (`change_request`, `incident`, `problem`, ...) updated since the last run, through the Table API | `servicenow_records` |
| `oxidized` | pull, and push from its hook | device configurations from oxidized-web: each node whose backup changed is fetched and kept as a version | `device_configs` |
| `mail` | push | emails handed over raw: headers, text, and the maintenance fields carriers attach (the BCOP `X-MAINTNOTE` iCalendar properties: provider, maintenance id, circuits, window, impact, status) | `mail_messages` |
| `webhook` | push | any system that posts JSON; preset `alertmanager` for Alertmanager's webhook | `webhook_events`, `alertmanager_alerts` |
| `http_json` | pull | any REST API that answers JSON: records at a path, the next page at another, a cursor kept from the records | `http_records` |

Other ways rows come in stay as they are, and the Connectors page lists their sources beside the connectors: OpenTelemetry logs (`POST /v1/logs`, [server.md](server.md#opentelemetry-logs)), streams with offsets applied once (`POST /v1/stream`, which Kafka consumers use), Postgres by logical replication ([`follow-postgres`](postgres.md)), and files ([SQLite](sqlite.md), [Parquet and S3](parquet.md)).

## Make one

`POST /v1/admin/connectors` with an admin key:

```json
{
  "name": "jira-changes",
  "kind": "jira",
  "target": "netops_changes",
  "every_s": 300,
  "config": {"url": "https://example.atlassian.net", "jql": "project = NET AND issuetype = Change", "username": "bot@example.com",
             "fields": {"window_start": "customfield_10050", "window_end": "customfield_10051"}},
  "secrets": {"token": "<an Atlassian API token>"}
}
```

- `name`: 1 to 48 lowercase letters, digits, `-` and `_`.
- `target`: the table it writes. Left out, the kind's own table, which is made then (its columns are in the table above's [defaults](#default-tables-and-presets)). A table that does not exist and is not the kind's own is refused, unless `config.create_target` is `true`, which makes it with the columns of the connector's own `map` when it has one (each typed by its `as`: `int` an `INT`, `float` a `FLOAT`, `bool` a `BOOL`, `time` or `{"now": true}` a `TIMESTAMP`, `list` a `KEYWORD[]`, `json` a `JSON`, `hash` a `KEYWORD`, any other `TEXT`; `id` its primary key, or none without an `id`), else with the kind's columns.
- `every_s`: seconds between pulls (at least 10; 0 runs only when asked). The defaults: 60 for Prometheus, 300 for Jira, ServiceNow and HTTP pulls, 900 for Oxidized. A failed pull waits twice as long each time, up to an hour; one that stopped at its limit (20,000 records a run) runs again at once.
- `config`: the kind's settings (below) and an optional `map`.
- `secrets`: passwords and tokens, by name. They are never written to a table nor answered: the server keeps them in `<db>.connectors.json` beside the database, readable by its user only (`--connector-secrets FILE`; a coordinator without one keeps them in memory, so give it one, or name each secret as `"env:VARIABLE"`, which the server reads from its environment at each run, as a Kubernetes Secret gives it). An `env:` secret names a variable whose name starts with `CHLOEDB_CONNECTOR_SECRET_` (`--connector-env-prefix` chooses another prefix), so that no connector reads the server's own tokens, model keys or cloud credentials; another name is refused with a 400.
- `enabled` (default `true`) and `note`.

The answer has the connector, `made_target`, and for a push connector its key's secret, `push_secret`, shown this once.

| | |
|---|---|
| `GET /v1/admin/connectors` | every connector with its `health`, `state` (cursor, last run, last ok, failures in a row, runs, rows written) and last 12 runs; the kinds and their settings; whether pulls run on this server (`scheduler`); the stream sources |
| `GET /v1/admin/connectors/NAME` | one, with its last 50 runs and the map it writes with |
| `POST /v1/admin/connectors/NAME` | changes the settings the body gives (`secrets` are merged, a `null` taking one out) |
| `DELETE /v1/admin/connectors/NAME` | its config, state, secrets and push key; the rows it wrote stay, and its runs for their 30 days |
| `POST /v1/admin/connectors/NAME/run` | a pull now; answers what it did |
| `POST /v1/admin/connectors/NAME/key` | a new push key in place of the old one, which stops working now |
| `POST /v1/admin/connectors/test` | a dry run: what a connector would read and the first ten rows it would write, nothing written. The body is a connector's (`kind`, `target`, `config`, `secrets`, given as values: an `env:` one is refused; or `name` alone for a saved one), with `sample`, the body of a push, for a pushed kind |
| `POST /v1/connectors/NAME/push` | a system's push: any body (JSON, or a raw email), written with the sender's key |

### Health

| health | |
|---|---|
| `ok` | the last run went well (and, for a pull, within three times its interval or 15 minutes) |
| `invalid` | its row fails the API's checks (`invalid` says why): it does not run |
| `failing` | the last run failed: `state.last_error` says why, `state.failures` how many in a row (an error names a URL without its query, and a secret's value nowhere: it reads `***`, since a read key reads these tables) |
| `stale` | a pull whose last good run is older than that |
| `pending`, `waiting` | a pull not run yet; a push connector nothing was pushed to yet |
| `running`, `disabled` | |

## Configured as rows

Each connector is a row of `chloedb_connectors` (`id` its name, `kind`, `target`, `enabled`, `every_s`, `config` as JSON text, `push_key` its key's id, `note`, `created`, `updated`), each run a row of `chloedb_connector_runs` (trigger, started, took, ok, records read, rows written, records that made no row, columns left out, error, cursor after; kept 30 days), and each connector's cursor and counts a row of `chloedb_connector_state`. The tables are made with the first connector, with `updated_by`, who changed the config last. SQL reads them, and only the server writes them: a statement or a write that changes one of them (`INSERT`, `UPDATE`, `DELETE`, `CREATE`, `ALTER`, `DROP`, `/v1/write`, `/v1/kv`) answers 403 `server_owned` for every key, an admin's too, since a row would choose the URL a connector sends its credentials to and the table its run's key writes. Change a connector through the API (`POST /v1/admin/connectors/NAME {"enabled": false}` turns one off), which checks it and writes the change in the audit log. The schedule reads `chloedb_connectors` again every 10 seconds and checks each row as the API does: a row that fails (one written with the server's own token, say, or by an older server) is shown with health `invalid` and why, and is neither pulled nor pushed to until an admin changes or deletes it.

```sql
SELECT connector, COUNT(*) AS runs, SUM(rows_) AS rows, AVG(took_ms) AS ms
FROM chloedb_connector_runs WHERE ok = false AND started > '2026-11-13' GROUP BY connector;
```

## What a connector reaches

A connector asks its system from inside the database's network, so by default it reaches the public internet only: an address on the server's own machine, a private network (10/8, 172.16/12, 192.168/16, 100.64/10, fc00::/7) or another reserved range is refused, checked on each URL and again on each address a name resolves to. A system inside the network is reached when the connector's config says so, `"allow_internal": true` (a Prometheus at `http://prometheus:9090`, oxidized-web on the management network). Link-local addresses, where clouds answer instance metadata (169.254.169.254, fe80::/10), are refused even then. The hosts of `HTTP_PROXY` and `HTTPS_PROXY` are reachable, as the proxy is the operator's.

A connector's credentials (its basic or bearer auth, and its `headers`, where `{secret:NAME}` is expanded) go to the origin of its `url` (`instance` for ServiceNow) only. It follows a redirect within that origin, five at most; a redirect to another origin fails the run with where it led. An `http_json` next page on another origin is asked without them, and a `{secret:NAME}` in a URL read from an answer is expanded only when the configured `url` names that secret and the page is on its origin.

An answer is read up to 32 MiB (`"max_body_mb"`, 1 to 1024), and a run that gets more fails saying so.

## Keys

A connector writes with a key of its own, never with the server's:

- **A pull** runs inside the server and writes its rows through the server's own API, with a key the server makes for the run (named `connector-run:<name>`, kept in memory only) that may write the target and nothing else; the connectors' own tables are written with another (`connectors`), which may touch only those. So a pull's rows are checked as any client's are, and a map cannot write another table.
- **A push connector** gets a key of the keys file when it is made, named `connector:<name>` and limited to writing its target (a [key limited to tables](server.md#security)). The sending system holds it, and a push is written with the key it came with: one leaked lets its holder write that table, nothing else, and read nothing: the server refuses such a key (by its name, `connector:`) every route but the push, `POST /v1/write` and `GET /v1/health` and `/v1/whoami`, so it cannot query or list even its own table. It is listed in Access and audit, rotated with `POST /v1/admin/connectors/NAME/key`, and revoked when the connector is deleted. On an open server (no token, no key) a push needs no key, and none is made.

## Who runs the pulls

A server of its own, or a coordinator; with several coordinators, the one that sends the alerts' messages (the lowest URL among those that answer), so each pull runs once. A replica or a shard runs none, and `--connectors off` (`CHLOEDB_CONNECTORS=off`) turns them off on any server. A push may go to any coordinator. A pull that runs twice anyway (a coordinator taking over) writes the same rows again, as upserts.

## Maps

A record is the JSON a connector read: a Jira issue, a point of a series, an alert, an email. `config.map` names each column of the row and how to fill it:

| spec | value |
|---|---|
| `"$.fields.summary"` | a path into the record: `$` the record, `.name` or `["name"]` a key, `[0]` an element, `[*]` every element |
| `"{key}-{fields.status.name}"` | a template, each `{path}` replaced by its value as text |
| any other value | itself |
| `{"path" \| "template" \| "const" \| "now": true, "split": ",", "values": {...}, "as": "...", "default": ...}` | the value, then in this order: text split into a list; a value replaced by another, by its text (`"*"` for any other); a conversion (`text`, `int`, `float`, `bool`, `time`, `list`, `first`, `json`, `lower`, `upper`, `hash`, `text_of`); a default when it came out empty |

A column the map does not name is left out; with no map at all (and no preset), the record is the row. The row is then fitted to the target's columns: a column the table does not have is left out and named in the run (`dropped`), a time in any of the forms systems write (ISO 8601 with any offset, ServiceNow's `2026-11-13 03:00:00`, an email's date, iCalendar's `20261113T030000Z`, seconds or milliseconds since 1970) becomes ISO 8601 text for a `TIMESTAMP`, a number in text a number, one value a list for a `KEYWORD[]` or `INT[]`, and an object or list text for a `TEXT` or `KEYWORD`. A row without the table's primary key makes no row, and the run counts it (`skipped`) with the first reason.

```json
"map": {
  "id": "$.number",
  "title": "$.short_description",
  "status": {"path": "$.state", "as": "lower", "values": {"scheduled": "approved", "implement": "running", "*": "planned"}},
  "window_start": "$.start_date",
  "devices": {"path": "$.cmdb_ci", "as": "list"}
}
```

### Default tables and presets

Each kind writes its own table with a map of its own. Some targets of the network operations app have presets, used when the connector writes that table and gives no map:

| kind | target | |
|---|---|---|
| `servicenow` | `netops_changes`, `netops_tickets` | a change's number, window, state as the app's status, CI as its devices; an incident's fields as a ticket |
| `jira` | `netops_changes`, `netops_tickets` | an issue as a change (custom fields named in `config.fields`: `window_start`, `window_end`, `devices`, `risk`, `approver`, `rollback_plan`) or a ticket |
| `oxidized` | `netops_configs` | a version of a device's configuration |
| `mail` | `netops_maintenance` | a carrier's notice: provider, maintenance id as the reference, its first circuit, window, impact and status, the text as `raw` |

A Prometheus connector writes any table whose columns are its query names and key labels: `{"queries": {"cpu": "...", "mem": "..."}, "key": ["device"]}` writes rows `{id, ts, device, cpu, mem}` (`id` the key labels' values and the time), so `netops_device_metrics` takes them as they are (`rename` renames a label: `{"instance": "device"}`).

## The kinds' settings

**prometheus**: `url` (the base URL); `queries` (`{"column": "PromQL"}`); `key` (the labels that name a row's subject; default every label but `__name__`); `rename`; `step_s` (default 60); `lag_s` (how far behind now to stay, default 60, so a step is complete); `backfill_s` (how far back the first run reads, default 3600); `auth` (`basic` with `username` and secret `password`, or `bearer` with secret `token`). A run asks for the steps after the last one it wrote, up to 10,000 steps, and keeps 20,000 records at most: when its series make more, it ends at an earlier step and the next run goes on from there; when one step's series alone are more, the run fails and says so (aggregate them with `sum by` or `topk`).

**jira**: `url`; `jql`; `username` (the account's email: Cloud signs in with it and the secret `token`, an API token; without `username`, `token` is sent as a bearer token, as a Data Center personal access token is); `api` (`cloud` or `server`; default `cloud` for `*.atlassian.net`); `fields` (`{"name": "customfield_10050"}`, read into the record's `custom`); `backfill_days` (default 30). The record: `key`, `url`, `project`, `issuetype`, `summary`, `description` (text, from Atlassian's document format too), `status`, `priority`, `assignee`, `reporter`, `labels`, `components`, `created`, `updated`, `resolution`, `resolved`, `due`, `custom`. Since a date in JQL is read in the account's time zone, a run asks for `updated >= "-Nm"` (minutes on Jira's own clock) with a minute or two of overlap.

**servicenow**: `instance`; `table`; `query` (an encoded query that narrows them); `fields`; `username` and the secret `password` (or the secret `token` as a bearer token); `backfill_days` (default 30). A record is read with `sysparm_display_value=all` and kept flat: each field's display value (a choice's label, a reference's name) and, where it differs, its value as `<field>_value`; a date-time field's value, which is UTC (its display is in the user's zone). A run asks for `sys_updated_on>=javascript:gs.minutesAgoStart(N)`, on the instance's own clock.

**oxidized**: `url` (oxidized-web's); `username` and secret `password` if it is behind basic auth; `max_nodes` (per run, default 200). A pull reads `/nodes.json` and fetches `/node/fetch/<group>/<node>` for each node whose last backup is newer than the one it fetched. The record: `id` (`<node>@<first 12 hex digits of the SHA-256>`), `device`, `ip`, `model`, `group`, `ts`, `version` (the backup's time in seconds, rising per device), `lines`, `sha256`, `body`. Oxidized's exec hook pushes a node as soon as it is stored:

```yaml
hooks:
  chloedb:
    type: exec
    events: [post_store]
    cmd: 'curl -sS -X POST https://chloedb.example.com/v1/connectors/oxidized/push -H "Authorization: Bearer $CHLOEDB_PUSH_KEY" -H "Content-Type: application/json" -d "{\"node\":\"$OX_NODE_NAME\",\"group\":\"$OX_NODE_GROUP\"}"'
```

**mail**: no settings. The body is the raw message (any content type but JSON), or JSON `{"raw": "..."}` (or a list of those). Postfix hands a mailbox's mail over with a pipe (`notices: "|curl -sS -X POST https://.../v1/connectors/carriers/push -H 'Authorization: Bearer sk_...' --data-binary @-"` in `/etc/aliases`), fetchmail with `--mda`, and providers' inbound routes post it. The record: `id` (the Message-ID), `from`, `sender`, `sender_domain`, `to`, `subject`, `date`, `received`, `body` (the first text part, else the HTML part's text), and from a BCOP calendar part `provider`, `account`, `maint_id`, `circuits`, `start`, `end`, `impact`, `status`, `calendar`; without one, `provider` is the sender's domain's name. A notice in another format keeps its text in `body` for a parser that reads it.

**webhook**: `records` (the path to the list of records in a request; default the body, or its list); `preset: "alertmanager"` (records `$.alerts`, each alert a row keyed by its fingerprint and start, its labels and annotations as searchable text). In Alertmanager:

```yaml
receivers:
  - name: chloedb
    webhook_configs:
      - url: https://chloedb.example.com/v1/connectors/alertmanager/push
        http_config:
          authorization:
            credentials: sk_...
```

**http_json**: `url` (`{cursor}` is replaced by the cursor, URL-encoded); `records`; `next` (the path to the next page's URL, or a path on the same host); `cursor` (the path in a record whose highest value is the next cursor) and `start` (the first one); `headers` (`{secret:NAME}` is replaced by a secret); `auth`, `username`.

## Limits

- One run reads 20,000 records at most (a push too: one of more answers 413, and a push's body is 32 MiB at most, or `--max-body-mb` when that is smaller), and writes them 500 rows (or about 4 MB) at a time; a run that stopped at the limit runs again at once. An `http_json` pull whose cursor does not move its URL on (no `cursor` path, or no `{cursor}` in the URL) keeps the next page's URL in its cursor instead (`{"next": URL, "cursor": ...}`, a secret in it kept as its `{secret:NAME}`), so the next run goes on from that page, and the run that reads the last page clears it. A run that fails partway keeps the rows it wrote and its cursor, so the next one reads them again, as upserts.
- A pull runs for 10 minutes at most; each request to the system waits 60 seconds.
- Pulls are not yet spread over coordinators or shards, and a connector's runs are one at a time.
- Not yet connectors: IMAP mailboxes (hand the mail over instead), Kafka topics read by the server itself (use a consumer that writes `/v1/stream`), and S3 prefixes watched for new Parquet files.
