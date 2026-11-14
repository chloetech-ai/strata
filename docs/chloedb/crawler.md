# chloedb-crawl: a site into a table

`chloedb-crawl` takes a URL, reads the site's pages, finds their fields, makes a table from them and fills it, and adds shards to the cluster as the table grows. It is a program of its own, in Rust, that talks to chloedb-server over HTTP only, so it runs on the machine next to the cluster or on another one; the command center shows it as an app.

## Run it

On one machine, build both programs, then start the crawler with a cluster of its own (on Kubernetes, one field of the cluster's resource starts it: [below](#on-kubernetes)):

```bash
(cd chloedb-server && cargo build --release)
cd chloedb-crawl && cargo build --release
target/release/chloedb-crawl serve --local ./crawl-cluster --server-bin ../chloedb-server/target/release/chloedb-server
```

Open `http://127.0.0.1:7700` and go to **Apps → Crawler**. Type the site's address under **New crawl** and press **Crawl**, or **Preview** first to see the table it would make; **Options** holds the rest of the settings below. Each crawl gets its own page, where you watch it fill the table and grow the cluster, and pause or stop it.

![The Crawler in the command center: the New crawl form, the crawls, and the cluster's shards](../assets/img/strata-crawler-new-crawl.png)

`--local DIR` starts one shard server and a coordinator on `127.0.0.1:7700` with their databases and logs in `DIR`, and adds a shard each time one holds more than `--split-rows` rows of a crawl's table (100,000 by default, at most `--max-shards`, 8). The same `DIR` again serves the same cluster, with the shards it grew.

### From the command line

The same crawl runs without the command center too. `crawl` takes the address, prints its progress every two seconds, then what it did (the table it made and why, the columns it added, the shards it added) and the `CREATE TABLE` it ran; add `--stay` to keep the cluster serving after the crawl. `preview` prints what the form's **Preview** shows, without ChloeDB:

```bash
target/release/chloedb-crawl crawl https://books.toscrape.com/ --local ./crawl-cluster \
  --server-bin ../chloedb-server/target/release/chloedb-server --max-pages 600
target/release/chloedb-crawl preview https://books.toscrape.com/ --pages 10
```

At the default pace, a page a second for one site ([below](#what-it-does-with-a-page)), 600 pages take about ten minutes. books.toscrape.com is a sandbox made for crawlers, so `--per-host 4 --delay-ms 100` is fine there; keep the defaults for sites you don't run.

### Against a cluster that runs elsewhere

The crawler writes to any chloedb-server, one server or a coordinator, with a key that can write (and an admin key to grow a cluster):

```bash
chloedb-crawl serve --server http://coordinator:7700 --key "$CHLOEDB_KEY" --token "$CRAWL_TOKEN" \
  --listen 0.0.0.0:7800 --grow kube --split-rows 5000000
```

`--server` was `--strata` before 0.3.2; the old name still works.

and the coordinator shows it as an app when told where it is, with the crawler's token:

```bash
CHLOEDB_APP_TOKEN="$CRAWL_TOKEN" chloedb-server --shards ... --app crawler=http://crawler-host:7800
```

`--grow` says how the cluster gets a shard: `kube` asks the Kubernetes operator for one more (`POST /v1/admin/kube`, the coordinator must run under the operator), `servers=URL,URL` adds empty servers started for it, by their gRPC URLs, one at a time, and `off` never grows. Each new shard gets its share of the slots by a rebalance (`POST /v1/admin/rebalance`) while the crawl keeps writing.

### On Kubernetes

A cluster the [operator](server.md#on-kubernetes) runs gets a crawler of its own from its `StrataCluster`, with nothing to start by hand:

```bash
kubectl -n strata patch strata demo --type merge -p '{"spec": {"crawler": {}}}'
```

The operator runs `chloedb-crawl serve` from the cluster's own image (it carries both programs) as a Deployment of one pod, `<cluster>-crawler`, behind a Service of that name, and restarts the coordinators so that they list it: **Apps → Crawler** appears in the command center, and a crawl starts from its form. The crawler writes with the cluster's admin token (`tokenSecret`), which its API requires too, so the coordinators send it that token and nothing else reaches it. It grows the cluster through the operator, a shard at a time:

| `spec.crawler` | default | |
|---|---|---|
| `grow` | `true` | add a shard (one more in `spec.shards`) when a crawl's table holds more than `splitRows` rows on one |
| `splitRows` | 100,000 | |
| `maxShards` | 8 | the most shards it grows the cluster to |
| `paused` | `false` | no crawler pod for now (the coordinators still list it, as an app that does not answer) |
| `args`, `resources` | | more arguments of `chloedb-crawl serve` (as `["--contact", "ops@example.com"]`, whom site owners reach), and its container's resources (50m CPU and 64 MiB asked, 1 GiB at most) |

The upgrader moves the crawler with the servers, since it runs their image. Removing `spec.crawler` deletes its Deployment and Service and restarts the coordinators without it; the tables it filled stay. Its crawls live in its pod, so a pod started again (a new image, a node replaced) starts with none.

## What it does with a page

**Fetching.** One queue per host, read breadth first. robots.txt is read before a site's first page and again once a day (RFC 9309: the longest rule wins, `*` and `$` in paths, its own groups, `chloedb-crawl` and `strata-crawl` (its old name), or else `*`); a 4xx answer for it allows everything, and a 5xx, a 429 or no answer keeps the crawler off the site until it reads it again, a minute later (or when its `Retry-After` says, up to 10 minutes). Sitemaps named there, or at `/sitemap.xml`, add their pages as seeds. Only HTML is kept, at most `max_bytes` a page; a URL is normalised once (no fragment, no tracking parameters, the query's parameters sorted) and fetched once, redirects included; a redirect (5 at most) is followed only to a page the crawl would follow as a link (its scope, `include` and `exclude`) and its site's robots.txt allows; robots.txt is read to 500 KB and a sitemap to 50 MB; a page whose text another page already had is counted as a duplicate and not written. Links are queued in turn from each shape of link on the page (`/catalogue/*/index.html`, `/catalogue/category/...`), so the first pages read are a site's items as well as its lists. `rel="nofollow"` links and pages with `noindex` or `nofollow` robots meta tags are honoured.

**Reach.** The crawler fetches the public internet only: an address on its machine (`localhost`, `127.0.0.1`), a private network (`10.x`, `192.168.x`, `fd00::/8`), a link-local one (`169.254.169.254`, cloud metadata) or another reserved range is refused, whether a page links to it, redirects to it or its name resolves to it (checked again as each connection is made). An intranet crawl says so when the crawler starts: `--allow-host HOST[:PORT]` (repeatable) for one site, `--allow-private` (or `CHLOEDB_CRAWL_ALLOW_PRIVATE=1`) for everything. A proxy named by `HTTPS_PROXY` is reachable whatever its address, and resolves the names it is asked for itself.

**Politeness.** A site gets at most `per_host` requests at once (1), and between the starts of two the longest of `delay_ms` (1 s), its robots.txt's `Crawl-delay` (up to 10 minutes; every site the crawl reaches, not only the seeds') and twice the time it has been taking to answer (up to 30 s), so a site that slows down is asked less often. A site that pushes back waits before its next request: a 429, 502, 503 or 504 as long as its `Retry-After` says, else 5 s, doubled at each refusal in a row up to 5 minutes, and the page is asked again later (4 times at most); a 403 or no answer waits the same, and the page is not asked again. After 10 refusals in a row, or when a site asks to wait more than 10 minutes, the crawl gives up on that site and drops its queued pages; the crawl's events say when a site was made to wait and when it was given up. The user agent names the crawler and links to [the page for site owners](crawler-bot.md), with the crawl's `contact`; a crawl's own `user_agent` keeps the crawler's name after it. Answers are asked compressed (gzip, Brotli).

**Fields.** Each page gives:

| from | as |
|---|---|
| the fetch | `url` (the key), `host`, `depth`, `status`, `fetched`, `bytes`, `fetch_ms` |
| the page | `title`, `body` (the main text: `<main>`, `<article>` or `[role=main]`, else the body without navigation, headers and footers), `words`, `links`, and when the page has them `description`, `headings`, `lang`, `canonical`, `published`, `modified`, `author`, `image`, `site_name`, `kind`, `keywords` |
| JSON-LD | `ld.*`: the page's main entity (`Product`, `Article`, `Recipe` ...), `@type` as `type` |
| Open Graph and other meta tags | `og.*` (`og:price:amount` is `og.price_amount`) |
| microdata | `md.*` |
| tables and lists of terms | `specs.*`: rows of a key and a value (`<th>UPC</th><td>a897...</td>`, `<dt>`/`<dd>`) |
| the site's template | `page.*`: learned from the first pages (below) |
| your selectors | `rule.*`: `name=selector`, `name=selector@attr`, `name[]=selector` for every match |

**The template.** On the first pages (`sample`, 30), the crawler looks for an element of one tag and classes (`p.price_color`) that most pages have at most three times, whose text changes from page to page and is not the title. Those are the site's own fields, named from their classes (`price_color`, `stock_level`), and read from every page after.

**The table.** It is made from the first pages: the fetch's and the page's columns above, a column for each other field that at least `promote_at` (0.3) of the pages have, typed by its values (a type fits when 90% of the values read as it: `BOOL`, `INT`, `FLOAT` with currencies and thousands separators read, `TIMESTAMP`, `KEYWORD` for short values, else `TEXT`, and lists), and `data JSON` holding every field of the page, so a field that is not a column is still a [path](query-language.md#json-fields-keys-no-schema-names) to query (`WHERE data.specs.isbn = '...'`). An existing table is used as it is, with the columns it lacks added. With `embed`, a vector field `emb` is embedded from the title and body by that model.

**As it grows.** Every 100 pages the crawler looks again at the fields the pages since the start have had: one that has reached `promote_at` (over 50 pages at least) gets a column (`ALTER TABLE ... ADD COLUMN IF NOT EXISTS`), and the rows written before it get its value from their `data` (a `set` by key for each). Rows are written in batches of `batch` (200) through `POST /v1/write`, each under an idempotency key, so a batch sent again after a lost answer writes once; a 429 waits as the server asks, a partial write is sent again with the key it returns, and rows a server refuses are found by halves and listed as problems.

## The command center's view

**Apps → Crawler** lists the crawls and the cluster's shards, each a bar of its rows against the split threshold, with the shards it added and the rebalances. **New crawl** takes a URL and the options above. **Preview** reads the URL and up to nine pages it links to and shows the table they would make: its columns with their types and where each comes from, the `CREATE TABLE`, the pages read, the template learned, every field found with the share of pages that had it, and three rows as they would be written. **Crawl** starts it.

A crawl's page shows pages fetched and rows written a second, with the moments the table was made and shards were added; the shards filling and splitting; the table's columns, with when each was added, and the fields found so far with the column each went to; a search of the table through ChloeDB (`MATCH` and `bm25` over the body, or the latest rows); what it did, with the SQL it ran; the last pages read; and problems (robots.txt, timeouts, answers that were not HTML, refused rows). Pause, Resume and Stop act at once; a stopped crawl writes the pages it has read.

The view is a module the crawler serves (`chloedb-crawl/ui/view.js`, compiled in; `CHLOEDB_CRAWL_UI_DIR=chloedb-crawl/ui` reads it from disk), loaded and run by the command center as the [apps](server.md#apps) section describes.

## Options

The same names are the JSON fields of `POST /v1/crawls` and `POST /v1/preview`, and the command line's options (`--max-pages`, `--no-robots`, `--rule`, ...).

| option | default | |
|---|---|---|
| `seeds` | | the URLs to start from |
| `table` | from the first seed's host (`books_toscrape_com`) | the table to fill |
| `scope` | `host` | which links to follow: `host` (the seeds' hosts), `domain` (their sites, subdomains included), `prefix` (under the seeds' paths), `any` |
| `include`, `exclude` | | regular expressions on URLs: follow only links that match one of `include`, never those that match `exclude` |
| `keep` | | regular expressions: write rows only for pages that match (the others are read for their links) |
| `max_pages` | 1,000 | URLs to fetch at most |
| `max_depth` | 5 | links from a seed; a list's next page (a link with `rel="next"`, or named "Next", "Next page", "Older posts" or an arrow) is as deep as the list, so a long list is read to its end. A crawl that ends says why: its page limit, its depth (with the links it left out, also in the status's `beyond_depth`), or no links left |
| `concurrency`, `per_host` | 16, 1 | requests at once, and to one host |
| `delay_ms` | 1,000 | between the starts of two requests to a host (longer when its robots.txt asks, or when it is slow to answer) |
| `robots`, `sitemaps` | on | obey robots.txt; read sitemaps |
| `sample` | 30 | pages the table is made from |
| `promote_at` | 0.3 | the share of pages a field needs to be a column |
| `rules` | | fields from CSS selectors, as above |
| `learn` | on | learn the site's template |
| `embed` | | a model to embed the title and body with, into `emb` |
| `user_agent` | `chloedb-crawl/0.3.3 (+https://chloetech-ai.github.io/chloedb/chloedb/crawler-bot.html)` | a crawl's own is sent with that after it, unless it names `chloedb-crawl` (or `strata-crawl`, its old name) |
| `contact` | `--contact` of `chloedb-crawl serve` (or `CHLOEDB_CRAWL_CONTACT`) | whom a site's owner reaches about the crawl, an email address or a URL, sent in the user agent: `chloedb-crawl/<version> (+https://...; ops@example.com)` |
| `timeout_ms`, `max_bytes`, `batch` | 20,000, 5 MB, 200 | |

## API

`chloedb-crawl serve` answers on `--listen` (`127.0.0.1:7800`). It takes no request that carries an `Origin` header, so no web page can drive it from a browser, and with `--token` (or `CHLOEDB_CRAWL_TOKEN`) none without `Authorization: Bearer <token>`; with `--local` and no token it makes one and gives it to the coordinator it starts.

| request | |
|---|---|
| `GET /v1/info` | the crawler's version, the ChloeDB it writes to and what it is, the growth policy and the default options |
| `POST /v1/crawls` | starts a crawl from the options: `{id, table}`; two crawls may not fill one table at once |
| `GET /v1/crawls` | the crawls, newest first |
| `GET /v1/crawls/:id` | one crawl: its phase, counts, rates and a second-by-second series, the table's plan and SQL, the fields found, the last pages and problems, what it did, and the cluster's growth |
| `POST /v1/crawls/:id/pause`, `/resume`, `/stop` | |
| `DELETE /v1/crawls/:id` | forgets a crawl that has ended (its table stays) |
| `POST /v1/preview` | the options and `pages` (6, at most 20): the preview, nothing written |
| `GET /v1/grow` | the shards' rows of the crawls' tables, slots moving, and the shards added |
| `GET /app.json`, `GET /ui/view.js` | the manifest and view the command center reads |

Through chloedb-server the crawler checks its callers (`"checks_callers": true` in its manifest): a key that may write starts crawls, which are its own, and pauses, resumes, stops and forgets only those (an admin of every table steers any); a key limited to tables lists only its own crawls. A crawl writes its table as the key that started it: the crawler sends its token narrowed to that key's grants (`x-chloedb-as-tables`, [server](server.md#apps)), so a key limited to `crawl_*` crawls into `crawl_*` tables only, and a table it may not write is the server's 403. Called directly with its token, it answers as before.

## Limits

- Pages are read as their HTML arrives: a page built by JavaScript in the browser shows only what its HTML holds (JSON-LD and meta tags often carry its fields anyway).
- A crawl lives in the process: its queue and seen set are in memory, and a restarted crawler starts again (rows are written by URL, so a page written twice is one row).
- A site's scope (`domain`) takes the last two labels of a host, or three when the second last is short (`example.co.uk`), without the public suffix list.
- The growth policy counts the rows of the crawls' tables on each shard, not their bytes, and adds a shard at a time; it does not remove shards.
- A list column added during the crawl is filled from the rows written after it only (a `set` by key takes one value).
