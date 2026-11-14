// Who may use the studio once others can reach it. On a laptop the studio
// listens on 127.0.0.1 and has no login. Beside a cluster, chloedb-server
// lists the engine as its app `apps` (--app apps=URL, STRATA_APP_TOKEN), and
// the engine lets in:
//
// - chloedb-server, which forwards /v1/apps/apps/<path> here with its
//   STRATA_APP_TOKEN once it has checked the caller's key (read scope for
//   GET, admin for the rest): the command center's page (web/cc-view.js)
//   and its calls;
// - a browser with a studio session, which it gets from a one-time link
//   the command center asks for (POST /api/handoff, so only with an admin
//   key): /login?code=... sets the session's cookie and opens the studio;
// - the engine's own headless browser, which opens the apps it builds to
//   check and draw them, with a cookie made at each start (selfCookie);
// - an agent's MCP server (mcp.mjs), with the engine's agent token
//   (DATA/agent.token, or STRATA_APPS_AGENT_TOKEN) as its bearer token: the
//   studio's rights, on a laptop and beside a cluster alike (audit 4, X-G7);
// - anyone, for what sandboxed frames load without cookies (map tiles, and
//   files named by ids nobody can guess), the login itself and /healthz.
//
// Sessions are rows of sa_settings (the app "_studio"), so they outlast a
// restart of the engine and work from any pod.

import { createHash, randomBytes, timingSafeEqual } from 'node:crypto';

const DAY = 86400000;
export const LOGIN_DAYS = 30;
const CODE_MS = 2 * 60000;
const COOKIE = 'sa_studio';
// Loaded from inside sandboxed frames, which send no cookies (deploy/gate.mjs
// lets the same through).
// /api/tls/ask is Caddy's question, before it gets a certificate for a host
// (spec.apps.publish): whether the host is a published app's, which anyone
// may learn by opening it.
const OPEN = [/^\/api\/tiles\/\d{1,2}\/\d{1,7}\/\d{1,7}\.png$/, /^\/api\/apps\/[a-z0-9]+\/files\/[a-z0-9]{6,20}$/, /^\/healthz$/, /^\/api\/tls\/ask$/];

const digest = (s) => createHash('sha256').update(String(s)).digest();
const bearerOf = (req) => { const m = String(req.headers.authorization || '').match(/^Bearer\s+(.+)$/i); return m ? m[1].trim() : null; };
const hex = (s) => digest(s).toString('hex');

export class Login {
  // appToken: the STRATA_APP_TOKEN chloedb-server sends; publicUrl: where
  // browsers reach the studio (STRATA_APPS_PUBLIC_URL), for the links.
  // agentToken: what an agent's MCP server sends (mcp.mjs).
  constructor({ store, appToken, agentToken, publicUrl, log = () => {} }) {
    this.store = store;
    this.want = appToken ? digest(appToken) : null;
    this.agent = agentToken ? digest(agentToken) : null;
    this.publicUrl = publicUrl ? publicUrl.replace(/\/$/, '') : null;
    this.secure = !!this.publicUrl?.startsWith('https://');
    this.log = log;
    this.codes = new Map(); // code hash -> {expires, next}
    this.sessions = new Map(); // token hash -> expires (ms)
    this.selfToken = randomBytes(32).toString('base64url');
    this.self = digest(this.selfToken);
  }

  // The cookie of the engine's own browser (checks.mjs), or null with no
  // login: a session no row holds, for this process only.
  get selfCookie() {
    return this.on ? { name: COOKIE, value: this.selfToken } : null;
  }

  // Is a login needed at all: only beside a cluster.
  get on() { return !!this.want; }

  // The request came through chloedb-server: it carries its app token.
  fromServer(req) {
    const b = bearerOf(req);
    return !!(this.want && b && timingSafeEqual(digest(b), this.want));
  }

  // The request is an agent's, with the agent token (mcp.mjs).
  fromAgent(req) {
    const b = bearerOf(req);
    return !!(this.agent && b && timingSafeEqual(digest(b), this.agent));
  }

  cookieOf(req) {
    const m = String(req.headers.cookie || '').match(/(?:^|;\s*)sa_studio=([A-Za-z0-9_-]{20,80})/);
    return m ? m[1] : null;
  }

  async signedIn(req) {
    const token = this.cookieOf(req);
    if (!token) return false;
    if (timingSafeEqual(digest(token), this.self)) return true;
    const id = hex(token);
    let expires = this.sessions.get(id);
    if (expires === undefined) {
      const v = await this.store.setting('_studio', `login:${id}`).catch(() => null);
      expires = v ? Date.parse(JSON.parse(v).expires) || 0 : 0;
      this.sessions.set(id, expires);
    }
    return expires > Date.now();
  }

