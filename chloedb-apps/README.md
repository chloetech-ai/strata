# chloedb-apps: apps that are built from a description and grow

A prototype app engine, introduced in [the guide](../docs/chloedb/apps.md):
an app is data in ChloeDB, made of capabilities (tables, views, server
functions) that a model writes and the engine checks, stores and runs, and of
shared components and helpers that every app uses from one library, linked to
each other in a graph that ChloeDB walks.

- **Start from a description.** "A reading list: I paste a link with a title
  and a note, tag it, and mark it read." The forge asks the model to plan the
  first version: tables, the views (screens), any server functions and new
  shared pieces, each with a spec. Then it has the model write every piece at
  once, one call each, so a big app takes the time of its biggest piece. The
  engine checks the code (a piece that fails is written again alone), creates
  the tables, stores every capability as a row in ChloeDB and puts the app
  live.
- **Ask for changes.** "Let me star a link", "add a Stats tab with a weekly
  chart". Each change is a new version of the app: the changed capabilities
  are new rows, and a version is a row listing the capabilities it is made
  of, so going back is one click. Tables only grow, so data survives every
  change and every step back.
- **It evolves, from what people do.** Each build proposes three next steps,
  and the engine records what people do (views opened, searches, filters,
  rows added and changed, functions run), who did it and on which version. It
  counts patterns in that record (a search typed again and again, a filter set
  every visit, the same field changed row after row, two views always opened
  together, a function run by hand) and, for a new one, builds a change that
  answers it beside the live version. The owner tries it on the real data,
  then keeps or drops it (see "Evolving from use" below).
- **It shares.** Pieces of interface that are not about one app's data (a
  month calendar, a chart, a day strip, a checklist) are written as shared
  components: one copy in ChloeDB that views call as `ui.calendar(props)`.
  The forge is shown every component with its props and uses them before
  writing anything, so a second app asking for a calendar gets the first
  app's. A component takes its data through props and never touches tables,
  so it fits any app. Each app pins the version it uses; when one app needs
  more from a component, the forge writes a new version (new props only,
  old ones keep their meaning) and the other apps are offered it, which
  takes a fraction of a second and no model call. Plain logic shared the same
  way is a **helper**, called as `lib.dates.addDays(day, 1)` in views,
  components and server functions alike.
- **It starts with a library.** 150 pieces ship in `library/` and are stored
  in ChloeDB at start-up like any other. The first 50 were written by hand;
  the next 100 were written by the model from a spec each and kept only after
  passing the checks a written piece must pass, their example calls (every
  helper's run as tests, in Node and in QuickJS, from `library/examples/`)
  and a render in Chromium without errors. A changed file is stored as a new
  version at the next start.
  - 86 components. Inputs: a form, tag input, filter chips, rating stars,
    date, time and date-range pickers, a colour picker, combobox, multi
    select, slider, number stepper, toggle switch, segmented control,
    password field, inline edit, editable and sortable lists, a Markdown
    editor, an uploader, a drawing pad. Layout and navigation: tabs,
    accordion, dialog, dropdown menu, stepper, pagination, search box, empty
    state, confirm button, tree view. Records: a data table, list view,
    grouped list, record card, key-value list, tag list, kanban, checklist,
    timeline, notifications, comments, chat, reactions. Time: a calendar,
    day strip, agenda, week view, Gantt chart, countdown, timer, booking
    slots, opening hours. Charts: bar, stacked bar, line, donut, histogram,
    funnel, heatmap, sparkline, gauge, progress bar and ring, stat tiles,
    leaderboard. Learning and voting: quiz question, flashcard, poll. Shops:
    product card, cart, line items, reviews. People and media: sign-in, an
    account menu, avatars, a people picker, images, a gallery, an image
    viewer, a file list, a map, a location picker, code and Markdown blocks,
    an email composer.
  - 44 helpers: dates, times of day, deadlines and business days, repeating
    schedules, scheduling free slots, streaks, statistics, forecasts,
    numbers, money, tax and tips, invoices, loans, budgets, settling shared
    expenses, inventory, grades, quizzes, spaced repetition, ranking and Elo,
    votes, nutrition, fitness, units, durations, geo, phones, addresses,
    URLs, files, CSV, text, templates, Markdown, diffs, lists, objects,
    trees, paging, IDs, random (seeded), colour, validation, access rules.
  - 20 starters an app copies as they are: screens (notes, tasks, contacts,
    inventory, expenses, events, journal, timesheet, orders, support tickets,
    recipes, workouts, goals, announcements, bookings, settings), each with
    the tables it names in its header, which a reuse adds to the app; and
    server functions for any table (export as CSV, a daily digest email,
    find duplicates, remove old rows).
- **Every app gets services.** People sign in (one account for every app on
  the engine; the first person in an app is its owner), upload files, send
  email and find places, through `app.me()`, `app.signIn()`, `app.upload()`,
  `app.email()`, `app.geocode()` and the rest, in views and in server
  functions (where `app.user` is the person whose action led to the call).
  Each app also has a page of its own at `/a/<id>`, without the studio
  around it, for a phone or to hand to someone.
- **It enforces who sees what, on the server.** Each table declares its
  access as SQL the planner writes in the plan's `"access"`, kept in the
  table's spec (so a version carries the access it had) and shown after its
  CREATE TABLE (`engine/access.mjs`):

  ```sql
  ALLOW READ ON products TO everyone;
  ALLOW WRITE ON products TO owner;
  ALLOW ALL ON receivables TO members;
  ALLOW ALL ON notes TO members WHERE owner_id = :user_id;
  ALLOW ALL ON invoices TO members WHERE company_id = :company_id;
  ALLOW ALL ON invoices TO admins;
  ```

  What is READ, ADD, CHANGE, REMOVE, WRITE (the three writes) or ALL; who is
  everyone (visitors too), members (signed in to the app), admins (whom the
  owner makes admins: `POST /api/apps/<id>/people/role {person, role}`) or
  the owner, each including the ones after it; WHERE keeps a person to their
  own rows or their company's. The engine holds every read and write of the
  app's people to it (`/sql`, `/insert`, `/update`, `/remove`, and `/call`,
  whose functions and rules run as the person who called them, as do the
  triggers their writes set off, unless the app marks a trigger to run as the
  app with `"as": "app"` or `RUNS AS APP` in its CREATE RULE, for a total over
  everyone's rows; schedules, and triggers of the studio's writes, run as the
  app): a read of a table they may not see is a 401
  ("Sign in first: ...") for a visitor and a 403 for a member; a scoped read
  gets the scope's condition joined to the WHERE of every SELECT that reads
  the table (so MATCH, bm25 and joins keep working on its scan); an add
  fills the scope's column and is refused when it names someone else; a
  change or remove reaches only rows in scope and cannot move one out.
  `:user_id`, `:user_email`, `:user_name` and `:company_id` are the engine's
  to bind (a page cannot send someone else's), and a query or function that
  names one with nobody signed in is refused as a 401 "Sign in first", not
  sent to ChloeDB unbound. Companies are the engine's, never the app's:
  `sa_companies` and `sa_memberships`, a person making one becomes its admin
  and invites people who have an account, who join it themselves
  (`GET/POST /api/apps/<id>/companies`, `POST .../companies/members {company,
  email, role}`, `POST .../companies/join {company}`,
  `.../companies/members/remove`; `app.companies()` (an invitation not taken
  is listed with `invited` and no role), `app.company()`,
  `app.createCompany(name)`, `app.useCompany(id)`, `app.addToCompany(...)`,
  `app.joinCompany(id)` in views; the studio adds people at once), and the
  one a person works in now is the `x-sa-company` header's when they belong
  to it, else their first. Company scope keeps companies apart from each
  other, not from their own admins: a company's admins decide who is in it,
  and anyone signed in may make companies of their own. In an app that
  declares no access at all, a table is as tables were before (open to
  whoever the publish choice lets in), so apps built before keep working;
  once any table of the app declares access, a table with no ALLOW statement
  is read by everyone and written only by the app's owner. The studio (the
  builder, not the public path) has every row. A visitor's rows are dated by
  the engine (`created` is not theirs to give). A person on the public path
  emails only the app's own people (its members' addresses), from a view or
  through a function or trigger their action runs; the app's admins and
  owner, the studio and schedules email anyone. A request that asks for signed-in people only, roles or
  companies fails the build's checks until the plan declares access for every
  table. On a laptop with no login every caller is the studio, so access
  applies to a published app's public beside a cluster and to "See it as a
  stranger".
