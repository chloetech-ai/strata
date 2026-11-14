# chloedb-apps: apps built from a description

`chloedb-apps` is a prototype app engine on ChloeDB. You describe an app in a sentence ("a stock room: items, suppliers, what is low, purchase orders"), and the engine has a model plan and write it, checks what it wrote, stores every piece in ChloeDB and runs it. You then change it by asking ("add a Check price online button on each item"), and every change is a new version you can go back from.

Everything an app is lives in ChloeDB, as rows:

- **Its pieces are SQL statements.** Screens are `CREATE SCREEN`, rules are `CREATE RULE`, tables are `CREATE TABLE`, and each is a row of `sa_capabilities`. A version is a row of `sa_compositions` that lists the pieces it is made of, so going back is one write. Tables only grow, so an app's data survives every change and every step back.
- **Pieces are shared.** Components (`ui.calendar(props)`) and helpers (`lib.dates.addDays(day, 1)`) are written once and used by every app. They are found for a new request by words (BM25) and by meaning (a vector column), and linked in a graph that ChloeDB walks (`REACHABLE`, `ppr`) to offer an app a newer version. 150 of them ship in `chloedb-apps/library/`.
- **What people do is recorded** in ChloeDB (`sa_events`), and the engine counts patterns in it (a search typed again and again, the same field changed row after row) to suggest changes, or to build one beside the live version for the owner to try, keep or drop.
- **Apps get services:** sign-in, files, email (held in an outbox unless a mail server is set), address search and maps, and, when the owner switches them on, a model and the web (below).

It is a program of its own, in Node.js, that talks to chloedb-server over HTTP. Views run in sandboxed frames in the browser, and server functions run in QuickJS compiled to WebAssembly, with no network and a time limit.

