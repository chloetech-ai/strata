// Who may read and write an app's tables, declared with the app and enforced
// by the engine on every read and write its people make (engine.mjs): the
// screens do not decide it, the server does.
//
// A table's access is a list of ALLOW statements, kept in the table's spec
// (its capability, so a version of the app carries the access it had):
//
//   ALLOW READ ON products TO everyone
//   ALLOW READ, WRITE ON receivables TO members
//   ALLOW ALL ON notes TO members WHERE owner_id = :user_id
//   ALLOW ALL ON invoices TO members WHERE company_id = :company_id
//   ALLOW ALL ON invoices TO admins
//
// - What: READ, ADD (insert), CHANGE (update), REMOVE (delete), WRITE (the
//   three writes) or ALL, several with commas.
// - Who: EVERYONE (visitors too), MEMBERS (anyone signed in to the app),
//   ADMINS (its admins and its owner) or OWNER. Each includes the ones
//   above it: a statement TO members also lets admins and the owner.
// - Which rows (optional): WHERE <column> = :user_id (the row is the
//   signed-in person's) or <column> = :company_id (the row is the company the
//   person works in now, from the memberships the engine keeps, see
//   services.mjs), several joined by OR. A read sees those rows only; an add
//   fills the column when it is missing and is refused when it names someone
//   else; a change or remove reaches those rows only and cannot move a row
//   out of them.
//
// A table with no statement, in an app that declares no access anywhere, is
// as tables were before access was declared: whoever reaches the app (as its
// publish choice says) reaches the table. Once any table of the app declares
// access, a table that declares none is read by everyone and written only by
// the app's owner (audit 4, X-G2: an app that says who may write some tables
// does not leave the others open to every visitor). A table with one
// statement or more lets only what its statements allow.
// The studio (the app's builder and owner, not through the public path)
// always has every row: the statements are for the app's people.
//
// Server functions called by people run as them (their reads and writes are
// held to the same statements), and so do the triggers their writes set off
// (audit 4, X-G4): a trigger reaches only what the person whose write fired
// it may reach, unless the app marks it to run as the app ("as": "app", or
// RUNS AS APP in a CREATE RULE), for a trigger that keeps a total over
// everyone's rows. Schedules, and triggers of the studio's writes, run as the
// app.

export const ACTIONS = ['read', 'add', 'change', 'remove'];
const ACTION_WORDS = { READ: ['read'], SELECT: ['read'], ADD: ['add'], INSERT: ['add'], CHANGE: ['change'], UPDATE: ['change'], EDIT: ['change'],
  REMOVE: ['remove'], DELETE: ['remove'], WRITE: ['add', 'change', 'remove'], ALL: ACTIONS };
export const WHO = ['everyone', 'members', 'admins', 'owner'];
const WHO_WORDS = { EVERYONE: 'everyone', PUBLIC: 'everyone', VISITORS: 'everyone', ANYONE: 'everyone', MEMBERS: 'members', MEMBER: 'members', USERS: 'members',
  'SIGNED IN': 'members', 'SIGNED-IN': 'members', ADMINS: 'admins', ADMIN: 'admins', OWNER: 'owner', OWNERS: 'owner' };
// A person's role in an app, from the least: someone not signed in, a member,
// an admin, the owner.
export const ROLES = ['visitor', 'member', 'admin', 'owner'];
const rank = (role) => Math.max(0, ROLES.indexOf(role));
const NAME = /^[a-z][a-z0-9_]*$/;
const BY = { user_id: 'user', 'user.id': 'user', company_id: 'company', 'company.id': 'company' };
// The parameters the engine binds itself: what a scope compares with.
export const SCOPE_PARAM = { user: 'sa_user_id', company: 'sa_company_id' };

const fail = (msg) => { throw Object.assign(new Error(`ALLOW: ${msg}`), { status: 400 }); };
const HOW = 'ALLOW READ | ADD | CHANGE | REMOVE | WRITE | ALL ON <table> TO everyone | members | admins | owner [WHERE <column> = :user_id | <column> = :company_id]';

