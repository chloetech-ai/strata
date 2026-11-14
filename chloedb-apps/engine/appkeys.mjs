// Each app's own key to ChloeDB, so that what an app's code reads and writes
// (app.sql, insert, update, remove, the rows its activity names) is held to
// the app's tables by the server, not only by the engine's rewrite of its
// SQL (sql.mjs). The engine itself keeps a key for its own tables (sa_*)
// and its apps' (app_*), and makes each app's key from it:
//
//   POST /v1/keys/own {name: "app-<id>", tables: {"app_<id>_*": "write", ...}}
//
// which the server makes only within the tables the engine's key has the
// admin scope on, names "<engine key>/app-<id>", and ends with the engine's
// key. An app's key is made the first time the app reads or writes, kept
// sealed in sa_settings (`<id>:key`, with the engine's secret key) so that
// a restart reuses it, and made again when the server no longer takes it.
//
// What the engine is given decides the mode (`mode`):
//
// - "per-app": a key limited to tables with the admin scope on sa_* and
//   app_* (what the operator gives it), or an admin credential, which the
//   engine trades for such a key of its own (`chloedb-apps`, made with
//   /v1/admin/keys and kept sealed as `_engine:key`), and follows the feed
//   of changes with it too where the server filters the feed by a limited
//   key's tables (`auth.limited_feed`); an older server's feed needs the
//   admin token, which the engine then keeps for that alone.
// - "shared": every app's reads and writes go through the engine's one
//   credential, as before such keys: an open server (no token, so no keys),
//   a server from before /v1/keys/own, a credential that may not make keys,
//   or STRATA_APPS_KEYS=shared. Said loudly at start, and in /api/status.

import { Strata } from './strata.mjs';
import { functionReadTables } from './sql.mjs';

const ENGINE = '_engine';
export const ENGINE_KEY_NAME = 'chloedb-apps';
export const ENGINE_GRANTS = { 'sa_*': 'admin', 'app_*': 'admin' };
const APP_ID = /^[a-z0-9]{1,32}$/;

// The tables app `id`'s key may touch: its own (write), and the engine's
// tables the library's table functions read (sa_numbers, read). (The
// names sql.mjs finds after FROM in the functions' text include words of
// their descriptions: only the engine's own tables are taken.)
export function appGrants(id, reads = functionReadTables()) {
  const out = { [`app_${id}_*`]: 'write' };
  for (const t of reads) if (/^sa_[a-z0-9_]+$/.test(t) && !['sa_people', 'sa_sessions', 'sa_settings', 'sa_members', 'sa_companies', 'sa_memberships'].includes(t)) out[t] = 'read';
  return out;
}

// A client of one app: its requests carry the app's key, and a key the
// server no longer takes (revoked, its maker made again) is made again once.
class AppClient extends Strata {
  constructor(url, token, renew) {
    super(url, token);
    this.own = true;
    this.renew = renew;
  }

  async req(method, route, body) {
    for (let tries = 0; ; tries++) {
      try {
        return await super.req(method, route, body);
      } catch (e) {
        if (e.status !== 401 || !this.renew || tries >= 5) throw e;
        this.token = await this.renew();
      }
    }
  }
}

export class AppKeys {
  // db: the engine's client (its token is changed in place when the engine
  // trades an admin credential for a key of its own); store: for
  // sa_settings; box: sealer() of reach.mjs.
  // ownServer: the engine started the server itself, with a token of its own.
  constructor({ db, store, box, log = () => {}, want = process.env.STRATA_APPS_KEYS || 'per-app', ownServer = false }) {
    this.ownServer = ownServer;
    this.db = db;
    this.store = store;
    this.box = box;
    this.log = log;
    this.want = want;
    this.mode = 'shared';
    this.why = 'not started';
    this.engineKey = null; // {name, tables} of the key the engine holds
    this.clients = new Map(); // app id -> AppClient
    this.making = new Map(); // app id -> the promise of its key
    this.made = new Map(); // app id -> when its key was last made (ms)
  }

  status() {
    return { mode: this.mode, why: this.why, engineKey: this.engineKey, apps: this.clients.size };
  }

  shared(why) {
    this.mode = 'shared';
    this.why = why;
    this.log(`APP KEYS OFF: ${why}. Every app reads and writes ChloeDB with the engine's one credential, and only the engine's SQL rewrite keeps an app to its own tables.`);
  }