- **It knows what goes with what.** Every app, view, function, table and
  shared piece is a node of `sa_graph`, and what each one uses, reads, calls
  or opens is its `EDGES` field. The forge ranks the library for a request
  by the words and meaning of what pieces do, fused with personalized
  PageRank from the pieces the app and the best matches use (pieces other views use together,
  pieces that call each other). A new version of a piece is offered to the
  apps found by walking the graph backwards from it (`REACHABLE ... dir =>
  'in'`), through the pieces that use it. The Library's Graph view and each
  app's Map draw it and ask ChloeDB about any node.
- **It answers questions about how pieces fit.** The Library's search box
  (and `GET /api/graph/ask?q=`) takes questions such as "which screens use
  the calendar", "which apps use lib.money", "which rules write votes",
  "what runs when a vote is added" or "what does the cart screen use", and
  answers them from `sa_graph` with no model call (`engine/ask.mjs`): the
  piece is found by its name ("the progress bar", "ui.progressBar", "the
  votes table in the bakery"), else by words and meaning; the question is a
  walk of the labelled edges (`REACHABLE(links, :pieces, dir => 'in', label
  => 'uses,calls')`); writes are told from reads by the code; and a piece
  found through others says through which (an app, through its screen
  menu, through ui.cart, uses lib.money). Screens record the patterns they
  were expanded from, and patterns are nodes too, so "which screens use the
  leaderboard pattern" has an answer.
- **It reuses whole screens too.** The forge searches the library of what
  every app runs now and can reuse a screen, rule or function as it is, or
  fork it. Search is two rankings fused (reciprocal rank fusion): BM25 over
  what each piece is and its code, and the cosine of the request's
  embedding and the piece's (`sa_capabilities.meaning`, its title and
  description embedded by ChloeDB as each version is written, with the
  model of the apps' meaning columns). So "let customers vote on which
  coffee we roast next" asked of a coffee shop finds the voting board's
  screen and its two rules, and "purchase the items in my basket" finds a
  checkout with none of its words. A piece found by meaning alone must be
  close (`STRATA_APPS_MEANING_MIN`, cosine 0.2). The plan prompt gets the
  best 3 screens, rules and functions (each saying whether its words, its
  meaning or both found it), and of the patterns apps added the best 3 in
  full and the others by name. The studio's Library search is the same
  search.
- **It repairs itself.** A view or function that fails at run time is sent
  back to the forge with its error, once per version, and the fixed version
  replaces it while the app stays open. A call nobody answered (the network,
  the engine restarting) is not a failure of the code: the view says so and
  reads again, and nothing is repaired; nor is a refusal (a rule's, or a
  visitor's access).
- **A build can be stopped.** Stop beside a running build (`POST
  /api/apps/<id>/stop`) kills its model calls at once; "Stop and try again"
  is offered once it has taken twice as long as builds here do. A failed
  build says what went wrong in a sentence, its error under Details.
- **Everything is live.** The engine follows ChloeDB's feed of committed
  writes, so every open window sees new rows and new versions as they land,
  whoever wrote them (another window, a trigger, another client of the
  database).

## Run it

You need Node.js 20 or later, Claude Code signed in (`claude` on the PATH:
the engine calls `claude -p`, so it uses your Claude Code sign-in and needs no
API key of its own), and chloedb-server, which `run.sh` builds when it finds
none (Rust needed for that).

```bash
cd chloedb-apps
./run.sh                     # http://127.0.0.1:7730/
```

`run.sh` starts chloedb-server on `127.0.0.1:7720` with its database in
`chloedb-apps/data/apps.strata` and a token of its own in
`chloedb-apps/data/strata.token` (made on first start; send it as
`Authorization: Bearer <token>` to reach that server yourself), and the engine
on `127.0.0.1:7730`. The engine gives each app a key of its own
(`STRATA_APPS_KEYS` below). Options:

| | |
|---|---|
| `--port N`, `--host H` | where the studio listens |
| `--data DIR` (or `STRATA_APPS_DATA`) | where the engine keeps its database |
| `--strata URL` (or `STRATA_URL`, with `STRATA_TOKEN`) | use a chloedb-server that is already running (started with `--seq-no` for live updates) |
| `STRATA_SERVER_BIN` | the chloedb-server to start |
| `STRATA_APPS_KEYS` | `per-app` (default): each app reads and writes ChloeDB with a key of its own that the server holds to the app's tables (`app_<id>_*`), made from the engine's key with `POST /v1/keys/own`; an engine given an admin credential trades it for a key of its own limited to `sa_*` and `app_*` (`chloedb-apps`), and follows the feed with it too (a chloedb-server from before limited keys could read the feed keeps the admin credential for that alone). `shared`: every app goes through the engine's one credential, as before. An open server (no `--token`) makes no keys, so there the engine says so and shares; `GET /api/status` says which (`keys`) |
| `STRATA_APPS_WAIT_S` | how long the engine waits at start for a chloedb-server named by `--strata` to answer (default 300 s: beside a cluster, its coordinators may start after it) |
| `STRATA_APP_TOKEN`, `STRATA_APPS_PUBLIC_URL` | beside a cluster: the token chloedb-server sends when it forwards `/v1/apps/apps/...` here (set, it turns the studio's login on), and where browsers reach the studio, for the one-time links the command center hands out ([On a cluster](../docs/chloedb/apps.md#on-a-cluster)) |
| `STRATA_APPS_TEST_SHARDS` | `npm test` against a coordinator over this many shards, in place of one server |
| `STRATA_APPS_MODEL` | the model (default `claude-opus-5-5`) |
| `STRATA_APPS_CLAUDE` | the Claude Code binary (default `claude`) |
| `STRATA_APPS_EFFORT` | the model's effort level (default: the model's own) |
| `STRATA_APPS_PROVIDER` | the CLI that answers: `claude` (default, Claude Code), `codex` (`codex exec`, read-only sandbox, no tools, no user config; off unless turned on, below) or `agy` (refused for now: no checked single-prompt mode). `POST /api/apps` and `POST /api/apps/:id/requests` take a `provider` of their own for that build only |
| `STRATA_APPS_CODEX_ON=1` | turn the `codex` provider on (`STRATA_APPS_PROVIDER=codex` does too). It is off by default because it answers with this machine's own Codex sign-in, which the engine cannot scope; a build that asks for it otherwise is refused with a 400 |
| `STRATA_APPS_CODEX`, `STRATA_APPS_CODEX_MODEL` | the Codex binary (default `codex`) and its model (default: Codex's own); the `claude-*` model settings go to claude only |
| `STRATA_APPS_CODEX_USD_PER_MTOK` | what a Codex answer costs, from the tokens it reports, in US dollars per million tokens as `input,cached input,output` (default `1.25,0.125,10`); it counts against `STRATA_APPS_SPEND_CAP_USD` like Claude's. Stop stops a Codex build as it does a Claude one |
| `STRATA_APPS_CHILD_ENV` | more variables, separated by commas, to pass to the model's CLI. `claude -p` and `codex exec` start with a reduced environment: PATH, HOME, the user, locale, temporary folder, proxy and certificate settings, and the CLI's own (`ANTHROPIC_*` and `CLAUDE_*`, the cloud's with `CLAUDE_CODE_USE_BEDROCK` or `_VERTEX`; `OPENAI_*` and `CODEX_*`), never the engine's tokens |
| `STRATA_APPS_AGENT_TOKEN` | the agent token the MCP server signs in with (below); unset, the engine makes one in `DATA/agent.token`, readable by its owner only |
| `STRATA_APPS_AGY`, `STRATA_APPS_AGY_MODEL` | the same for Antigravity's `agy`, kept for when it is wired |
| `STRATA_APPS_LIBRARY` | the built-in library's folder (default `library/`) |
| `STRATA_APPS_SPLIT` | `first` (default): first versions are planned, then written piece by piece at once; `always`: changes too; `off`: one answer holds the plan and all the code |
| `STRATA_APPS_IDLE_S`, `STRATA_APPS_TIMEOUT_S` | a build stops when the model writes nothing for this many seconds (default 90), or after this many in all (default 1200); its owner can stop it at any time |
| `STRATA_APPS_AUTOREPAIR=0` | do not send failing capabilities back to the forge |
| `STRATA_APPS_SCREEN_MODEL` | the model for the plan step and for changes, which write the screens and rules (default `claude-sonnet-5-5`; set it to `claude-opus-5-5` for Opus everywhere); code views, functions and repairs keep `STRATA_APPS_MODEL` |
| `STRATA_APPS_RULES=off`, `STRATA_APPS_TESTS=off` | functions and triggers only as JavaScript; no tests in plans |
| `STRATA_APPS_RULE_SQL=off` | rules written as JSON steps rather than CREATE RULE statements |
| `STRATA_APPS_STATEMENTS=off` | screens written as JSON rather than CREATE SCREEN statements (still stored as the statement that makes them) |
| `STRATA_APPS_PARALLEL=on` | a first version's plan names its screens and rules, and each is then written by its own writer at once (with the example rows and tests by one more), rather than the plan holding their statements; measured slower and dearer on 12 first versions (36.6 s against 33.7 s each), so it is off by default |
| `STRATA_APPS_SMALL_MODEL` | the model a small change (a label, a colour, a sort order, renaming, hiding or moving something) goes to first, before the usual one if its answer fails the checks (default `claude-haiku-5-5`; `off` sends every change to the usual model) |
| `STRATA_APPS_SUGGEST=inline` | the build's own answer suggests what to ask next, rather than `STRATA_APPS_SUGGEST_MODEL` (default `claude-haiku-5-5`) once the version is live |
| `STRATA_APPS_DUMP` | a folder that keeps every answer of the model, with its timing, tokens and cost, to study what it writes |
| `STRATA_APPS_OPEN_VIEWS=off` | do not open each view in a headless browser after a build |
| `SMTP_URL`, `SMTP_FROM` | a mail server (`smtps://user:pass@host:465`, or `smtp://` with STARTTLS when offered) and the From address; without them email waits in the outbox, marked held |
| `STRATA_APPS_GEOCODER` | the address search (default OpenStreetMap's Nominatim, at most one request a second, cached; `off` turns it off) |
| `STRATA_APPS_TILES` | the map tile URL template (default OpenStreetMap's, fetched by the engine with a user agent and kept under `DATA/tiles`; `off` turns maps off) |
| `STRATA_APPS_UNSPLASH_KEY` | an Unsplash access key (free at unsplash.com/developers): front pages get photos of any subject from Unsplash, credited as its guidelines ask |
| `STRATA_APPS_PHOTOS=builtin` | photos from the built-in set only (no Wikimedia Commons) |
| `STRATA_APPS_PHOTO_CACHE` | where found photos and searches are cached (default `photo-cache/` beside the data) |
| `STRATA_APPS_SPEND_CAP_USD` | stop the model once the engine has spent this much, in US dollars, counted from what Claude Code reports and kept in `DATA/spend.json` across restarts; apps' own questions (`app.ask`) have a share of it of their own and builds and checks the rest, so that one never stops the other |
| `STRATA_APPS_ASK_CAP_USD`, `STRATA_APPS_ASK_USD_PER_DAY` | what all the apps' own questions may cost in a day (default a quarter of `STRATA_APPS_SPEND_CAP_USD`, and $20 with no cap; 0 is no daily stop), and what one app's questions may cost in a day (default $2), whatever its count of questions |
| `STRATA_APPS_ASK_MODEL`, `STRATA_APPS_ASKS_PER_DAY`, `STRATA_APPS_FETCHES_PER_DAY` | apps' own model (`app.ask`, default `claude-haiku-5-5`), the questions an app may ask it each day unless its owner sets another number (default 200), and the pages an app may fetch each day (default 1000); see [A model and the web](../docs/chloedb/apps.md#a-model-and-the-web-per-app) |
| `STRATA_APPS_SECRET_KEY` | what apps' secrets (API keys) are sealed with; without it the engine makes a key in `DATA/secret.key` (mode 0600) on first start. Beside a cluster, set it from a Kubernetes secret so a new pod can open them |
| `STRATA_APPS_SCHEDULES=off` | do not run apps' schedules (sources included) in this engine |
| `STRATA_APPS_EMBED_MODEL` | the embedding model of meaning columns and of the library's search (default `potion-retrieval-32M`); a ChloeDB that cannot load it searches the library by words alone |
| `STRATA_APPS_MEANING_MIN` | the least cosine at which a piece found by meaning alone is offered (default 0.2) |

A first version of a small app takes 50 seconds to a minute; a bigger one takes 2 to 4 minutes (a CRM with a pipeline board 110 s, a course on linked lists with lessons, a playground and a quiz 216 s; written in one answer, that course took 7.5 to 9 minutes). The studio shows how far the model has got while it writes. A change takes 20 seconds to 2 minutes.

**Screens.** Most views are written as data instead of code: a screen is JSON (its state, the SELECTs it reads, values derived from them, and a body of blocks such as forms, tables, number tiles, chips, lists, shared components by name, and small `code` blocks for what blocks cannot say), and `web/screen.js` draws it, fetches its data again after every write and redraws when its state changes. The model writes a screen inside the plan, so a first version with only screens takes one call and no writers; the forge checks each screen (its shape, that every `=expression` parses, that its SQL is a SELECT, that its forms name tables and columns the app has). Views that are mostly code (a game, a drawing surface) stay code. `STRATA_APPS_SCREENS=off` turns screens off.

**Front pages.** A screen with `"page": "front"` is a landing page drawn edge to edge, not a form: a navigation bar that turns solid on scroll, a hero (over a photo with a slow Ken Burns zoom, beside one, or on the design's drifting gradient), carousels (full-width slides that move on by themselves, pause under the pointer and take swipes and the arrow keys, or a shelf of cards that snaps), photo galleries with a lightbox, features, numbers that count up, quotes, pricing, steps, an FAQ, a marquee, photo bands with parallax and a footer (`web/front.js`, `web/front.css`). Parts rise into view as the page scrolls; `*word*` in a title is drawn in the design's gradient. Each design carries a `gradient` (three colors, derived from the accent when not given) and a `motion` (calm, lively or still; still, and any reader who asks for reduced motion, gets none). The model writes the front page as the app's first view when the app faces the public (a shop, a studio, an event).

**Photos.** A block names a photo by what it shows (`"photo": "barista pouring latte art"`), and when a version is stored `engine/photos.mjs` finds one and writes its `src`. A photo shows the product itself: the query's subject (its first word that names a thing, "taco" in "taco veggie") and the words that say which kind of it ("veggie"; words after "on", "in", "at" only say where it is) must fit. In order: Unsplash with `STRATA_APPS_UNSPLASH_KEY`; the built-in set (`library/photos.json`: 304 Unsplash photos, each checked by eye and described in words) when a photo has the subject and its kind; Wikimedia Commons (no key; every file there is freely licensed) when a JPEG large and wide enough has the subject in its title and half its kind at least, leaving out finds catalogues, archive scans, museum pieces and plants or animals that share the thing's name; the built-in set again for the subject alone; and last a drawn picture in the art direction's accent (soft shapes, never letters), or the design's gradient behind a hero. A Commons photo is downloaded once into a cache beside the data (`photo-cache/`, the searches too) and stored with the app's files, so the page never hotlinks it; its credit (title, author, license, "Wikimedia Commons") goes in the front page footer's small print, or under the photo when there is no footer. Example rows may hold `"photo:taco carnitas"` in a column: the row's query, then its category, then its name, each a photo not yet on the page before one again; a row's photo that needs a credit is used only when the app has a footer to give it in. Every search a build needs starts in parallel as soon as its plan passes the checks, each with a time limit, so photos cost a build a few seconds. Hand edits that name a photo get one the same way. `STRATA_APPS_PHOTOS=builtin` keeps to the set.

**Screen patterns.** `library/patterns/` holds whole parts of a screen with parameters: `crud` (a table's rows with an add and edit form, search, filter chips, a done toggle, big numbers), `board` (a kanban over a status column), `detail` (one row with its details, editing and related rows), `summary` (a total and a bar per group), `checks` (a thing done each day: one big tap for today per item, the last days beside it, the streak by its name and how many are done today; a phone shows yesterday and today), `week` (a row per thing with its seven days to tick, such as habits or chores), `calendar` (a month with a day's rows and a form to add one), `flashcards` (spaced repetition: a card, its answer, Again/Hard/Good/Easy), `quiz` (multiple choice with a score, saved per try), `search` (words and meaning ranked together, the picked row and rows like it) `dashboard` (a value over the last N days against the N before, per day, by a group, and the newest rows), `offers` (things people order or book while some are left: products with stock, workshops with seats, races with places, each a card with its photo, date and price, 'Only 2 left' when few remain and the out word, 'Sold out' or 'Full', with no action when none; the action opens the view that finishes the order or calls a rule that checks and lowers the count in one step) and `lessons` (a course's lessons with each learner's progress, the next lesson as the main action and the certificate once all are done). A pattern block takes `id` (a nav link's target) and `tint` like any front-page block. A screen block `{"use": "crud", "table": "guests", ...}` is expanded by `engine/patterns.mjs` before the screen is checked, and again whenever its statement is read (see "Every piece is stored as SQL" below); a screen written as JSON holds ordinary blocks that later changes patch. Patterns are rows of `sa_capabilities` (kind `pattern`, versioned like components, found by the library's search): the files are seeded into ChloeDB at start-up, and the forge expands from the rows. A build may add a pattern (`"patterns": [{name, title, about, params, example, screen}]` in its plan, at most one): its example is expanded and checked like a screen, the plan's own screens may use it at once, and once stored every app's builds see it (the chores app added `leaderboard`; the voting app's next change used it).

**Public pages and owner pages.** A view's plan entry may say who it is for: `"for": "owner"` (managing the rows a public page shows, settings) or `"members"`. A visitor of a published app, and "See it as a stranger", get the app without the owner's views (`engine/audience.mjs`), so its menu never offers them editing tools; what any view may read and write is still the tables' access. A rewrite of a view keeps who it was for.

**Designs.** Every app has three designs, and its owner picks one in the studio's Design tab; a person using the app on its own can pick their own under Aa (kept in their browser). A design is a few tokens (colors for light and dark, fonts, corners, density, surfaces, navigation, width; `web/design.js`), and `web/kit.css` draws everything from them as CSS variables, so switching posts a block of variables to each view and redraws nothing. The three are made by one call to a small model (`STRATA_APPS_DESIGN_MODEL`, default `claude-haiku-5-5`, about 12 s) beside an app's first build, or on first open for older apps, checked for contrast, and kept in `sa_designs`; presets stand in when the model fails. `GET/POST /api/apps/:id/designs` read and choose; `POST .../designs/new` replaces the two not in use. Forms lay themselves out: one of more than three fields, or with a long one, is a grid of labelled fields with as many columns as make even rows (five fields: three and two), long fields across, checkboxes together and the buttons below; a short one stays on one line, every field labelled when any needs a label; every control in a form has the same height.

**The UX brief.** A new app's plan starts with its name (2 to 4 words, what a person would call it: "Habits", not the request cut short) and its brief, `ux`: who uses it and where, the job they come back for, how often (several times a day, daily, weekly, monthly, now and then), the device (phone, desktop or both), the screen that opens first and what it shows at a glance. The tables and screens are planned from it, in the order Do, Review, Manage: first the screen where the job is done (in a tap or two when it is done daily: a check per item, a quick add, a toggle), then the ones to look back, last the ones to set up. The forge checks the brief (`checkBrief`), gives the screen it names the lowest order (`orderByBrief`), and keeps it with the app (`sa_apps.ux`); every later change is shown it and keeps to it, and the Activity view says it under its title. Rebuilt from the same sentence as before, the habit tracker came out as "Habits", made for one person on a phone, daily, opening on Today.

**Rules every screen gets.** Whatever a screen says, the renderer and `web/kit.css` keep it usable: under a finger every button, field and chip is at least 44 px (a check 44 px on a phone); a table of more than three columns is a list of cards on a phone, each cell named by its column; a button repeated on every row is named by its row for a screen reader (`Delete: Basil`); the grey of secondary text is mixed from the design's colors until it reads at 4.6:1 on the page and on cards (`mutedOf` in `web/design.js`); a long app name shortens in the header rather than pushing the design menu off a phone. Every remove offers Undo for seven seconds (`app.remove` in the frame; removes close together share one toast, and Undo puts the rows back with their ids), so a delete asks nothing first: the renderer skips a `confirm` on a handler that removes, and the patterns have none.

**The drawn-page check.** After each live version, `engine/look.mjs` draws every screen the way people see it, on a phone (390 px, touch) and on a computer (1280 px), with the use log left alone. Rules that hold for any app measure each screen (`measure`, run in the app's frame): nothing scrolls sideways, every tap on a phone is 44 by 44 px, text reads at 4.5:1 against what is behind it (3:1 for large text; colors as the browser draws them), no raw row ids, no screen that draws nothing; then the model looks once at the phone screens against the brief (`lookAt`, a few cents): is the job on the first screen, in a tap or two? What stands in the way goes to the repair as one request (source `look`, `repairRequest`): not after a fix it asked for, and at most once in ten minutes per app. The studio says what the check found; `GET /api/apps/:id` carries the last result as `drawn`; `STRATA_APPS_LOOK=off` turns it off and `STRATA_APPS_LOOK_MODEL=off` keeps the rules without the look. What it finds across apps becomes rules every screen gets: a form of three fields or more above a list folds behind its button on a phone (`"fold": false` keeps it open), a tick in a row is a 44 px target and strikes its row through, big numbers are one compact row, and a table's phone card is a title and one line, not a line per column.

**Art direction, layouts and the numbers checked.** A new app's plan writes its art direction before any screen (`art` in the plan): the idea in one sentence from the subject's own world, the first screen's layout, the title face, the accent of the main buttons, what its photos show (the product itself, never a related ingredient or a background) and the one main action, in its words. The three designs follow it (an editorial page gets the narrow width), and the screens keep the rules the model is given with it (`ART_DOC` and `TASTE_DOC` in `engine/quality.mjs`): white or genuinely dark pages, no emoji icons or ornamental bars, 44 px targets on a phone, text at 4.5:1, the main action on the first phone screen, labels in the subject's words, honest copy. Every screen chooses its page structure with `LAYOUT hero | split | editorial | dashboard | gallery | tool`, and one that gives none gets the one that fits its parts (`engine/layouts.mjs`: a front page the art direction's, numbers over time a dashboard, a check-in, quiz or lone form a tool, one row or lessons editorial, photos a gallery; a board, calendar or plain list keeps the kit's page), written into its statement too (`"layout"` in its JSON; `PAGE front LAYOUT editorial` on a front page): `web/screen.js` gathers a working screen's title and numbers as its head and its parts into groups (each starts at a heading or a row of numbers) and places them as the layout says, and the kit draws each from the design's tokens (`web/kit.css`, `web/front.css`); on a front page the layout also picks how a hero that does not say its look is drawn. The drawn-page check measures, besides the rules above: button words against the button (and text over a gradient against each of its colors), the main action below the phone's first screen, and labels that are a column's name, a placeholder or one generic word beside a number ("Groups 4"); the checks it shares with the engine are plain functions (`qualityKit`) sent into the frame. Without Playwright the same labels, the main action and the design's own color pairs are checked from the stored screens and the design (`checkStatic`), and go to the repair the same way, once.

**A screen has a budget.** Changes pile up: each one adds a part, a field or a few lines of code, and after ten a screen does everything badly. So each screen has a budget, told to the model up front and measured on the screen as written (`budgetOf` in `engine/forge.mjs`, before patterns expand): at most 7 parts, 4 big numbers, 8 fields in a form (more go under `{"section"}` headings) and 1,000 characters of code in its expressions and code blocks. A change that takes a screen past one goes back to be consolidated: fold what it adds into a part that is there (a column, a filter, an action on the row), move it to a later screen, or say it with a part's clauses. Two rules hold however small the screen: a change may not give up a part (CHECKS, LIST, ...) for code that does what the part did, since the part's rules (taps, phone layout, empty states) go with it, and it may grow a screen's code by at most 400 characters. The budget is soft: an answer over it only in that is kept on the last try, and the progress line says so. What a person asks (and suggestions they take) while a change is being built waits as one request, built together as one design (`joinAsks` in `engine/changes.mjs`), not one change on top of another; and the drawn-page check does not send a repair while a build runs, so it never fixes a version that is already gone. CHECKS takes `SKIP <bool column>`: a thing can be skipped today, a skipped day keeps the streak without adding to it, and it is not counted as to do.

**Measured from use.** The frame counts every tap in the app (and the studio the app's tabs), and each write or run carries the taps since the one before it (`taps` in `sa_events`, sent as `x-sa-taps`). `engine/measures.mjs` reads the use log as visits (a tab's events with no half-hour gap) and measures what the brief asks of the job: the taps and the seconds from opening the app to doing something, how much of what people do is done on the first screen, forms given up, rows added and removed within ten minutes, removes undone. Each is judged against targets from the brief (a job done daily or more: 3 taps or fewer, under 15 s; less often: 5 taps, 30 s; half of it on the first screen) once there are five visits, forms or rows to judge it on, and measured version by version, so a change shows what it did. What misses a target is a use signal (`ux:taps`, `ux:first`...): it goes to the suggestions from use, which answer it by making the job quicker rather than adding to the app, and a change kept for one is judged by the same measure before and after. The Activity view shows them as "How easy it is". The drawn-page check also grades each version from 1 to 5 for its brief (5: the job is on the first screen, in a tap or two).

**CREATE SCREEN.** The model writes each screen as an SQL-shaped statement in its code block (`CREATE SCREEN guests TITLE 'Guests'`, then `LIST guests`, `SEARCH name`, `FILTER rsvp IN (...)`, `FORM (...)`, `KPI 'Coming' = COUNT(*) WHERE rsvp = 'yes'`, `SUMMARY guests AS meals`, `GROUP BY meal`...). `engine/statement.mjs` compiles it into pattern blocks; the statement is about half the characters of the pattern JSON and a quarter of the screen's. Every pattern has a part: `LIST`, `BOARD`, `DETAIL`, `SUMMARY`, `CHECKS` (`CHECKS checkins ITEMS habits ITEM habit DAY day ADD`), `WEEK`, `CALENDAR`, `FLASHCARDS`, `QUIZ`, `FIND` (search), `DASHBOARD` and `TREND` (a value per day, week or month: `TREND runs DATE day VALUE SUM(km) PER week LAST 8`), with `BLOCKS [...]` for anything else. Formatting is said in words rather than written as code: a column or a KPI ends with how it reads (`MONEY`, `DATE`, `DAYS` for "in 3 days", `WEEKDAY`, `MONTH`, `DURATION` for minutes, `CLOCK` for seconds, `PCT`, `HOURS`, `STARS`...), a `KPI ... FROM <table>` outside a part is a big number over any table (`:today` is set for it), and `ACTION 'Remove' DELETE` names the delete button every list already has. A form field `FROM <table> (value, label)` picks among that table's rows (`category FROM categories (name)`, `client FROM clients (id, name)`) rather than a fixed list. `EXPORT 'Download CSV'` on a `LIST` is a button that downloads the rows shown (searched and filtered) as CSV with the list's columns (`app.csv` and `app.download` in the frame; a cell a spreadsheet would run as a formula is prefixed with `'`). The statement is kept with the screen as its source: the model sees it when asked for a change and answers with `edits` to it (or a new statement), and the studio's Edit tab shows it in a text box that saves through `POST /api/apps/:id/screens/:slug {"statement"}` (compiled, checked and dry-run before it is stored; a JSON patch on such a screen is refused). The photos a build finds for a screen (`"photo": "a sushi counter"` in a block) are kept in the statement too, as a `PHOTOS [...]` part. `STRATA_APPS_STATEMENTS=off` has the model write the screen's JSON instead.

**Every piece is stored as SQL.** A row of `sa_capabilities` holds the statement that makes the piece, never its compiled form (`engine/source.mjs`): a table is its `CREATE TABLE`, a screen its `CREATE SCREEN`, a rule its `CREATE RULE`, and a piece written in JavaScript or JSON is wrapped in one (`CREATE FUNCTION send-digest AFTER INSERT ON orders LANGUAGE javascript AS $$ ... $$`, `CREATE COMPONENT`, `CREATE HELPER`, `CREATE PATTERN ... LANGUAGE json`). The store compiles a row when it reads it (a screen's patterns are expanded then, so a better pattern reaches every screen that uses it) and keeps the compiled form only in memory. A screen or rule written as JSON is stored as a statement made from that JSON, checked to compile back into exactly it, and marked `derived` in its spec (it is still shown to the model and changed as JSON). On start the engine rewrites any piece an earlier engine stored compiled as its SQL, in place, when that SQL gives back exactly what it runs. The studio shows each piece's SQL (and, for a screen or rule, what it compiles into), and the Inside tab's "Whole app as SQL" gives the app as one script to copy or download (`GET /api/apps/:id/source`).

**Rules.** A function that is a query, or does a few writes (a checkout, a stock move), or a trigger that keeps a count, a total or a stock level right, is a rule (`engine/rules.mjs`): the engine runs it directly, as one transaction, with no writer and no QuickJS. The model writes it as SQL, a CREATE RULE statement that `engine/rulesql.mjs` compiles into the rule's steps:

```sql
CREATE RULE checkout AS
  REQUIRE (SELECT COUNT(*) FROM cart WHERE owner = :user.id) > 0 ELSE 'Your cart is empty';
  INSERT INTO orders (customer, status) VALUES (:user.id, 'new') AS order;
  FOR line IN (SELECT c.id, c.product, c.qty, p.stock, p.name FROM cart c JOIN products p ON p.id = c.product WHERE c.owner = :user.id) DO
    REQUIRE :line.stock >= :line.qty ELSE 'Only :line.stock left of :line.name';
    INSERT INTO order_lines (order_id, product, qty) VALUES (:order.id, :line.product, :line.qty);
    UPDATE products SET stock = stock - :line.qty WHERE id = :line.product;
    DELETE FROM cart WHERE id = :line.id;
  END;
  RETURN :order.id;

CREATE RULE count_votes AFTER INSERT OR DELETE ON votes AS
  UPDATE features SET votes = (SELECT COUNT(*) FROM votes WHERE feature = :row.feature) WHERE id = :row.feature;
```

Statements: `REQUIRE cond ELSE 'message'` (stops the rule, writes nothing, shows the message), `LET`, `INSERT ... [ON CONFLICT (cols) DO UPDATE SET ... | DO NOTHING] [AS name]`, `UPDATE` and `DELETE` (`WHERE id = ...` for one row, any other condition for each row it selects), `CALL fn(name => value)`, `FOR row IN (SELECT ...) DO ... END`, `IF ... THEN ... [ELSE ...] END` and `RETURN`; a rule that is one SELECT is a query (`RETURNS ROW | VALUE`). `SEND EMAIL TO :to SUBJECT 'New this week' [TEXT '...'] [ROWS (SELECT ...)] [NONE '...']` emails a line per row; the engine holds it until the rule's writes are stored, so a rule that stops sends nothing (an email step in the JSON form). The statement is kept as the rule's source: the model sees it and changes it with edits, and the studio shows it. A test's `"do"` is written the same way. `STRATA_APPS_RULE_SQL=off` has the model write the JSON steps instead (`{"query": ...}` or `{"steps": [...]}`). A query rule has the shape of a SQL function with named parameters, so it can become one when ChloeDB has them.

**Schedules, sources, hosts and secrets.** An app can reach the web on a clock, and only where its owner said:

```sql
CREATE SOURCE news FROM FEED 'https://news.example.com/rss' EVERY 1 HOUR
  INTO stories (title, link, published, summary, image) KEY link;

CREATE RULE morning_brief EVERY DAY AT '07:00' TZ 'Europe/Madrid' AS
  SEND EMAIL TO (SELECT email FROM readers) SUBJECT 'Your morning brief'
    ROWS (SELECT title, summary, link FROM stories WHERE days_between(published, :today) <= 1 ORDER BY published DESC LIMIT 15)
    NONE 'Nothing new today.';
```

- `EVERY ...` on a rule (or `"every": "day at 07:00"` on a JavaScript function's plan entry) runs it on its own: every N minutes (15 at least) or hours, every day or N days at a time, on days of the week (`EVERY MONDAY`, `EVERY WEEKDAY`, `EVERY WEEK ON MONDAY, THURSDAY`), in a time zone (`TZ 'Area/City'`, UTC without one), with `:today` the day there (`engine/schedule.mjs`). A run missed while the engine was down runs once when it is back. A schedule stops itself after 5 failures in a row; a refusal (a switch off, a limit) is said but not counted.
- `CREATE SOURCE` reads an RSS, Atom or RDF feed (`FROM FEED`) or a JSON list (`FROM JSON 'url' AT 'data.items'`, fields by path: `rate = value.mid`) on a schedule and adds each new item to a table once, by its `KEY` (`engine/feeds.mjs`). A feed item's fields are title, link, guid, published, updated, summary, content, author, categories and image, whatever the format called them. Inside any rule, `PULL FEED | JSON 'url' (or :row.url) INTO t (...) KEY col AS got` does the same and gives `{added, items}`, so a rule can go through feeds kept in a table. The fetch is an `app.fetch`, under the same switches, hosts and limits. A trigger on the table (in JavaScript, with `app.ask`) can then summarize or sort each new item.
- **Hosts.** Under "Web pages and APIs", the owner allows each host an app reads from (GET, HEAD) and, separately, each it may send to (POST). A host covers its subdomains. A host the code names (an address in a function or rule, or a view's `app.fetch('https://...')`) or tried and was refused is listed for the owner to allow, with one tap, and every hop of a redirect must be allowed too. So a change, whether asked for, built from use or a repair, never widens what an app reaches by itself. Apps switched on before host lists keep "any public host" until the owner replaces it.
- **Secrets.** An API key is pasted once by the owner, bound to the hosts it may be sent to, and put in `Authorization: Bearer`, a header of its own or a query parameter. Code names it, `app.fetch(url, {secret: 'weather'})` or `PULL JSON ... SECRET 'weather'`, and never holds it. It is sealed with AES-256-GCM (`STRATA_APPS_SECRET_KEY`) in `sa_settings`, outside the app's pieces, so the model, the studio's SQL and the exports never show it. It goes on a request only to one of its hosts (a redirect elsewhere goes without it), and an answer that echoes it back has it replaced by `[secret]`. A key, token or password written into code fails the build's checks.
- **Schedules** is a fourth switch beside the model, web search and the web, off until the owner switches it on. The studio lists each schedule with its next run and how the last one went, with Run now and Pause (`GET /api/apps/:id/schedules`, `POST .../schedules/:slug/run`, `.../pause`, `.../resume`); secrets are `GET/POST /api/apps/:id/secrets` and `POST .../secrets/:name/delete`; hosts are `POST /api/apps/:id/reach` with `allow: {host, send}`, `forget: host` or `hosts: [...]`, and `GET` answers what the code `wants` that is not allowed yet.
- One engine runs the schedules of the apps in its ChloeDB; two engines on one database would each run them (there is no lease between them yet).

**Tests as data.** A plan may carry `"tests"`: steps as in rules, then SELECTs and the value or row count each must give (`engine/checks.mjs`). They run once the version is live, on the tables it creates while they are still empty, and those tables are emptied again before the example rows go in. A failing test goes to repair like an error a person saw. After each build, every view is also opened in a headless browser (when Playwright is installed), and errors there are reported the same way.

**Small changes to code.** A change to a view or function written in JavaScript can be `"edits": [{"old": "...", "new": "..."}]` in its plan entry instead of the whole code: each `old` must occur exactly once. "Make the heading pink" is one line, not a rewrite.

**Edits by hand.** The studio's Edit tab (on a screen view) renames, moves and hides parts of the screen, and reorders, renames or hides a table's columns and a form's fields. Each edit is a JSON Patch sent to `POST /api/apps/:id/screens/:slug`, which checks the screen, runs its queries and stores it as a new version without the model (about 0.2 s).

**Evolving from use.** Every event names where it happened and what it was about, in columns of `sa_events`: the browser tab's session (one sitting), the screen, the table and row written, the fields an update changed, the words searched for or the filter set and how many rows they found, and how long (the time spent on a screen when leaving it, the time typing into a form given up). The studio sends the session and the screen with each request (headers `x-sa-session`, `x-sa-view`); a screen records a search with its results, a filter once its rows are in, and a form typed into and closed unsaved (Cancel, another screen, or a redraw without it) as `abandon`. After every 25 actions (at most every 3 minutes), or when the owner asks, ChloeDB counts the patterns in them with the engine's table functions (`engine/signals.sql`, defined when the engine starts; an older chloedb-server without stored functions leaves the counting to `engine/evolve.mjs`): searches as the words someone stopped typing at (3 or more), searches and filters that find nothing (2), filters set (4), the same change, add or delete one row at a time in one sitting (5 to 6), rows deleted within ten minutes of being added (3), forms given up (3), two views opened within 30 s of each other (4), a function run by hand (5). A new pattern costs one model call: the suggestions it gets name the pattern they answer. In the "build" mode (the default, `STRATA_APPS_EVOLVE` or per app in the studio: build, suggest, off; a published app whose owner set none suggests only, since its visitors would start builds the studio pays for) the strongest answered one is built at once as a *candidate*: a version beside the live one (`sa_candidates`), checked like any build, that the studio's **Built from use** lists with **Try it**, **Keep** and **Drop**. Trying runs the app on that version (`?v=N` on every request, the app's own page included) on the real data while everyone else stays on the live one. Keeping makes it live: as built when nothing changed since, merged over the live version when the two changed different pieces, and built again on the live version when both changed the same piece. At most 2 candidates wait at a time and 2 are built a day per app (`STRATA_APPS_EVOLVE_PER_DAY`). Each candidate names what should change in people's use if it works (fewer searches for "vegetarian"); once kept, the studio shows that count before and since. Events carry the version they happened on, so the same measure can later compare two versions served side by side.

**Activity.** **Activity** in an app's header shows what people did in it (`#/app/<id>/~activity`, from `GET /api/apps/:id/activity?days=N`, `engine/activity.mjs`), drawn in the app's own design and told in its own words: each write keeps the name of its row with the event (`sa_events.label`: the row's name or title, else the name of the row it points to with its day, "Read 20 pages · Nov 8"), so a removed row is still named, and older events are named from the rows as they are now. It shows people, visits, things done, time and searches over the last day, week, month or all time; things done (or visits, or time) by hour, day or week; a weekday by hour grid; the app's screens with how often each is opened, for how long, and arcs for going from one straight to another; what changed per table (counts, the fields changed most, the rows touched); what needs a look (searches that found nothing, forms left unsaved, rows added and removed within ten minutes, errors, and the patterns the engine noticed); and every visit as a timeline ("Added a watering **Monstera · Nov 8**", "Searched “cactus” · found nothing"). It reads the newest 5,000 events of the period and refreshes every 15 s while open.

**Learning across apps.** Every change someone asks for is sorted into kinds (a chart, a search, a reminder, an export... `engine/changes.mjs`), and the kinds go into a graph in ChloeDB, `sa_changes`: each app points at the kinds of change it made (`did`), and each kind at the kinds that came next in an app (`then`). The kinds an app may want next are ranked by personalized PageRank from the app and its latest change, both ways along the edges (from what it did, to the apps that did the same, to what they did next), leaving out what it did already. Suggestions also learn which kinds people take: the share of each kind taken when it was offered, across every app. Both go into the suggestion prompts (after a build and from use), and a kind rarely taken (under one in ten of five or more) is listed last. Predictions wait for ten other apps with changes (`STRATA_APPS_NEXT_MIN_APPS`); `node test/next-change-eval.mjs STRATA_URL... --scratch URL` measures them by hiding each app's last change: over 19 apps the graph put it in its top 3 for 6 of 14, "what most apps added" for 8, so it needs more apps than this before it beats counting.

Each build keeps its timeline. Under Versions, **Timeline** on a version shows what took the time (reading ChloeDB, the model planning, each piece's writer, the checks, a second try, storing), what each model call cost and read, and what was decided for every piece of the app and when: used from the library as it is, copied from a starter, forked, rewritten, written from scratch, a new version of a shared piece, or kept. The top of the tab sums up every build of the app.

## What is where

| | |
|---|---|
| `engine/server.mjs` | HTTP API, live updates (server-sent events), the request queue, repairs, suggestions from use |
| `engine/mcp.mjs` | the engine as an MCP server over stdio, for an agent (Claude Code, Codex, any MCP client): `node engine/mcp.mjs --url http://127.0.0.1:7730`, each tool a call to the HTTP API (`app_create`, `app_request` and `app_wait`, `screen_edit`, read-only `data_query`, versions, library, publish). It signs in with the engine's agent token (`STRATA_APPS_AGENT_TOKEN`, or the file `--token-file` names, by default `DATA/agent.token`) and does not start without one; `publish` asks for `confirm: true` at every access |
| `engine/forge.mjs` | the forge: the prompt, the context it reads from ChloeDB (the app's code, tables, sample rows, what you did, library matches), the checks, and applying a plan as the next version |
| `engine/engine.mjs` | the runtime: SQL, inserts, updates, removes and function calls on an app's data; triggers; events; following ChloeDB's feed |
| `engine/store.mjs` | the system tables in ChloeDB and the app's tables |
| `engine/graph.mjs` | what each capability links to, read from its code, and the rows of `sa_graph` |
| `engine/library.mjs`, `library/` | the built-in library, one file each (`components/`, `helpers/`, and the starters in `views/` and `functions/`), seeded into ChloeDB at start-up; `library/examples/` holds each piece's example (props, calls with their results, rows) |
| `engine/sql.mjs` | rewrites `FROM tasks` to the app's physical table and refuses anything but one `SELECT` over the app's own tables |
| `engine/services.mjs` | people and sessions (scrypt passwords, session cookie), files (bytes under `DATA/files`, at most 15 MB, served with a sandbox policy and `nosniff`), the email outbox and a small SMTP client, address search |
| `engine/schedule.mjs` | schedules: `EVERY ...` read and said back, the next time in a time zone, and the scheduler that runs what is due |
| `engine/feeds.mjs` | RSS, Atom and RDF feeds, and JSON lists, as the items CREATE SOURCE and PULL add |
| `engine/reach.mjs` | a model, web search and the web for apps: the owner's switches, hosts and secrets, the cache and the daily limits, and the guard against addresses that are not the public internet |
| `engine/sandbox.mjs` | server functions run in QuickJS compiled to WebAssembly: a fresh interpreter per call, 64 MiB, 10 s, no network, files or Node APIs, `app.*` calls only |
| `engine/rules.mjs` | functions and triggers written as data: their checks and their runtime |
| `engine/ask.mjs` | questions about the graph ("which screens use the calendar"), answered from `sa_graph` |
| `engine/rulesql.mjs` | CREATE RULE: rules written as SQL, compiled into those steps |
| `engine/evolve.mjs` | evolving from use: patterns in events, candidates built beside the live version, keep (merge) and drop, and each candidate's measure |
| `engine/changes.mjs` | learning across apps: requests sorted into kinds of change, the graph of what apps changed next (`sa_changes`, ranked by ppr), and how often each kind of suggestion is taken |
| `engine/activity.mjs`, `web/activity.js` | the Activity view: a period's events with the names of their rows, drawn in the app's design |
| `engine/signals.sql` | the patterns of use as ChloeDB table functions over `sa_events` (searches, empty results, filters, repeated writes, undone adds, forms given up, paths, calls, time on screens) |
| `engine/designs.mjs`, `web/design.js` | each app's three designs: one small model call, design tokens, CSS variables |
| `web/front.js`, `web/front.css` | front pages: their blocks, motion and photos |
| `engine/photos.mjs`, `library/photos.json` | finding the photos a screen or an example row names (the built-in set, Wikimedia Commons, a drawn picture) |
| `engine/layouts.mjs` | the page structure a screen gets when it names none |
| `engine/audience.mjs` | who a view is for: the owner's views are left out for visitors |
| `engine/checks.mjs` | a build's tests, run on its new tables, and opening each view in a headless browser |
| `engine/llm.mjs` | the model, through `claude -p` with tools, settings and MCP switched off |
| `web/` | the studio, and the runtime (`frame.js`, `kit.css`) each view runs with in a sandboxed frame |
| `engine/access.mjs` | declared access: ALLOW statements per table (what, to whom, which rows by `:user_id` or `:company_id`), the decision for a person, the scope's SQL, and the forge's check that a request asking for sign-in declares it |
| `engine/publish.mjs` | apps open to the public: the Publish switch, `/a/<id>` and `<name>.<domain>` (`STRATA_APPS_DOMAIN`), hosts of the owner's own, what the public may call, and each visitor's rate ([On a cluster](../docs/chloedb/apps.md#on-a-cluster)) |
| `web/cc-view.js`, `engine/provenance.mjs` | the app creator, a page of a cluster's command center (Apps → App creator): describe and build apps, the live app on a phone, tablet or desktop, and **Made of**, how much of the code that runs the engine generated from CREATE statements and how much the agent wrote from scratch, piece by piece (`GET /api/apps/:id/made`); it gets the frame's runtime from `GET /api/kit` and live updates from long polls of `GET /api/events` ([On a cluster](../docs/chloedb/apps.md#on-a-cluster)) |

In ChloeDB:

| table | holds |
|---|---|
| `sa_apps` | each app and its live version (`head`) |
| `sa_capabilities` | every capability ever written, one row per version, with its code as SQL (the statement that makes it): `id` is `app/kind/slug@version`, and `lib/component/name@version` or `lib/helper/name@version` for a shared piece |
| `sa_compositions` | each version of each app: the capability ids it is made of (the versions of shared pieces it pins included), the request and a summary |
| `sa_graph` | one row per app, per live table, view and function, and per shared piece (by name, `live` naming its version); `links` (an `EDGES` field) holds labelled edges: `has`, `uses`, `data`, `on`, `calls`, `opens`, `forked` |
| `sa_requests` | requests to the forge, with their progress log, cost and errors |
| `sa_traces` | each build's timeline, by request: its steps and model calls with their times, tokens and cost, and the decision for each piece (`engine/trace.mjs`) |
| `sa_events` | what people did in each app: who (a person, or a visitor's cookie), on which version, in which session and screen, and the table, row (and its name), fields, words, results and time it was about |
| `sa_suggestions` | suggestions, from builds and from use (with the pattern they answer) |
| `sa_changes` | the kinds of change each app made and what came next, as a graph (an EDGES column) |
| `sa_candidates` | changes built from use beside the live version: the pattern, the version built, its measure, and whether it was kept |
| `sa_designs`, `sa_settings` | each app's three designs and the one in use; per-app settings (the evolve mode) |
| `sa_people`, `sa_members`, `sa_sessions` | accounts (one per email, for every app), who belongs to which app and with what role, and sessions (by the hash of their token) |
| `sa_files` | each uploaded file's name, type, size and who uploaded it; the bytes sit in `DATA/files/<app>/<id>` |
| `sa_outbox` | every email an app sent, with its status: held, sending, sent or failed |
| `app_<app>_<table>` | each app's data |

## How views and functions run

A **view** is JavaScript that defines `async function render(root, app)`. The
studio runs it in an `<iframe sandbox="allow-scripts ...">` with no same
origin and a content security policy that blocks the network, so a view can
reach data only through `app`, whose calls go to the studio as messages and
from there to the engine: `app.sql` (one `SELECT`, over the app's own tables),
`app.insert`, `app.update`, `app.remove`, `app.call`, `app.onChange`,
`app.open`, `app.track`, and the services: `app.me`, `app.signIn`,
`app.signUp`, `app.signOut`, `app.people`, `app.upload`, `app.files`,
`app.email`, `app.geocode`. Files and map tiles load from the engine by URL
(`app.fileUrl(id)`, `app.tiles`), the one origin the frame's policy allows
images from.

A **component** is JavaScript that defines `function component(props)` and
returns an element. Every frame gets the components its app pins, each in its
own script and loaded on first call; an error inside one is reported (and
repaired) against the component. Components may call other components and
helpers, and add a stylesheet once with `css(name, text)`.

A **helper** is JavaScript that defines `function helper()` returning an
object of plain functions, called as `lib.name.fn(...)`. It has no page and no
data calls, and is synchronous, so the same code runs in a view's frame and in
a server function's QuickJS.

A **function** is JavaScript that defines `function run(input, app)`. It runs
on the server in QuickJS (WebAssembly), with the same `app` calls answered by
the engine synchronously, and may be a trigger (`on: {table, event}`) that
runs after each insert, update or delete of a table.

The forge's checks: the plan's shape, names and types; types of existing
columns never change; every view parses and defines `render`, uses no
`fetch`, `import` or `localStorage`; every component defines `component`
and uses no `app.sql`, `app.insert` or other data call (services are
allowed); every helper uses no `app` at all; every `ui.name` a
view or component calls exists; every function compiles in QuickJS,
defines `run` and uses no `async`; triggers name tables that exist. An answer
that fails gets one more try with the problems listed.

## What it is not (yet)

A prototype: people can sign in to apps, but run alone the studio itself
has no sign-in (anyone who can reach it can change any app; put it behind
`deploy/gate.mjs` to share it, or run it beside a cluster, where it signs in
the cluster's admins), a table limits its rows only as far as its ALLOW
statements say (a WHERE compares one column with the person or their
company, no richer row rules yet), what
the model costs is limited only in all (`STRATA_APPS_SPEND_CAP_USD`, with a
share kept apart for apps' own questions) and per app for apps' own
questions (a count and dollars a day), and a capability that passes the checks can
still be wrong (the repair loop and going back are the safety net). The doc
lists what a real version needs.

## Tests

```bash
npm test     # the SQL rewrite, the answer parser, the plan checks, the sandbox, pinning, the built-in library (and every helper's example calls), the graph's edges,
             # functions seeing the signed-in person and the services, passwords, and SMTP against a fake mail server
```
