// Server functions run here: QuickJS compiled to WebAssembly, one fresh
// interpreter per call, with a memory limit, a deadline, no network, no files
// and no Node APIs. The only way out is `app`, whose calls the host answers
// (and checks) through one asyncified function, so the guest's code reads
// like ordinary synchronous JavaScript.

import { newAsyncContext, newQuickJSWASMModule, shouldInterruptAfterDeadline } from 'quickjs-emscripten';

const PRELUDE = `
const __call = (m, a) => { const r = JSON.parse(__host(m, JSON.stringify(a === undefined ? [] : a))); if (r.err !== undefined) throw new Error(r.err); return r.ok; };
const __fmt = (a) => a.map(v => typeof v === 'string' ? v : JSON.stringify(v)).join(' ');
const console = { log: (...a) => __log(__fmt(a)), info: (...a) => __log(__fmt(a)), warn: (...a) => __log(__fmt(a)), error: (...a) => __log(__fmt(a)) };
const app = Object.freeze({
  sql: (sql, params) => __call('sql', [sql, params || {}]),
  insert: (table, doc) => __call('insert', [table, doc || {}]),
  update: (table, id, fields) => __call('update', [table, id, fields || {}]),
  remove: (table, id) => __call('remove', [table, id]),
  call: (name, input) => __call('call', [name, input === undefined ? null : input]),
  user: __user,
  me: () => __user,
  people: () => __call('people', []),
  email: (msg) => __call('email', [msg || {}]),
  geocode: (q) => __call('geocode', [String(q || '')]),
  files: (ids) => __call('files', [[].concat(ids || [])]),
  ask: (prompt, opts) => __call('ask', [prompt, opts || {}]),
  fetch: (url, opts) => __call('fetch', [String(url || ''), opts || {}]),
  now: () => new Date().toISOString(),
  log: (...a) => __log(__fmt(a)),
});
`;

function errorText(dumped) {
  if (dumped && typeof dumped === 'object') {
    const msg = dumped.message || dumped.name || JSON.stringify(dumped);
    return dumped.stack ? `${msg}\n${String(dumped.stack).split('\n').slice(0, 3).join('\n')}` : msg;
  }
  return String(dumped);
}