  async init() {
    if (this.want === 'shared') return this.shared('STRATA_APPS_KEYS=shared');
    let me;
    try { me = await this.db.req('GET', '/v1/whoami'); } catch (e) { return this.shared(`GET /v1/whoami failed (${e.message})`); }
    const auth = me.auth || {};
    if (auth.open) return this.shared('ChloeDB here is open (no token, no key), so it makes no keys; start it with --token to give each app its own');
    if (!auth.own_keys) return this.shared('this chloedb-server is from before keys a key makes for its own (/v1/keys/own); upgrade it');
    const tables = me.tables || null;
    if (tables) {
      const admin = Object.entries(tables).filter(([, s]) => s === 'admin').map(([t]) => t);
      if (!admin.some((t) => t === '*' || 'app_x'.startsWith(t.replace(/\*$/, '')))) {
        return this.shared(`the engine's key ${JSON.stringify(me.name)} has no admin scope on app_* (${JSON.stringify(tables)}), so it cannot make its apps' keys`);
      }
      this.engineKey = { name: me.name, tables };
    } else if (me.scope === 'admin') {
      // An admin credential: traded for a key limited to the engine's tables.
      try {
        const secret = await this.engineSecret();
        const was = me.key ? `the key ${JSON.stringify(me.name)}, which reaches every table` : this.ownServer ? 'the token of the server it started' : "the cluster's admin token";
        this.db.useToken(secret);
        // A server that filters its feed by a limited key's tables lets the
        // engine follow it with its own key, and drop the admin credential.
        if (auth.limited_feed) this.db.feedToken = secret;
        const now = await this.db.req('GET', '/v1/whoami');
        this.engineKey = { name: now.name, tables: now.tables };
        const feed = auth.limited_feed ? 'follows the feed of changes with it too, and holds the admin credential no longer' : 'keeps the admin credential for the feed of changes alone (this chloedb-server is from before a limited key could read the feed)';
        this.log(`the engine was given ${was}: it works with a key of its own, ${JSON.stringify(now.name)}, limited to ${Object.keys(ENGINE_GRANTS).join(' and ')} (kept sealed in sa_settings), and ${feed}.${this.ownServer ? '' : ' Give it a limited key instead (the operator does).'}`);
      } catch (e) {
        return this.shared(`the engine holds an admin credential and could not make a key of its own limited to sa_* and app_* (${e.message})`);
      }
    } else {
      return this.shared(`the engine's credential ${JSON.stringify(me.name)} has the ${me.scope} scope, which makes no keys`);
    }
    this.mode = 'per-app';
    this.why = `each app has a key of its own, made by ${JSON.stringify(this.engineKey.name)}`;
    this.log(`app keys on: ${this.why}`);
  }

  // The engine's own key, from sa_settings, or made with the admin
  // credential the engine holds now.
  async engineSecret() {
    const kept = await this.opened(ENGINE);
    if (kept) {
      const probe = new Strata(this.db.url, kept);
      const me = await probe.req('GET', '/v1/whoami').catch(() => null);
      if (me && me.key && me.tables) return kept;
    }
    const body = { name: ENGINE_KEY_NAME, tables: ENGINE_GRANTS, owner: 'chloedb-apps', note: 'the app engine\'s key, made by the engine from the admin credential it was given' };
    let made;
    try {
      made = await this.db.req('POST', '/v1/admin/keys', body);
    } catch (e) {
      if (e.status !== 409) throw e;
      // One of that name from an earlier start whose secret is lost.
      const listed = await this.db.req('GET', '/v1/admin/keys');
      for (const k of listed.keys || []) if (k.name === ENGINE_KEY_NAME && !k.revoked_ms) await this.db.req('DELETE', `/v1/admin/keys/${k.id}`);
      made = await this.db.req('POST', '/v1/admin/keys', body);
    }
    await this.keep(ENGINE, made.secret);
    return made.secret;
  }

  async opened(app) {
    const v = await this.store.setting(app, 'key').catch(() => null);
    if (!v) return null;
    try { return this.box.open(JSON.parse(v)); } catch { return null; }
  }

  keep(app, secret) {
    return this.store.putSetting(app, 'key', JSON.stringify(this.box.seal(secret)));
  }

  // The client app `id`'s code reads and writes with: its own, or with
  // "shared", the engine's.
  async client(id) {
    if (this.mode !== 'per-app' || !APP_ID.test(String(id))) return this.db;
    const have = this.clients.get(id);
    if (have) return have;
    if (!this.making.has(id)) {
      const p = (async () => {
        const secret = (await this.opened(id)) || (await this.make(id));
        const c = new AppClient(this.db.url, secret, () => this.renew(id));
        this.clients.set(id, c);
        return c;
      })().finally(() => this.making.delete(id));
      this.making.set(id, p);
    }
    return this.making.get(id);
  }

  async make(id) {
    const made = await this.db.req('POST', '/v1/keys/own', { name: `app-${id}`, tables: appGrants(id), note: `the key of app ${id} of the app engine` });
    this.made.set(id, Date.now());
    await this.keep(id, made.secret);
    return made.secret;
  }

  // A key the server refused: just made (another coordinator has not had
  // it yet), so wait for it; else made again, at most once a minute.
  async renew(id) {
    const c = this.clients.get(id);
    const at = this.made.get(id) || 0;
    if (Date.now() - at < 10_000) {
      await new Promise((r) => setTimeout(r, 1000));
      return c.token;
    }
    if (Date.now() - at < 60_000) throw Object.assign(new Error(`ChloeDB refuses app ${id}'s key, made again less than a minute ago`), { status: 401 });
    this.log(`ChloeDB no longer takes app ${id}'s key: making it again`);
    return this.make(id);
  }
}