![The studio: an app on the left, and on the right the request box, the version's summary, and the Model and web switches](../assets/img/strata-apps-studio.png)

## Run it

You need Node.js 20 or later, chloedb-server, and Claude Code signed in (`claude` on the PATH). The engine calls `claude -p`, so it uses that sign-in and needs no API key of its own.

```bash
(cd chloedb-server && cargo build --release)
cd chloedb-apps && npm ci
./run.sh                     # the studio at http://127.0.0.1:7730/
```

`run.sh` starts a chloedb-server of its own on `127.0.0.1:7720`, with its database in `chloedb-apps/data/`. `--strata URL` uses a server that is already running instead (started with `--seq-no`, for live updates). Every option is in [the engine's README](../../chloedb-apps/README.md#run-it).

A first version of a small app takes about a minute and a bigger one 2 to 4 minutes; a change takes 20 seconds to 2 minutes. The time and the cost of each build are on its timeline (Versions → Timeline).

## On a cluster

A cluster the [operator](server.md#on-kubernetes) runs gets the engine from its `StrataCluster`, beside the crawler and the spreadsheets:

```bash
kubectl -n strata create secret generic anthropic --from-literal=key=sk-ant-...   # the model's key, once
kubectl -n strata patch strata demo --type merge -p '{"spec": {
  "secretEnv": [{"name": "ANTHROPIC_API_KEY", "secret": "anthropic"}],
  "apps": {"serviceType": "NodePort", "nodePort": 30731, "publicUrl": "http://203.0.113.5:30731"}}}'
```

The operator runs the engine's own image (`ghcr.io/chloetech-ai/chloedb-apps`: Node, Playwright's Chromium for the drawn checks, and Claude Code) as a Deployment of one pod, `<cluster>-apps`, behind a Service of that name, and restarts the coordinators so that they list it: **Apps → App creator** appears in the command center.

- **The app creator is a page of the command center.** Describe an app and build it there: the conversation of changes and builds (each build's progress, its screens drawn while the model writes the rest, its timeline), the app itself on a phone, a tablet or a computer (drawn 1280 px wide and scaled to fit the stage), and what it is made of: **Made of** measures how much of the code that runs the engine generated from the CREATE TABLE, CREATE SCREEN and CREATE RULE statements the agent wrote, how much the agent wrote from scratch (JavaScript, or a screen written out as JSON), and how much is the library's, piece by piece (`GET /api/apps/<id>/made`, `engine/provenance.mjs`); **Code** reads each piece's SQL and edits a CREATE SCREEN with no model; **Time** is where its builds' time and cost went; **Data** (the first tab after a build, with each table as a CSV), **Versions**, **Design** and **Reach** are the app's tables, versions, looks and switches. The bar shows what the app's builds cost in all, and its menu renames the app (`POST /api/apps/<id>/rename {name}`; a name given there, or when the app is made, stays: a later plan does not rename it, and a label that starts the description, like `acme-shop`, stays in front of the name the plan picks), duplicates it with its rows (`POST /api/apps/<id>/duplicate`), exports its rows as JSON (`GET /api/apps/<id>/export`) and deletes it (`POST /api/apps/<id>/delete`): its own tables `app_<id>_*` and no other app's, its pieces, versions, use, settings, members and files; the shared library stays. **Usage**, beside the app's screens, is the studio's Activity view over the whole stage: each visit step by step, the paths people take between screens, how easy the app is to use and what needs a look. The app's screens run as they do in the studio: in frames with the studio's sandbox and policy (no same origin, so a view reads nothing of the command center's, its key included, and no network), each read and write sent to the page as a message, which calls the engine for that one app. People's sign-in does not pass through chloedb-server, so the creator shows an app as a visitor sees it; with `publicUrl`, **Its own page** opens the app as people use it, and **Studio** opens the standalone studio. Live updates come by long polls of `GET /api/events`, since chloedb-server forwards whole answers.

- **Its apps are rows of the cluster, each read and written with a key of its own.** The engine reaches the coordinators with a key limited to its tables, `sa_*` and `app_*` (the operator makes it, `<cluster>-apps-credentials`, when the coordinators can, [keys for apps](server.md#apps); `spec.apps.tables` changes it; an operator whose ClusterRole cannot make Secrets gives it the cluster's admin token, `tokenSecret`, instead), and gives every app a key made from that one (`POST /v1/keys/own`), limited to the app's tables `app_<id>_*` (write) and the table its library's table functions read (`sa_numbers`, read), named `apps-app/app-<id>`. An app's `app.sql`, inserts, updates and removes go with its key, so the server refuses another app's rows, the engine's people and sessions, and every other table of the cluster, whatever the engine's rewrite of the SQL lets through; the keys end when the engine's key is revoked. An engine given the admin token (an operator from before such keys, `deploy/kube.yaml`) makes itself the limited key, follows the feed of changes with it too (the server sends a limited key the operations of its tables alone), and keeps the token only for an older server's feed, and says so in its log; `GET /api/status` shows which (`keys`). A coordinator over shards walks no graphs, so the engine walks its own (`engine/walk.mjs`), and it tells the model that an app reads its links a step at a time.
- **The studio and each app's own page have an address of their own**, `publicUrl`, through a node's port, a load balancer, or an Ingress of your own in front of the Service. Files and map tiles the creator's frames show are loaded from there too (without it, through chloedb-server, which needs the key the frames cannot send). **Studio** asks the engine, through chloedb-server and so only with an admin key, for a link that signs the browser in once, within two minutes, for 30 days. Anything else that reaches the studio's address gets a page that says where to sign in.
- **Keys limited to tables build apps by their tables.** Beside a cluster the engine checks its callers (`"checks_callers": true`, `engine/callers.mjs`): reading an app takes read on its tables (`app_<id>_*`), using it as its screens do (rows, functions, files) takes write there, and changing, publishing, renaming or deleting it takes admin there; building a new one, or a copy, takes admin on every app's tables (`app_*`), which the app-owner preset gives. A key limited to tables lists only the apps whose tables it reads, follows one app's events (`/api/events?app=<id>`), and never gets the studio's login or the graph of every app. A key of every table keeps read to look and admin to change.
- **Apps their owners publish are open to everyone, as far as the owner says.** **Publish** in the app creator asks *who can see this?* (`POST /api/apps/<id>/publish {on, access, name, hosts}`, `engine/publish.mjs`): **only me** (not published, the default), **visitors can look** (`access: "look"`: read what its screens show, no writes), **customers can look and add** (`"add"`: also add rows, upload files and run its functions, and edit or delete only the rows they added themselves, as `sa_events` records who added each), or **anyone can do anything** (`"change"`, which asks for the app's name to be typed). An app that never chose (a publish that names no access, or an app published before the choice existed) is at `look` until its owner picks more. A visitor who may only look is not offered the app's forms, nor the buttons and row actions that add, change or remove; a call their access does not allow is a 403 marked `refused` that says what was refused ("read-only for visitors"), shown as a quiet note, never as an error. **See it as a stranger** opens the app's own page as someone with no account would see it under the choice being weighed (each call carries `x-sa-as: visitor`, which only ever lowers what the studio's session may do), and the tab lists the columns that look like emails, phone numbers or addresses, which a public app shows to anyone. A published app is reached at `/a/<id>` on the engine's address, at `<name>.<domain>` when the engine has a domain (`spec.apps.publish`, below), and at hosts of the owner's own (a CNAME to `<name>.<domain>`). The public reaches the app as it runs and nothing of how it is made: its page and the runtime's files, its state without its requests, versions, people or outbox, and the calls its screens make under its access, and `app.ask`, `app.fetch` and `app.email` under the app's Reach switches and its model calls a day. What a visitor writes is checked on the server: a column whose forms offer a fixed list of choices takes only those (and the values the app's own code writes to it), a field is at most 10 KB, a row 64 KB and a list 100 items (`STRATA_APPS_PUBLIC_FIELD_MAX`, `STRATA_APPS_PUBLIC_ROW_MAX`), and a refusal is a 400 that says why; a failure tells a visitor nothing of shards or physical tables. An error a visitor's browser or call hits is recorded under Usage and never starts a repair, so nothing a stranger sends reaches the model. The app's people (`GET /api/apps/<id>/people`) are listed only to its signed-in members, with their emails only for its owner; one account serves every app on the engine, but a visitor is a member of an app only once they sign up or in on it, and signing up with an email that has an account signs in with the right password and otherwise says only that the email and password cannot be used. Building and changing it, other apps, versions built to try, the library and the studio stay behind the studio's login. Each visitor is held to 240 calls a minute and 20 of the dear ones (model, web, mail, accounts, uploads); `STRATA_APPS_PUBLIC_RATE` and `STRATA_APPS_PUBLIC_DEAR_RATE` change that.
- **The model's key is the cluster's** (`secretEnv`'s `ANTHROPIC_API_KEY`, as the agents use), and `spendCapUsd` stops every model call once the engine has spent that much (25 US dollars unless set; 0 for no limit).
- **What it keeps outside the cluster** (files people gave its apps, map tiles, what the model has cost, Claude Code's home) is on a volume of its own, `<cluster>-apps`, kept when `spec.apps` goes and deleted with the cluster.

| `spec.apps` | default | |
|---|---|---|
| `publicUrl` | | where browsers reach the studio and each app's own page; without it the creator works in the command center, but cannot open the studio or an app's own page, and its frames show no files |
| `publish` | | open published apps to everyone, at `<name>.<domain>`: `domain` is your own (`apps.example.com`, with a DNS record `*.apps.example.com` pointing at the load balancer) or `sslip` (the load balancer's address as a name of sslip.io, `kids-reading.203-0-113-5.sslip.io`, with no record); `email` for Let's Encrypt. The operator adds a load balancer, `<cluster>-apps-public` (ports 80 and 443; most clouds bill for one), and Caddy beside the engine, which gets a published app's certificate the first time someone opens it, after asking the engine whether the host is one of its apps (`GET /api/tls/ask`), and keeps the certificates on the engine's volume |
| `serviceType`, `nodePort` | `ClusterIP` | the Service's type: `ClusterIP` (put an Ingress in front), `NodePort` (30000 to 32767) or `LoadBalancer` (most clouds bill for one) |
| `spendCapUsd` | `25` | what the model may cost, in US dollars, counted across restarts |
| `storage` | `1Gi` | its volume's `size` (it only grows) and `storageClassName` |
| `image` | `ghcr.io/chloetech-ai/chloedb-apps:latest` | the engine's image |
| `env` | | the engine's own settings (`STRATA_APPS_MODEL`, `STRATA_APPS_SMALL_MODEL`, ...); secrets go in `secretEnv` |
| `paused` | `false` | no pod for now (the apps stay in their tables) |
| `args`, `resources` | | more arguments of the engine, and its container's resources (250m CPU and 600 MiB asked, 1.5 GiB at most) |

The image runs alone as well, with a chloedb-server of its own on its volume: `docker run -p 7730:7730 -v apps:/data -e ANTHROPIC_API_KEY ghcr.io/chloetech-ai/chloedb-apps` (without `STRATA_APP_TOKEN` there is no login, so keep that on your own machine).

## A model and the web, per app

An app's code can ask a model, reach the web and run on a schedule, each only once the app's owner has switched it on in the studio, under **Model and web**:

| | what the app calls | limits |
|---|---|---|
| A model | `app.ask(prompt, {json, system})`: a small model (Haiku; `STRATA_APPS_ASK_MODEL`) summarizes, sorts, extracts or drafts | the same question gets the kept answer (`sa_asks`) at no cost; 200 questions a day per app by default (the owner can change it; `STRATA_APPS_ASKS_PER_DAY`), 20 a minute |
| Web search | `app.ask(prompt, {web: true})`: the model searches the web before it answers | kept answers expire after 6 hours; a call stops at $0.25 |
| Web pages and APIs | `app.fetch(url, {method, json, headers, secret})`: JSON parsed, an HTML page's readable text and title; `CREATE SOURCE` and `PULL` read feeds and JSON lists into tables | the hosts the owner allowed, on public addresses only (below); 2 MB and 10 seconds a request; GET answers kept 10 minutes; 1000 a day per app |
| Schedules | `EVERY ...` on a rule or function, and `CREATE SOURCE` | at most every 15 minutes; stops after 5 failures in a row |

Until the owner switches it on, the call answers that it is off, and the app shows that instead of failing (it is not sent to the repair). A build says which of these the code uses, which are still off, and which hosts and secrets it needs that the owner has not given it. In a build's tests the calls answer empty and reach nothing.

Under the web, the owner also allows each **host** an app reads from (GET, HEAD) and, separately, each host it may send to (POST); a host covers its subdomains. Hosts the code names, or tried and was refused, are listed for the owner to allow, and every hop of a redirect must be allowed too, so a change (asked for, built from use, or a repair) never widens what an app reaches by itself. Apps switched on before host lists keep "any public host" until the owner replaces it.

An API key is a **secret**: the owner pastes it once, binds it to the hosts it may go to, and says where it goes (`Authorization: Bearer`, a header of its own, or a query parameter). Code names it, `app.fetch(url, {secret: 'weather'})`, and never holds it. Secrets are sealed with AES-256-GCM in ChloeDB, apart from the app's code, with `STRATA_APPS_SECRET_KEY` (or a key the engine keeps in `DATA/secret.key`), sent only to their hosts, never shown again, and replaced by `[secret]` in an answer that echoes them. A key written into code fails the build's checks.

A fourth switch, **Schedules**, lets rules and functions run on their own: `CREATE RULE brief EVERY DAY AT '07:00' TZ 'Europe/Madrid' AS ...`, or a source that fills a table from a feed or a JSON API, `CREATE SOURCE news FROM FEED 'https://news.example.com/rss' EVERY 1 HOUR INTO stories (title, link, published, summary) KEY link` (each item added once, by its key). A schedule runs at most every 15 minutes, under the app's daily limits, and stops after 5 failures in a row; the studio lists each with its next run, its last result, Run now and Pause.

`app.fetch` refuses addresses on the machine or its networks: loopback, private and link-local ranges, carrier-grade NAT, unique-local and link-local IPv6, and IPv4 inside IPv6. Every address a name resolves to is checked when the connection is made, and again after every redirect, so a name that points inside the network is refused too. Only ports 80, 443, 8080 and 8443 are open, and a URL may not carry a name or password.

## Before you share it

It is a prototype for one person or a small team:

- **Run alone, the studio has no sign-in.** Anyone who can reach it can change any app and spend on the model. Keep it on `127.0.0.1`, put it behind the shared-key gate in `chloedb-apps/deploy/` (a link with a key, kept in a cookie), or run it beside a cluster ([above](#on-a-cluster)), where it signs in the cluster's admins.
- **An app is for the people who run the studio until its owner publishes it.** Beside a cluster, an app's own pages need the studio's sign-in, so only the cluster's admins open them, until its owner publishes it ([above](#on-a-cluster)): then anyone with its address uses it as far as the owner chose, and signs up and in with the engine's own accounts. Signing in with Google and others is planned.
- **Set a spending limit.** `STRATA_APPS_SPEND_CAP_USD` stops the model once the engine has spent that much, counted from what Claude Code reports and kept across restarts. The apps' own questions count apart: together they stop at a day's share (`STRATA_APPS_ASK_CAP_USD`, a quarter of the limit by default, and $20 a day with no limit set), and builds and checks get the rest, so a published app's visitors cannot stop the studio's builds, nor builds the apps, nor one app the others for more than a day; one app's questions stop at $2 a day (`STRATA_APPS_ASK_USD_PER_DAY`). A published app suggests changes from its use rather than building them, unless its owner sets its mode to build.
- **App data is checked in code, not on the server.** People sign in to apps, but row rules (`lib.access`) run in the app's views and functions; the server does not enforce them yet.
- **A piece that passes the checks can still be wrong.** The engine opens every view after a build, draws each on a phone and a computer (with Playwright installed), fixes what its rules measure, and runs the app's tests; going back a version is the safety net for the rest.

## Tests

```bash
cd chloedb-apps && npm test
```

The tests that need a chloedb-server find `chloedb-server/target/release/chloedb-server` (or `STRATA_SERVER_BIN`) and are skipped without one; CI runs them all against the server it builds (`STRATA_APPS_NEED_SERVER=1`), and again against a cluster of two shards (`STRATA_APPS_TEST_SHARDS=2`, which starts the shards and a coordinator over them in place of a server). No test calls a model.
