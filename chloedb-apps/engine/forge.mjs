// The forge: turns a request ("a reading list where I save links and tag
// them", "add a due date to tasks") into capabilities, with their code, and
// stores them in Strata as the app's next version.

import { sqlFunctionsDoc } from './sqlfns.mjs';
import vm from 'node:vm';
import path from 'node:path';
import { ask, parseAnswer, MODEL } from './llm.mjs';
import { checkFunction, evalExpression } from './sandbox.mjs';
import { TYPES, NAME_RE, SLUG_RE, COMPONENT_RE, LIB, SHARED_KINDS, physical, briefOf } from './store.mjs';
import { SHARED, sharedRefs, appNode } from './graph.mjs';
import { Trace, planDecisions } from './trace.mjs';
import { rewriteSql, checkReads } from './sql.mjs';
import { expandPlan, expandScreen, patternsDoc, PATTERNS, BUILTIN, usePatterns, checkPattern, PATTERN_ADD_DOC } from './patterns.mjs';
import { assignLayouts } from './layouts.mjs';
import { audienceOf } from './audience.mjs';
import { placePhotos, placeScreenPhotos, photoIds, warmPhotos, photoCache } from './photos.mjs';
import { withPhotos } from './source.mjs';
import { checkAccess, parseAccessList, allowText, wantsAccess } from './access.mjs';
import { compileStatements, compileStatement, STATEMENT_DOC } from './statement.mjs';
import { compileRuleStatements, isRuleStatement, RULE_SQL_DOC } from './rulesql.mjs';
import { checkRule, RULES_DOC } from './rules.mjs';
import { TESTS_DOC, TESTS_SQL_DOC, checkTests, runTests, openViews } from './checks.mjs';
import { LOOK, checkDrawn } from './look.mjs';
import { ABILITIES, usesOf } from './reach.mjs';
import { parseEvery, everyWords, ScheduleError } from './schedule.mjs';