// One statement -> {table, actions, who, scope: [{col, by}]}.
export function parseAllow(text) {
  const s = String(text || '').trim().replace(/;\s*$/, '').replace(/\s+/g, ' ');
  const m = s.match(/^ALLOW\s+(.+?)\s+ON\s+([A-Za-z_][A-Za-z0-9_]*)\s+TO\s+(.+?)(?:\s+WHERE\s+(.+))?$/i);
  if (!m) fail(`write it as ${HOW}, not ${JSON.stringify(s.slice(0, 120))}`);
  const actions = new Set();
  for (const w of m[1].split(/\s*,\s*|\s+AND\s+/i)) {
    const a = ACTION_WORDS[w.trim().toUpperCase()];
    if (!a) fail(`${w.trim()} is not READ, ADD, CHANGE, REMOVE, WRITE or ALL`);
    for (const x of a) actions.add(x);
  }
  const table = m[2].toLowerCase();
  if (!NAME.test(table)) fail(`${m[2]} is not a table name`);
  const who = WHO_WORDS[m[3].trim().toUpperCase().replace(/^THE\s+(APP'?S\s+)?/, '').replace(/\s+ONLY$/, '')];
  if (!who) fail(`TO ${m[3].trim()}: say everyone, members, admins or owner`);
  const scope = [];
  if (m[4]) {
    for (const term of m[4].split(/\s+OR\s+/i)) {
      const t = term.trim().replace(/^\((.*)\)$/, '$1').trim().match(/^([A-Za-z_][A-Za-z0-9_]*)\s*=\s*:([A-Za-z_.]+)$/);
      if (!t || !BY[t[2].toLowerCase()]) fail(`WHERE takes <column> = :user_id or <column> = :company_id (joined by OR), not ${JSON.stringify(term.trim())}`);
      const col = t[1].toLowerCase();
      if (col === 'id' || col === 'created') fail(`WHERE ${col}: scope rows by a column that holds a person's or a company's id (owner_id, company_id)`);
      scope.push({ col, by: BY[t[2].toLowerCase()] });
    }
  }
  return { table, actions: ACTIONS.filter((a) => actions.has(a)), who, scope };
}

// The statement as the engine keeps and shows it.
export function allowText(rule) {
  const what = rule.actions.length === ACTIONS.length ? 'ALL' : rule.actions.map((a) => a.toUpperCase()).join(', ');
  const where = rule.scope.length ? ` WHERE ${rule.scope.map((t) => `${t.col} = :${t.by}_id`).join(' OR ')}` : '';
  return `ALLOW ${what} ON ${rule.table} TO ${rule.who}${where}`;
}

// The statements of a plan's "access" (a list, or text with one per line or
// separated by ;), grouped by table: Map table -> [rule].
export function parseAccessList(list) {
  const items = Array.isArray(list) ? list : String(list || '').split(/;|\n/);
  const out = new Map();
  for (const raw of items) {
    if (!String(raw || '').trim()) continue;
    const r = parseAllow(raw);
    if (!out.has(r.table)) out.set(r.table, []);
    out.get(r.table).push(r);
  }
  return out;
}

// Problems with a plan's access statements, against the app's tables and
// columns (Map table -> Set of columns).
export function checkAccess(list, columns) {
  const problems = [];
  const items = Array.isArray(list) ? list : list ? [list] : [];
  for (const raw of items) {
    let r;
    try { r = parseAllow(raw); } catch (e) { problems.push(`access: ${e.message}`); continue; }
    if (!columns.has(r.table)) { problems.push(`access: ${allowText(r)} names the table ${r.table}, which the app does not have`); continue; }
    for (const t of r.scope) if (!columns.get(r.table).has(t.col)) problems.push(`access: ${allowText(r)}: ${r.table} has no column ${t.col} (add it to the table, a keyword column)`);
  }
  return problems;
}

const compiled = new Map();
// A table spec's rules (spec.access holds the statements), or null when the
// table declares none.
export function rulesOf(spec) {
  const list = spec && Array.isArray(spec.access) ? spec.access : null;
  if (!list || !list.length) return null;
  const key = list.join('\n');
  let rules = compiled.get(key);
  if (!rules) {
    rules = [];
    for (const s of list) { try { rules.push(parseAllow(s)); } catch { /* a statement that no longer parses allows nothing */ } }
    if (compiled.size > 2000) compiled.clear();
    compiled.set(key, rules);
  }
  return rules;
}

const PEOPLE = { everyone: 'everyone', members: "the app's signed-in members", admins: "the app's admins", owner: "the app's owner" };
const VERB = { read: 'see', add: 'add to', change: 'change', remove: 'remove from' };
const refusal = (status, message) => Object.assign(new Error(message), { status, refused: status === 401 ? 'sign-in' : 'access' });

// What `who` may do: {all: true} (every row), {scope: [{col, by}]} (those
// rows), or a thrown 401 (sign in first) / 403. who is {role, user, company}
// (a person of the app, or role visitor and no user); null is the studio.
//
// closed: the app declares access on some table, so a table that declares
// none is read by everyone and written by the owner only (CLOSED).
const CLOSED = ['ALLOW READ ON t TO everyone', 'ALLOW WRITE ON t TO owner'].map(parseAllow);
export function decide(spec, action, who, { closed = false } = {}) {
  const rules = rulesOf(spec) || (closed ? CLOSED : null);
  if (!rules || !who) return { all: true };
  const table = spec.name;
  const mine = rules.filter((r) => r.actions.includes(action));
  const fits = mine.filter((r) => rank(who.role) >= WHO.indexOf(r.who));
  if (!fits.length) {
    const least = mine.length ? mine.map((r) => WHO.indexOf(r.who)).sort()[0] : -1;
    const whom = least >= 0 ? PEOPLE[WHO[least]] : null;
    if (rules === CLOSED) throw refusal(403, `${table} is read-only here: only the app's owner can ${VERB[action]} it`);
    if (!who.user) throw refusal(401, `Sign in first: ${whom ? `only ${whom} can ${VERB[action]} ${table}` : `${table} is not open to visitors`}`);
    throw refusal(403, whom ? `Only ${whom} can ${VERB[action]} ${table}` : `Nobody can ${VERB[action]} ${table} here`);
  }
  if (fits.some((r) => !r.scope.length)) return { all: true };
  const terms = [];
  for (const r of fits) for (const t of r.scope) if (!terms.some((x) => x.col === t.col && x.by === t.by)) terms.push(t);
  const usable = terms.filter((t) => (t.by === 'user' ? !!who.user : !!who.company));
  if (!usable.length) {
    if (!who.user) throw refusal(401, `Sign in first: ${table} shows each person their own rows`);
    throw refusal(403, `Join or choose a company first: ${table} holds each company's own rows`);
  }
  return { scope: usable };
}

const valueOf = (t, who) => (t.by === 'user' ? who.user?.id : who.company);

// The scope as SQL on a table's alias, with the engine's own parameters.
export function scopeSql(scope, alias, { left = false } = {}) {
  const terms = scope.map((t) => `${alias}.${t.col} = :${SCOPE_PARAM[t.by]}`).join(' OR ');
  return left ? `(${terms} OR ${alias}.id IS NULL)` : `(${terms})`;
}

// The engine's parameters for a scope: never what the caller sent.
export function scopeParams(who) {
  const out = {};
  if (who?.user?.id) out[SCOPE_PARAM.user] = String(who.user.id);
  if (who?.company) out[SCOPE_PARAM.company] = String(who.company);
  return out;
}

export const inScope = (row, scope, who) => !!row && scope.some((t) => row[t.col] != null && String(row[t.col]) === String(valueOf(t, who)));

// An add under a scope: the scope's columns the row leaves out are filled
// with the person's (or company's) id; a row that names someone else is
// refused.
export function fillScope(fields, scope, who, table) {
  const out = { ...(fields || {}) };
  for (const t of scope) if (out[t.col] === undefined || out[t.col] === null || out[t.col] === '') out[t.col] = valueOf(t, who);
  if (!inScope(out, scope, who)) throw refusal(403, `You can add to ${table} only as yourself (${scope.map((t) => t.col).join(' or ')})`);
  return out;
}

// Whether a request asks that the data be for signed-in people, roles or
// companies only (the forge's check that the plan declares access).
export const WANTS_ACCESS = /\b(only (?:signed[- ]?in|logged[- ]?in|registered|authenticated|members|admins?|the owner)|sign[- ]?in (?:is )?(?:required|first|only)|(?:must|have to|need to|should) (?:be )?(?:sign(?:ed)?[- ]?in|log(?:ged)?[- ]?in)|require[sd]? (?:a )?(?:sign[- ]?in|log[- ]?in|login|authentication|an account)|add (?:user )?(?:authentication|auth|login|sign[- ]?in)|authentication|authenticated|roles?\b|admins? (?:can|only|may)|per[- ](?:company|workspace|team|organi[sz]ation|tenant)|each (?:company|workspace|team|organi[sz]ation|tenant)|multi[- ]?tenant|their own (?:rows|data|records|notes|items|entries)|private to (?:each|the) (?:user|person))/i;
export const wantsAccess = (text) => WANTS_ACCESS.test(String(text || ''));