// Checks that `code` parses in QuickJS and defines run(), without running it.
let syncModule = null;
export async function checkFunction(code) {
  const problems = [];
  if (/\basync\s+function\s+run\b|\brun\s*=\s*async\b/.test(code)) problems.push('run must be a plain synchronous function: app calls return their results directly, so do not use async or await');
  if (/\bawait\b/.test(code)) problems.push('do not use await: app.sql, app.insert and the other app calls return their results directly');
  if (code.length > 64 * 1024) problems.push('the code is longer than 64 KiB');
  syncModule ||= await newQuickJSWASMModule();
  const vm = syncModule.newContext();
  try {
    vm.runtime.setInterruptHandler(shouldInterruptAfterDeadline(Date.now() + 1000));
    // Compile only: wrap in a function that is never called.
    const res = vm.evalCode(`(function () {\n${code}\n;return typeof run;\n})`, 'check.js');
    if (res.error) { problems.push('syntax: ' + errorText(vm.dump(res.error))); res.error.dispose(); }
    else res.value.dispose();
  } finally {
    vm.dispose();
  }
  if (!/\bfunction\s+run\s*\(|\brun\s*=\s*(function|\()/.test(code)) problems.push('the code must define function run(input, app)');
  return problems;
}

// Works out one expression (a screen's PARAMS `= ...`, as the build check
// guesses it before the screen ever runs) in a fresh QuickJS interpreter:
// no Node, no host functions, a deadline and a memory limit. `scope` is
// plain data (JSON); `state`, `today`, `params` and `app.today/now/me` are
// what the screen's first state offers, and `lib.<anything>` is a function
// that gives null. The answer comes back as JSON, or undefined when the
// expression fails, runs out of time or gives something JSON cannot hold.
// (Audit 4, X-G1: this used to be `new Function` in the engine's own process.)
export async function evalExpression(expr, scope = {}, { timeoutMs = 200, memoryBytes = 16 * 1024 * 1024 } = {}) {
  if (typeof expr !== 'string' || expr.length > 4096) return undefined;
  syncModule ||= await newQuickJSWASMModule();
  const vm = syncModule.newContext();
  try {
    vm.runtime.setMemoryLimit(memoryBytes);
    vm.runtime.setMaxStackSize(256 * 1024);
    vm.runtime.setInterruptHandler(shouldInterruptAfterDeadline(Date.now() + timeoutMs));
    const today = String(scope.today || new Date().toISOString().slice(0, 10));
    const program = `(function () {
const __s = JSON.parse(${JSON.stringify(JSON.stringify({ state: scope.state ?? {}, params: scope.params ?? {}, today }))});
const nothing = new Proxy(function () { return null; }, { get: () => nothing, apply: () => null });
const s = { state: __s.state, params: __s.params, today: __s.today, lib: nothing,
  app: { today: () => __s.today, now: () => new Date().toISOString(), me: () => null } };
const __v = (function (s) { with (s) { return (${expr}\n); } })(s);
if (__v instanceof Promise || typeof __v === 'function') return undefined;
return JSON.stringify(__v === undefined ? null : __v);
})()`;
    const res = vm.evalCode(program, 'param.js');
    if (res.error) { res.error.dispose(); return undefined; }
    const out = vm.dump(res.value);
    res.value.dispose();
    if (typeof out !== 'string') return undefined;
    return JSON.parse(out);
  } catch {
    return undefined;
  } finally {
    vm.dispose();
  }
}

// Runs `code`'s run(input, app). `host(method, args)` answers app calls.
// `helpers` are the shared helpers the app pins, available as lib.<name>.
// `user` is the signed-in person whose action led to the call, or null.
// Waiting for a model or a web page (app.ask, app.fetch) does not count in
// the function's time, up to waitMs in all.
const WAITS = new Set(['ask', 'fetch']);
export async function runFunction(code, input, host, { timeoutMs = 10000, waitMs = 180000, memoryBytes = 64 * 1024 * 1024, helpers = [], user = null } = {}) {
  const vm = await newAsyncContext();
  const logs = [];
  const started = Date.now();
  let waited = 0;
  // Refusals (a service switched off, a limit reached) stay refusals when the
  // function lets them through: an answer for the person, not a bug.
  const refusals = new Map(); // message -> its status (a 401 "sign in first" stays one)
  const used = () => Date.now() - started - waited;
  try {
    vm.runtime.setMemoryLimit(memoryBytes);
    vm.runtime.setMaxStackSize(1024 * 1024);
    vm.runtime.setInterruptHandler(() => used() > timeoutMs);
    const hostFn = vm.newAsyncifiedFunction('__host', async (mH, aH) => {
      const method = vm.getString(mH);
      const args = JSON.parse(vm.getString(aH));
      const t0 = Date.now();
      try {
        if (used() > timeoutMs) throw new Error('the function ran out of time');
        if (WAITS.has(method) && waited >= waitMs) throw new Error(`the function waited more than ${Math.round(waitMs / 1000)} s for app.ask and app.fetch`);
        const out = await host(method, args);
        return vm.newString(JSON.stringify({ ok: out === undefined ? null : out }));
      } catch (e) {
        if (e && e.refused) refusals.set(String(e.message), e.status || null);
        return vm.newString(JSON.stringify({ err: String(e && e.message || e) }));
      } finally {
        if (WAITS.has(method)) waited += Date.now() - t0;
      }
    });
    vm.setProp(vm.global, '__host', hostFn);
    hostFn.dispose();
    const logFn = vm.newFunction('__log', (h) => { if (logs.length < 200) logs.push(vm.getString(h)); });
    vm.setProp(vm.global, '__log', logFn);
    logFn.dispose();
    const arg = JSON.stringify(input === undefined ? null : input);
    const libs = helpers.map((x) => `lib[${JSON.stringify(x.name)}] = (function () {\n${x.code}\n;return helper();\n})();`).join('\n');
    const who = user ? JSON.stringify({ id: user.id, name: String(user.name || '').trim() || user.email || 'you', email: user.email, role: user.role }) : 'null';
    const program = `const __user = Object.freeze(${who});\n${PRELUDE}\nconst lib = {};\n${libs}\n${code}\n;globalThis.__last = run(${arg}, app);\nJSON.stringify({ result: globalThis.__last === undefined ? null : globalThis.__last });`;
    const res = await vm.evalCodeAsync(program, 'capability.js');
    if (res.error) {
      const text = errorText(vm.dump(res.error));
      res.error.dispose();
      const err = new Error(text);
      err.logs = logs;
      const hit = [...refusals].find(([m]) => text.includes(m));
      if (hit) { err.refused = true; if (hit[1]) err.status = hit[1]; }
      throw err;
    }
    const out = JSON.parse(vm.getString(res.value));
    res.value.dispose();
    return { result: out.result, logs, ms: Date.now() - started };
  } finally {
    vm.dispose();
  }
}