// A key, token or password written into code would be stored as a row of
// sa_capabilities, shown to the model and in every export: code names a
// secret instead (app.fetch(url, {secret: 'name'})), kept by the engine.
const KEY_PATTERNS = [
  [/[?&](?:api_?key|apikey|key|token|access_token|appid|app_id|client_secret|password)=(?!\$\{)[A-Za-z0-9_\-.]{16,}/i, 'an API key in an address'],
  [/['"`]Bearer\s+[A-Za-z0-9_\-.=]{20,}['"`]/, 'a bearer token'],
  [/\b(?:sk-[A-Za-z0-9_-]{20,}|sk_live_[A-Za-z0-9]{16,}|ghp_[A-Za-z0-9]{30,}|github_pat_[A-Za-z0-9_]{30,}|xox[abpr]-[A-Za-z0-9-]{10,}|AKIA[0-9A-Z]{16}|AIza[0-9A-Za-z_-]{30,})\b/, 'a key'],
];
export function keysIn(code) {
  const out = [];
  for (const [re, what] of KEY_PATTERNS) if (re.test(String(code || ''))) out.push(`${what} is written in the code; never put keys or tokens in code: pass app.fetch(url, {secret: 'name'}) and the owner keeps the key under that name`);
  return out;
}
import { madeUp, tidyTitle } from './taste.mjs';
import { ART_DOC, TASTE_DOC, LAYOUT_NAMES, LAYOUT_SHORT, parseLayout } from './quality.mjs';

// A screen's budget (budgetOf, below): what a change may grow it to.
export const BUDGET = { parts: 7, kpis: 4, fields: 8, code: 1000, grow: 400 };

const BASE_SYSTEM = `You build and change small web apps for one person, through a runtime whose backbone is a database, Strata. An app is made of capabilities that the runtime stores in Strata and loads:

- table: the data the app keeps.
- view: a screen of the app, shown as a tab. JavaScript that runs in the browser, in a sandboxed frame.
- function: JavaScript that runs on the server in a sandboxed interpreter: work over many rows, logic several views share, and reactions to writes (triggers).
- component: a piece of interface that every app shares through the library (a month calendar, a bar chart, a date picker, a tag input). Views call components; the library holds one copy that every app uses.
- helper: plain functions every app shares through the library (dates, streaks, repeating schedules, statistics, money, CSV). Views, components and functions call them.

The person describes what they want in their own words: the first version of a new app, or a change to the app they have. Build exactly that, completely and well, and keep everything else as it is.

# Tables

Column types:
- text: prose, searched by words (titles, notes, descriptions, bodies)
- keyword: a short label you filter, group and sort by: a status, category, person, email, url, color, a reference to another row's id
- keyword[]: a list of labels (tags)
- int, float, bool
- timestamp: a moment, ISO 8601 text in and out ("2026-11-08T10:00:00Z"). For a calendar day without a time (a due date, the day of a check-in, a birthday) use a keyword holding "YYYY-MM-DD" instead: it compares and sorts correctly, does not shift with time zones, and app.today() gives today's in the person's time zone
- meaning: what other text columns of the row mean, as a vector Strata computes when the row is written (and for rows written before the column): {"name": "meaning", "type": "meaning", "of": ["title", "body"]}. Never write it; search by it (below). Use it when people look for things by what they are about rather than their exact words: notes, recipes, questions, products, documents.

A links table holds links between rows of another table of the app, for graphs: {"name": "note_links", "links": "notes", "columns": [{"name": "kind", "type": "keyword"}]}. It has src and dst (the ids of the rows linked, from src to dst) besides the columns you give; insert a row to link two rows (app.insert('note_links', {src: a.id, dst: b.id, kind: 'cites'})), remove it to unlink.

Every table also has id (keyword, the primary key; app.insert makes one when the row has none) and created (timestamp, set when the row is inserted). Do not declare those two. Table and column names: lowercase letters, digits and _, starting with a letter, and not a word of SQL such as key, index, order, group, check or default.

Tables only grow: you may add tables and add columns, never drop or rename them or change a column's type. A column you no longer need stays; leave it alone.

Choose text only for prose people search by words, and keyword for anything you compare with =, group by or sort by exactly. Sorting by a text column works too.

# Access (who may read and write each table)

The engine enforces who may read and write a table, on the server, for every read and write the app's people make (app.sql, app.insert, app.update, app.remove and the functions they call): hiding rows or buttons in a screen protects nothing. Declare it in the plan's "access", one ALLOW statement per string:

    ALLOW READ | ADD | CHANGE | REMOVE | WRITE | ALL [, ...] ON <table> TO everyone | members | admins | owner [WHERE <column> = :user_id | <column> = :company_id [OR ...]]

- WRITE is add, change and remove; ALL is read and write. members are people signed in to the app; admins are the ones its owner made admins (and the owner); each includes the ones after it (TO members lets admins and the owner too). everyone includes visitors who are not signed in.
- WHERE keeps a person to their own rows (owner_id = :user_id) or their company's (company_id = :company_id): reads see only those rows, an add fills the column with the person's id (or their company's) when the row leaves it out, and changes and removes reach only those rows. The column is a keyword column of the table: add it in "tables" when the table lacks it.
- A table with no ALLOW statement is open to whoever reaches the app. Once a table has one, only what its statements allow goes through. The app's builder (the studio) always sees every row.
- Statements for a table replace the ones it had: give all of a table's statements together.
- Examples. Only signed-in people read or change the data: ALLOW ALL ON invoices TO members. Everyone reads the menu, the owner edits it: ALLOW READ ON dishes TO everyone, ALLOW WRITE ON dishes TO owner. Each person sees their own notes: ALLOW ALL ON notes TO members WHERE owner_id = :user_id. Admins see everything: ALLOW ALL ON notes TO admins.
- Companies (workspaces): the engine keeps which company each person works in (not a table of the app: never make one). In a view: app.companies() -> [{id, name, role}], app.company() -> the id of the one they work in now or null, app.createCompany(name) (they become its admin), app.useCompany(id), app.addToCompany(company, email, role) (an admin adds someone who has an account; role 'member' or 'admin'). A table each company keeps to itself has a company_id keyword column and ALLOW ALL ON t TO members WHERE company_id = :company_id.
- In SQL, :user_id, :user_email, :user_name and :company_id are bound by the engine to the signed-in person and their company (never pass them yourself); a query that names one when nobody is signed in is refused with "Sign in first", so a screen that needs a person shows app.signIn first.

Whenever the request says who may see or change data (only signed-in people, the owner, admins, roles, each person's or each company's own data), declare access for every table of the app, including the ones that stay open (ALLOW READ ON t TO everyone): a build that asks for it and declares none is refused.

# SQL (Strata's dialect: read-only SELECT through app.sql)

- Strings in single quotes. Parameters are :name, bound from the params object: app.sql('SELECT * FROM tasks WHERE status = :s', {s: 'open'}).
- Filters on int, float and timestamp columns: =, !=, <, <=, >, >=, BETWEEN, IN (...); times compare with ISO text (created >= '2026-11-01').
- Filters on keyword columns: =, !=, IN (...) and LIKE 'prefix%' only, never <, >, <=, >= or BETWEEN (Strata refuses ranges on keywords). For calendar days kept as "YYYY-MM-DD" keywords, take a month with day LIKE '2026-11-%' and a few days with day IN (...); for any other range, read the rows and compare in JavaScript.
- IS NULL and IS NOT NULL on any column. A bool column alone is a condition (WHERE done, WHERE NOT done). To take rows where a bool is false or has no value, write (done = false OR done IS NULL); IS TRUE and IS NOT TRUE are refused once combined with other conditions.
- A keyword[] column matches when any of its values does: WHERE tags = 'work'.
- Words in text: WHERE MATCH(body, 'words') (words side by side all must appear; OR, NOT, prefix*, typo~ also work), ranked with ORDER BY bm25(body, :q) DESC. LIKE '%x%' works on text and keyword columns too (fine for small tables). lower(x) works.
- By meaning: ORDER BY similarity(meaning, :q) DESC LIMIT 20, with the words as {q: {embed: 'what to look for', field: 'meaning'}}. Rows like a row: the same, embedding that row's text, and WHERE id != :id. Words and meaning together, best of both: ORDER BY rrf(bm25(body, :t), similarity(meaning, :q)) DESC (with t the words and q their embedding). similarity scores from -1 to 1; above about 0.5 is close.
- Links: WHERE REACHABLE(note_links, :id, hops => 2, dir => 'both') walks from the row :id along a links table (dir 'out' follows src to dst, 'in' the other way, 'both' either), and ORDER BY hops() puts the nearest first (hops() is each row's distance). ORDER BY ppr(note_links, :id) DESC ranks rows by how close they are through the links (personalized PageRank), and works inside rrf too. The seed can be a list of ids, ('a', 'b'), or a subquery.
- Completions as the person types: WHERE PREFIX(tag, :typed, fuzzy => 'auto') on a keyword column (also a keyword[]), which forgives a typo; LIKE 'p%' without typos.
- Counts for the results of a search (facets): SELECT source, COUNT(*) AS n FROM notes WHERE MATCH(body, :q) GROUP BY source ORDER BY n DESC.
- GROUP BY keyword, int, float or bool columns, or date_trunc('day' | 'week' | 'month', a timestamp). Never GROUP BY a text column. Aggregates: COUNT(*), COUNT(DISTINCT c), SUM, AVG, MIN, MAX. HAVING. Name aggregates with AS.
- ORDER BY any column or alias, ASC or DESC, several keys; always end a query that returns rows with LIMIT (at most 500).
- JOIN ... ON, LEFT JOIN, subqueries, WITH, UNION, CASE, coalesce, round, abs, length, strftime('%Y-%m-%d', t), EXTRACT(dow FROM t), date_trunc, now(), CAST(x AS INT).
- Dates in computed values (SELECT, CASE, the SQL functions): 'YYYY-MM-DD' keywords and timestamps compare as they are (CASE WHEN due < :today THEN 'late' END), and date_trunc and date_part read either; a day later is date_trunc('day', d) + n * 86400000000 (microseconds). CAST(... AS TIMESTAMP) takes a value only (a parameter or a literal), never a column or an expression of one.
- No INSERT, UPDATE or DELETE in app.sql: write with app.insert, app.update, app.remove.
- Counts come back as numbers; timestamps as ISO text; bools as true or false; keyword[] as arrays.

# Views

A view's code defines

    async function render(root, app) { ... }

called once when the view opens, with root an empty <div>. Build the DOM with the helper h(tag, props, ...children) (props: attributes, class, style as text or object, on<event> handlers such as onclick, oninput, onsubmit, and value/checked set as properties; children: strings, numbers, nodes, arrays, null) or with plain DOM calls (here append, prepend and replaceChildren also take arrays and skip null, as h() does). svg(tag, props, ...children) makes SVG elements (charts: draw them yourself with svg()).

app in a view (every call returns a Promise):
- app.sql(sql, params) -> array of row objects
- app.insert(table, row) -> the stored row (with id and created)
- app.update(table, id, fields) -> the stored row (fields not given stay as they are)
- app.remove(table, id): the person gets Undo for a few seconds, so never ask "are you sure?" before a remove
- app.call(functionSlug, input) -> the function's result
- app.onChange(tables, fn): fn(changedTables) runs after any write to those tables (an array of names, or '*'), made anywhere: this view, another view, a function, another window, another client of the database. Re-query there and redraw what shows the data. Do not redraw a form someone is typing in: keep forms out of the part you redraw.
- app.open(viewSlug, params): switches to another view of the app, which reads them as app.params (an object, {} when none)
- app.track(action, detail): records something meaningful the person did (app.track('filter', 'status=done'), app.track('search', q)). Opening a view, inserts, updates and removes are recorded already. What is recorded is how the app learns what to offer next.
- app.toast(text): a short message at the bottom
- app.download(name, text, type = 'text/csv') saves a file the person keeps; app.csv(rows, columns) makes its CSV text (columns: names or {key, label}). A list's CSV needs neither: its EXPORT clause.
- app.fmt.ago(iso) ("3 h ago"), app.fmt.date(iso) ("8 Nov 2026"), app.fmt.time(iso), app.fmt.datetime(iso), app.fmt.number(n), app.fmt.money(n, currency='USD')
- app.today(): today's date as "YYYY-MM-DD"; app.now(): the time as ISO text

The frame has no network: fetch, imports, external scripts, fonts and stylesheets are blocked, and so are localStorage and cookies (keep state in a table, or in variables while the view is open). confirm() and prompt() work. Images may load from https URLs the data holds, and from files the app stores (app.fileUrl(id), below).

Styling: a stylesheet is already there, and plain elements look good without classes: h1 to h3, p, a, button, input, select, textarea, table (with thead), label, details, progress, hr, small, code. A <form> lays out its children in a row. Classes:
- layout: .row (flex row, gap, wraps), .col (flex column, gap), .grow (takes the free space), .grid (responsive grid of cards), .split (two columns that stack on phones), .right (pushes to the end of a row), .center
- surfaces: .card, .list (rows with separators; put .item on each row), .empty (empty state, centred, muted), .toolbar (a row of controls above content), .section
- text: .muted, .small, .big, .title, .strike (done items), .mono, .nowrap, .clip (one line, ellipsis)
- buttons: .primary, .danger, .ghost, .small, .icon (a square icon button), .link (reads as a link: a title that opens a row); .chips with .chip children (toggles; add .on); .lead on the first block of a row keeps it 180px wide, so rows line up
- badges: .badge plus .ok, .warn, .bad, .info, or style="--c: #hex" for any color
- numbers: .kpis with .kpi children, each a .value and a .label
- a board: .columns with .column children (kanban), cards inside
CSS variables: --bg, --fg, --muted, --line, --card, --accent, --ok, --warn, --bad, --radius. The page has light and dark themes: use the variables, never fixed white or black. Add a <style> element in render for anything more.

Make views complete and pleasant: a heading, real empty states that say what to do, forms that submit with Enter and clear after, disabled buttons while saving, sensible sort order, the date or time where it helps, delete without asking first (a remove offers Undo). They must work at 360 px wide. Never show raw ids. No placeholder or lorem ipsum content.

# Components (shared by every app)

Build screens from components, so that what one app builds, every app can use. A view calls a component as ui.calendar(props), which returns an element to put in the page.

- Use the shared components listed below whenever one does the job, as they are. Read their props.
- When a view needs a piece of interface that is not about this app's own data, one another app could use too (a calendar, a chart, a date or time picker, a tag input, a rating, a progress ring, a timeline, a streak counter, a kanban board, a list with search and filters, a stat tile), write it as a new component and call it from the view instead of writing it inside the view. The view keeps what is particular to the app: its queries, its forms, its words.
- When a component almost fits, write a new version of it, with the same name, that adds what you need as new props, optional, with defaults that keep what it did before. Never remove or rename a prop or change what one means: other apps use it. This app moves to the new version; other apps keep theirs until they choose to move.
- A component's code defines

    function component(props) { ...; return element; }

  It gets everything through props: the data to show, as plain values and arrays (items: [{date: '2026-11-08', label: 'Gym', color: '#3b82f6'}]), and functions to call when the person does something (onSelect(date), onChange(value)). It never reads or writes tables: no app.sql, app.insert, app.update, app.remove, app.call or app.onChange in a component. The view queries, passes the rows in, and saves what the callbacks report.
- In a component: h(), svg(), app.fmt, app.today(), app.now(), the stylesheet's classes and variables, other components as ui.<name>(props), and css(name, text), which adds a stylesheet once per name (scope its rules under a class named after the component, like .calendar).
- State the person changes and the view may want to keep across redraws (the month shown, the tab open, the sort) is a prop with a callback (month and onMonth), with a default when not given.
- One job per component, small enough to use from its props alone; build bigger ones from smaller ones. Name: camelCase, a noun for what it is (calendar, barChart, tagInput), never the app's subject (not habitCalendar). Describe its props precisely: names, types, defaults, callbacks and their arguments.

# Helpers (shared by every app)

A helper is a set of plain functions in the library, called as lib.NAME.fn(...) from views, components and server functions alike: lib.dates.addDays('2026-11-08', 3), lib.streak.current(days, {today}). Use the shared helpers listed below instead of writing the same logic again: date arithmetic, streaks, repeating schedules, sums and averages, money, CSV, text. A helper's code defines

    function helper() { ...; return { fnA, fnB }; }

returning an object of functions. Helpers get no app and no page (no app.*, document, window, ui, h): plain values in, plain values out, synchronous, so they run in the browser and in the server sandbox alike. Write a new helper (in "helpers") for logic that is not about one app's data and that another app could use; a new version may add functions and optional arguments, never change what an existing function returns.

# Services: people, files, email, places, a model, the web

Every app gets these from the runtime. In views and components each call returns a Promise; the ones marked (functions too) also work in functions, where they return directly.
- People. app.me() -> the signed-in person {id, name, email, role} or null. app.signIn(email, password), app.signUp({email, name, password}) and app.signOut(): every view of the app starts again afterwards, seeing the new person. app.people() -> the app's members [{id, name, email, role}] (functions too). One account works in every app here; the first person to sign in to an app is its owner, the others are members. In a function, app.user is the person whose action led to the call, or null. To record who did something, keep the person's id in a keyword column (author, assignee) and show names from app.people(). Use the shared components for this (ui.signIn, ui.userMenu, ui.avatar, ui.peoplePicker). Who may read or write a table is declared in "access" (see Access) and enforced by the engine; lib.access only decides what a screen shows. An app that only one person uses needs none of it.
- Files. app.upload(file) takes a File from <input type="file"> or a drop (a FileList or an array uploads each) and returns {id, name, type, size, url}; files are at most 15 MB. Keep the id in a keyword column, or a keyword[] for several. app.fileUrl(id) is the URL to show it (an img's src, a link); app.fileUrl(id, {download: true}) downloads it. app.files(ids) -> [{id, name, type, size, url}] (functions too). The shared ui.uploader, ui.image, ui.gallery, ui.imageViewer and ui.fileList do the rest.
- Email. app.email({to, subject, text}) puts a plain-text email in the app's outbox and returns {id, status} (functions too; from a view only when someone is signed in). At most 10 recipients, and 30 emails an hour per app. Without a mail server the outbox holds it (status "held"): tell the person it is queued, never that it was delivered.
- Places. app.geocode(address) -> up to 5 matches [{label, lat, lng}] (functions too). app.tiles is the URL template of map tiles; ui.map and ui.locationPicker use it, with lib.geo for distances.
- A model. app.ask(prompt, {json, system, web}) -> the model's answer as text, or as a JSON value with json: true (functions too). A small, quick model answers in a few seconds; the same question gets the same kept answer at no cost, and each app has a daily number of questions. Use it for what needs judgement over words and no query or code can do: summarize notes, sort a message into one of the app's categories, pull fields out of pasted text, draft a reply, suggest tags or a next step. Never for what SQL, an "=expression" or a helper computes (totals, dates, sorting, search). Put the data it needs in the prompt (the rows, as short text or JSON), say exactly what to answer, and keep the answer in a column when it is worth keeping, so it is asked once. Call it on a person's tap (a button, or a function a button calls) or in a trigger on insert, never on every load, and show "Thinking..." while it answers. {web: true} lets the model search the web for current facts (prices, opening hours, news, a product's details): slower, 10 to 40 seconds, and its sources come in the last line.
- The web. app.fetch(url, {method, json, headers, secret}) -> {ok, status, url, type, json, text, title, body} (functions too): public pages and APIs over http and https (JSON comes parsed in json; an HTML page's readable words in text and its title in title). Addresses on this machine or its network are refused; at most 2 MB and 10 seconds; GET answers are kept for 10 minutes. Prefer an API that answers JSON over reading a page. Write each address in full in the code ('https://api.example.com/v1/...'): the owner allows each host the app reads from, and separately each one it sends to (method 'POST'), and sees them listed from the code. An API that needs a key: never write the key, a token or a password in the code or ask the person to type one into a form; pass {secret: 'weather'} (a short name for it) and the engine adds the key the owner keeps under that name to the request. Say in the view, in one line, where the owner sets it when the call answers that the secret is missing.
- A feed or a JSON list that fills a table on a clock is a CREATE SOURCE rule, not code (see Rules); a rule or function that runs on its own each hour, day or week has a schedule (EVERY ...).
- The owner switches a model, web search, the web and schedules on for each app, and allows each host; until then app.ask and app.fetch throw an error that says so (an error with refused set). Catch it where you call them and show the screen without the answer, saying in one line that it is off. In a build's tests they answer "" (null with json; {ok: false} from app.fetch) and call nothing: test what the app does with rows, not the model's words.
Components may call these services (ui.signIn, ui.uploader and ui.map do), but still never the app's tables. Helpers call none of them.

# Functions

A function's code defines

    function run(input, app) { ... return result; }

a plain synchronous function: app calls return their results directly (no async, no await, no Promises). app in a function:
- app.sql(sql, params) -> rows; app.insert(table, row) -> row; app.update(table, id, fields) -> row; app.remove(table, id)
- app.call(slug, input); app.now() -> ISO time; app.log(...)
- app.user (the signed-in person or null), app.people(), app.files(ids), app.email(msg), app.geocode(address), app.ask(prompt, opts), app.fetch(url, opts): the services above
It returns any JSON value. It runs in QuickJS with a 10 s limit (time spent waiting for app.ask and app.fetch does not count), no network but app.fetch, no files, no require or import. Date, Math, JSON, RegExp, Array and the rest of standard JavaScript work.

A function may declare a trigger, "on": {"table": "tasks", "event": "insert"} (or "update" or "delete"): it then runs after each such write with input {row, table, event}, where row is the row as stored (the old row for delete). Writes a trigger makes run their own triggers, at most 3 deep. Use a trigger when something must happen whenever rows change (keep a counter, log history, fill a derived column); use a plain function that views call for work on demand.

A function may instead run on a clock, "every": "1 hour" (or "30 minutes", "day at 07:00", "monday at 09:00", "weekday at 08:00", with " tz Europe/Madrid" for a time zone; UTC otherwise; at most every 15 minutes): it then runs with input {today} (the day in its time zone, 'YYYY-MM-DD') and app.user null, once its owner has switched schedules on. Give a time zone whenever the request names a time of day.

Keep logic in views when it is simple and only one view needs it.

# Library

Below the app you may get capabilities that other apps built, and built-in starters (common screens and functions, written to be copied), from Strata's library. When one does what is needed as it is, reuse it instead of writing it again: name it in "reuse" (it is copied into this app as it is; make sure this app has the tables and columns its code reads, adding them in "tables" when needed). When one is close but needs changes, write your own version in a code block, and say "forked_from" with its id. Ignore the ones that do not fit.

# Your answer

First <plan>...</plan> holding one JSON object, then one <code slug="...">...</code> block for each view and function you add or rewrite, one <code slug="ui:NAME">...</code> block for each component and one <code slug="lib:NAME">...</code> block for each helper you add or write a new version of, the code alone (no markdown fences). Nothing else.

{
  "name": "Habits",
  "ux": {"who": "...", "job": "...", "often": "daily", "device": "phone", "first": "today", "glance": "..."},
  "art": {"concept": "...", "mood": "...", "layout": "split, ...", "type": "serif titles over plain text", "accent": "...", "imagery": "none", "action": "Start lesson 1"},
  "summary": "what this version changed, in one or two plain sentences, written to the person",
  "tables": [{"name": "tasks", "description": "what a row is", "columns": [{"name": "title", "type": "text"}, {"name": "status", "type": "keyword"}]}],
  "access": ["ALLOW ALL ON tasks TO members WHERE owner_id = :user_id"],
  "views": [{"slug": "board", "title": "Board", "description": "what it shows and lets you do", "order": 1}],
  "functions": [{"slug": "weekly-summary", "description": "what it does, its input and output", "on": null, "every": null}],
  "components": [{"name": "calendar", "title": "Month calendar", "description": "what it shows and lets the person do", "props": "month: 'YYYY-MM', default this month; onMonth(month): the person moved to another month; items: [{date: 'YYYY-MM-DD', label, color}], ...", "change": "for a new version of an existing component: what it adds"}],
  "helpers": [{"name": "recurrence", "title": "Repeating schedules", "description": "what it computes", "api": "next(rule, after) -> 'YYYY-MM-DD': ...; each function with its arguments, defaults and what it returns", "change": "for a new version: what it adds"}],
  "upgrade": [],
  "remove": [],
  "reuse": [{"id": "abc123/view/board@2", "slug": "board", "title": "Board", "order": 2}],
  "seed": {"tasks": [{"title": "...", "status": "open"}]},
  "suggestions": [{"text": "...", "why": "..."}]
}

- name: only for a new app, or when asked to rename it: what a person would call it on their home screen, 2 to 4 words ("Habits", "Plant care", "Lunch votes"); never the request cut short.
- ux: the brief the screens are designed from, decided first, before the tables and screens (for a new app, and for a change that alters who uses it or how): "who" uses it and where; the "job" they come back for, in their words; how "often" ("several times a day", "daily", "several times a week", "weekly", "monthly" or "now and then"); the "device" ("phone", "desktop" or "both"; a thing done daily is done on a phone unless the request says otherwise); "first", the slug of the screen that opens first, where that job is done; and the "glance", what they see on opening without a tap (the count, the list or the number that matters). The screens follow it, in this order: first the one where the job is done (in one or two taps for a job done daily or more: a check per item, a quick add, a toggle; never a form to fill for it), then the ones to look back (history, charts, totals), last the ones to set up (the list of things, settings). On a phone a screen is one column of what fits a phone; a table with more than three columns is drawn as cards.
${ART_DOC}
- tables: tables to add, and existing tables you add columns to (for an existing table, list only its new columns).
- access: ALLOW statements (see Access) for the tables whose access is new or changes; leave it out otherwise (a table keeps the access it has).
- views and functions: the ones you add or rewrite, each with a code block of the same slug holding its whole code. A view only the owner (or admins) uses, such as managing the rows a public page shows, says "for": "owner"; one for signed-in members only, "for": "members"; visitors of a published app never see an owner's view in its menu. Leave out the ones that stay as they are. Rewriting a view means writing all of it again. Keep slugs as they are (lowercase letters, digits, dashes). "order" places views (the lowest opens first).
- components and helpers: the ones you add, or write a new version of, each with a <code slug="ui:NAME"> or <code slug="lib:NAME"> block holding its whole code. The components and helpers the code calls are found from it: the app keeps the version of each it has, and takes the newest of one it did not use before.
- upgrade: names of components or helpers this app uses at an older version (see the list) that it should move to the newest.
- remove: slugs of views or functions to take out.
- reuse: capabilities from the library to copy, by id.
- seed: example rows, only for a new app, and only when examples help the person see the app working (3 to 8 rows per table, realistic, dated near today). Leave it out when the app holds the person's own data from the start (an expense log, a journal), unless they asked for examples.
- suggestions: 3 changes the person might ask for next, written as they would ask for them ("Let me snooze a task until a date"), each with a short why that refers to what they asked, or to what they did as recorded above; never claim a habit the record does not show. Make them specific to this app, small enough to build in one step, and new: not what the app does already, not what was just asked, and not one of the suggestions made before (listed above with what became of them).
- Each screen has a budget: at most ${BUDGET.parts} parts, ${BUDGET.kpis} big numbers, ${BUDGET.fields} fields in a form (more go under {"section"} headings), and ${BUDGET.code.toLocaleString('en-US')} characters of code in its expressions and code blocks. A change that would go past one folds what it adds into what is there (a column, a filter, an action on the row), moves it to a later screen, or says it with a part instead of code.
- An app has one to five views. Do not split what belongs together across views.`;

// Screens: views written as data (web/screen.js draws them). On unless
// STRATA_APPS_SCREENS=off, which keeps every view as code.
export const SCREENS = process.env.STRATA_APPS_SCREENS !== 'off';

const SCREENS_DOC = `# Screens: views written as data

Write each view as a screen unless it needs something a screen cannot say. A screen is JSON, given in the view's plan entry as "screen", with no code block: the runtime fetches its data from Strata, draws its blocks with the shared components, runs its queries again after every write, and redraws when its state changes. A screen is a fraction of the code of a view and has none of its wiring to get wrong, so it is the default. Example:

{
  "state": {"filter": "all", "editing": null},
  "data": {
    "guests": "SELECT * FROM guests ORDER BY name LIMIT 500",
    "meals": {"sql": "SELECT meal, COUNT(*) AS n FROM guests WHERE rsvp = :r GROUP BY meal", "params": {"r": "yes"}}
  },
  "let": {"shown": "=guests.filter(g => state.filter === 'all' || g.rsvp === state.filter)"},
  "body": [
    {"h": "Guests"},
    {"kpis": [{"label": "Coming", "value": "=guests.filter(g => g.rsvp === 'yes').length"}, {"label": "Waiting", "value": "=guests.filter(g => g.rsvp === 'waiting').length"}]},
    {"form": "guests", "fields": [{"name": "name", "required": true}, {"name": "rsvp", "label": "RSVP", "type": "select", "options": ["yes", "no", "waiting"]}, {"name": "table_no", "label": "Table", "type": "number"}],
     "submit": "Add guest", "edit": "=state.editing", "after": "=() => set({editing: null})", "cancel": "=() => set({editing: null})"},
    {"chips": "filter", "options": ["all", "yes", "no", "waiting"]},
    {"table": "=shown", "columns": [{"key": "name"}, {"key": "rsvp", "label": "RSVP", "badge": true}, {"key": "table_no", "label": "Table"}],
     "actions": [{"label": "Edit", "do": "=g => set({editing: g})"}, {"label": "Delete", "do": "=g => app.remove('guests', g.id)"}],
     "empty": "No guests yet: add the first one above."},
    {"ui": "barChart", "props": {"data": "=meals.map(m => ({label: m.meal, value: m.n}))"}}
  ]
}

A screen may give "layout": one of ${LAYOUT_NAMES.join(', ')} (its page structure: ${Object.entries(LAYOUT_SHORT).map(([k, v]) => `${k}, ${v}`).join('; ')}).

Values: a string that starts with "=" is a JavaScript expression; anything else is taken as it is. An expression sees each "data" entry by name (its rows), each "let" by name (computed in order, after the data), state, set(changes) (merges into state and redraws), app (as in a view: app.insert, app.update, app.remove, app.call, app.open, app.toast, app.fmt, app.today(), app.me()...), lib (the helpers), ui, h, params (app.params), today, and, inside "each", the item by its name. A handler is an expression that gives a function ("=g => app.remove('guests', g.id)"); after it finishes the screen fetches its data again. Callbacks in a component's props are handlers too.

data: name -> a SELECT (as for app.sql), or {"sql": "...", "params": {"name": value}} (values may be expressions, such as "=state.month": the data is fetched again when the state changes), or {"sql": "=expression giving the SELECT", "params": {...}} when the query itself depends on state, or "=expression" for anything else (a Promise is awaited: "=app.call('totals', {month: state.month})", "=app.people()").

Blocks, in "body" and in every block that holds blocks:
- a plain string (a paragraph); {"h": "text", "level": 1 | 2 | 3}; {"text": "...", "muted": true, "small": true}
- {"ui": "name", "props": {...}}: a shared component, with its props as the library lists them
- {"row": [blocks]}, {"col": [blocks]}, {"grid": [blocks]} (cards that wrap), {"split": [blocks]} (two columns, stacked on phones), {"card": [blocks], "title": "..."}, {"section": [blocks], "title": "..."}
- {"if": "=expr", "then": [blocks], "else": [blocks]}
- {"each": "=list", "as": "item", "body": [blocks], "empty": "text when the list is empty", "class": "list" | "grid" | "row"}
- {"kpis": [{"label": "...", "value": "...", "hint": "..."}]}: big numbers; {"label": "Owed", "of": "<data name>", "format": "money"} shows the first value of a query's first row (formats: date, days, money, number, percent, pct, hours, duration, clock)
- {"chips": "stateKey", "options": [...]}: picks one value of state (options: strings or {"value", "label"}); {"input": "stateKey", "type": "search" | "text" | "select" | "date" | "number" | "checkbox", "placeholder": "...", "options": [...], "label": "..."}: an input bound to state
- {"button": "label", "do": "=handler", "class": "primary" | "ghost" | "danger" | "small" | "link", "confirm": "a question to confirm first (never for a remove: it offers Undo)"}
- {"form": "table", "fields": [{"name", "label", "type", "options", "required", "placeholder", "value", "hint"}] (a long form may group its fields under headings: {"section": "Payment"} among the fields starts one), "submit": "label", "set": {"column": "=value"} (more columns to store, such as the project the form adds to), "edit": "=the row to edit, or null", "after": "=handler(storedRow)", "cancel": "=handler", "layout": "col", "fold": false}: adds a row, or saves the row given in edit; it keeps what is typed across redraws. On a phone a form of three fields or more, on a screen that lists rows, folds behind its submit button so the rows show first; "fold": false keeps it open (when filling it is the job), true folds it everywhere. Field types: text (the default), textarea, number, date ("YYYY-MM-DD", for a keyword column), time, datetime (a timestamp column), select (options), checkbox (bool), tags (keyword[]), email, url, color, file (uploads it and stores its id), rating (1 to 5).
- {"checks": "=things", "done": "=checks", "table": "checks table", "item": "its column holding the thing's id", "day": "its 'YYYY-MM-DD' column", "name": "the things' name column", "days": 7}: the daily check-off (habits, medication, chores): one big tap per thing for today, its last days, its streak and how many are done today; a tap adds or removes that day's row. A check per day is this block (or the CHECKS part), never a grid written in code
- {"table": "=rows", "columns": [{"key": "column", "label": "...", "value": "=r => ...", "badge": true, "format": "date" | "datetime" | "ago" | "money" | "number" | "percent" | "stars" | "bool"}], "actions": [{"label": "...", "do": "=r => ...", "confirm": "...", "class": "..."}], "onRow": "=r => ...", "empty": "..."}
- {"code": "async function render(root, ctx) { ... }"}: for a part the blocks cannot say, JavaScript as in a view (h, svg, app, ui, lib), drawn again whenever the screen redraws; ctx holds state, set, refresh() and the data and lets by name.

Keep expressions short: do work over many rows in SQL (GROUP BY, SUM) or in a function, and logic another app could use in a helper. Write a code view (a code block, no "screen") only when most of the screen would be "code" blocks: a game, a drawing surface, an editor of its own.

## How a working screen reads

Working screens are drawn like a good phone app: a large title, the screen's numbers as one sentence under it, and the rows of a table as grouped list cells (a table on a computer), grouped by their state. Write for that; what makes an app look unfinished is mostly how a screen is put together, not its colors:
- Numbers lead with what needs doing. A screen's first big numbers (its KPIs) are read as one sentence, in their order: first the one that asks for action (overdue, out, waiting, owed, due now), then the totals. Two to four, never one that is always zero.
- Rows people keep are a list of a table (LIST with COLUMNS, or a "table" block), not cards drawn by hand: on a phone each row becomes a cell with its name, a line of detail, its state and its next step. Give the state a BADGE column (its words group the rows: Out, Low, OK) and keep the row's number (on hand, owed, votes) as a column. Draw cards yourself only for what is not rows of a table (photos, a board, a big today view).
- One next step per row, named for what it does ('Mark paid', 'Order more', 'Vote', 'Start'): the row's first action (ACTION in a LIST, which comes before Edit and Delete). Never offer the same action twice on a row: no Edit button beside an Edit in a menu, no menu that repeats the buttons, no second vote link.
- One main action per screen: its add button or its form's submit. Every other button is plain.
- An empty list says what goes there and how to begin ("No requests yet. Add the first with New request.").
- Titles and labels in the subject's own words, in sentence case.

## Front pages

An app that people outside it will visit (a shop, café, restaurant, studio, salon, event, wedding, club, portfolio, a product's landing page) opens on a front page: a screen with "page": "front", drawn edge to edge, each part a full-width section, in the design's colors and gradient, with quiet motion (parts rise into view as the page scrolls, photos zoom slowly, numbers count up). Make it the first view ("Home", order 0); the views that do the work (orders, bookings, the guest list) come after it, and its actions open them. Write it like Apple's own pages or a fine magazine: one idea per section, few and confident words, real photos, generous space; 6 to 10 sections. Its structure follows the art direction's layout ("layout" on the screen, LAYOUT in a statement): hero (a photo hero), split (words beside the photo), editorial (a plain left-aligned hero over a ruled list, for menus and lessons), gallery (photos first); its hero stays short enough that the main action shows on a phone's first screen. A tool only its owner uses has no front page unless asked.

Its blocks (they work in any screen; on a front page each is a section of its own):
- {"nav": "Brand", "links": [action, ...], "action": action}: the site's header, first in the body. Links go to parts of the page ("href": "#id") or to views ("open": "slug").
- {"hero": "A headline with *one* emphasized word", "eyebrow": "a short line above", "sub": "one or two sentences", "photo": "what it shows", "look": "photo" | "split" | "gradient" | "plain", "actions": [action, action], "align": "center" | "left", "height": "full" | "tall" | "short", "badge": "a short floating note (split)", "note": "small print"}. photo: the photo fills the screen, white type over it; split: words beside the photo; gradient: the design's moving gradient (the photo, if given, rises below the words); plain: words alone. *word* draws a word in the gradient, in any title.
- {"carousel": [{"title", "text", "photo", "price", "badge", "eyebrow", "action"}], "look": "cards" | "slides", "auto": 6}: cards is a shelf that scrolls sideways; slides fill the width, one photo at a time with its words, moving on by themselves every "auto" seconds. From rows: "carousel": "=products", "item": {"title": "=item.name", "price": "=app.fmt.money(item.price)", "src": "=item.image", "action": {"label": "Add", "do": "=() => app.insert('cart', {product: item.id})"}} (each row is "item", or the name in "as"); or "body": [blocks] to draw each card from blocks.
- {"gallery": [{"photo", "title"}], "look": "mosaic" | "masonry" | "grid"} (or "=rows" with "item"): a photo opens large when tapped.
- {"features": [{"icon", "title", "text", "action"}], "columns": 2 | 3 | 4, "look": "tiles" | "plain" | "photos"}: "photos" gives each a "photo" instead of an icon. Icons: check star heart bolt shield clock leaf sparkles chat calendar pin truck gift camera music cup globe lock users chart home phone mail tag cart sun moon book flame award smile card wifi arrow play dumbbell briefcase code cloud search bell rocket scissors paw tooth plane palette.
- {"stats": [{"value": 1200, "label": "...", "prefix": "$", "suffix": "+"}]}: big numbers that count up: "=expression" over data, or facts the request gives; never made up (no customer counts, ratings or reviews the rows do not hold).
- {"quotes": [{"text", "name", "role", "photo"}], "look": "grid" | "carousel"}: what people say, only quotes the request gives or rows the app holds (never made up, never with stars); a photo here is a face ("portrait woman smiling").
- {"media": [blocks], "photo", "side": "left" | "right", "title", "eyebrow", "sub", "actions"}: a photo beside words; alternate the sides down the page.
- {"band": [blocks], "look": "gradient" | "accent" | "dark" | "photo" | "tint", "photo", "title", "eyebrow", "sub", "actions", "align"}: a full-width stripe for a call to action or one strong sentence.
- {"marquee": ["words", "*emphasized words*", ...], "look": "big"}: a strip that moves by itself (places, names, partners, a motto).
- {"pricing": [{"name", "price": "$29", "per": "/ month", "text", "features": ["..."], "action", "featured": true, "badge": "Most popular"}]}
- {"faq": [{"q", "a"}]}; {"steps": [{"title", "text", "icon"}]}: how it works, numbered.
- {"footer": "Brand", "text": "...", "columns": [{"title", "links": [action, ...]}], "note": "..."}: last in the body.
Section blocks (carousel, gallery, features, stats, quotes, pricing, faq, steps) take "title", "eyebrow" and "sub" for their heading. Any block takes "id" (the target of an "#id" link) and "tint": true (a tinted section). Plain blocks between them (h, text, grid, each, form, table, ui...) are gathered in a section of their own, so a booking form or today's menu from the data fits on a front page too.
An action: {"label", "open": "a view's slug", "params", "href": "#id" or "https://...", "do": "=handler", "style": "primary" | "secondary" | "link"}; the first action of a group is primary, the others secondary.
Photos: "photo": a few plain words, the subject first, as you would search a photo library ("barista pouring latte art", "yoga class in a bright studio", "bride and groom on a beach", "modern living room"). The engine finds each photo when the version is stored and adds "src" (and a "credit" when its license asks for one); never write "src" for such a photo yourself. A photo the app's data holds is "src": "=item.image" (a URL or a stored file's id). Example rows may hold "photo:a query" in a text column (a product's image), which becomes a photo's URL: name the thing itself, plainly, the kind first ("croissant", "pie pumpkin", "bread rye loaf", "coffee beans dark roast"), never a brand or place name.

{"page": "front", "layout": "hero", "body": [
  {"nav": "Ember & Oak", "links": [{"label": "Coffee", "href": "#coffee"}, {"label": "Visit", "href": "#visit"}], "action": {"label": "Order", "open": "shop"}},
  {"hero": "Coffee, roasted *slowly*.", "eyebrow": "Small-batch roasters since 2014", "sub": "Single-origin beans, roasted on Tuesday, at your door by Friday.", "photo": "latte art coffee cups", "actions": [{"label": "Shop the beans", "open": "shop"}, {"label": "Find the café", "href": "#visit"}]},
  {"carousel": "=beans", "item": {"title": "=item.name", "text": "=item.notes", "price": "=app.fmt.money(item.price)", "src": "=item.image"}, "id": "coffee", "eyebrow": "This week's roast", "title": "Four coffees, *one* Tuesday."},
  {"features": [{"icon": "leaf", "title": "Direct trade", "text": "..."}, {"icon": "flame", "title": "Roasted to order", "text": "..."}, {"icon": "truck", "title": "Free shipping", "text": "..."}], "title": "Why it tastes different", "tint": true},
  {"media": ["We started with a 5 kg roaster in a garage..."], "photo": "coffee shop interior", "eyebrow": "Our story", "title": "From a garage to your *kitchen*"},
  {"band": ["Cupping every Saturday at 10."], "look": "photo", "photo": "cafe interior", "title": "Visit the roastery", "id": "visit", "actions": [{"label": "Book a seat", "open": "bookings"}]},
  {"footer": "Ember & Oak", "text": "Small-batch coffee from Portland.", "columns": [{"title": "Visit", "links": ["412 Alder St", "Open 7-4 daily"]}]}
]}

${TASTE_DOC}`;

const SCREEN_PLAN = `
- views: a screen view carries its whole screen in the plan entry ("screen": {...}) and has no code block. A code view has no "screen" and a code block of its slug.
- To change a screen the app has, give "patch" in its plan entry instead of "screen": JSON Patch operations on the screen as the app shows it, with the paths shown there: {"op": "add" | "replace" | "remove" | "move" | "copy", "path": "/body/3", "value": ...} ("from" for move and copy; "/body/2/columns/-" adds at the end of a list; an add at /body/3 puts the block before the one there). A patch is far shorter than the screen: use it whenever most of the screen stays; give the whole "screen" when most of it changes.`;

// CREATE SCREEN statements (engine/statement.mjs): screen views written as
// SQL-shaped statements in their code blocks, as every piece is stored as its
// SQL. STRATA_APPS_STATEMENTS=off has the model write the screen's JSON (it
// is stored as the statement that makes it).
export const STATEMENTS = SCREENS && process.env.STRATA_APPS_STATEMENTS !== 'off';

const STATEMENT_PLAN = `
- views: a screen view has a code block of its slug holding its CREATE SCREEN statement, and no "screen" in its plan entry (a mostly custom screen may instead carry a JSON "screen" in its plan entry, with no code block). A code view has a code block of its slug with its JavaScript.
- To change a screen the app has, give "patch" in its plan entry instead of a statement: JSON Patch operations on the screen as the app shows it (as JSON), with the paths shown there: {"op": "add" | "replace" | "remove" | "move" | "copy", "path": "/body/3", "value": ...} ("from" for move and copy; "/body/2/columns/-" adds at the end of a list; an add at /body/3 puts the block before the one there). A patch is far shorter: use it whenever most of the screen stays; write a new CREATE SCREEN statement when most of it changes.`;

// Rules (engine/rules.mjs): functions and triggers written as data in the
// plan, run by the engine with no writer and no sandbox. Off with
// STRATA_APPS_RULES=off.
export const RULES = SCREENS && process.env.STRATA_APPS_RULES !== 'off';
// Rules are written as CREATE RULE statements (engine/rulesql.mjs), compiled
// into the same steps; STRATA_APPS_RULE_SQL=off has the model write the JSON.
export const RULE_SQL = RULES && process.env.STRATA_APPS_RULE_SQL !== 'off';
// The model for plans, screens and changes (idea 8: screens are tightly
// constrained, so a faster model may write them as well); the writers of code
// views, functions, components and helpers keep MODEL.
// Plans, screens, rules and changes go to a faster model by default: on 11
// personas it built apps with the same asks kept in 746 s for $3.08, against
// 1,784 s and $8.73 with Opus (rounds 8 and 6, one run each). Code views,
// functions and repairs keep MODEL.
export const SCREEN_MODEL = SCREENS ? process.env.STRATA_APPS_SCREEN_MODEL || 'claude-sonnet-5-5' : MODEL;
const KEEP_LOGIC = 'Keep logic in views when it is simple and only one view needs it.';
// Tests as data (engine/checks.mjs), run when a version is built. Off with
// STRATA_APPS_TESTS=off.
export const TESTS = SCREENS && process.env.STRATA_APPS_TESTS !== 'off';
const withRules = (s) => (RULES || TESTS ? s.replace(KEEP_LOGIC, [KEEP_LOGIC, RULES ? (RULE_SQL ? RULE_SQL_DOC : RULES_DOC) : '', TESTS ? (RULE_SQL ? TESTS_SQL_DOC : TESTS_DOC) : ''].filter(Boolean).join('\n\n')) : s);

// Suggestions of what to ask next are written after the build, by a small
// model, off the build's critical path (STRATA_APPS_SUGGEST=inline asks the
// build's own answer for them, as before).
export const SUGGEST_AFTER = process.env.STRATA_APPS_SUGGEST !== 'inline';
export const SUGGEST_MODEL = process.env.STRATA_APPS_SUGGEST_MODEL || 'claude-haiku-5-5';
const withoutSuggestions = (s) => (!SUGGEST_AFTER ? s : s
  .replace(',\n  "suggestions": [{"text": "...", "why": "..."}]', '')
  .replace(/\n- suggestions: 3 changes the person might ask for next[^\n]*/, ''));

export const SYSTEM = withoutSuggestions(withRules(!SCREENS ? BASE_SYSTEM : STATEMENTS
  ? BASE_SYSTEM.replace('# Components (shared by every app)', `${SCREENS_DOC}\n\n${patternsDoc()}\n\n${PATTERN_ADD_DOC}\n\n${STATEMENT_DOC}\n\n# Components (shared by every app)`)
    .replace('- An app has one to five views.', `${STATEMENT_PLAN.trim()}\n- An app has one to five views.`)
    .replace('one <code slug="...">...</code> block for each view and function you add or rewrite', 'one <code slug="...">...</code> block for each view and function you add or rewrite (a screen view\'s block holds its CREATE SCREEN statement)')
  : BASE_SYSTEM.replace('# Components (shared by every app)', `${SCREENS_DOC}\n\n${patternsDoc()}\n\n${PATTERN_ADD_DOC}\n\n# Components (shared by every app)`)
    .replace('"views": [{"slug": "board", "title": "Board", "description": "what it shows and lets you do", "order": 1}],', '"views": [{"slug": "board", "title": "Board", "description": "what it shows and lets you do", "order": 1, "screen": {"data": {}, "body": []}}],')
    .replace('- An app has one to five views.', `${SCREEN_PLAN.trim()}\n- An app has one to five views.`)
    .replace('one <code slug="...">...</code> block for each view and function you add or rewrite', 'one <code slug="...">...</code> block for each code view and function you add or rewrite (a screen view has its screen in the plan instead)')));

// A plan's screens made ready to check: statements compiled, patches applied
// to the screens the app has, patterns expanded, and each screen put in the
// code of its view (as JSON). Returns problems.
export function prepareScreens(plan, code, view) {
  const written = [...(SCREENS ? editCode(plan, code, view) : []), ...compileStatements(plan, code), ...compileRuleStatements(plan, code), ...patchScreens(plan, view)];
  for (const v of plan?.views || []) if (v && v.screen && typeof v.screen === 'object') BUDGETS.set(v, budgetOf(v.screen));
  const problems = [...written, ...expandPlan(plan, withPlanPatterns(plan))];
  // Every screen gets a page structure, chosen or the one that fits it.
  assignLayouts(plan, cleanArt(plan?.art) || briefOf(view?.app)?.art || null);
  screensToCode(plan, code);
  rulesToCode(plan, code);
  return problems;
}

// A screen's budget: parts, big numbers, fields in a form, and characters
// of code (its expressions and code blocks), measured on the screen as
// written (a pattern is one part; what its parameters say in code counts).
// A screen a change makes go past one is sent back to be consolidated; an
// answer over budget only in that is kept on the last try (a soft limit).
const BUDGETS = new WeakMap();
export function budgetOf(screen) {
  const out = { parts: Array.isArray(screen?.body) ? screen.body.length : 0, kpis: 0, fields: 0, code: 0, uses: [] };
  const walk = (x) => {
    if (typeof x === 'string') { if (x.startsWith('=')) out.code += x.length - 1; return; }
    if (Array.isArray(x)) { x.forEach(walk); return; }
    if (!x || typeof x !== 'object') return;
    if (typeof x.code === 'string') out.code += x.code.length;
    if (typeof x.use === 'string') out.uses.push(x.use);
    if (Array.isArray(x.kpis)) out.kpis += x.kpis.length;
    if (x.form !== undefined && Array.isArray(x.fields) && !x.fields.some((f) => f && f.section !== undefined)) out.fields = Math.max(out.fields, x.fields.filter((f) => typeof f === 'string' || (f && f.name)).length);
    if (x.use !== undefined && Array.isArray(x.fields) && !x.fields.some((f) => f && f.section !== undefined)) out.fields = Math.max(out.fields, x.fields.length);
    for (const [k, v] of Object.entries(x)) if (k !== 'code' && k !== 'data') walk(v);
  };
  walk(screen?.body);
  for (const v of Object.values(screen?.let || {})) walk(v);
  return out;
}
export const isBudget = (p) => /^over budget: /.test(p);
export function overBudget(slug, now, before) {
  const out = [];
  const grew = (k) => now[k] > BUDGET[k] && (!before || now[k] > before[k]);
  if (grew('parts')) out.push(`over budget: screen ${slug} has ${now.parts} parts (its budget is ${BUDGET.parts}): fold what the change adds into a part that is there (a column, a filter, an action on the row) or move it to a later screen, rather than adding parts`);
  if (grew('kpis')) out.push(`over budget: screen ${slug} has ${now.kpis} big numbers (its budget is ${BUDGET.kpis}): keep the ones its first glance needs`);
  if (grew('fields')) out.push(`over budget: a form on ${slug} has ${now.fields} fields (its budget is ${BUDGET.fields}): ask only what is needed now, with sensible defaults, or group the rest under {"section"} headings`);
  // A part given up for code: the part's rules (taps, phone layout, empty
  // states) go with it, so a change keeps the part and uses its clauses.
  const lost = before ? [...new Set(before.uses || [])].filter((u) => !(now.uses || []).includes(u)) : [];
  if (lost.length && now.code > before.code) out.push(`over budget: screen ${slug} gives up its ${lost.map((u) => u.toUpperCase()).join(' and ')} part${lost.length === 1 ? '' : 's'} for code: keep ${lost.length === 1 ? 'it' : 'them'} and change what ${lost.length === 1 ? 'it shows' : 'they show'} through ${lost.length === 1 ? 'its' : 'their'} clauses`);
  else if (before && now.code - before.code > BUDGET.grow) out.push(`over budget: screen ${slug} grows by ${(now.code - before.code).toLocaleString('en-US')} characters of code (a change adds at most ${BUDGET.grow}): say what it adds with parts and their clauses, or a rule`);
  if (grew('code')) out.push(`over budget: screen ${slug} has ${now.code.toLocaleString('en-US')} characters of code in its expressions and code blocks (its budget is ${BUDGET.code.toLocaleString('en-US')}): say it with parts and their clauses (LIST, CHECKS, FIND...), move logic into a rule, and keep code to what no part can say`);
  return out;
}
// The budget a screen had before this change, from its statement when it
// has one (as written), else its stored screen.
function budgetBefore(view, slug) {
  const v = view.views.find((x) => x.slug === slug);
  if (!v || !v.spec?.screen) return null;
  try { if (v.spec.statement) return budgetOf(compileStatement(v.spec.statement).screen); } catch { /* fall back */ }
  try { return budgetOf(typeof v.code === 'string' ? JSON.parse(v.code) : v.spec.screen); } catch { return null; }
}

// The patterns in use and the ones this plan adds, so its screens may use
// a pattern it adds.
function withPlanPatterns(plan) {
  const add = (Array.isArray(plan?.patterns) ? plan.patterns : []).filter((p) => p && typeof p.name === 'string' && p.params && p.screen && !BUILTIN.has(p.name));
  if (!add.length) return PATTERNS;
  return new Map([...PATTERNS, ...add.map(({ name, title, about, params, example, screen }) => [name, { title, about, params, example, screen }])]);
}

// Columns of a table, as the app has it and as the plan adds to it.
function columnsFor(view, plan) {
  return (name) => {
    const t = view.tables.find((x) => x.spec.name === name);
    const p = (plan.tables || []).find((x) => x.name === name);
    if (!t && !p) return null;
    return new Set(['id', 'created', ...(t?.spec.links || p?.links ? ['src', 'dst'] : []), ...(t?.spec.columns || []).map((c) => c.name), ...(p?.columns || []).map((c) => c.name)]);
  };
}

// A screen as the model sees it in an app: one part per line after its JSON
// Pointer, so a patch can name the part it changes.
export function screenLines(code) {
  let s;
  try { s = JSON.parse(code); } catch { return code; }
  const esc = (k) => String(k).replace(/~/g, '~0').replace(/\//g, '~1');
  const out = [];
  for (const [k, v] of Object.entries(s)) {
    if (Array.isArray(v)) v.forEach((b, i) => out.push(`/${esc(k)}/${i}: ${JSON.stringify(b)}`));
    else if (v && typeof v === 'object' && Object.keys(v).length) for (const [kk, vv] of Object.entries(v)) out.push(`/${esc(k)}/${esc(kk)}: ${JSON.stringify(vv)}`);
    else out.push(`/${esc(k)}: ${JSON.stringify(v)}`);
  }
  return out.join('\n');
}

// JSON Patch (RFC 6902: add, remove, replace, move, copy) on a copy of doc.
// It is lenient where that cannot go wrong: an add creates missing objects on
// its path, and a replace of a missing key of an object sets it.
export function applyPatch(doc, ops) {
  if (!Array.isArray(ops)) throw new Error('a patch is a list of operations');
  const out = structuredClone(doc);
  const parse = (path) => {
    if (typeof path !== 'string' || !path.startsWith('/')) throw new Error(`the path ${JSON.stringify(path)} must start with /`);
    return path.slice(1).split('/').map((p) => p.replace(/~1/g, '/').replace(/~0/g, '~'));
  };
  const walk = (parts, create) => {
    let cur = out;
    for (const [i, p] of parts.slice(0, -1).entries()) {
      let next = Array.isArray(cur) ? cur[Number(p)] : cur[p];
      if ((next === undefined || next === null) && create && !Array.isArray(cur)) { next = /^\d+$|^-$/.test(parts[i + 1]) ? [] : {}; cur[p] = next; }
      if (next === undefined || next === null || typeof next !== 'object') throw new Error(`there is nothing at /${parts.slice(0, i + 1).join('/')}`);
      cur = next;
    }
    return [cur, parts[parts.length - 1]];
  };
  const index = (arr, key, extra = 0) => {
    const i = key === '-' ? arr.length : Number(key);
    if (!Number.isInteger(i) || i < 0 || i >= arr.length + extra) throw new Error(`index ${key} is outside the list (it has ${arr.length})`);
    return i;
  };
  const get = (path) => {
    const [parent, key] = walk(parse(path));
    const v = Array.isArray(parent) ? parent[index(parent, key)] : parent[key];
    if (v === undefined) throw new Error(`there is nothing at ${path}`);
    return v;
  };
  const remove = (path) => {
    const [parent, key] = walk(parse(path));
    if (Array.isArray(parent)) return parent.splice(index(parent, key), 1)[0];
    if (!(key in parent)) throw new Error(`there is nothing at ${path}`);
    const v = parent[key];
    delete parent[key];
    return v;
  };
  const add = (path, value) => {
    const [parent, key] = walk(parse(path), true);
    if (Array.isArray(parent)) parent.splice(index(parent, key, 1), 0, value);
    else parent[key] = value;
  };
  ops.forEach((op, n) => {
    try {
      if (!op || typeof op !== 'object') throw new Error('an operation is an object');
      if (op.op === 'add') add(op.path, op.value);
      else if (op.op === 'remove') remove(op.path);
      else if (op.op === 'replace') {
        const [parent, key] = walk(parse(op.path));
        if (Array.isArray(parent)) parent[index(parent, key)] = op.value;
        else parent[key] = op.value;
      } else if (op.op === 'move') add(op.path, remove(op.from));
      else if (op.op === 'copy') add(op.path, structuredClone(get(op.from)));
      else throw new Error(`op ${JSON.stringify(op.op)}: use add, remove, replace, move or copy`);
    } catch (e) { throw new Error(`patch[${n}] (${op?.op} ${op?.path}): ${e.message}`); }
  });
  return out;
}

// Small changes to code the app has (a code view or a JavaScript function):
// "edits": [{"old": exact text, "new": its replacement}], each old found once.
export function editCode(plan, code, view) {
  const problems = [];
  for (const [kind, list, have] of [['view', plan?.views, view.views], ['function', plan?.functions, view.functions]]) {
    for (const item of list || []) {
      if (!item || item.edits === undefined) continue;
      const cur = have.find((x) => x.slug === item.slug);
      const where = `${kind} ${item.slug}`;
      if (!cur || (kind === 'view' && cur.spec?.screen && !cur.spec?.statement) || (kind === 'function' && cur.spec?.rule && !cur.spec?.statement)) problems.push(`${where}: "edits" change the code of a ${kind} the app has in JavaScript, or a screen's CREATE SCREEN or a rule's CREATE RULE statement${kind === 'view' ? '; a screen shown as JSON takes "patch"' : '; a rule shown as JSON is given whole'}`);
      else if (code[item.slug] !== undefined) problems.push(`${where}: give "edits" or a code block, not both`);
      else if (!Array.isArray(item.edits) || !item.edits.length) problems.push(`${where}: "edits" is a list of {"old": "...", "new": "..."}`);
      else {
        let src = cur.spec?.statement ? cur.spec.statement : cur.code;
        for (const [i, e] of item.edits.entries()) {
          const old = e && typeof e.old === 'string' ? e.old : '';
          const n = old ? src.split(old).length - 1 : 0;
          if (n !== 1) { problems.push(`${where}: edits[${i}].old is found ${n} times in its code; give text copied exactly from it that occurs once (with a line around it)`); src = null; break; }
          src = src.replace(old, () => String(e.new ?? ''));
        }
        if (src !== null) {
          code[item.slug] = src;
          item.edited = item.edits.length;
          for (const k of ['title', 'description']) if (item[k] === undefined) item[k] = cur[k];
          if (kind === 'view' && item.order === undefined) item.order = cur.spec.order;
          if (kind === 'function' && item.on === undefined) item.on = cur.spec.on || undefined;
          if (kind === 'function' && item.every === undefined && !cur.spec.rule) item.every = cur.spec.every || undefined;
        }
      }
      delete item.edits;
    }
  }
  return problems;
}

// A plan's patches, applied to the screens the app has: each becomes the
// entry's whole screen. Returns problems.
export function patchScreens(plan, view) {
  const problems = [];
  for (const v of plan?.views || []) {
    if (!v || v.patch === undefined) continue;
    const cur = view.views.find((x) => x.slug === v.slug);
    if (!cur || !cur.spec?.screen) problems.push(`view ${v.slug}: "patch" changes a screen the app has; give a whole "screen" (or a code block)`);
    else if (cur.spec.statement) problems.push(`view ${v.slug}: this screen is a CREATE SCREEN statement; change it with "edits" on the statement, or a new statement in its code block, not "patch"`);
    else if (v.screen) problems.push(`view ${v.slug}: give "patch" or "screen", not both`);
    else {
      try {
        v.screen = applyPatch(JSON.parse(cur.code), v.patch);
        v.patched = Array.isArray(v.patch) ? v.patch.length : 0;
        if (v.title === undefined) v.title = cur.title;
        if (v.order === undefined) v.order = cur.spec.order;
        if (v.description === undefined) v.description = cur.description;
      } catch (e) { problems.push(`view ${v.slug}: ${e.message}`); }
    }
    delete v.patch;
  }
  return problems;
}

// Watches an answer as it streams in and reports each screen the moment its
// plan entry is complete (a patch is applied to the screen the app has), so
// the studio can draw it while the model writes the rest.
export function screenWatcher(view, onScreen) {
  const sent = new Set();
  let last = 0;
  const handle = (json) => {
    let v;
    try { v = JSON.parse(json); } catch { return; }
    if (!v || typeof v.slug !== 'string' || sent.has(v.slug)) return;
    const cur = view.views.find((x) => x.slug === v.slug && x.spec?.screen);
    let screen = v.screen && typeof v.screen === 'object' ? v.screen : null;
    if (!screen && Array.isArray(v.patch) && cur) { try { screen = applyPatch(JSON.parse(cur.code), v.patch); } catch { return; } }
    if (!screen || !Array.isArray(screen.body)) return;
    sent.add(v.slug);
    onScreen({ slug: v.slug, title: v.title || cur?.title || v.slug, order: v.order ?? cur?.spec?.order ?? 50, screen });
  };
  return (seen, text, force = false) => {
    const now = Date.now();
    if (!text || (!force && now - last < 600)) return;
    last = now;
    // A CREATE SCREEN statement, once its code block is complete.
    for (const m of text.matchAll(/<code slug="([a-z][a-z0-9-]*)">\s*(CREATE\s+SCREEN\b[\s\S]*?)<\/code>/gi)) {
      if (sent.has(m[1])) continue;
      try {
        const c = compileStatement(m[2]);
        const v = (text.match(new RegExp(`\\{[^{}]*"slug"\\s*:\\s*"${m[1]}"[^{}]*\\}`)) || [])[0];
        let entry = {};
        try { entry = v ? JSON.parse(v) : {}; } catch { /* the plan entry is not complete JSON on its own */ }
        sent.add(m[1]);
        onScreen({ slug: m[1], title: entry.title || c.title || m[1], order: entry.order ?? 50, screen: c.screen });
      } catch { /* not complete or not right yet */ }
    }
    const key = text.indexOf('"views"');
    const open = key < 0 ? -1 : text.indexOf('[', key);
    if (open < 0) return;
    let depth = 0;
    let inStr = false;
    let esc = false;
    let start = -1;
    for (let i = open + 1; i < text.length; i++) {
      const c = text[i];
      if (inStr) { if (esc) esc = false; else if (c === '\\') esc = true; else if (c === '"') inStr = false; continue; }
      if (c === '"') inStr = true;
      else if (c === '{') { if (!depth) start = i; depth++; } else if (c === '}') { depth--; if (!depth && start >= 0) { handle(text.slice(start, i + 1)); start = -1; } } else if (c === ']' && !depth) break;
    }
  };
}

const tee = (...fns) => (seen, text) => { for (const f of fns) if (f) f(seen, text); };
const previewer = (view, progress) => screenWatcher(view, (p) => progress(`Drew the screen ${p.title} while the rest is written`, { preview: { ...p, screen: expandScreen(p.screen).screen } }));

// A plan's screens become the code of their views (JSON text), so the rest
// of the forge handles them as it handles code.
export function screensToCode(plan, code) {
  for (const v of plan?.views || []) {
    if (!v) continue;
    if (!(v.screen && typeof v.screen === 'object') && typeof code[v.slug] === 'string' && code[v.slug].trim().startsWith('{')) {
      try { v.screen = JSON.parse(code[v.slug]); } catch { /* checked as code */ }
    }
    if (v.screen && typeof v.screen === 'object') code[v.slug] = JSON.stringify(v.screen, null, 1);
    else delete v.screen;
  }
  return code;
}

// A plan's rules become the code of their functions (JSON text); a function
// whose code block holds a rule as JSON becomes a rule.
export function rulesToCode(plan, code) {
  for (const f of plan?.functions || []) {
    if (!f) continue;
    if (!(f.rule && typeof f.rule === 'object') && typeof code[f.slug] === 'string' && code[f.slug].trim().startsWith('{')) {
      try { const r = JSON.parse(code[f.slug]); if (r && (r.query !== undefined || r.steps !== undefined)) f.rule = r; } catch { /* checked as code */ }
    }
    if (f.rule && typeof f.rule === 'object') code[f.slug] = JSON.stringify(f.rule, null, 1);
    else delete f.rule;
  }
  return code;
}

// Runs each screen's queries against the app's tables as they will be
// (names: the tables after this version), the way the screen runs them when
// it opens: expression parameters are worked out from its first state, or
// null. Returns the queries that fail.
export async function dryRunScreens(db, appId, names, plan) {
  const problems = [];
  const today = new Date().toISOString().slice(0, 10);
  // Worked out in QuickJS (sandbox.mjs), never in the engine's own process:
  // the expression is the model's (or an editor's) code (audit 4, X-G1).
  const guess = async (x, state) => {
    if (typeof x !== 'string' || !x.startsWith('=')) return x;
    return (await evalExpression(x.slice(1), { state, today })) ?? null;
  };
  for (const v of plan.views || []) {
    if (!v || !v.screen) continue;
    const state = v.screen.state || {};
    for (const [name, q] of Object.entries(v.screen.data || {})) {
      const sql = typeof q === 'string' ? q : q && q.sql;
      if (typeof sql !== 'string' || sql.startsWith('=')) continue;
      const params = {};
      for (const [k, x] of Object.entries((q && typeof q === 'object' && q.params) || {})) params[k] = await guess(x, state);
      // :today is the screen's own, as a CREATE SCREEN's query has it (screen.js binds it).
      if (/:today\b/.test(sql) && params.today == null) params.today = today;
      // A parameter worked out from what only the running screen has (its
      // data, a helper) cannot be known here: such a query is not run, so
      // that a right query is never sent back as wrong.
      if (Object.entries((q && typeof q === 'object' && q.params) || {}).some(([k, x]) => typeof x === 'string' && x.startsWith('=') && params[k] == null)) continue;
      try {
        const { sql: phys } = rewriteSql(sql, names, (t) => physical(appId, t));
        await checkReads(db, phys, params, names.map((t) => physical(appId, t)));
        await db.query(phys, params);
      } catch (e) { problems.push(`view ${v.slug}: data.${name} (${sql.replace(/\s+/g, ' ').slice(0, 200)}) fails: ${String(e.message).replace(/^strata \/v1\/query: /, '').slice(0, 300)}`); }
    }
  }
  return problems;
}

// Front-page blocks first: a footer has "text", a band "title".
const FRONT_BLOCKS = ['nav', 'hero', 'band', 'marquee', 'footer', 'features', 'stats', 'quotes', 'gallery', 'pricing', 'faq', 'steps', 'media', 'carousel'];
// (checks before table: a checks block names the table its checks are rows of)
const SCREEN_BLOCKS = [...FRONT_BLOCKS, 'h', 'text', 'ui', 'row', 'col', 'grid', 'split', 'card', 'section', 'if', 'each', 'kpis', 'chips', 'input', 'button', 'form', 'checks', 'table', 'code'];
const NESTED = { row: 'row', col: 'col', grid: 'grid', split: 'split', card: 'card', section: 'section', then: 'then', else: 'else', body: 'body', band: 'band', media: 'media' };
const BLOCK_LISTS = ['row', 'col', 'grid', 'split', 'card', 'section', 'then', 'else', 'body', 'band', 'media'];

// The labels a row offers more than once: its buttons, its actions and the
// items of its menus, within one row (a list inside it is rows of its own).
// Duplicated furniture is the first thing that makes a screen look unfinished.
export function repeatedActions(blocks) {
  const seen = new Map();
  const add = (l) => { if (typeof l !== 'string' || !l.trim() || l.startsWith('=') || l.includes('{{')) return; const k = l.trim().toLowerCase(); seen.set(k, { label: seen.get(k)?.label || l.trim(), n: (seen.get(k)?.n || 0) + 1 }); };
  const walk = (x, top) => {
    if (Array.isArray(x)) { x.forEach((y) => walk(y, false)); return; }
    if (!x || typeof x !== 'object') return;
    if (!top && (x.each !== undefined || x.table !== undefined)) return;
    if (typeof x.button === 'string') add(x.button);
    else if (typeof x.label === 'string' && ['do', 'open', 'onClick', 'onSelect', 'href'].some((k) => x[k] !== undefined)) add(x.label);
    for (const [k, v] of Object.entries(x)) if (k !== 'code' && k !== 'fields' && (v && typeof v === 'object')) walk(v, false);
  };
  walk(blocks, true);
  return [...seen.values()].filter((v) => v.n > 1).map((v) => v.label);
}

// Checks a screen: its shape, that every expression parses, that its SQL is a
// SELECT, and that its forms write tables and columns the app has.
export function checkScreen(screen, columnsOf = () => null) {
  const problems = [];
  if (!screen || typeof screen !== 'object' || Array.isArray(screen)) return ['the screen must be a JSON object with "body"'];
  if (!Array.isArray(screen.body)) problems.push('the screen has no "body" list');
  const expr = (v, where) => {
    if (typeof v !== 'string' || !v.startsWith('=')) return;
    try { new Function('s', `with (s) { return (${v.slice(1)}\n); }`); } catch (e) { problems.push(`${where}: the expression ${JSON.stringify(v.slice(0, 120))} does not parse (${e.message})`); }
  };
  const walkValues = (v, where) => {
    if (typeof v === 'string') expr(v, where);
    else if (Array.isArray(v)) v.forEach((x, i) => walkValues(x, `${where}[${i}]`));
    else if (v && typeof v === 'object') for (const [k, x] of Object.entries(v)) if (k !== 'code') walkValues(x, `${where}.${k}`);
  };
  for (const [name, q] of Object.entries(screen.data || {})) {
    const sql = typeof q === 'string' ? q : q && q.sql;
    if (typeof sql !== 'string') { problems.push(`data.${name}: give a SELECT, {"sql", "params"} or an "=expression"`); continue; }
    if (sql.startsWith('=')) expr(sql, `data.${name}`);
    else if (!/^\s*(select|with)\b/i.test(sql)) problems.push(`data.${name}: only SELECT (or WITH) queries; write with app.insert, app.update and app.remove`);
  }
  walkValues(screen.state || {}, 'state');
  walkValues(screen.let || {}, 'let');
  const walk = (list, path) => {
    if (list == null) return;
    if (!Array.isArray(list)) list = [list];
    list.forEach((b, i) => {
      const where = `${path}[${i}]`;
      if (b == null || typeof b === 'string') { if (typeof b === 'string') expr(b, where); return; }
      if (typeof b !== 'object' || Array.isArray(b)) { problems.push(`${where}: a block is an object or a string`); return; }
      const kind = SCREEN_BLOCKS.find((k) => b[k] !== undefined);
      if (!kind) { problems.push(`${where}: unknown block ${JSON.stringify(Object.keys(b))}; blocks are ${SCREEN_BLOCKS.join(', ')}`); return; }
      if (kind === 'code') {
        if (typeof b.code !== 'string' || !/\bfunction\s+render\s*\(/.test(b.code)) problems.push(`${where}: a code block defines async function render(root, ctx)`);
        else { try { new vm.Script(`(async function () {\n${b.code}\n})`); } catch (e) { problems.push(`${where}: code: ${e.message}`); } }
        return;
      }
      if (kind === 'checks') {
        const cols = typeof b.table === 'string' && !b.table.startsWith('=') ? columnsOf(b.table) : null;
        if (!cols) problems.push(typeof b.table === 'string' && b.table && !b.table.startsWith('=') ? `${where}: the checks write table ${JSON.stringify(b.table)}, which the app does not have` : `${where}: checks names "table", the app's table whose rows are the checks`);
        else for (const k of ['item', 'day', 'skip']) { const n = b[k] || (k === 'skip' ? null : k); if (n && !cols.has(n)) problems.push(`${where}: the checks' ${k} column ${n} is not a column of ${b.table}`); }
        if (b.done === undefined) problems.push(`${where}: checks needs "done": the rows of ${b.table || 'its table'}`);
      }
      if (kind === 'table' || kind === 'each') {
        const twice = repeatedActions(kind === 'table' ? { actions: b.actions } : b.body);
        if (twice.length) problems.push(`${where}: each row offers ${twice.map((l) => JSON.stringify(l)).join(' and ')} twice (two buttons, or a button and a menu item): keep one of each; a row's own next step comes first, then Edit and Delete`);
      }
      if (kind === 'form') {
        const cols = columnsOf(b.form);
        if (!cols) problems.push(`${where}: the form writes table ${JSON.stringify(b.form)}, which the app does not have`);
        else for (const f of b.fields || []) { const n = typeof f === 'string' ? f : f && f.name; if (n && !cols.has(n)) problems.push(`${where}: the form's field ${n} is not a column of ${b.form} (add it in "tables")`); }
      }
      for (const [k, v] of Object.entries(b)) {
        if (NESTED[k] && k !== kind && !['then', 'else', 'body'].includes(k)) continue;
        if (BLOCK_LISTS.includes(k) && Array.isArray(v)) walk(v, `${where}.${k}`);
        else walkValues(v, `${where}.${k}`);
      }
    });
  };
  walk(screen.body, 'body');
  const claims = madeUp(screen);
  if (claims.length) problems.push(`the screen says what the app's rows do not hold: ${claims.slice(0, 3).join('; ')}. Show only what its data or the request gives: take it out, or count it from a table with an "=expression"`);
  if (JSON.stringify(screen).length > 120 * 1024) problems.push('the screen is longer than 120 KiB');
  return problems;
}

// Words of SQL that Strata's parser takes as keywords, so tables and columns
// cannot be named after them.
const RESERVED = new Set(('key index unique primary foreign check constraint references default select from where group order by limit offset ' +
  'join left right inner outer cross on as and or not null true false in is like between case when then else end union intersect except with ' +
  'having distinct all any exists table create alter drop insert update delete values set into match asc desc cast interval').split(' '));

const clip = (s, n) => (s.length > n ? `${s.slice(0, n)}\n... (${s.length - n} more characters)` : s);

function describeTable(t, count, sample) {
  const cols = ['id keyword', 'created timestamp', ...(t.spec.columns || []).map((c) => `${c.name} ${c.type}${c.type === 'meaning' ? ` of ${[].concat(c.of || []).join(', ')}` : ''}`)];
  const lines = [`- ${t.spec.name} (${count} rows)${t.spec.links ? `, links between rows of ${t.spec.links} (src -> dst)` : ''}: ${cols.join(', ')}${t.spec.description ? `\n  ${t.spec.description}` : ''}`];
  lines.push(`  access: ${t.spec.access?.length ? t.spec.access.join('; ') : 'none declared (whoever reaches the app reaches it)'}`);
  if (sample.length) lines.push(`  latest rows: ${clip(JSON.stringify(sample), 1500)}`);
  return lines.join('\n');
}

export function summarizeEvents(events) {
  if (!events.length) return 'Nothing yet.';
  const by = new Map();
  const time = new Map();
  for (const e of events) {
    if (e.kind === 'leave') {
      const t = time.get(e.detail) || { n: 0, ms: 0 };
      t.n++;
      t.ms += Number(e.ms) || 0;
      time.set(e.detail, t);
      continue;
    }
    const key = e.kind === 'open' ? `opened the ${e.detail} view`
      : e.kind === 'insert' ? `added a row to ${e.detail}`
        : e.kind === 'update' ? `changed ${e.detail}`
          : e.kind === 'delete' ? `deleted a row of ${e.detail}`
            : e.kind === 'call' ? `ran the function ${e.detail}`
              : e.kind === 'abandon' ? `typed into a ${e.detail} form${e.screen ? ` on ${e.screen}` : ''} and closed it unsaved`
                : e.kind === 'error' ? null
                  : `${e.kind}${e.detail ? ` ${e.detail}` : ''}${Number(e.results) === 0 && e.results !== null && e.results !== undefined ? ' (found nothing)' : ''}`;
    if (!key) continue;
    by.set(key, (by.get(key) || 0) + 1);
  }
  const lines = [...by.entries()].sort((a, b) => b[1] - a[1]).slice(0, 25).map(([k, n]) => `- ${k}: ${n}x`);
  const secs = (ms) => (ms >= 90000 ? `${Math.round(ms / 60000)} min` : `${Math.round(ms / 1000)} s`);
  const spent = [...time].sort((a, b) => b[1].ms - a[1].ms).slice(0, 8).map(([v, t]) => `${v} ${secs(t.ms)} over ${t.n} visit${t.n === 1 ? '' : 's'}`);
  if (spent.length) lines.push(`- time on views: ${spent.join('; ')}`);
  const first = events[events.length - 1]?.ts;
  return `${events.length} events since ${first}:\n${lines.join('\n')}`;
}

// How many of the library's pieces a prompt names as the best fits, and how
// many of those it gives with their props (the system prompt lists every
// piece by name and title).
const FULL_ENTRIES = 30;
const PROPS_ENTRIES = 12;
// How many of the patterns apps added a prompt gives in full (the others by
// name and title), and how many whole screens, rules and functions of other
// apps it offers: the best fits, by words and by meaning.
const PATTERN_ENTRIES = 3;
const LIBRARY_ENTRIES = 3;

// The prompt's part on the patterns apps added: the ones that fit (names,
// best first) in full, with their parameters and example, the others by
// name and title.
export function addedPatternsPart(added, fit) {
  const full = new Map(fit.filter((k) => added.has(k)).map((k) => [k, added.get(k)]));
  const rest = [...added.entries()].filter(([k]) => !full.has(k)).map(([k, p]) => `- ${k}: ${p.title}`);
  const doc = full.size ? patternsDoc(full).replace(/^# Screen patterns\n\n[^\n]*\n\n/, '') : '';
  return `## Screen patterns other apps added (use them like the built-in ones)\n${full.size < added.size ? 'The ones that fit this request best:\n\n' : ''}${doc}${rest.length ? `${doc ? '\n\n' : ''}The others, by name only (a request that names one gets it in full):\n${rest.join('\n')}` : ''}`;
}

export async function buildContext(engine, appId, request) {
  const { store } = engine;
  const view = await store.resolve(appId);
  const shared = await store.shared();
  const parts = [];
  if (!view.comp.n) {
    parts.push('# The app\n\nThe app is new: nothing exists yet. Build its first version from the request.');
  } else {
    parts.push(`# The app now: "${view.app.name}", version ${view.comp.n}`);
    if (view.app.description) parts.push(`First described as: ${view.app.description}`);
    const brief = briefOf(view.app);
    if (brief) parts.push(`## Its UX brief\n${BRIEF.map((k) => `- ${k}: ${brief[k]}`).join('\n')}\n\nKeep to it: the first screen stays the one where the job is done, and a change goes where its use belongs (what is done as often as the job, on the first screen; looking back, on a later one; setting up, last). Give "ux" again only when the request changes who uses the app or how.`);
    if (brief?.art) parts.push(`## Its art direction\n${ART.map((k) => `- ${k}: ${brief.art[k] || '-'}`).join('\n')}\n\nWhat you add keeps to it (its words, its photos). Give "art" again only when the request asks for another feel.`);
    const tables = [];
    for (const t of view.tables) {
      const name = physical(appId, t.spec.name);
      const count = await store.countRows(name);
      let sample = [];
      try { sample = await engine.db.query(`SELECT * FROM ${name} ORDER BY created DESC LIMIT 3`); } catch { /* none */ }
      tables.push(describeTable(t, count, sample));
    }
    parts.push(`## Tables\n${tables.join('\n') || '(none)'}`);
    const views = view.views.slice().sort((a, b) => (a.spec.order ?? 99) - (b.spec.order ?? 99))
      .map((v) => `<view slug="${v.slug}" title="${v.title}" order="${v.spec.order ?? ''}" id="${v.id}"${v.spec.statement ? ' format="statement"' : v.spec.screen ? ' format="screen"' : ''}>\n${v.spec.statement || (v.spec.screen ? screenLines(v.code) : v.code)}\n</view>`);
    parts.push(`## Views\n${views.some((v) => v.includes('format="statement"')) ? 'A view with format="statement" is a screen written as a CREATE SCREEN statement, its source. To change it, give "edits" in its plan entry ([{"old": "text copied exactly from the statement, occurring once", "new": "..."}]), or a new statement in its code block when most of it changes.\n\n' : ''}${views.some((v) => v.includes('format="screen"')) ? 'A view with format="screen" is a screen, shown one part per line after its path. To change it, give "patch" (operations on those paths) or its whole "screen" in its plan entry; to turn it into a code view, give a code block and no "screen".\n\n' : ''}${SCREENS && view.views.some((v) => !v.spec.screen) ? 'To change a view or function written in JavaScript a little (a color, a label, a line of logic), give "edits" in its plan entry instead of a code block: [{"old": "text copied exactly from its code, occurring once", "new": "its replacement"}]. Give a whole code block only when most of it changes.\n\n' : ''}${views.join('\n\n') || '(none)'}`);
    const fns = view.functions.map((f) => `<function slug="${f.slug}" on='${JSON.stringify(f.spec.on || null)}'${f.spec.every && !f.spec.rule ? ` every="${everyWords(f.spec.every).replace(/^every /, '')}"` : ''} id="${f.id}"${f.spec.statement ? ' format="rule-statement"' : f.spec.rule ? ' format="rule"' : ''}>\n${f.description && !f.spec.rule ? `// ${f.description}\n` : ''}${f.spec.statement || f.code}\n</function>`);
    parts.push(`## Functions\n${view.functions.some((f) => f.spec.statement) ? 'A function with format="rule-statement" is a rule written as a CREATE RULE statement, its source. To change it, give "edits" in its plan entry ([{"old": "text copied exactly from the statement, occurring once", "new": "..."}]), or a new statement in its code block.\n\n' : ''}${view.functions.some((f) => f.spec.rule && !f.spec.statement) ? `A function with format="rule" is a rule (JSON). To change it, ${RULE_SQL ? 'give a CREATE RULE statement in its code block' : 'give its whole "rule" in its plan entry'}; to turn it into JavaScript, give a code block and no "rule".\n\n` : ''}${fns.join('\n\n') || '(none)'}`);
    // Code only for the pieces this app wrote: the others (built-in, or from
    // another app) are used by their props, and their code is long.
    const mine = view.shared.map((c) => {
      const newest = shared.get(`${c.kind}/${c.slug}`);
      const note = newest && newest.version > Number(c.version) ? ` newest="${newest.version}" (the newest adds: ${newest.spec?.change || 'see the library'})` : '';
      const own = c.spec?.by === appId;
      return `<${c.kind} name="${c.slug}" version="${c.version}"${note}${own ? '' : ` by="${c.author === 'builtin' ? 'built in' : 'another app'}"`}>\n// ${c.title}: ${c.description}\n// ${c.kind === 'helper' ? 'api' : 'props'}: ${c.spec?.props || ''}${own ? `\n${c.code}` : ''}\n</${c.kind}>`;
    });
    parts.push(`## Components and helpers this app uses (shared with other apps)\nThe code of the ones this app did not write is left out: use them by their props and api. To change one, write a new version of it from its props and description.\n\n${mine.join('\n\n') || '(none)'}`);
    const live = new Set(view.comp.caps);
    const events = (await store.recentEvents(appId, 300)).filter((e) => e.kind !== 'error' || live.has(e.cap));
    parts.push(`## What the person did lately\n${summarizeEvents(events)}`);
    const errors = events.filter((e) => e.kind === 'error').slice(0, 5);
    if (errors.length) parts.push(`## Errors lately, from the code that runs now\n${errors.map((e) => `- ${e.cap}: ${e.detail}`).join('\n')}`);
    const history = SUGGEST_AFTER ? [] : await store.suggestionHistory(appId, 30);
    if (history.length) parts.push(`## Suggestions made before\n${history.map((h) => `- [${h.status === 'taken' ? 'taken' : h.status === 'dismissed' ? 'dismissed' : h.status === 'open' ? 'still open' : 'replaced'}] ${h.text}`).join('\n')}`);
  }
  // What to look for: the request, and for the shared pieces the app's
  // description too (they serve the whole app). Whole screens, rules and
  // patterns are looked for by the request alone, so that a change to a shop
  // ("let customers vote on the next roast") finds voting, not more shop.
  const want = `${request.text} ${view.app.description || ''}`;
  // Patterns apps added (the built-in ones are in the system prompt): the
  // ones that fit the request best in full, the others by name, so the
  // prompt stays short however many apps add.
  const added = new Map([...PATTERNS].filter(([k]) => !BUILTIN.has(k)));
  if (added.size) {
    // A pattern the request names comes first (the library's "Use in..."
    // names it), then the best fits by words and meaning.
    const named = [...added.keys()].filter((k) => new RegExp(`\\b${k}\\b`, 'i').test(request.text));
    const fit = added.size <= PATTERN_ENTRIES ? [...added.keys()]
      : [...new Set([...named, ...(await store.searchCapabilities(request.text, { kinds: ['pattern'], limit: PATTERN_ENTRIES })).map((c) => c.slug).filter((k) => added.has(k))])].slice(0, Math.max(PATTERN_ENTRIES, named.length));
    parts.push(addedPatternsPart(added, fit));
  }
  // Shared components and helpers: all of them by name and props, and, for a
  // change to an app that exists, the code of the few that fit best (to use a
  // piece the model needs its props; it needs the code only to write a new
  // version of it, which a first version almost never does, and every
  // character here is read on every build). Fit is two rankings fused:
  // BM25 of the request over what pieces are and do, and PageRank over the
  // graph from this app and the best text matches, which finds the pieces
  // that go with them (used by the same views, called by them).
  const own = new Set(view.shared.map((c) => `${c.kind}/${c.slug}`));
  let libIndex = -1;
  const libEntries = new Map();
  const others = [...shared.entries()].filter(([k]) => !own.has(k)).map(([, c]) => c);
  if (others.length) {
    const users = await store.sharedUsers();
    const key = (c) => `${c.kind}/${c.slug}`;
    const hits = await store.searchCapabilities(want, { kinds: SHARED_KINDS, limit: FULL_ENTRIES });
    const near = await store.related([appNode(appId), ...hits.slice(0, 3).map((h) => `${LIB}/${key(h)}`)], { limit: FULL_ENTRIES });
    const fused = new Map();
    const vote = (k, i) => fused.set(k, (fused.get(k) || 0) + 1 / (60 + i));
    hits.forEach((h, i) => vote(key(h), i));
    near.forEach((r, i) => vote(key(r), i));
    const top = [...fused.entries()].sort((a, b) => b[1] - a[1]).slice(0, FULL_ENTRIES);
    const rank = new Map(top.map(([k], i) => [k, i]));
    others.sort((a, b) => (rank.get(key(a)) ?? 99) - (rank.get(key(b)) ?? 99) || (a.kind === b.kind ? 0 : a.kind === 'helper' ? 1 : -1));
    libIndex = parts.length;
    const best = [];
    const more = [];
    for (const [i, c] of others.entries()) {
      const used = (users.get(key(c)) || []).map((u) => `${u.name} (v${u.version})`).join(', ') || 'no app yet';
      libEntries.set(key(c), `<${c.kind} name="${c.slug}" version="${c.version}" title="${c.title}" used_by="${used}">\n// ${c.description}\n// ${c.kind === 'helper' ? 'api' : 'props'}: ${c.spec?.props || ''}\n</${c.kind}>`);
      if (i < PROPS_ENTRIES) best.push(libEntries.get(key(c)));
      else if (i < FULL_ENTRIES) more.push(`${c.kind === 'helper' ? 'lib' : 'ui'}.${c.slug}`);
    }
    // Every piece is named in the system prompt (the same for every call, so
    // it is read from the prompt cache); here, the best fits with their props.
    parts.push(`# Shared components (ui.NAME(props)) and helpers (lib.NAME.fn(...)) that fit this request best\n\nThe best first: by the request's words, and by what goes with the pieces this app and the request's best matches use (other views use them together, or they call each other). The rest of the library is named in "The library" above; to use one of those, name it in "uses" (its writer gets its props).\n\n${best.join('\n\n')}${more.length ? `\n\nAlso fitting: ${more.join(', ')}.` : ''}`);
  } else if (!own.size) {
    parts.push('# Shared components and helpers in the library\n\nNone yet: the ones you write now start it.');
  }
  // The library: whole screens, rules and functions other apps built that
  // fit this request best, by its words and by what it means.
  const lib = await store.searchCapabilities(request.text, { excludeApp: appId, kinds: ['view', 'function'], limit: LIBRARY_ENTRIES });
  const how = new Map(lib.map((c) => [c.id, c.found === 'meaning' ? `found by meaning (cosine ${c.cosine})` : c.found === 'both' ? 'found by its words and its meaning' : 'found by its words']));
  const libCaps = await store.getCapabilities(lib.map((c) => c.id));
  let capIndex = -1;
  const capEntries = new Map();
  if (libCaps.length) {
    const entries = [];
    for (const c of libCaps) {
      const starter = c.app === LIB;
      const src = starter ? null : await store.resolve(c.app).catch(() => null);
      const specs = starter ? (c.spec?.tables || []) : src ? src.tables.map((t) => t.spec) : [];
      const tables = specs.map((t) => `${t.name}(${(t.columns || []).map((x) => `${x.name} ${x.type}`).join(', ')})`).join('; ');
      const about = starter
        ? (c.kind === 'view' ? `// a built-in starter screen; reusing it adds its tables to the app when missing: ${tables || 'none'}` : `// a built-in starter function, for any table; input: ${c.spec?.input || ''}`)
        : `// its app's tables: ${tables}`;
      const format = c.kind === 'view' ? (c.spec?.statement || c.spec?.screen ? 'screen' : 'code') : c.spec?.rule ? 'rule' : 'code';
      const head = `<capability id="${c.id}" kind="${c.kind}" format="${format}" title="${c.title}" app="${starter ? 'built-in starter' : src?.app?.name || c.app}">\n// ${c.description}\n// ${how.get(c.id) || 'found by its words'}\n${about}`;
      // A screen or rule written as a statement is shown as its statement:
      // shorter than its JSON, and what a fork edits.
      const body = clip(c.spec?.statement || c.code, 5000);
      // A starter is reused as it is, so its header says enough to choose it;
      // its code goes to the writer of a version forked from it.
      entries.push(starter ? `${head}\n</capability>` : `${head}\n${body}\n</capability>`);
      capEntries.set(c.id, `${head}\n${body}\n</capability>`);
    }
    capIndex = parts.length;
    parts.push(`# Library: screens, rules and functions other apps have that fit this request best\n\nThe best first, found by the request's words and by what it means. Reuse or fork one when it does what the request asks; leave them otherwise.\n\n${entries.join('\n\n')}`);
  }
  const source = { user: 'the person asked for this', suggestion: 'the person accepted this suggestion', repair: 'the runtime reported this error; fix its cause', library: 'the person asked to bring this in from the library', evolve: 'nobody asked: the engine saw this in how people use the app, and builds it as a version they can try beside the live one and keep or drop. Make it small, in the app\'s own style, and change nothing it does not need' }[request.source] || 'the person asked for this';
  parts.push(`# The request (${source})\n\n${request.text}`);
  // The same context for one piece's writer: of the library's shared pieces,
  // only the ones the plan says it calls.
  // Of the library's whole views and functions, only the one it is forked
  // from, if any.
  const forPiece = (uses = [], forkedFrom = null) => {
    const copy = parts.slice();
    if (libIndex >= 0) {
      const keys = uses.map((u) => String(u).replace(/^ui[:.]/, 'component/').replace(/^lib[:.]/, 'helper/'));
      const picked = keys.map((k) => libEntries.get(k)).filter(Boolean);
      copy[libIndex] = `# Shared components (ui.NAME(props)) and helpers (lib.NAME.fn(...)) from the library that this piece calls\n\n${picked.join('\n\n') || '(none)'}`;
    }
    if (capIndex >= 0) {
      const src = forkedFrom && capEntries.get(forkedFrom);
      copy[capIndex] = src ? `# Library: the capability this piece is forked from\n\n${src}` : '';
    }
    return copy.filter(Boolean).join('\n\n');
  };
  const found = new Map(lib.map((c) => [c.id, c.found]));
  return { prompt: parts.join('\n\n'), forPiece, view, library: libCaps.map((c) => ({ ...c, found: found.get(c.id) })), shared, system: systemWith(shared), request };
}

// The system prompt with the whole library of shared pieces, by name and
// props, in a fixed order: it changes only when a piece is added or gets a
// new version, so calls in a row read it from the prompt cache.
// The library's SQL functions, once the engine has them in Strata
// (engine/sqlfns.mjs): the model reads them after the system prompt.
let sqlFnsDoc = '';
export const useSqlFunctions = (fns) => { sqlFnsDoc = sqlFunctionsDoc(fns); };

// On a cluster, whose coordinator walks no graphs, apps read their links
// a step at a time instead (engine/walk.mjs says why).
let onCluster = false;
export const useCluster = (on) => { onCluster = !!on; };
const CLUSTER_LINKS = "- Links: this Strata is a cluster of shards, which does not walk links: never write REACHABLE, hops() or ppr. Read the rows a row links to with SELECT dst FROM note_links WHERE src = :id (and the other way, WHERE dst = :id), one query per step, then read those rows by id with WHERE id IN (...).";

export function systemWith(shared) {
  const list = [...shared.values()].sort((a, b) => (a.kind === b.kind ? (a.slug < b.slug ? -1 : 1) : a.kind === 'component' ? -1 : 1));
  const system = onCluster ? SYSTEM.replace(/^- Links: WHERE REACHABLE.*$/m, CLUSTER_LINKS) : SYSTEM;
  const withFns = sqlFnsDoc ? `${system}\n\n${sqlFnsDoc}` : system;
  if (!list.length) return withFns;
  const line = (c) => `${c.kind === 'helper' ? 'lib' : 'ui'}.${c.slug} (${c.title})`;
  return `${withFns}\n\n# The library: every shared component and helper, by name\n\n${list.map(line).join(', ')}.`;
}

export { SHARED, sharedRefs };

// The components a piece of code calls, by name.
export function uiRefs(code) {
  return new Set([...sharedRefs(code)].filter((k) => k.startsWith('component/')).map((k) => k.slice('component/'.length)));
}

export function checkShared(kind, code) {
  const problems = [];
  if (!SHARED[kind].define.test(code)) problems.push(`must define ${SHARED[kind].defines}`);
  const data = code.match(/\bapp\s*\.\s*(sql|insert|update|remove|call|onChange|open|track)\b/);
  if (data) problems.push(kind === 'component'
    ? `uses app.${data[1]}: a component gets its data through props and reports what the person does through callbacks`
    : `uses app.${data[1]}: a helper gets plain values and returns plain values`);
  if (kind === 'helper') {
    const page = code.match(/\b(document|window|localStorage)\b|(?<![\w$.])(ui|h|svg|css)\s*[.(]/);
    if (page) problems.push(`uses ${page[1] || page[2]}: helpers also run on the server, where there is no page`);
    const svc = !data && code.match(/(?<![\w$.])app\s*\.\s*(\w+)/);
    if (svc) problems.push(`uses app.${svc[1]}: a helper gets no app; take what it needs as arguments`);
    if (/\bawait\b|\basync\b/.test(code)) problems.push('a helper is synchronous: no async or await');
  }
  if (/\bimport\s*\(|\brequire\s*\(/.test(code)) problems.push('no import or require');
  if (/\bfetch\s*\(/.test(code)) problems.push('no fetch: there is no network');
  if (kind === 'component' && /\blocalStorage\b/.test(code)) problems.push('no localStorage');
  try { new vm.Script(`(function () {\n${code}\n})`, { filename: `${kind}.js` }); } catch (e) { problems.push(`syntax: ${e.message}`); }
  if (code.length > 60 * 1024) problems.push(`the code is longer than 60 KiB: split it`);
  return problems;
}

// Pins the shared pieces an app's code calls, following pieces that call
// others: one written in this version, else the version the app had (unless
// upgraded), else the newest in the library. Keys are kind/name.
export async function pinShared(store, rootIds, { written = new Map(), had = new Map(), upgrade = new Set(), shared } = {}) {
  const lib = shared || await store.shared();
  const codeOf = async (id) => (await store.getCapabilities([id]))[0]?.code || '';
  const want = new Map();
  const queue = [];
  for (const id of rootIds) queue.push(...sharedRefs(await codeOf(id)));
  while (queue.length) {
    const key = queue.shift();
    if (want.has(key)) continue;
    const name = key.split('/')[1];
    const id = written.get(key) || (!upgrade.has(name) && !upgrade.has(key) && had.get(key)) || lib.get(key)?.id;
    if (!id) continue;
    want.set(key, id);
    queue.push(...sharedRefs(await codeOf(id)));
  }
  return want;
}

export function checkView(code) {
  const problems = [];
  if (!/\basync\s+function\s+render\s*\(|\bfunction\s+render\s*\(/.test(code)) problems.push('must define async function render(root, app)');
  if (/\bimport\s*\(|\brequire\s*\(/.test(code)) problems.push('no import or require: the frame has nothing to load');
  if (/\bfetch\s*\(/.test(code)) problems.push('no fetch: the frame has no network; use app.sql and app.call');
  if (/\blocalStorage\b/.test(code)) problems.push('no localStorage: keep state in a table');
  try { new vm.Script(`(async function () {\n${code}\n})`, { filename: 'view.js' }); } catch (e) { problems.push(`syntax: ${e.message}`); }
  if (code.length > 120 * 1024) problems.push('the code is longer than 120 KiB');
  return problems;
}

// Checks a plan against the app as it is. Returns a list of problems.
// The plan's access statements (access.mjs): each parses and names a table
// and columns the app has; and a request that asks for signed-in people
// only, roles or companies declares access for every table of the app.
export function accessProblems(plan, view, requestText = '') {
  const columns = new Map();
  for (const t of [...view.tables.map((x) => x.spec), ...(plan.tables || [])]) {
    const set = columns.get(t.name) || new Set(['id', 'created', ...(t.links ? ['src', 'dst'] : [])]);
    for (const c of t.columns || []) set.add(c.name);
    columns.set(t.name, set);
  }
  if (plan.access !== undefined && !Array.isArray(plan.access)) return ['access: a list of ALLOW statements, one string each'];
  const problems = checkAccess(plan.access || [], columns);
  if (!problems.length && wantsAccess(requestText)) {
    const declared = new Set([...view.tables.filter((x) => x.spec.access?.length).map((x) => x.spec.name), ...parseAccessList(plan.access || []).keys()]);
    const open = [...columns.keys()].filter((t) => !declared.has(t));
    if (open.length) problems.push(`access: the request asks who may reach the data (signed-in people, roles or companies), but ${open.join(', ')} ${open.length === 1 ? 'declares' : 'declare'} no access: give "access" ALLOW statements for ${open.length === 1 ? 'it' : 'each'} (the engine enforces them on the server; hiding data in the screens is not enough). A table anyone may read says so: ALLOW READ ON t TO everyone`);
  }
  return problems;
}

export async function validate(plan, code, view, library, shared = new Map(), requestText = '') {
  const problems = checkBrief(plan, view);
  problems.push(...accessProblems(plan, view, requestText));
  const tables = new Map(view.tables.map((t) => [t.spec.name, t.spec]));
  for (const t of plan.tables || []) {
    if (!NAME_RE.test(t.name || '')) { problems.push(`table name ${JSON.stringify(t.name)}: lowercase letters, digits and _`); continue; }
    if (RESERVED.has(t.name)) problems.push(`table name ${t.name} is a word of SQL; choose another`);
    const have = new Map((tables.get(t.name)?.columns || []).map((c) => [c.name, c.type]));
    for (const c of t.columns || []) {
      if (!NAME_RE.test(c.name || '')) problems.push(`column ${t.name}.${c.name}: lowercase letters, digits and _`);
      else if (c.name === 'id' || c.name === 'created') problems.push(`column ${t.name}.${c.name}: every table has id and created already; do not declare them`);
      else if (RESERVED.has(c.name)) problems.push(`column ${t.name}.${c.name}: ${c.name} is a word of SQL; choose another name (such as ${t.name.replace(/s$/, '')}_${c.name} or sort_${c.name})`);
      if (!TYPES[c.type]) problems.push(`column ${t.name}.${c.name}: type ${JSON.stringify(c.type)} is not one of ${Object.keys(TYPES).join(', ')}`);
      if (have.has(c.name) && have.get(c.name) !== c.type) problems.push(`column ${t.name}.${c.name} is ${have.get(c.name)} already; types never change (add a new column instead)`);
      if (c.type === 'meaning') {
        const of = [].concat(c.of || []);
        const textCols = new Map([...(tables.get(t.name)?.columns || []), ...(t.columns || [])].map((x) => [x.name, x.type]));
        if (!of.length) problems.push(`column ${t.name}.${c.name}: a meaning column says which text columns it means: "of": ["title", "body"]`);
        for (const o of of) if (!['text', 'keyword'].includes(textCols.get(o))) problems.push(`column ${t.name}.${c.name}: "of" names ${o}, which is not a text or keyword column of ${t.name}`);
      }
    }
    if (t.links) {
      const target = tables.has(t.links) || (plan.tables || []).some((x) => x.name === t.links);
      if (!target) problems.push(`table ${t.name}: "links" names ${t.links}, which the app does not have`);
      if (tables.has(t.name) && !tables.get(t.name).links) problems.push(`table ${t.name} exists already as a plain table; a links table must be a new table`);
    }
  }
  const slugs = new Set();
  for (const [kind, list] of [['view', plan.views], ['function', plan.functions]]) {
    for (const item of list || []) {
      if (!SLUG_RE.test(item.slug || '')) { problems.push(`${kind} slug ${JSON.stringify(item.slug)}: lowercase letters, digits and dashes`); continue; }
      if (slugs.has(item.slug)) problems.push(`${item.slug} is given twice`);
      slugs.add(item.slug);
      if (kind === 'view' && item.screen) {
        for (const x of checkScreen(item.screen, columnsFor(view, plan))) problems.push(`view ${item.slug}: ${x}`);
        if (BUDGETS.has(item)) problems.push(...overBudget(item.slug, BUDGETS.get(item), budgetBefore(view, item.slug)));
        continue;
      }
      const src = code[item.slug];
      if (!src || !src.trim()) { problems.push(`${kind} ${item.slug} has no <code slug="${item.slug}"> block`); continue; }
      if (kind === 'view' && /^\s*CREATE\s+SCREEN\b/i.test(src)) continue; // its problems came from compiling it
      if (kind === 'function' && !item.rule && isRuleStatement(src)) continue; // likewise
      if (kind === 'view') for (const x of keysIn(src)) problems.push(`view ${item.slug}: ${x}`);
      const p = kind === 'view' ? checkView(src) : item.rule ? checkRule(item.rule, columnsFor(view, plan)) : await checkFunction(src);
      for (const x of p) problems.push(`${kind} ${item.slug}: ${x}`);
      // "on": "none" (or {"table": "none"}) is no trigger.
      if (kind === 'function' && item.on && (typeof item.on !== 'object' || !item.on.table || /^(none|null|no|-)$/i.test(String(item.on.table)))) item.on = null;
      // "every": a schedule, as words ("day at 07:00") or as parsed already.
      if (kind === 'function' && item.every && typeof item.every === 'string') {
        if (/^(none|null|no|-)$/i.test(item.every.trim())) item.every = null;
        else try { item.every = parseEvery(item.every.replace(/\btz\s+([A-Za-z_]+\/[A-Za-z_\/+-]+)\s*$/i, "TZ '$1'").replace(/\bat\s+(\d{1,2}(?::\d{2})?)\b/i, "AT '$1'")); } catch (e) {
          if (!(e instanceof ScheduleError)) throw e;
          problems.push(`function ${item.slug}: every: ${e.message}`);
          item.every = null;
        }
      }
      if (kind === 'function' && item.every && item.on) problems.push(`function ${item.slug}: it runs on a schedule ("every") or after writes ("on"), not both`);
      if (kind === 'function') for (const x of keysIn(src)) problems.push(`function ${item.slug}: ${x}`);
      if (kind === 'function' && item.on) {
        const on = item.on;
        const known = tables.has(on.table) || (plan.tables || []).some((t) => t.name === on.table);
        if (!known) problems.push(`function ${item.slug}: its trigger names table ${on.table}, which the app does not have`);
        if (![].concat(on.event || 'insert').every((e) => ['insert', 'update', 'delete'].includes(e))) problems.push(`function ${item.slug}: trigger event is insert, update or delete (or a list of them)`);
      }
    }
  }
  if (TESTS) problems.push(...checkTests(plan.tests, columnsFor(view, plan)));
  if (plan.patterns !== undefined) {
    if (!Array.isArray(plan.patterns)) problems.push('"patterns" is a list');
    else if (plan.patterns.length > 1) problems.push('"patterns": add at most one pattern in a build');
    else for (const p of plan.patterns) problems.push(...checkPattern(p, checkScreen));
  }
  const known = new Set([...shared.keys(), ...view.shared.map((c) => `${c.kind}/${c.slug}`)]);
  const writtenCode = [];
  for (const [kind, { ns, plural }] of Object.entries(SHARED)) {
    for (const c of plan[plural] || []) {
      if (!COMPONENT_RE.test(c.name || '')) { problems.push(`${kind} name ${JSON.stringify(c.name)}: camelCase letters and digits, starting with a lowercase letter`); continue; }
      known.add(`${kind}/${c.name}`);
      const src = code[`${ns}:${c.name}`];
      if (!src || !src.trim()) { problems.push(`${kind} ${c.name} has no <code slug="${ns}:${c.name}"> block`); continue; }
      for (const x of checkShared(kind, src)) problems.push(`${kind} ${c.name}: ${x}`);
      writtenCode.push([`${kind} ${c.name}`, src, kind]);
    }
  }
  for (const v of plan.views || []) writtenCode.push([`view ${v.slug}`, code[v.slug], 'view']);
  for (const f of plan.functions || []) writtenCode.push([`function ${f.slug}`, code[f.slug], 'function']);
  for (const [what, src, kind] of writtenCode) {
    for (const key of sharedRefs(src)) {
      const [k, name] = key.split('/');
      if ((kind === 'function' || kind === 'helper') && k === 'component') { problems.push(`${what} calls ui.${name}: components are for views, ${kind}s run where there is no page`); continue; }
      if (!known.has(key)) problems.push(`${what} calls ${SHARED[k].ns}.${name}, which is not a shared ${k}; write it (in "${SHARED[k].plural}") or use one that exists`);
    }
  }
  for (const name of plan.upgrade || []) if (!view.shared.some((c) => c.slug === name)) problems.push(`upgrade ${name}: the app does not use a shared piece of that name`);
  const libIds = new Set(library.map((c) => c.id));
  for (const r of plan.reuse || []) {
    if (!libIds.has(r.id)) problems.push(`reuse ${r.id}: not in the library given`);
  }
  const existing = new Set([...view.views, ...view.functions].map((c) => c.slug));
  for (const s of plan.remove || []) if (!existing.has(s)) problems.push(`remove ${s}: the app has no view or function ${s}`);
  const viewsAfter = new Set([...view.views.map((v) => v.slug), ...(plan.views || []).map((v) => v.slug), ...(plan.reuse || []).filter((r) => library.find((c) => c.id === r.id)?.kind === 'view').map((r) => r.slug)]);
  for (const s of plan.remove || []) viewsAfter.delete(s);
  if (!viewsAfter.size) problems.push('the app would have no view');
  if (!view.comp.n && !(plan.tables || []).length) problems.push('a new app needs at least one table');
  return problems;
}

// A reused starter view brings its tables: the ones the app and the plan
// lack are added to the plan, and so are missing columns of ones they have.
// The UX brief: who uses the app, for what job, how often, on what, which
// screen opens first and what it shows at a glance. A new app decides it
// with its name, before its tables and screens; a change may restate it.
export const OFTEN = ['several times a day', 'daily', 'several times a week', 'weekly', 'monthly', 'now and then'];
export const DEVICES = ['phone', 'desktop', 'both'];
// A brief in other words is taken in the nearest of these, not refused:
// "a few times a week" is several times a week, "mobile" a phone.
const OFTEN_AS = [[/(several|many|a few) times a day|hourly|all day/i, 'several times a day'], [/(several|a few|\d) times a week|most days|every other day/i, 'several times a week'],
  [/daily|every ?day|each day|a day/i, 'daily'], [/week/i, 'weekly'], [/month/i, 'monthly'], [/./, 'now and then']];
const deviceAs = (w) => {
  const phone = /phone|mobile|ios|android/i.test(w);
  const desk = /desk|computer|laptop|web|browser|tablet/i.test(w);
  return /\b(both|either|any|all)\b/i.test(w) || (phone && desk) ? 'both' : phone ? 'phone' : desk ? 'desktop' : null;
};
export function normalizeBrief(plan) {
  const ux = plan?.ux;
  if (!ux || typeof ux !== 'object' || Array.isArray(ux)) return;
  if (typeof ux.often === 'string' && ux.often.trim() && !OFTEN.includes(ux.often)) ux.often = OFTEN_AS.find(([re]) => re.test(ux.often))[1];
  if (typeof ux.device === 'string' && ux.device.trim() && !DEVICES.includes(ux.device)) ux.device = deviceAs(ux.device) || ux.device;
}
const BRIEF = ['who', 'job', 'often', 'device', 'first', 'glance'];
export { briefOf };

// The art direction (plan.art), kept with the brief: what the app should feel
// like, what its photos show, its accent and the type of its titles. Doable
// writes one before any interface; the designs and the screens follow it.
const ART = ['concept', 'mood', 'layout', 'type', 'accent', 'imagery', 'action'];
const TYPES_OF_TITLES = ['system', 'serif', 'rounded', 'mono'];
export function cleanArt(a) {
  if (!a || typeof a !== 'object' || Array.isArray(a) || typeof a.concept !== 'string' || !a.concept.trim()) return null;
  const out = Object.fromEntries(ART.map((k) => [k, typeof a[k] === 'string' ? a[k].trim().replace(/\s+/g, ' ').slice(0, 240) : '']));
  const t = out.type.toLowerCase();
  out.type = TYPES_OF_TITLES.find((x) => t.includes(x)) || (/editorial|elegant|classic/.test(t) ? 'serif' : /friendly|playful|soft/.test(t) ? 'rounded' : /code|technical|terminal/.test(t) ? 'mono' : 'system');
  if (!out.imagery) out.imagery = 'none';
  // The first screen's structure, as the kit names it (quality.mjs LAYOUTS).
  out.structure = parseLayout(out.layout) || '';
  return out;
}
// The brief to keep for a version: the plan's (or the app's, when the plan
// restates only the art), with the art the plan gives or the one it had.
// The app's name from the plan's, keeping what the owner asked for: a first
// line "Name: Pawsome Grooming" (or "Title:") is the name as it is, and a
// leading label like "acme-shop" or "team_42" stays in front of it.
export function appTitle(planned, app) {
  const name = String(planned || '').trim().slice(0, 32);
  const first = String(app?.description || '').trim().split('\n')[0];
  const given = first.match(/^(?:app\s+)?(?:name|title|called)\s*[:=]\s*["“']?([^"”'.\n]{1,60})/i);
  if (given) return given[1].trim();
  const label = first.match(/^([A-Za-z0-9]+(?:[-_][A-Za-z0-9]+)+)(?=[\s:,.]|$)/);
  if (!label || name.toLowerCase().includes(label[1].toLowerCase())) return name;
  return `${label[1]} ${name}`.slice(0, 60).trim();
}

export function briefToKeep(plan, app) {
  const had = briefOf(app);
  const art = cleanArt(plan?.art) || had?.art || null;
  const ux = plan?.ux && typeof plan.ux === 'object' && !Array.isArray(plan.ux) ? plan.ux : (cleanArt(plan?.art) ? had : null);
  if (!ux) return null;
  return { ...Object.fromEntries(BRIEF.map((k) => [k, String(ux[k] ?? '').slice(0, 300)])), ...(art ? { art } : {}) };
}

export function checkBrief(plan, view) {
  const problems = [];
  const fresh = !view.comp.n;
  if (fresh || plan.name !== undefined) {
    const n = typeof plan.name === 'string' ? plan.name.trim() : '';
    if (!n) problems.push('"name": a new app needs a short name, 2 to 4 words ("Habits", "Plant care")');
    else if (n.length > 32 || n.split(/\s+/).length > 5) problems.push(`"name" ${JSON.stringify(n)} is long: 2 to 4 words, at most 32 characters`);
  }
  const ux = plan.ux;
  if (ux === undefined && !fresh) return problems;
  if (!ux || typeof ux !== 'object' || Array.isArray(ux)) return [...problems, `"ux": a new app gives its brief, {${BRIEF.map((k) => `"${k}"`).join(', ')}}, before its tables and screens`];
  for (const k of ['who', 'job', 'glance']) if (typeof ux[k] !== 'string' || !ux[k].trim()) problems.push(`ux.${k}: ${{ who: 'who uses the app, and where', job: 'the job they come back for, in their words', glance: 'what they see on opening, without a tap' }[k]}`);
  if (!OFTEN.includes(ux.often)) problems.push(`ux.often ${JSON.stringify(ux.often)}: one of ${OFTEN.map((x) => `"${x}"`).join(', ')}`);
  if (!DEVICES.includes(ux.device)) problems.push(`ux.device ${JSON.stringify(ux.device)}: one of ${DEVICES.map((x) => `"${x}"`).join(', ')}`);
  const gone = new Set(plan.remove || []);
  const slugs = [...new Set([...(plan.views || []), ...(plan.reuse || []), ...view.views].map((v) => v.slug).filter((x) => x && !gone.has(x)))];
  if (!slugs.includes(ux.first)) problems.push(`ux.first ${JSON.stringify(ux.first)}: the slug of the screen that opens first, where the job is done (one of ${slugs.join(', ') || 'the views'})`);
  return problems;
}

// The screen the brief names opens first: it gets the lowest order.
export function orderByBrief(plan, view) {
  const first = plan?.ux?.first;
  if (!first) return;
  const entries = [...(plan.views || []), ...(plan.reuse || [])];
  const mine = entries.find((v) => v.slug === first);
  if (!mine) return;
  const others = [...entries.filter((v) => v !== mine), ...view.views.filter((v) => !entries.some((e) => e.slug === v.slug)).map((v) => ({ order: v.spec?.order }))]
    .map((v) => Number(v.order)).filter(Number.isFinite);
  const low = Math.min(...others);
  if (others.length && !(Number(mine.order) < low)) mine.order = low - 1;
}

export function completePlan(plan, view, library) {
  if (plan) { normalizeBrief(plan); orderByBrief(plan, view); }
  for (const v of plan?.views || []) if (v && typeof v.title === 'string') v.title = tidyTitle(v.title);
  if (!plan || !Array.isArray(plan.reuse)) return plan;
  const have = new Map(view.tables.map((t) => [t.spec.name, new Set((t.spec.columns || []).map((c) => c.name))]));
  plan.tables = Array.isArray(plan.tables) ? plan.tables : [];
  for (const r of plan.reuse) {
    const src = library.find((c) => c.id === r.id);
    if (!src || src.app !== LIB) continue;
    for (const t of src.spec?.tables || []) {
      let entry = plan.tables.find((x) => x.name === t.name);
      const known = new Set([...(have.get(t.name) || []), ...((entry?.columns || []).map((c) => c.name))]);
      const missing = (t.columns || []).filter((c) => !known.has(c.name));
      if (!missing.length) continue;
      if (!entry) { entry = { name: t.name, description: `from the starter ${src.title}`, columns: [] }; plan.tables.push(entry); }
      entry.columns = [...(entry.columns || []), ...missing];
    }
  }
  return plan;
}

// Reports how far the model has got, at most every 5 s, on one line that
// each report replaces.
function writing(progress, what = 'The model') {
  const t0 = Date.now();
  let last = 0;
  return ({ chars, thinking }) => {
    const now = Date.now();
    if (now - last < 5000) return;
    last = now;
    const secs = Math.round((now - t0) / 1000);
    progress(chars ? `${what} is writing: ${chars.toLocaleString('en-US')} characters so far (${secs} s)` : `${what} is thinking (${secs} s${thinking ? `, ${thinking.toLocaleString('en-US')} characters` : ''})`, { replace: true });
  };
}

// A build in two steps, for apps with several pieces: the model plans the
// version (tables, and every view, function, component and helper with a
// spec), and then writes each piece in its own call, all at once. A big app
// is written in the time of its biggest piece instead of all of them, and a
// piece that fails the checks is written again alone.
export const SPLIT = process.env.STRATA_APPS_SPLIT || 'first';

const PLAN_STEP = `# This step: the plan alone

Answer with the <plan> alone this time, and no code blocks${STATEMENTS || RULE_SQL ? ` (but for the ${[STATEMENTS ? 'screens\' CREATE SCREEN' : '', RULE_SQL ? 'rules\' CREATE RULE' : ''].filter(Boolean).join(' and ')} statements)` : ''}. Each view, function, component and helper in it is then written by its own writer, all at once, and each writer sees this plan and the request but nothing the others write. So the plan is the contract between them:
- For a new app, decide "name" and "ux" first: the tables and screens follow from them.
- Give every view, function, component and helper a "spec": what it shows or computes, in enough detail to be written alone. For a view: the tables and columns it reads and writes, the functions it calls with their input and output, and the components and helpers it calls with the props or arguments it passes. For a function: its input and what it returns. For content (lessons, explanations, questions, texts), an outline of it; the writer writes it out in full.
- Give each one "uses": the shared components and helpers its code calls, as "ui:name" and "lib:name", new ones and ones from the library.
- Give components "props" and helpers "api" exactly as their code will take them, with every argument and what each function returns.
- Keep the scope to what the request asks: the fewest pieces, each the smallest that does its job well. Content gets an outline of what it covers, sized to the request (a short course is 4 to 6 lessons, a quiz 5 to 8 questions, unless the person asked for more).
- Keep "summary"${SUGGEST_AFTER ? ' and "seed"' : ', "seed" and "suggestions"'} as always.${STATEMENTS ? '\n- A screen view has no writer: after the plan, give its CREATE SCREEN statement now, complete, in a <code slug="..."> block of its slug (the only code blocks of this answer). Only code views, functions, components and helpers are written after the plan.' : SCREENS ? '\n- A screen view has no writer: give its whole "screen" in its plan entry now, complete. Only code views, functions, components and helpers are written after the plan.' : ''}${RULE_SQL ? '\n- A function that is a rule has no writer either: after the plan, give its CREATE RULE statement now, complete, in a <code slug="..."> block of its slug. Only functions written in JavaScript get a writer.' : RULES ? '\n- A function that is a rule has no writer either: give its whole "rule" in its plan entry now, complete. Only functions written in JavaScript get a writer.' : ''}`;

// First versions written in parallel: the plan names every screen and rule
// with a spec and no statement, and each is then written by its own writer
// at once, with the example rows and tests by one more, so a first version
// takes the time of its plan and its biggest piece rather than of all of it
// in one answer. Measured on 12 first versions (2026-11-08) it was not
// faster (36.6 s each against 33.7 s, the plan alone taking 14 s) and cost
// more, so it is off unless STRATA_APPS_PARALLEL=on.
export const PARALLEL = STATEMENTS && process.env.STRATA_APPS_PARALLEL === 'on';

const PLAN_STEP_PARALLEL = `# This step: the plan alone

Answer with the <plan> alone this time, and no code blocks. Each view, function, component and helper in it is then written by its own writer, all at once, and the example rows and tests by one more; each writer sees this plan and the request but nothing the others write. So the plan is the contract between them, and it is short: no statements and no code in it.
- Give every view "format": "screen" (a CREATE SCREEN statement; the default) or "code" (JavaScript, only for what a screen cannot say), and every function "format": "rule" (a CREATE RULE statement; the default for reading and writing rows) or "javascript" (the model, outside services, or logic a rule cannot say; email is a rule's SEND EMAIL).
- Give every view, function, component and helper a "spec", in a few lines, enough to be written alone. For a screen: its parts in order (LIST, BOARD, DETAIL, SUMMARY, WEEK, CALENDAR, FLASHCARDS, QUIZ, FIND, DASHBOARD, HEADING, or custom BLOCKS), the table and columns of each, the form's fields with their types and choices, filters, KPIs, row actions, the views it opens and the functions it calls with their input. For a code view, the same and the components and helpers it calls with their props or arguments. For a rule or function: its input, the rows it reads and writes, what it returns, and its trigger ("on"). For content (lessons, explanations, questions, texts), an outline; the writer writes it out in full.
- For a new app, decide "name" and "ux" first: the tables and screens follow from them.
- Use the same names everywhere: view and function slugs, table and column names, the values a status column takes.
- Give each one "uses": the shared components and helpers its code calls, as "ui:name" and "lib:name", new ones and ones from the library.
- Give components "props" and helpers "api" exactly as their code will take them, with every argument and what each function returns.
- Keep the scope to what the request asks: the fewest pieces, each the smallest that does its job well.
- Keep "summary"${SUGGEST_AFTER ? '' : ' and "suggestions"'} as always. Leave out "seed" and "tests": their writer adds them.`;

const screenStep = (slug) => `# This step: write the screen ${slug}

Write the screen ${slug} alone, as one CREATE SCREEN statement, as the plan above and its spec say: what the spec lists and nothing beyond it. Other writers write the other pieces at the same time, from the same plan: use the tables and columns as the plan gives them, open views by their slugs and call functions by their slugs with the input the plan gives. Answer with one <code slug="${slug}">CREATE SCREEN ...</code> block, and nothing else.`;

const ruleStep = (slug) => `# This step: write the function ${slug}

Write the function ${slug} alone, as one CREATE RULE statement (with its trigger, AFTER ... ON, when the plan gives one), as the plan above and its spec say. Other writers write the other pieces at the same time, from the same plan: use the tables and columns as the plan gives them. If what the spec asks cannot be said as a rule (the model, outside services; email can: SEND EMAIL), write it in JavaScript instead. Answer with one <code slug="${slug}">...</code> block, and nothing else.`;

const EXAMPLES_STEP = `# This step: example rows and tests

Write the example rows ("seed") and the tests ("tests") for the plan above, as described in the system prompt: rows that show the app working (3 to 8 per table, realistic, dated near today), left out when the app holds the person's own data from the start; and tests of its rules and functions. Other writers write the screens and functions at the same time, from the same plan: use the tables, columns, function slugs and inputs as the plan gives them. Answer with one <code slug="examples">{"seed": {...}, "tests": [...]}</code> block holding that JSON, and nothing else.`;

const pieceStep = (what, slug) => `# This step: write ${what}

Write the code of ${what} alone, as the plan above and its spec say: what the spec lists and nothing beyond it, in compact code. Other writers write the other pieces at the same time, from the same plan: call them exactly as the plan gives them (components by their props, helpers by their api, functions by their input and output, tables by their columns). Answer with one <code slug="${slug}">...</code> block holding the whole code, and nothing else.`;

// The pieces a plan says to write, each with the slug of its code block.
function planPieces(plan, parallel = false) {
  const out = [];
  for (const v of plan.views || []) {
    if (v.screen) continue;
    if (parallel && v.format !== 'code') out.push({ what: `the screen ${v.slug}`, label: `view ${v.slug}`, slug: v.slug, item: v, step: screenStep(v.slug), model: SCREEN_MODEL, statement: true });
    else out.push({ what: `the view ${v.slug}`, label: `view ${v.slug}`, slug: v.slug, item: v });
  }
  for (const f of plan.functions || []) {
    if (f.rule) continue;
    if (parallel && f.format !== 'javascript') out.push({ what: `the function ${f.slug}`, label: `function ${f.slug}`, slug: f.slug, item: f, step: ruleStep(f.slug), model: SCREEN_MODEL, statement: true });
    else out.push({ what: `the function ${f.slug}`, label: `function ${f.slug}`, slug: f.slug, item: f });
  }
  if (parallel && !plan.seed && !plan.tests) out.push({ what: 'the example rows and tests', label: 'examples', slug: 'examples', item: {}, step: EXAMPLES_STEP, model: SCREEN_MODEL, owns: (p) => /^tests\[|^seed|^examples/.test(p) });
  for (const [kind, { ns, plural }] of Object.entries(SHARED)) {
    for (const c of plan[plural] || []) out.push({ what: `the shared ${kind} ${c.name}`, label: `${kind} ${c.name}`, slug: `${ns}:${c.name}`, item: c });
  }
  return out;
}

// Runs fn over items, at most n at a time.
async function pool(items, n, fn) {
  const out = new Array(items.length);
  let next = 0;
  await Promise.all(Array.from({ length: Math.min(n, items.length) }, async () => {
    while (next < items.length) { const i = next++; out[i] = await fn(items[i], i); }
  }));
  return out;
}

async function buildSplit({ prompt, forPiece, view, library, shared, system = SYSTEM, request = null }, progress, tr = new Trace()) {
  let cost = 0;
  let plan;
  let problems = [];
  let decidedAt = 0;
  // A first version is planned short and written in parallel (screens and
  // rules too); otherwise the plan carries the screens' and rules' statements.
  const parallel = PARALLEL && !view.comp.n;
  const STEP = parallel ? PLAN_STEP_PARALLEL : PLAN_STEP;
  let planPrompt = `${prompt}\n\n${STEP}`;
  for (let attempt = 1; attempt <= 2; attempt++) {
    progress(attempt === 1 ? `Asking the model (${SCREEN_MODEL}) to plan the first version` : `Asking the model again to fix the plan: ${problems.slice(0, 3).join('; ')}`, { step: attempt === 1 ? 'plan' : 'fix' });
    const span = tr.start('plan', attempt === 1 ? 'The model plans the first version' : 'The model plans again, to fix the plan');
    const answer = await ask({ system, prompt: planPrompt, model: SCREEN_MODEL, onWrite: tee(writing(progress, 'The model'), previewer(view, progress)) });
    tr.call(span, answer, system.length + planPrompt.length);
    cost += answer.cost || 0;
    await tr.span('check', 'Check the plan', async (c) => {
      try {
        const parsed = parseAnswer(answer.text);
        plan = parsed.plan;
        // The plan's CREATE SCREEN statements come with it; other code waits for the writers.
        const statements = Object.fromEntries(Object.entries(parsed.code || {}).filter(([, c]) => /^\s*CREATE\s+SCREEN\b/i.test(c) || isRuleStatement(c)));
        const preparing = prepareScreens(plan, statements, view);
        completePlan(plan, view, library);
        problems = [...preparing, ...(await validate(plan, {}, view, library, shared, request?.text)).filter((p) => !/has no <code slug=/.test(p))];
      } catch (e) { problems = [e.message]; }
      c.problems = problems.slice(0, 8);
    });
    if (problems.length && problems.every(isBudget) && attempt === 2) {
      progress(`Kept over a screen's budget: ${problems.map((p) => p.replace(/^over budget: /, '').split(':')[0]).join('; ')}`);
      problems = [];
    }
    if (!problems.length) { decidedAt = span.end; progress(`The model planned it in ${Math.round(answer.ms / 1000)} s`); break; }
    planPrompt = `${prompt}\n\n${STEP}\n\n# Your previous plan\n\n${answer.text}\n\n# Its problems\n\n${problems.map((p) => `- ${p}`).join('\n')}\n\nAnswer again with the whole plan, these fixed.`;
  }
  if (problems.length) return { plan, code: {}, cost, problems, decidedAt };
  const pieces = planPieces(plan, parallel);
  const code = rulesToCode(plan, screensToCode(plan, {}));
  const planText = `# The plan, decided\n\n<plan>\n${JSON.stringify(plan, null, 1)}\n</plan>`;
  // Each screen's writer has its own watcher, so a screen is drawn as soon as
  // its statement is complete, even when another writer reported just before.
  const previews = new Map();
  const preview = (slug, w, text, force) => { if (!previews.has(slug)) previews.set(slug, previewer(view, progress)); previews.get(slug)(w, text, force); };
  // The example rows and tests come as JSON from their writer.
  let examples = '';
  const takeExamples = () => {
    if (typeof code.examples !== 'string') return;
    examples = code.examples;
    delete code.examples;
    let out = null;
    try { out = JSON.parse(examples.trim().replace(/^```(?:json)?\s*/, '').replace(/```\s*$/, '')); } catch { /* none */ }
    if (out && typeof out === 'object') {
      if (out.seed && typeof out.seed === 'object') plan.seed = out.seed; else delete plan.seed;
      if (Array.isArray(out.tests)) plan.tests = out.tests; else delete plan.tests;
    }
  };
  // What a piece was written as, for its writer to fix: a screen's or a
  // rule's statement once compiled.
  const sourceOf = (pc) => {
    if (pc.slug === 'examples') return examples;
    const item = [...(plan.views || []), ...(plan.functions || [])].find((x) => x.slug === pc.slug);
    return (item && typeof item.statement === 'string' ? item.statement : null) ?? code[pc.slug] ?? '';
  };
  // Each piece's state for the studio: writing (with its characters), done, again.
  const board = new Map();
  const shown = () => [...board.values()];
  const write = async (pieces, notes = new Map()) => {
    const seen = new Map();
    const t0 = Date.now();
    let last = 0;
    const done = [];
    // A plan whose pieces are all screens written as JSON has none to write.
    if (!pieces.length) return;
    for (const pc of pieces) board.set(pc.slug, { label: pc.label.replace(/^(view|function) /, ''), kind: pc.label.split(' ')[0], state: notes.has(pc.slug) ? 'again' : 'writing', chars: 0 });
    // Writing again what the checks found is a step of its own (fix), so the
    // estimate counts it rather than calling the build late.
    progress(`Writing ${pieces.length === 1 ? pieces[0].label : `${pieces.length} pieces at once: ${pieces.map((p) => p.label).join(', ')}`}`, { step: notes.size ? 'fix' : 'write', pieces: shown() });
    await pool(pieces, 8, async (pc) => {
      const fix = notes.get(pc.slug);
      const span = tr.start(fix ? 'fix' : 'write', fix ? `${pc.label}, again` : pc.label, { piece: pc.slug, ...(fix ? { problems: fix.problems.slice(0, 4) } : {}) });
      const extra = fix ? `\n\n# Your previous code for it\n\n<code slug="${pc.slug}">\n${fix.code}\n</code>\n\n# Its problems\n\n${fix.problems.map((p) => `- ${p}`).join('\n')}\n\nWrite it again, whole, with these fixed.` : '';
      const piecePrompt = `${forPiece(pc.item.uses, pc.item.forked_from)}\n\n${planText}\n\n${pc.step || pieceStep(pc.what, pc.slug)}${extra}`;
      const answer = await ask({
        system,
        prompt: piecePrompt,
        ...(pc.model ? { model: pc.model } : {}),
        onWrite: (w, text) => {
          const { chars } = w;
          if (pc.statement && text) preview(pc.slug, w, text);
          seen.set(pc.slug, chars);
          if (board.has(pc.slug)) board.get(pc.slug).chars = chars;
          const now = Date.now();
          if (now - last < 1000) return;
          last = now;
          const total = [...seen.values()].reduce((a, b) => a + b, 0);
          progress(`The model is writing ${pieces.length - done.length} of ${pieces.length}: ${total.toLocaleString('en-US')} characters so far (${Math.round((now - t0) / 1000)} s)${done.length ? `; done: ${done.join(', ')}` : ''}`, { replace: true, pieces: shown() });
        },
      });
      tr.call(span, answer, system.length + piecePrompt.length);
      cost += answer.cost || 0;
      if (pc.statement && answer.text && !fix) preview(pc.slug, {}, answer.text, true);
      const blocks = parseAnswer(`<plan>{}</plan>\n${answer.text}`).code;
      code[pc.slug] = blocks[pc.slug] ?? Object.values(blocks)[0] ?? '';
      // A screen or rule rewritten by its writer is compiled again from it.
      if (pc.statement) { delete pc.item.screen; delete pc.item.rule; delete pc.item.statement; }
      done.push(pc.label);
      if (board.has(pc.slug)) Object.assign(board.get(pc.slug), { state: 'done', secs: Math.round(answer.ms / 1000) });
      progress(`Wrote ${pc.label} in ${Math.round(answer.ms / 1000)} s`, { pieces: shown() });
    });
  };
  await write(pieces);
  takeExamples();
  problems = await tr.span('check', 'Check the code', async (c) => { const p = [...prepareScreens(plan, code, view), ...await validate(plan, code, view, library, shared, request?.text)]; c.problems = p.slice(0, 8); return p; });
  if (problems.length) {
    const notes = new Map();
    for (const pc of pieces) {
      const mine = problems.filter((p) => (pc.owns ? pc.owns(p) : p.startsWith(`${pc.label}:`) || p.startsWith(`${pc.label} `)));
      if (mine.length) notes.set(pc.slug, { code: sourceOf(pc), problems: mine });
    }
    const again = pieces.filter((pc) => notes.has(pc.slug));
    progress(`Checking the code: ${problems.length} problem${problems.length === 1 ? '' : 's'} (${problems.slice(0, 3).join('; ').slice(0, 400)})${again.length ? `; writing ${again.map((p) => p.label).join(', ')} again` : ''}`);
    if (again.length) {
      await write(again, notes);
      takeExamples();
      problems = await tr.span('check', 'Check the code again', async (c) => { const p = [...prepareScreens(plan, code, view), ...await validate(plan, code, view, library, shared, request?.text)]; c.problems = p.slice(0, 8); return p; });
    }
  }
  // A screen's budget was asked of the plan already; over it, it is kept.
  if (problems.some(isBudget)) {
    progress(`Kept over a screen's budget: ${problems.filter(isBudget).map((p) => p.replace(/^over budget: /, '').split(':')[0]).join('; ')}`);
    problems = problems.filter((p) => !isBudget(p));
  }
  // Tests that are still wrong are left out rather than holding up the app.
  if (parallel && problems.length && problems.every((p) => /^tests\[/.test(p))) {
    progress(`Left out the tests: ${problems.slice(0, 2).join('; ').slice(0, 300)}`);
    plan.tests = [];
    problems = [];
  }
  return { plan, code, cost, problems, decidedAt };
}

// Runs a request through the model and applies its answer as the next version.
// Where found photos go: the app's files (when the engine has them), the
// shared cache beside them, and the art direction's accent for a drawing.
function photoOptions(engine, appId, plan, view) {
  const files = engine.services?.filesDir;
  if (files) photoCache(path.join(files, '..', 'photo-cache'));
  const save = engine.services?.saveFile ? async (bytes, meta) => (await engine.services.saveFile(appId, bytes, meta)).id : null;
  return { save, accent: plan?.art?.accent || briefOf(view?.app)?.art?.accent || '' };
}

export async function forge(engine, appId, request, progress = () => {}) {
  const tr = new Trace();
  try {
    return await forgeTraced(engine, appId, request, progress, tr);
  } catch (e) {
    e.trace = tr.toJSON();
    throw e;
  }
}

// Small changes (a label, a colour, a sort order, renaming, hiding or moving
// something) go to a small model first; when its answer fails the checks, the
// usual model answers instead, with the small one's answer and its problems.
// STRATA_APPS_SMALL_MODEL=off sends every change to the usual model.
export const SMALL_MODEL = process.env.STRATA_APPS_SMALL_MODEL === 'off' ? null : (process.env.STRATA_APPS_SMALL_MODEL || 'claude-haiku-5-5');
const SMALL_ASK = /\b(rename|call (it|them|the)|change (the |its |their )?(colou?rs?|titles?|labels?|names?|wording|text|icons?|order|defaults?|headings?)|sort|order (it |them )?by|reorder|move|hide|remove the|bigger|smaller|bolder|bold|darker|lighter|default to|instead of|red|green|blue|orange|grey|gray|colou?r)\b/i;
const NOT_SMALL = /\b(add|new|track|let me|import|export|e-?mail|notify|remind|chart|graph|calculat|search|upload|photo|map|login|sign|share|invite|pay|schedule|every (day|week|month))\w*/i;
export const smallChange = (text, view) => !!SMALL_MODEL && !!view?.comp?.n && String(text).length <= 160 && SMALL_ASK.test(text) && !NOT_SMALL.test(text);

async function forgeTraced(engine, appId, request, progress, tr) {
  const { store } = engine;
  const started = Date.now();
  progress('Reading the app and its history from ChloeDB', { step: 'read' });
  const context = await tr.span('strata', 'Read the app, its data and the library from ChloeDB', () => buildContext(engine, appId, request));
  const { prompt, view, library, shared, system } = context;
  if (library.length) progress(`Found ${library.length} capabilit${library.length === 1 ? 'y' : 'ies'} in the library that may fit: ${library.map((c) => `${c.title} (${c.found === 'meaning' ? 'by meaning' : c.found === 'both' ? 'by words and meaning' : 'by words'})`).join(', ')}`);
  let answer;
  let plan;
  let code;
  let cost = 0;
  let problems = [];
  let attemptPrompt = prompt;
  let decidedAt = 0;
  const split = SPLIT === 'always' || (SPLIT === 'first' && !view.comp.n);
  if (split) ({ plan, code, cost, problems, decidedAt } = await buildSplit(context, progress, tr));
  const small = !split && smallChange(request.text, view);
  for (let attempt = 1; attempt <= (small ? 3 : 2) && !split; attempt++) {
    const model = small && attempt === 1 ? SMALL_MODEL : SCREEN_MODEL;
    progress(attempt === 1 ? `Asking the model (${model}${small ? ', for a small change' : ''}) to ${view.comp.n ? 'change the app' : 'build the first version'}` : `Asking the model${small && attempt === 2 ? ` (${SCREEN_MODEL})` : ''} again to fix: ${problems.slice(0, 3).join('; ')}`, { step: attempt === 1 ? 'plan' : 'fix' });
    const span = tr.start('model', attempt === 1 ? (view.comp.n ? 'The model plans and writes the change' : 'The model plans and writes the first version') : 'The model answers again, to fix problems', { model });
    answer = await ask({ system, prompt: attemptPrompt, model, onWrite: tee(writing(progress), previewer(view, progress)) });
    tr.call(span, answer, system.length + attemptPrompt.length);
    cost += answer.cost || 0;
    progress(`The model answered in ${Math.round(answer.ms / 1000)} s`);
    await tr.span('check', 'Check the code', async (c) => {
      try {
        ({ plan, code } = parseAnswer(answer.text));
        const preparing = prepareScreens(plan, code, view);
        completePlan(plan, view, library);
        problems = [...preparing, ...await validate(plan, code, view, library, shared, request?.text)];
      } catch (e) {
        problems = [e.message];
      }
      c.problems = problems.slice(0, 8);
    });
    if (problems.length && problems.every(isBudget) && attempt === (small ? 3 : 2)) {
      progress(`Kept over a screen's budget: ${problems.map((p) => p.replace(/^over budget: /, '').split(':')[0]).join('; ')}`);
      problems = [];
    }
    if (!problems.length) { decidedAt = span.end; break; }
    progress(`Checking the code: ${problems.length} problem${problems.length === 1 ? '' : 's'}`);
    attemptPrompt = `${prompt}\n\n# Your previous answer\n\n${answer.text}\n\n# Its problems\n\n${problems.map((p) => `- ${p}`).join('\n')}\n\nAnswer again, in full, with these fixed.`;
  }
  if (problems.length) {
    const err = new Error(`the model's answer did not pass the checks: ${problems.join('; ')}`);
    err.cost = cost;
    throw err;
  }
  progress('The code passed the checks; storing it in ChloeDB', { step: 'store' });
  // The photos the screens and example rows name are searched for while the
  // tables are made and the queries run (engine/photos.mjs).
  const photoOpts = photoOptions(engine, appId, plan, view);
  warmPhotos(plan).catch(() => {});
  // A new app's three designs are made from its art direction, beside the
  // storing (the server's hook; the drawn-page check waits for them).
  const art = !view.comp.n && !request.candidate ? cleanArt(plan.art) : null;
  const designing = art && typeof engine.onDirection === 'function'
    ? Promise.resolve(engine.onDirection(appId, { art, views: (plan.views || []).map((v) => ({ title: v.title })), tables: (plan.tables || []).map((t) => t.name) })).catch(() => null)
    : null;
  if (art) progress(`Art direction: ${art.concept}${art.structure ? `; ${art.structure} layout` : ''}${art.accent ? `; accent ${art.accent}` : ''}${art.type !== 'system' ? `; ${art.type} titles` : ''}${art.action ? `; main action "${art.action}"` : ''}`);
  let storing = tr.start('strata', 'Store the code and the new version in ChloeDB');

  // Tables first: data the new code reads must exist when it goes live.
  const caps = new Map(view.comp.caps.map((id) => {
    const c = store.capCache.get(id);
    return [`${c.kind}/${c.slug}`, id];
  }));
  const existingTables = new Set(await store.tableNames());
  const tableSpecs = new Map(view.tables.map((t) => [t.spec.name, t.spec]));
  const written = [];
  // The plan's ALLOW statements, by table, as the engine keeps them.
  const accessOf = new Map([...parseAccessList(plan.access || [])].map(([name, rules]) => [name, rules.map(allowText)]));
  const stored = new Map();
  for (const t of plan.tables || []) {
    const old = tableSpecs.get(t.name);
    const columns = [...(old?.columns || [])];
    for (const c of t.columns || []) if (!columns.find((x) => x.name === c.name)) columns.push({ name: c.name, type: c.type, ...(c.type === 'meaning' ? { of: [].concat(c.of || []) } : {}) });
    const links = old?.links || t.links || null;
    if (links) for (const n of ['src', 'dst']) if (!columns.find((x) => x.name === n)) columns.unshift({ name: n, type: 'keyword' });
    const spec = { name: t.name, description: t.description || old?.description || '', columns, ...(links ? { links } : {}), ...(old?.access?.length ? { access: old.access } : {}) };
    const access = accessOf.get(t.name);
    if (access) spec.access = access;
    stored.set(t.name, spec);
    const res = await store.ensureTable(appId, spec, existingTables);
    if (res.created) progress(`Created table ${res.created}`);
    if (res.added.length) progress(`Added ${res.added.join(', ')} to ${t.name}`);
    if (!res.created && !res.added.length && old && !access) continue;
    const id = await store.putCapability({ app: appId, slug: t.name, kind: 'table', title: t.name, description: spec.description, spec, code: '' });
    caps.set(`table/${t.name}`, id);
    written.push(id);
  }
  // Access declared for a table the plan does not otherwise change: a new
  // version of the table with it (its statements replace the ones it had).
  for (const [name, access] of accessOf) {
    if (stored.has(name)) continue;
    const old = tableSpecs.get(name);
    if (!old || JSON.stringify(old.access || []) === JSON.stringify(access)) continue;
    const spec = { ...old, access };
    const id = await store.putCapability({ app: appId, slug: name, kind: 'table', title: name, description: spec.description, spec, code: '' });
    caps.set(`table/${name}`, id);
    written.push(id);
  }
  if (accessOf.size) progress(`Access: ${[...accessOf.values()].flat().join('; ')}`);
  // Screens: their queries run now against the tables as they are, and the
  // model gets one try to fix the screens whose queries fail.
  if ((plan.views || []).some((v) => v.screen)) {
    const names = [...new Set([...view.tables.map((t) => t.spec.name), ...(plan.tables || []).map((t) => t.name)])];
    const run = (label) => tr.span('check', label, async (c) => { const p = await dryRunScreens(engine.db, appId, names, plan); c.problems = p.slice(0, 8); return p; });
    let failed = await run("Run the screens' queries");
    if (failed.length) {
      tr.end(storing);
      progress(`Running the screens' queries: ${failed.length} failed (${failed.slice(0, 2).join('; ').slice(0, 300)}); asking the model to fix them`);
      const span = tr.start('fix', 'The model fixes the screens whose queries fail', { problems: failed.slice(0, 4) });
      const fixPrompt = `${prompt}\n\n# Your plan\n\n<plan>\n${JSON.stringify(plan, null, 1)}\n</plan>\n\n# These queries failed when run against the app's tables\n\n${failed.map((p) => `- ${p}`).join('\n')}\n\n# This step\n\nAnswer with a <plan> holding only "views": each screen view above whose queries failed, whole, with its queries fixed (the same slug, title and order). No code blocks, nothing else.`;
      try {
        const answer = await ask({ system, prompt: fixPrompt, onWrite: writing(progress) });
        tr.call(span, answer, system.length + fixPrompt.length);
        cost += answer.cost || 0;
        const fixed = parseAnswer(answer.text).plan;
        const cols = columnsFor(view, plan);
        for (const v of fixed.views || []) {
          const i = (plan.views || []).findIndex((x) => x.slug === v.slug);
          if (i < 0 || !v.screen || typeof v.screen !== 'object') continue;
          const ex = expandScreen(v.screen);
          if (ex.problems.length || checkScreen(ex.screen, cols).length) continue;
          v.screen = ex.screen;
          plan.views[i] = { ...plan.views[i], screen: v.screen };
          code[v.slug] = JSON.stringify(v.screen, null, 1);
        }
      } catch (e) { if (!span.end) tr.end(span, { error: e.message }); }
      failed = await run("Run the screens' queries again");
      progress(failed.length ? `Still failing, stored as they are (the runtime reports them): ${failed.slice(0, 2).join('; ').slice(0, 300)}` : "The screens' queries run");
      storing = tr.start('strata', 'Store the code and the new version in ChloeDB');
    }
  }
  // Photos the screens name by what they show become pictures (engine/photos.mjs).
  const photos = await tr.span('strata', 'Find the photos the screens name', () => placePhotos(plan, code, photoOpts));
  if (photos.count) progress(`Found ${photos.count} photo${photos.count === 1 ? '' : 's'}: ${photos.sources}`);
  for (const [kind, list] of [['view', plan.views], ['function', plan.functions]]) {
    for (const item of list || []) {
      // The patterns a screen was expanded from, for the graph: this
      // version's, and a patched screen keeps its earlier ones.
      const patterns = kind === 'view' && item.screen ? [...new Set([...(item.patched !== undefined ? view.views.find((x) => x.slug === item.slug)?.spec?.patterns || [] : []), ...(item.patterns || [])])] : [];
      const spec = kind === 'view' ? { order: item.order ?? 50, ...audienceOf(item, view), ...(item.screen ? { screen: true, ...(typeof item.statement === 'string' ? { statement: item.statement } : {}), ...(patterns.length ? { patterns } : {}) } : {}) } : { on: item.on || null, ...(item.every ? { every: item.every } : {}), ...(item.source ? { source: true } : {}), ...(item.rule ? { rule: true, ...(typeof item.statement === 'string' ? { statement: item.statement } : {}) } : {}) };
      const id = await store.putCapability({ app: appId, slug: item.slug, kind, title: item.title || item.slug, description: item.description || '',
        code: code[item.slug], spec, forked_from: item.forked_from || null, author: request.source === 'repair' ? 'repair' : 'forge' });
      caps.set(`${kind}/${item.slug}`, id);
      written.push(id);
    }
  }
  for (const r of plan.reuse || []) {
    const src = library.find((c) => c.id === r.id);
    const slug = r.slug || src.slug;
    const spec = { ...src.spec, ...(src.kind === 'view' && r.order !== undefined ? { order: r.order } : {}) };
    const id = await store.putCapability({ app: appId, slug, kind: src.kind, title: r.title || src.title, description: src.description,
      code: src.code, spec, forked_from: src.id, author: 'library' });
    caps.set(`${src.kind}/${slug}`, id);
    written.push(id);
    progress(`Reused ${src.title} from the library (${src.id})`);
  }
  for (const s of plan.remove || []) {
    caps.delete(`view/${s}`);
    caps.delete(`function/${s}`);
  }
  // A pattern the plan adds becomes a row of the library, for every app.
  for (const p of Array.isArray(plan.patterns) ? plan.patterns : []) {
    const { name, title, about, params, example, screen } = p;
    const id = await store.putCapability({ app: LIB, slug: name, kind: 'pattern', title, description: about,
      code: `${JSON.stringify({ title, about, params, example, screen }, null, 1)}\n`, spec: { params: Object.keys(params), from: appId }, author: appId });
    await store.usePatterns();
    progress(`Added the pattern ${name} to the library (${id}); every app can use it now`);
  }
  // Components and helpers go to the shared library; then the app pins what
  // its views and functions call.
  const writtenShared = new Map();
  const newVersions = [];
  for (const [kind, { ns, plural }] of Object.entries(SHARED)) {
    for (const c of plan[plural] || []) {
      const prev = shared.get(`${kind}/${c.name}`);
      const id = await store.putCapability({ app: LIB, slug: c.name, kind, title: c.title || c.name, description: c.description || '',
        code: code[`${ns}:${c.name}`], spec: { props: String(c.props || c.api || ''), change: prev ? String(c.change || '') : '', by: appId }, author: request.source === 'repair' ? 'repair' : 'forge' });
      writtenShared.set(`${kind}/${c.name}`, id);
      written.push(id);
      const version = Number(id.split('@').pop());
      progress(prev ? `Wrote version ${version} of the shared ${kind} ${c.name}` : `Wrote a new shared ${kind}, ${c.name}`);
      if (prev) newVersions.push({ kind, name: c.name, id, version, title: c.title || prev.title, change: String(c.change || '') });
    }
  }
  const had = new Map(view.shared.map((c) => [`${c.kind}/${c.slug}`, c.id]));
  const roots = [...caps.entries()].filter(([k]) => k.startsWith('view/') || k.startsWith('function/')).map(([, id]) => id);
  const pinned = await pinShared(store, roots, { written: writtenShared, had, upgrade: new Set(plan.upgrade || []), shared });
  for (const k of [...caps.keys()]) if (SHARED_KINDS.some((kind) => k.startsWith(`${kind}/`))) caps.delete(k);
  for (const [key, id] of pinned) {
    caps.set(key, id);
    const [kind, name] = key.split('/');
    if (!writtenShared.has(key) && had.get(key) !== id) progress(`Uses the shared ${kind} ${name} (version ${id.split('@').pop()})`);
  }
  // A candidate (built from use, to try) is a version beside the live one.
  const candidate = !!request.candidate;
  const n = await store.writeComposition(appId, [...caps.values()], request.text, plan.summary || '', { head: !candidate });
  // The live version's name and brief are the app's.
  if (!candidate) {
    const named = typeof plan.name === 'string' && plan.name.trim() && !(await store.setting(appId, 'named')) ? { name: appTitle(plan.name, view.app) } : {};
    const brief = briefToKeep(plan, view.app);
    const ux = brief ? { ux: JSON.stringify(brief) } : {};
    if (named.name || ux.ux) await store.updateApp(appId, { ...named, ...ux });
    // What was suggested for the version before (the build's and the drawn
    // check's) may be done now: new ones follow this version.
    await retireSuggestions(engine, appId, ['forge', 'look']).catch(() => {});
  }
  tr.end(storing, { written: written.length });
  tr.decisions.push(...planDecisions({ plan, view, library, shared, pinned, written: writtenShared, code, at: decidedAt }));
  progress(candidate
    ? `Version ${n} is ready to try: ${written.length} capabilit${written.length === 1 ? 'y' : 'ies'} written; the app stays on version ${view.comp.n} until it is kept`
    : `Version ${n} is live: ${written.length} capabilit${written.length === 1 ? 'y' : 'ies'} written`);
  // What the code uses of the model and the web, said in the build, with what
  // is still switched off (reach.mjs: the owner switches each on).
  if (engine.reach) {
    const now = await store.resolve(appId, candidate ? n : undefined).catch(() => null);
    const uses = now ? usesOf(now) : {};
    const on = await engine.reach.switches(appId).catch(() => ({}));
    const used = Object.keys(ABILITIES).filter((k) => uses[k]);
    if (used.length) {
      const off = used.filter((k) => !on[k]);
      progress(`It uses ${used.map((k) => ABILITIES[k].title.toLowerCase()).join(', ')}${off.length ? `; switch on ${off.map((k) => `"${ABILITIES[k].title}"`).join(' and ')} for it in the studio, under "Model and web"; until then ${off.length === 1 ? 'it answers' : 'they answer'} that ${off.length === 1 ? 'it is' : 'they are'} off` : ''}`);
      const wants = now ? await engine.reach.wants(appId, now).catch(() => null) : null;
      if (wants?.hosts.length) progress(`It reaches ${wants.hosts.join(', ')}, which its owner has not allowed yet: allow ${wants.hosts.length === 1 ? 'it' : 'them'} under "Model and web"`);
      if (wants?.secrets.length) progress(`It needs ${wants.secrets.length === 1 ? 'the secret' : 'the secrets'} ${wants.secrets.map((x) => `"${x}"`).join(', ')}: its owner adds ${wants.secrets.length === 1 ? 'it' : 'them'} under "Model and web"`);
    }
  }
  const problemsLeft = [];
  // The app's tests run on the tables this version creates, while they are
  // empty; a failure goes to the repair like an error a person saw.
  const fresh = new Set((plan.tables || []).map((t) => t.name).filter((n) => !view.tables.some((t) => t.spec.name === n)));
  if (TESTS && Array.isArray(plan.tests) && plan.tests.length && fresh.size) {
    const testing = tr.start('check', "Run the app's tests");
    const results = await runTests(engine, appId, plan.tests.slice(0, 5), fresh).catch((e) => [{ name: 'tests', ok: false, problem: e.message }]);
    const ran = results.filter((r) => r.ok !== null);
    const failed = results.filter((r) => r.ok === false);
    progress(failed.length ? `Tests: ${ran.length - failed.length} of ${ran.length} pass; ${failed.map((r) => `"${r.name}": ${r.problem}`).join('; ').slice(0, 400)}` : `Tests: ${ran.length} of ${ran.length} pass${ran.length < results.length ? ` (${results.length - ran.length} skipped)` : ''}`);
    tr.end(testing, { tests: results });
    // A candidate's failing test is the candidate's (shown with it), not the live app's to repair.
    for (const r of failed) {
      if (candidate) problemsLeft.push(`test "${r.name}" fails: ${r.problem}`);
      else if (r.cap) engine.reportError(appId, r.cap, `test "${r.name}" fails: ${r.problem}`, 'in a test the build ran');
    }
  }
  // Example rows go in once the tables are part of the live version.
  if (!view.comp.n && plan.seed) {
    const seeding = tr.start('strata', 'Add example rows');
    let added = 0;
    for (const [table, rows] of Object.entries(plan.seed)) {
      for (const row of (rows || []).slice(0, 20)) {
        try { await engine.insert(appId, table, row, { depth: 1 }); added++; } catch (e) { progress(`Example row skipped: ${e.message}`); }
      }
    }
    if (added) progress(`Added ${added} example rows`);
    tr.end(seeding, { rows: added });
  }
  // Every view is opened in a headless browser, when one is installed; an
  // error there reaches the engine as a person's would, and is repaired.
  // (A candidate's views are opened before it is offered: its errors are its own.)
  const opening = openViews(engine.baseUrl, appId, { v: candidate ? n : null }).then((r) => {
    const bad = r.views.filter((v) => v.errors.length);
    if (r.ran) engine.log(`[${appId}] opened ${r.views.length} view${r.views.length === 1 ? '' : 's'} after version ${n}${bad.length ? `: errors in ${bad.map((v) => `${v.tab || 'the app'} (${v.errors[0]})`).join('; ')}` : ', no errors'}`);
    return bad;
  }).catch((e) => { engine.log(`[${appId}] opening the views: ${e.message}`); return []; });
  // The drawn-page check (look.mjs): the live version drawn on a phone and a
  // computer, measured, and looked at against the brief, after the build has
  // answered; the server sends what stands in the way to the repair.
  if (LOOK && !candidate && engine.baseUrl) {
    const designed = designing ? Promise.race([designing, new Promise((ok) => setTimeout(ok, 30_000))]) : null;
    Promise.all([opening, designed]).then(() => checkDrawn(engine, appId)).then((r) => { if (r.ran) engine.emit('drawn', { app: appId, n, source: request.source || 'user', ...r }); })
      .catch((e) => engine.log?.(`[${appId}] drawing the screens: ${e.message}`));
  }
  if (candidate) {
    for (const b of await opening) problemsLeft.push(`${b.tab || 'the app'}: ${b.errors[0]}`);
    return { n, summary: plan.summary || '', written, cost, ms: Date.now() - started, trace: tr.toJSON(), candidate: true, base: view.comp.n, problems: problemsLeft };
  }
  await offerNewVersions(engine, appId, newVersions, progress);
  if (SUGGEST_AFTER) {
    suggestNext(engine, appId, request, plan.summary || '').catch((e) => engine.log?.(`[${appId}] suggestions: ${e.message}`));
  } else {
    const suggestions = (plan.suggestions || []).filter((s) => s && s.text).slice(0, 3)
      .map((s) => ({ text: String(s.text).slice(0, 300), why: String(s.why || '').slice(0, 300), source: 'forge' }));
    if (suggestions.length) {
      await retireSuggestions(engine, appId);
      await store.putSuggestions(appId, suggestions);
    }
  }
  return { n, summary: plan.summary || '', written, cost, ms: Date.now() - started, trace: tr.toJSON() };
}

// A new version of a shared component or helper is offered to every other
// app that uses an older one: taking it needs no model, the code is in Strata.
async function offerNewVersions(engine, appId, newVersions, progress) {
  if (!newVersions.length) return;
  const { store } = engine;
  const from = (await store.getApp(appId))?.name || appId;
  for (const nv of newVersions) {
    // Who depends on the piece, directly or through other pieces: the graph
    // walked backwards from it. The version each app has is in its pins.
    const deps = await store.dependents(`${LIB}/${nv.kind}/${nv.name}`).catch(() => []);
    const users = [];
    for (const d of deps.filter((x) => x.kind === 'app' && x.app !== appId)) {
      const v = await store.resolve(d.app).catch(() => null);
      const pin = v?.shared.find((c) => c.kind === nv.kind && c.slug === nv.name);
      if (pin) users.push({ app: d.app, name: v.app.name, version: Number(pin.version) });
    }
    for (const u of users) {
      if (u.app === appId || u.version >= nv.version) continue;
      const open = (await store.suggestions(u.app)).filter((s) => s.source === 'component' && String(s.cap || '').startsWith(`${LIB}/${nv.kind}/${nv.name}@`));
      if (open.length) await engine.db.write(open.map((s) => ({ op: 'set', table: 'sa_suggestions', key: s.id, field: 'status', value: 'stale' })));
      await store.putSuggestions(u.app, [{ text: `Use the newer ${nv.title} (version ${nv.version}, made for ${from})`, why: nv.change ? `It adds: ${nv.change}` : `A newer version of a shared ${nv.kind} this app uses (it has version ${u.version}).`, source: 'component', cap: nv.id }]);
      progress(`Offered ${nv.name} version ${nv.version} to ${u.name}, which has version ${u.version}`);
    }
  }
}

// Moves an app to another version of a shared component or helper, as a new
// version of the app. No model call: the code is in Strata already.
export async function useSharedVersion(engine, appId, capId, progress = () => {}) {
  const started = Date.now();
  const { store } = engine;
  const view = await store.resolve(appId);
  const [cap] = await store.getCapabilities([capId]);
  if (!cap || !SHARED_KINDS.includes(cap.kind)) throw new Error(`no shared component or helper ${capId}`);
  progress(`Moving ${cap.title} to version ${cap.version}: the code is in ChloeDB already, so no model is needed`);
  const caps = new Map(view.caps.map((c) => [`${c.kind}/${c.slug}`, c.id]));
  const had = new Map(view.shared.map((c) => [`${c.kind}/${c.slug}`, c.id]));
  const roots = [...view.views, ...view.functions].map((c) => c.id);
  const pinned = await pinShared(store, roots, { written: new Map([[`${cap.kind}/${cap.slug}`, cap.id]]), had });
  for (const k of [...caps.keys()]) if (SHARED_KINDS.some((kind) => k.startsWith(`${kind}/`))) caps.delete(k);
  for (const [key, id] of pinned) caps.set(key, id);
  const summary = `${cap.title} is now version ${cap.version}${cap.spec?.change ? `: ${String(cap.spec.change).replace(/\.\s*$/, '')}` : ''}.`;
  const n = await store.writeComposition(appId, [...caps.values()], `Use ${cap.title} version ${cap.version}`, summary);
  progress(`Version ${n} is live`);
  return { n, summary, written: [], cost: 0, ms: Date.now() - started };
}
export const useComponentVersion = useSharedVersion;

async function retireSuggestions(engine, appId, sources = ['forge']) {
  const open = await engine.store.suggestions(appId);
  const ops = open.filter((s) => sources.includes(s.source)).map((s) => ({ op: 'set', table: 'sa_suggestions', key: s.id, field: 'status', value: 'stale' }));
  if (ops.length) await engine.db.write(ops);
}

// ---- suggestions of what to ask next, written once a version is live

// What the engine learned across its apps (engine/changes.mjs): the kinds of
// change apps like this one made next, and the kinds of suggestion people take.
const learned = async (engine, appId) => (engine.changes ? engine.changes.promptPart(appId).catch(() => '') : '');

const NEXT_SYSTEM = `A person builds a small app by asking for changes, and a change they asked for was just built. Suggest 3 changes they might ask for next, written as they would ask for them ("Let me snooze a task until a date"), each with a short why that refers to what they asked, or to what they did as recorded; never claim a habit the record does not show. Make them specific to this app, small enough to build in one step, and new: not what the app does already (read the versions, which say what each change added), not what was just asked, and not one of the suggestions made before.

Answer with JSON only: {"suggestions": [{"text": "the change, in one sentence", "why": "one plain sentence under 25 words"}]}.`;

export async function suggestNext(engine, appId, request, summary) {
  const { store } = engine;
  const view = await store.resolve(appId);
  if (!view.comp.n) return [];
  const versions = await store.compositions(appId);
  const history = await store.suggestionHistory(appId, 30);
  const live = new Set(view.comp.caps);
  const events = (await store.recentEvents(appId, 200)).filter((e) => e.kind !== 'error' || live.has(e.cap));
  const prompt = [
    `# The app: ${view.app.name}\n${view.app.description || ''}`,
    `## Tables\n${view.tables.map((t) => `- ${t.spec.name}: ${(t.spec.columns || []).map((c) => c.name).join(', ')}`).join('\n') || '(none)'}`,
    `## Views\n${view.views.map((v) => `- ${v.title}: ${v.description || ''}`).join('\n') || '(none)'}`,
    `## Functions\n${view.functions.map((f) => `- ${f.slug}: ${f.description || ''}`).join('\n') || '(none)'}`,
    `## Versions, oldest first (what each change added)\n${versions.slice().reverse().map((c) => `- v${c.n}: ${c.summary || c.request || ''}`).join('\n')}`,
    `## Just built\nAsked: ${request.text}\nBuilt: ${summary}`,
    `## Suggestions made before\n${history.map((h) => `- [${h.status === 'taken' ? 'taken' : h.status === 'dismissed' ? 'dismissed' : h.status === 'open' ? 'still open' : 'replaced'}] ${h.text}`).join('\n') || '(none)'}`,
    `## What the person did lately\n${summarizeEvents(events)}`,
    await learned(engine, appId),
  ].filter(Boolean).join('\n\n');
  const answer = await ask({ system: NEXT_SYSTEM, prompt, model: SUGGEST_MODEL, effort: 'low' });
  let out;
  try {
    const m = answer.text.match(/\{[\s\S]*\}/);
    out = JSON.parse(m ? m[0] : answer.text);
  } catch { return []; }
  const items = (out.suggestions || []).filter((x) => x && x.text).slice(0, 3)
    .map((x) => ({ text: clipWords(x.text, 300), why: clipWords(x.why, 240), source: 'forge' }));
  if (!items.length) return [];
  // A newer version may have been built meanwhile: its own suggestions win.
  if ((await store.resolve(appId)).comp.n !== view.comp.n) return [];
  await retireSuggestions(engine, appId);
  const put = await store.putSuggestions(appId, engine.changes ? await engine.changes.rank(items) : items);
  engine.emit?.('suggestions', { app: appId });
  return put;
}

// ---- suggestions from use

// At most n characters, ending at a word, with an ellipsis when cut.
export const clipWords = (text, n) => {
  const t = String(text || '').trim();
  if (t.length <= n) return t;
  const cut = t.slice(0, n - 1);
  return `${cut.slice(0, Math.max(cut.lastIndexOf(' '), n / 2)).replace(/[\s,;:.]+$/, '')}…`;
};

const SUGGEST_SYSTEM = `You watch how one person uses a small app and suggest what to add or change next. You get the app (its tables, views and functions), what the person did with it lately, and capabilities other apps have in a shared library. Suggest up to 3 changes that the person's behaviour shows they would want: a step they repeat by hand, a filter they keep setting, a view they open to look for one thing, a field they keep writing into notes, something another app does that fits theirs. Each must be small enough to build in one step, new (not what the app does already: read the versions, which say what each change added), specific, and not one of the suggestions made before. The why names what in the record points to it; never claim a habit the record does not show. Errors listed come from the code that runs now.

The engine counts patterns in the record and lists them under "Patterns", each with a key in brackets; the ones marked NEW have no suggestion yet. Answer the NEW patterns first, the strongest first, at most one suggestion per pattern, and give that pattern's key as "signal" (null for a suggestion no pattern backs). A good answer to a pattern removes the repetition: a search typed again and again becomes a filter, a saved view or a section; a filter set every visit becomes the default or a tab; the same field changed row after row becomes a bulk action or a rule that does it; two views always opened together get a link or a summary of one on the other; a function run by hand becomes a trigger or a button where it is needed. A pattern keyed ux: is a measure of how hard the app is to use, against what it is for (the taps or the seconds it takes to do something, what is done away from the first screen, forms given up, rows added by mistake): answer it by making what people do there quicker, not by adding to it: put it on the first screen or open on its screen, ask fewer fields with defaults, one tap where there is a form.

Answer with JSON only: {"suggestions": [{"text": "the change, as the person would ask for it, in one sentence", "why": "what in their use points to it: one plain sentence under 25 words", "signal": "a pattern's key, or null", "library": "the id of a library capability it would reuse, or null"}]}. When nothing in the use points anywhere, answer {"suggestions": []}.`;

export async function suggestFromUse(engine, appId, { signals = [], fresh = [] } = {}) {
  const { store } = engine;
  const view = await store.resolve(appId);
  if (!view.comp.n) return [];
  const live = new Set(view.comp.caps);
  const events = (await store.recentEvents(appId, 400)).filter((e) => e.kind !== 'error' || live.has(e.cap));
  const versions = await store.compositions(appId);
  const history = await store.suggestionHistory(appId, 40);
  const tables = [];
  for (const t of view.tables) tables.push(`- ${t.spec.name} (${await store.countRows(physical(appId, t.spec.name))} rows): ${(t.spec.columns || []).map((c) => `${c.name} ${c.type}`).join(', ')}`);
  const lib = await store.searchCapabilities(`${view.app.name} ${view.app.description} ${view.views.map((v) => v.title).join(' ')}`, { excludeApp: appId, limit: 8 });
  const prompt = [
    `# The app: ${view.app.name}\n${view.app.description || ''}`,
    `## Tables\n${tables.join('\n')}`,
    `## Views\n${view.views.map((v) => `- ${v.slug}: ${v.title}. ${v.description}`).join('\n')}`,
    `## Functions\n${view.functions.map((f) => `- ${f.slug}: ${f.description}${f.spec.on ? ` (runs on ${f.spec.on.event} ${f.spec.on.table})` : ''}${f.spec.every ? ` (runs ${everyWords(f.spec.every)})` : ''}`).join('\n') || '(none)'}`,
    `## Versions, oldest first (what each change added)\n${versions.slice().reverse().map((c) => `- v${c.n}: ${c.summary || c.request}`).join('\n')}`,
    `## Suggestions made before\n${history.map((h) => `- [${h.status}] ${h.text}`).join('\n') || '(none)'}`,
    `## Patterns (counted by the engine)\n${signals.map((s) => `- [${s.key}]${fresh.some((f) => f.key === s.key) ? ' NEW' : ''} ${s.text}`).join('\n') || '(none yet)'}`,
    `## What the person did\n${summarizeEvents(events)}`,
    `## Recent actions in order (newest first)\n${events.slice(0, 60).map((e) => `- ${e.ts} ${e.kind} ${e.detail}`).join('\n')}`,
    `## Library\n${lib.map((c) => `- ${c.id} (${c.kind}): ${c.title}. ${c.description}`).join('\n') || '(empty)'}`,
    await learned(engine, appId),
  ].filter(Boolean).join('\n\n');
  const answer = await ask({ system: SUGGEST_SYSTEM, prompt });
  let out;
  try {
    const m = answer.text.match(/\{[\s\S]*\}/);
    out = JSON.parse(m ? m[0] : answer.text);
  } catch { return []; }
  const keys = new Set(signals.map((s) => s.key));
  const items = (out.suggestions || []).filter((s) => s && s.text).slice(0, 3)
    .map((s) => ({ text: clipWords(s.text, 300), why: clipWords(s.why, 240), source: 'use', cap: s.library || '', signal: keys.has(s.signal) ? s.signal : null }));
  return items.length ? store.putSuggestions(appId, engine.changes ? await engine.changes.rank(items) : items) : [];
}

// A person's own change to a screen, with no model: a patch on the screen as
// the app has it (or a whole screen), expanded, checked and its queries run,
// then stored as the view's next version in a new version of the app. A
// screen that fails a check is not stored; the problems come back.
export async function editScreen(engine, appId, slug, { patch, screen, statement, note } = {}) {
  const store = engine.store;
  const view = await store.resolve(appId);
  const cur = view.views.find((v) => v.slug === slug);
  if (!cur || !cur.spec?.screen) throw Object.assign(new Error(`the app has no screen ${slug}`), { status: 404 });
  // A screen written as a statement is edited as its statement (its source).
  if (cur.spec.statement && statement === undefined && !(screen && typeof screen === 'object')) throw Object.assign(new Error(`${slug} is a CREATE SCREEN statement: edit its statement`), { status: 400 });
  let next;
  let source = null;
  let photos = null;
  try {
    if (typeof statement === 'string') {
      const out = compileStatement(statement);
      if (out.slug !== slug) throw new Error(`the statement creates ${out.slug}; this screen is ${slug}`);
      next = out.screen;
      photos = out.photos;
      source = statement.trim();
    } else next = screen && typeof screen === 'object' ? screen : applyPatch(JSON.parse(cur.code), Array.isArray(patch) ? patch : []);
  } catch (e) { throw Object.assign(new Error(e.message), { status: 400 }); }
  const ex = expandScreen(next);
  // The photos the statement keeps (its PHOTOS part) go back where they were.
  if (photos && !ex.problems.length) withPhotos(ex.screen, photos);
  const problems = [...ex.problems, ...checkScreen(ex.screen, columnsFor(view, {}))];
  // A photo the edit names is found as for a built screen.
  if (!problems.length) await placeScreenPhotos(ex.screen, { ...photoOptions(engine, appId, null, view), used: new Set([...view.views.filter((v) => v.slug !== slug).flatMap((v) => photoIds(v.code)), ...photoIds(JSON.stringify(ex.screen))]) });
  if (!problems.length) problems.push(...await dryRunScreens(engine.db, appId, view.tables.map((t) => t.spec.name), { views: [{ slug, screen: ex.screen }] }));
  if (problems.length) throw Object.assign(new Error(problems.slice(0, 4).join('; ')), { status: 400, problems });
  const code = JSON.stringify(ex.screen, null, 1);
  if (code === cur.code && (source || null) === (cur.spec.statement || null)) return { n: view.comp.n, unchanged: true };
  const spec = { ...cur.spec };
  if (source) spec.statement = source; else delete spec.statement;
  // The patterns it was expanded from: a patch keeps the earlier ones.
  const used = [...new Set([...(!source && !(screen && typeof screen === 'object') ? cur.spec.patterns || [] : []), ...ex.used])];
  if (used.length) spec.patterns = used; else delete spec.patterns;
  // Shared components the screen names must exist; the app pins the ones it
  // did not use before (a pattern may bring the kanban, the bar chart...).
  const shared = await store.shared();
  const missing = [...sharedRefs(code)].filter((k) => !shared.has(k));
  if (missing.length) throw Object.assign(new Error(`the library has no ${missing.map((k) => k.replace('component/', 'ui.').replace('helper/', 'lib.')).join(', ')}`), { status: 400 });
  const id = await store.putCapability({ app: appId, slug, kind: 'view', title: cur.title, description: cur.description, code, spec, author: 'person' });
  const own = view.comp.caps.filter((c) => !view.shared.some((x) => x.id === c)).map((c) => (c === cur.id ? id : c));
  const roots = own.filter((c) => /\/(view|function)\//.test(c));
  const pinned = await pinShared(store, roots, { had: new Map(view.shared.map((c) => [`${c.kind}/${c.slug}`, c.id])), shared });
  const text = String(note || `Edited the screen ${cur.title} by hand`).slice(0, 300);
  const n = await store.writeComposition(appId, [...own, ...pinned.values()], text, text);
  return { n, cap: id, patterns: ex.used };
}