  // null when the request may go on; else what to answer.
  async check(req, url) {
    if (OPEN.some((re) => re.test(url.pathname)) || url.pathname === '/login') return null;
    // A bearer token is checked with a login or without: one that is neither
    // chloedb-server's nor the agent token is refused, so an agent with a
    // wrong or stale token learns it rather than passing as nobody.
    if (bearerOf(req) && !this.fromServer(req) && !this.fromAgent(req)) {
      return { status: 401, json: { error: 'that bearer token is not this engine\'s: an agent uses the engine\'s agent token (DATA/agent.token, or STRATA_APPS_AGENT_TOKEN)' } };
    }
    if (!this.on) return null;
    if (this.fromServer(req) || this.fromAgent(req) || await this.signedIn(req)) return null;
    return url.pathname.startsWith('/api/')
      ? { status: 401, json: { error: 'sign in to the studio from the cluster\'s command center: Apps, then App creator, then Studio on an app' } }
      : { status: 401, html: PAGE };
  }

  // POST /api/handoff, through chloedb-server (an admin key): a link that
  // signs this browser in once, within two minutes, and opens the studio at
  // next (#/...), or an app as a stranger sees it (a/<id>?stranger=<access>).
  handoff(req, next = '') {
    if (!this.fromServer(req)) throw Object.assign(new Error('a login link is made only for the command center, through chloedb-server'), { status: 403 });
    if (!this.publicUrl) throw Object.assign(new Error('the studio has no address browsers reach: set STRATA_APPS_PUBLIC_URL (spec.apps.publicUrl)'), { status: 409 });
    const now = Date.now();
    for (const [k, c] of this.codes) if (c.expires < now) this.codes.delete(k);
    const code = randomBytes(24).toString('base64url');
    this.codes.set(hex(code), { expires: now + CODE_MS, next: /^#[A-Za-z0-9=&/_-]{0,200}$/.test(next) || /^a\/[a-z0-9]{1,20}\?stranger=(look|add|change)$/.test(next) ? next : '' });
    return { url: `${this.publicUrl}/login?code=${code}`, expires: new Date(now + CODE_MS).toISOString() };
  }

  // GET /login?code=...: the code, once, for a session; then the studio.
  async redeem(url, res) {
    const key = hex(url.searchParams.get('code') || '');
    const c = this.codes.get(key);
    this.codes.delete(key);
    if (!c || c.expires < Date.now()) {
      res.writeHead(401, { 'content-type': 'text/html; charset=utf-8', 'cache-control': 'no-store' });
      return res.end(PAGE.replace('<p>', '<p>That link has been used or is more than two minutes old. '));
    }
    const token = randomBytes(32).toString('base64url');
    const expires = Date.now() + LOGIN_DAYS * DAY;
    await this.store.putSetting('_studio', `login:${hex(token)}`, JSON.stringify({ expires: new Date(expires).toISOString() }));
    this.sessions.set(hex(token), expires);
    this.log('studio: signed in from the command center');
    res.writeHead(302, {
      location: `/${c.next}`,
      'set-cookie': `${COOKIE}=${token}; Path=/; HttpOnly; SameSite=Lax; Max-Age=${LOGIN_DAYS * 86400}${this.secure ? '; Secure' : ''}`,
      'cache-control': 'no-store',
    });
    res.end();
  }

  // Origins a POST may come from: this host, and the public address.
  sameOrigin(req) {
    const origin = req.headers.origin;
    if (!origin) return true;
    const host = req.headers.host;
    return origin === `http://${host}` || origin === `https://${host}` || (!!this.publicUrl && origin === new URL(this.publicUrl).origin);
  }
}

const PAGE = `<!doctype html><meta charset="utf-8"><meta name="viewport" content="width=device-width, initial-scale=1">
<title>Sign in to the studio</title>
<style>body{font:16px/1.5 system-ui,sans-serif;margin:0;min-height:100vh;display:grid;place-items:center;background:#f4f5f7;color:#1d2433}
main{max-width:28rem;padding:2rem;background:#fff;border:1px solid #dfe2e8;border-radius:8px}h1{font-size:1.2rem;margin:0 0 .5rem}p{margin:0}
@media (prefers-color-scheme: dark){body{background:#14171c;color:#e6e8ec}main{background:#1c2027;border-color:#2c313a}}</style>
<main><h1>Sign in to the studio</h1><p>Open it from the cluster's command center: Apps, then App creator, then Studio on an app. That needs an admin key, and this browser then stays signed in for ${LOGIN_DAYS} days.</p></main>`;
