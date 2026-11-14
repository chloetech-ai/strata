// The model behind the forge. It runs Claude through the Claude Code CLI
// (`claude -p`), so it uses whatever sign-in Claude Code has on this machine
// (claude.ai login or ANTHROPIC_API_KEY) and needs no key of its own. Tools,
// settings, plugins and MCP servers are switched off: the model only answers.

import { spawn } from 'node:child_process';
import { readFile, writeFile, rename, mkdtemp, mkdir, rm } from 'node:fs/promises';
import { AsyncLocalStorage } from 'node:async_hooks';
import os from 'node:os';
import path from 'node:path';

export const MODEL = process.env.STRATA_APPS_MODEL || 'claude-opus-5-5';
const CLAUDE = process.env.STRATA_APPS_CLAUDE || 'claude';

// Which CLI answers: claude (Claude Code, the default), codex (OpenAI's
// Codex CLI, `codex exec`) or agy (Google's Antigravity CLI, not usable
// here yet: see askAgy). STRATA_APPS_PROVIDER picks it for the engine; a
// build can name its own (withProvider, from the `provider` field of
// POST /api/apps and POST /api/apps/:id/requests), for its model calls only.
export const PROVIDERS = ['claude', 'codex', 'agy'];
const CODEX = process.env.STRATA_APPS_CODEX || 'codex';
const AGY = process.env.STRATA_APPS_AGY || 'agy';
// The model of each CLI; unset, the CLI's own default. The model names the
// forge passes (claude-...) are Claude's and go to claude only.
export const CODEX_MODEL = process.env.STRATA_APPS_CODEX_MODEL || null;
export const AGY_MODEL = process.env.STRATA_APPS_AGY_MODEL || null;

// A provider named by a request or the environment, checked: null for none.
export function checkProvider(p) {
  if (p === undefined || p === null || p === '') return null;
  const v = String(p).trim().toLowerCase();
  if (!PROVIDERS.includes(v)) throw Object.assign(new Error(`provider is one of ${PROVIDERS.join(', ')}`), { status: 400 });
  if (v === 'codex' && !codexOn()) throw Object.assign(new Error(CODEX_OFF), { status: 400 });
  return v;
}
// Codex is off unless whoever runs the engine turns it on: it answers with
// the host user's own Codex sign-in, which the engine cannot scope (audit 4,
// X-G6). STRATA_APPS_CODEX_ON=1, or STRATA_APPS_PROVIDER=codex, turns it on.
export const codexOn = () => process.env.STRATA_APPS_CODEX_ON === '1' || String(process.env.STRATA_APPS_PROVIDER || '').trim().toLowerCase() === 'codex';
export const CODEX_OFF = 'the codex provider is off on this engine: whoever runs it turns it on with STRATA_APPS_CODEX_ON=1 (it answers with that machine\'s own Codex sign-in)';
export const PROVIDER = checkProvider(process.env.STRATA_APPS_PROVIDER) || 'claude';
const chosen = new AsyncLocalStorage();
// fn's model calls (and what they set off) go to `provider`; none keeps the engine's.
export function withProvider(provider, fn) {
  const p = checkProvider(provider);
  return p ? chosen.run(p, fn) : fn();
}
export const currentProvider = () => chosen.getStore() || PROVIDER;
// The model a provider is asked for: Claude's names only to claude.
export function modelFor(provider, model) {
  if (provider === 'claude') return model || MODEL;
  const own = provider === 'codex' ? CODEX_MODEL : AGY_MODEL;
  return model && !/^claude/i.test(model) ? model : own;
}

// A cap on what the model may cost here, for a studio other people can
// reach: STRATA_APPS_SPEND_CAP_USD, in US dollars, counted from the cost
// Claude Code reports for each answer and kept in a file (useSpendFile) so a
// restart does not reset it. Once the total reaches the cap every call is
// refused until the cap is raised. Calls already running finish, so the total
// can pass the cap by what they cost; a call stopped before its answer is not
// counted.
//
// Apps' own model calls (app.ask, which a published app's visitors set off)
// count against a share of the cap of their own, `asksCap`
// (STRATA_APPS_ASK_CAP_USD, by default a quarter of the cap), and builds
// against the rest, so that traffic to apps never stops the studio's builds,
// nor builds the apps (audit N-G3).
const CAP = Number(process.env.STRATA_APPS_SPEND_CAP_USD) || 0;
const ASKS_CAP = process.env.STRATA_APPS_ASK_CAP_USD !== undefined && process.env.STRATA_APPS_ASK_CAP_USD !== '' ? Math.max(0, Number(process.env.STRATA_APPS_ASK_CAP_USD) || 0) : CAP / 4;
export const spend = { cap: CAP, asksCap: CAP ? Math.min(ASKS_CAP, CAP) : ASKS_CAP, usd: 0, asksUsd: 0, calls: 0, refused: 0, since: new Date().toISOString(), file: null };
let saving = Promise.resolve();
export async function useSpendFile(file) {
  spend.file = file;
  try {
    const j = JSON.parse(await readFile(file, 'utf8'));
    if (Number.isFinite(j.usd)) Object.assign(spend, { usd: j.usd, asksUsd: Number(j.asksUsd) || 0, calls: j.calls || 0, refused: j.refused || 0, since: j.since || spend.since });
  } catch { /* first start */ }
  return spend;
}
function saveSpend() {
  if (!spend.file) return;
  const { usd, asksUsd, calls, refused, since, cap, asksCap } = spend;
  saving = saving.then(() => writeFile(`${spend.file}.tmp`, JSON.stringify({ usd, asksUsd, calls, refused, since, cap, asksCap }))
    .then(() => rename(`${spend.file}.tmp`, spend.file))).catch(() => {});
}
// Why a call of `bucket` ('build': the studio's own; 'asks': an app's
// app.ask) may not run now, if it may not.
export function overCap(bucket = 'build') {
  const refuse = (text) => {
    spend.refused++;
    saveSpend();
    return Object.assign(new Error(text), { status: 402, refused: true });
  };
  if (bucket === 'asks') {
    if (spend.asksCap && spend.asksUsd >= spend.asksCap) return refuse(`this studio's apps have reached their share of its spending limit for the model ($${spend.asksUsd.toFixed(2)} of $${spend.asksCap}); the studio's builds go on; ask whoever runs it to raise STRATA_APPS_ASK_CAP_USD`);
    if (spend.cap && spend.usd >= spend.cap) return refuse(`this studio has reached its spending limit for the model ($${spend.usd.toFixed(2)} of $${spend.cap}); ask whoever shared it to raise the limit`);
    return null;
  }
  if (!spend.cap) return null;
  const builds = spend.usd - spend.asksUsd;
  const share = spend.cap - spend.asksCap;
  if (builds < share) return null;
  return refuse(`this studio has reached its spending limit for the model ($${builds.toFixed(2)} of $${share} for builds${spend.asksCap ? ', the rest kept for apps\' own calls' : ''}); ask whoever shared it to raise the limit`);
}

function counted(cost, bucket) {
  spend.usd += cost;
  if (bucket === 'asks') spend.asksUsd += cost;
  spend.calls++;
  saveSpend();
}

// How long one answer may take. A build is stopped when the model goes
// quiet (nothing new from it, not even a line of thinking, for IDLE seconds),
// runs past the hard cap, or its owner stops it (buildScope), so a big app
// that is still being written is not cut off, and one that says nothing does
// not hold its app for minutes.
export const IDLE_S = Number(process.env.STRATA_APPS_IDLE_S) || 90;
export const TIMEOUT_S = Number(process.env.STRATA_APPS_TIMEOUT_S) || 1200;

// The build a call belongs to, as the server runs it: buildScope.run({signal},
// fn). Every model call made inside it (the plan, each piece, each fix
// round) stops at once when the signal aborts, with an error marked stopped.
export const buildScope = new AsyncLocalStorage();
export const stoppedError = () => Object.assign(new Error('stopped by its owner'), { stopped: true });

// The environment a model's CLI starts with: what it needs to run and sign
// in, not the engine's own (audit 4, X-G6, and audit 1's A5). The engine's
// environment holds the cluster's tokens (STRATA_TOKEN, STRATA_APP_TOKEN),
// the sealing key's path and every other secret of the machine; none of that
// is the model's. Kept: the process basics (PATH, HOME, the user, locale,
// the temporary folder), the network's proxy and certificates, and each
// CLI's own sign-in and settings (ANTHROPIC_* and CLAUDE_* for claude,
// OPENAI_* and CODEX_* for codex). STRATA_APPS_CHILD_ENV names more
// variables to keep, separated by commas.
const BASE_ENV = ['PATH', 'HOME', 'USER', 'LOGNAME', 'SHELL', 'LANG', 'LANGUAGE', 'TZ', 'TERM', 'TMPDIR', 'TMP', 'TEMP',
  'XDG_CONFIG_HOME', 'XDG_DATA_HOME', 'XDG_CACHE_HOME', 'XDG_STATE_HOME', 'XDG_RUNTIME_DIR',
  'HTTPS_PROXY', 'HTTP_PROXY', 'NO_PROXY', 'ALL_PROXY', 'https_proxy', 'http_proxy', 'no_proxy', 'all_proxy',
  'SSL_CERT_FILE', 'SSL_CERT_DIR', 'NODE_EXTRA_CA_CERTS', 'REQUESTS_CA_BUNDLE', 'CURL_CA_BUNDLE',
  'SYSTEMROOT', 'APPDATA', 'LOCALAPPDATA', 'USERPROFILE'];
const OWN_ENV = { claude: /^(ANTHROPIC_|CLAUDE_|LC_)/, codex: /^(OPENAI_|CODEX_|AZURE_OPENAI_|LC_)/ };
// Claude through Bedrock or Vertex signs in with the cloud's own variables,
// kept only when Claude Code is told to use that cloud.
const CLOUD_ENV = [['CLAUDE_CODE_USE_BEDROCK', /^AWS_/], ['CLAUDE_CODE_USE_VERTEX', /^(GOOGLE_|GCLOUD_|CLOUDSDK_|CLOUD_ML_|VERTEX_)/]];
export function childEnv(provider, from = process.env) {
  const keep = new Set([...BASE_ENV, ...String(from.STRATA_APPS_CHILD_ENV || '').split(',').map((x) => x.trim()).filter(Boolean)]);
  const own = OWN_ENV[provider] || OWN_ENV.claude;
  const env = {};
  const clouds = provider === 'claude' ? CLOUD_ENV.filter(([flag]) => from[flag] && from[flag] !== '0').map(([, re]) => re) : [];
  for (const [k, v] of Object.entries(from)) if (v !== undefined && (keep.has(k) || own.test(k) || clouds.some((re) => re.test(k)))) env[k] = v;
  // ChloeCtx's hooks (and Engram's, from before the rename) stay out of it.
  env.CHLOECTX_DISABLE = '1';
  env.ENGRAM_DISABLE = '1';
  if (provider === 'claude') env.CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC = '1';
  return env;
}

// What a Codex answer cost, from the tokens it reports: Codex reports no
// dollars. Prices in US dollars per million tokens, from
// STRATA_APPS_CODEX_USD_PER_MTOK as "input,cached input,output" (default
// 1.25,0.125,10, GPT-5's list prices); with no usage reported, from the
// lengths of the prompt and the answer at four characters a token, so that
// an answer never counts as free (audit 4, X-G6).
export function codexCost(usage, { promptChars = 0, answerChars = 0 } = {}, prices = process.env.STRATA_APPS_CODEX_USD_PER_MTOK) {
  const [pin, pcached, pout] = String(prices || '1.25,0.125,10').split(',').map((x) => Math.max(0, Number(x) || 0));
  const u = usage || {};
  let input = Number(u.input_tokens) || 0;
  const cached = Math.min(input, Number(u.cached_input_tokens) || 0);
  let output = (Number(u.output_tokens) || 0) + (Number(u.reasoning_output_tokens) || 0);
  if (!input && !output) { input = Math.ceil(promptChars / 4); output = Math.ceil(answerChars / 4); }
  return ((input - cached) * pin + cached * (pcached ?? pin) + output * (pout ?? pin)) / 1e6;
}

// onWrite(progress, text) is called as the answer streams in, with
// {chars, thinking}: characters of answer and of thinking so far, and the
// answer so far. images: [{data (base64), type ('image/png')}] go before the
// prompt, in one message.
// tools: built-in tools the model may use (WebSearch, WebFetch for an app's
// web ask); none by default. maxBudgetUsd stops a call that costs more.
// bucket: 'asks' for an app's own call (app.ask), counted apart (overCap).
// provider: claude, codex or agy; by default the build's (withProvider) or
// STRATA_APPS_PROVIDER. Claude reports what an answer cost; Codex's cost is
// worked out from its tokens (codexCost, out.costReported is false).
export function ask(opts) {
  let provider;
  try { provider = checkProvider(opts.provider) || currentProvider(); } catch (e) { return Promise.reject(e); }
  if (provider === 'codex') return askCodex(opts);
  if (provider === 'agy') return askAgy(opts);
  return askClaude(opts);
}

// The arguments of `claude -p` for one answer.
export function claudeArgs({ system, model = MODEL, effort, tools = [], maxBudgetUsd, images = [] }) {
  const args = ['-p', '--output-format', 'stream-json', '--verbose', '--include-partial-messages', '--model', model, '--system-prompt', system,
    '--tools', tools.join(','), '--setting-sources', '', '--strict-mcp-config', '--disable-slash-commands', '--no-session-persistence'];
  if (tools.length) args.push('--allowedTools', tools.join(','));
  if (maxBudgetUsd) args.push('--max-budget-usd', String(maxBudgetUsd));
  if (effort) args.push('--effort', effort);
  if (images.length) args.push('--input-format', 'stream-json');
  return args;
}

function askClaude({ system, prompt, images = [], model = MODEL, effort = process.env.STRATA_APPS_EFFORT, idleMs = IDLE_S * 1000, timeoutMs = TIMEOUT_S * 1000, onWrite, bin = CLAUDE, tools = [], maxBudgetUsd, bucket = 'build' }) {
  const over = overCap(bucket);
  if (over) return Promise.reject(over);
  const signal = buildScope.getStore()?.signal;
  if (signal?.aborted) return Promise.reject(stoppedError());
  return new Promise((resolve, reject) => {
    const args = claudeArgs({ system, model, effort, tools, maxBudgetUsd, images });
    const env = childEnv('claude');
    const child = spawn(bin, args, { env, stdio: ['pipe', 'pipe', 'pipe'], cwd: process.env.TMPDIR || '/tmp' });
    let buf = '';
    let err = '';
    let result = null;
    let text = '';
    const seen = { chars: 0, thinking: 0 };
    let settled = false;
    const onStop = () => fail(stoppedError());
    const fail = (e) => { if (settled) return; settled = true; clearTimeout(cap); clearTimeout(idle); signal?.removeEventListener('abort', onStop); child.kill('SIGKILL'); reject(e); };
    signal?.addEventListener('abort', onStop, { once: true });
    const cap = setTimeout(() => fail(new Error(`the model took longer than ${Math.round(timeoutMs / 1000)} s`)), timeoutMs);
    let idle;
    const quiet = () => {
      clearTimeout(idle);
      idle = setTimeout(() => fail(new Error(`the model wrote nothing for ${Math.round(idleMs / 1000)} s (${seen.chars} characters written)`)), idleMs);
    };
    quiet();
    const line = (l) => {
      if (!l.trim()) return;
      let j;
      try { j = JSON.parse(l); } catch { return; }
      quiet();
      if (j.type === 'result') { result = j; return; }
      if (j.type !== 'stream_event') return;
      const d = j.event?.type === 'content_block_delta' ? j.event.delta : null;
      if (!d) return;
      if (d.type === 'text_delta') { text += d.text; seen.chars += d.text.length; } else if (d.type === 'thinking_delta') seen.thinking += (d.thinking || '').length;
      else return;
      onWrite?.(seen, text);
    };
    child.stdout.on('data', (d) => {
      buf += d;
      let i;
      while ((i = buf.indexOf('\n')) >= 0) { line(buf.slice(0, i)); buf = buf.slice(i + 1); }
    });
    child.stderr.on('data', (d) => { err += d; });
    child.on('error', (e) => fail(new Error(`could not run ${bin}: ${e.message}. Install Claude Code and sign in, or set STRATA_APPS_CLAUDE`)));
    child.on('close', (code) => {
      line(buf);
      if (settled) return;
      settled = true;
      clearTimeout(cap);
      clearTimeout(idle);
      signal?.removeEventListener('abort', onStop);
      const data = result;
      if (!data) return reject(new Error(`claude exited ${code}: ${(err || buf || text).slice(0, 600)}`));
      if (data.is_error || data.subtype !== 'success' && data.subtype) {
        if (data.total_cost_usd) counted(data.total_cost_usd, bucket);
        return reject(new Error(`claude: ${data.result || data.terminal_reason || data.subtype}`));
      }
      const out = { text: data.result || text, usage: data.usage || {}, cost: data.total_cost_usd || 0, ms: data.duration_ms || 0, model };
      counted(out.cost, bucket);
      // STRATA_APPS_DUMP=dir keeps every answer there, to study what the model writes.
      if (process.env.STRATA_APPS_DUMP) {
        import('node:fs').then((fs) => fs.promises.mkdir(process.env.STRATA_APPS_DUMP, { recursive: true })
          .then(() => fs.promises.writeFile(`${process.env.STRATA_APPS_DUMP}/${Date.now()}-${model}.json`, JSON.stringify({ model, effort: effort || null, ms: out.ms, cost: out.cost, usage: out.usage, thinking: seen.thinking, prompt: prompt.length, text: out.text }))))
          .catch(() => {});
      }
      resolve(out);
    });
    child.stdin.end(!images.length ? prompt : `${JSON.stringify({ type: 'user', message: { role: 'user', content: [
      ...images.map((i) => ({ type: 'image', source: { type: 'base64', media_type: i.type || 'image/png', data: i.data } })),
      { type: 'text', text: prompt }] } })}\n`);
  });
}

// Codex's reasoning efforts; Claude's others (max) are not passed on.
const CODEX_EFFORTS = new Set(['minimal', 'low', 'medium', 'high', 'xhigh']);
// The arguments of `codex exec` for one answer: no tools (no shell, apps,
// browser, computer use, image generation or web search), a read-only
// sandbox in an empty folder, nothing of the user's config.toml (its MCP
// servers, profiles), no rules files and no session kept. The system prompt
// goes in as developer instructions; the prompt is read from stdin ('-');
// events come as JSONL (--json) and the final answer also lands in outFile.
// images: paths of files.
export function codexArgs({ system, model = null, effort, images = [], outFile, cwd }) {
  const args = ['exec', '--json', '--color', 'never', '--sandbox', 'read-only', '--ephemeral', '--ignore-user-config', '--ignore-rules', '--skip-git-repo-check',
    '--disable', 'shell_tool', '--disable', 'apps', '--disable', 'browser_use', '--disable', 'computer_use', '--disable', 'image_generation',
    '-c', 'web_search="disabled"', '-c', 'approval_policy="never"'];
  if (cwd) args.push('--cd', cwd);
  if (outFile) args.push('--output-last-message', outFile);
  if (model) args.push('--model', model);
  if (effort && CODEX_EFFORTS.has(effort)) args.push('-c', `model_reasoning_effort="${effort}"`);
  if (system) args.push('-c', `developer_instructions=${JSON.stringify(String(system))}`);
  for (const f of images) args.push('--image', f);
  args.push('-');
  return args;
}

// What one line of `codex exec --json` says: {text} for an answer, {thinking}
// for reasoning, {usage} at the end of the turn, {error} when it failed.
export function codexEvent(l) {
  let j;
  try { j = JSON.parse(l); } catch { return null; }
  if (j.type === 'item.completed' || j.type === 'item.updated') {
    const it = j.item || {};
    if (it.type === 'agent_message') return { text: String(it.text || ''), done: j.type === 'item.completed' };
    if (it.type === 'reasoning') return { thinking: String(it.text || '') };
    return { other: true };
  }
  if (j.type === 'turn.completed') return { usage: j.usage || {} };
  if (j.type === 'turn.failed') return { error: j.error?.message || 'the turn failed' };
  if (j.type === 'error') return { warning: String(j.message || '') };
  return { other: true };
}

function askCodex({ system, prompt, images = [], model, effort = process.env.STRATA_APPS_EFFORT, idleMs = IDLE_S * 1000, timeoutMs = TIMEOUT_S * 1000, onWrite, bin = CODEX, tools = [], bucket = 'build' }) {
  if (!codexOn()) return Promise.reject(Object.assign(new Error(CODEX_OFF), { status: 400 }));
  if (tools.length) return Promise.reject(new Error(`the codex provider runs without tools; ${tools.join(', ')} needs the claude provider`));
  const over = overCap(bucket);
  if (over) return Promise.reject(over);
  const signal = buildScope.getStore()?.signal;
  if (signal?.aborted) return Promise.reject(stoppedError());
  const use = modelFor('codex', model);
  return (async () => {
    const dir = await mkdtemp(path.join(process.env.TMPDIR || os.tmpdir(), 'sa-codex-'));
    try {
      const files = [];
      for (const [i, im] of images.entries()) {
        const f = path.join(dir, `image-${i}.${/jpe?g/.test(im.type || '') ? 'jpg' : 'png'}`);
        await writeFile(f, Buffer.from(im.data, 'base64'));
        files.push(f);
      }
      const outFile = path.join(dir, 'answer.txt');
      const work = path.join(dir, 'work');
      await mkdir(work);
      const args = codexArgs({ system, model: use, effort, images: files, outFile, cwd: work });
      const t0 = Date.now();
      let out;
      try {
        out = await runCodex(bin, args, prompt, { idleMs, timeoutMs, onWrite, cwd: work, signal });
      } catch (e) {
        // A failed or stopped turn still cost what it used.
        if (e.usage && Object.keys(e.usage).length) counted(codexCost(e.usage), bucket);
        throw e;
      }
      const last = await readFile(outFile, 'utf8').catch(() => '');
      const text = last.trim() ? last : out.text;
      const cost = codexCost(out.usage, { promptChars: String(prompt || '').length + String(system || '').length, answerChars: text.length });
      counted(cost, bucket);
      if (!text.trim()) throw new Error(`codex gave no answer${out.warning ? `: ${out.warning.slice(0, 400)}` : ''}`);
      return { text, usage: out.usage, cost, costReported: false, ms: Date.now() - t0, model: use || 'codex default', provider: 'codex' };
    } finally {
      await rm(dir, { recursive: true, force: true }).catch(() => {});
    }
  })();
}

function runCodex(bin, args, prompt, { idleMs, timeoutMs, onWrite, cwd, signal }) {
  return new Promise((resolve, reject) => {
    const env = childEnv('codex');
    const child = spawn(bin, args, { env, stdio: ['pipe', 'pipe', 'pipe'], cwd });
    let buf = '';
    let err = '';
    let text = '';
    let usage = {};
    let failed = null;
    let warning = '';
    const seen = { chars: 0, thinking: 0 };
    let settled = false;
    // The owner's Stop (buildScope's signal) kills the child at once, as for claude.
    const onStop = () => fail(stoppedError());
    const fail = (e) => { if (settled) return; settled = true; clearTimeout(cap); clearTimeout(idle); signal?.removeEventListener('abort', onStop); child.kill('SIGKILL'); e.usage = usage; reject(e); };
    signal?.addEventListener('abort', onStop, { once: true });
    const cap = setTimeout(() => fail(new Error(`codex took longer than ${Math.round(timeoutMs / 1000)} s`)), timeoutMs);
    let idle;
    const quiet = () => {
      clearTimeout(idle);
      idle = setTimeout(() => fail(new Error(`codex wrote nothing for ${Math.round(idleMs / 1000)} s (${seen.chars} characters written)`)), idleMs);
    };
    quiet();
    const line = (l) => {
      if (!l.trim()) return;
      const e = codexEvent(l);
      if (!e) return;
      quiet();
      if (e.text !== undefined) {
        if (e.done) { text = text ? `${text}\n${e.text}` : e.text; seen.chars = text.length; onWrite?.(seen, text); }
      } else if (e.thinking !== undefined) { seen.thinking += e.thinking.length; onWrite?.(seen, text); }
      else if (e.usage) usage = e.usage;
      else if (e.error) failed = e.error;
      else if (e.warning) warning = e.warning;
    };
    child.stdout.on('data', (d) => {
      buf += d;
      let i;
      while ((i = buf.indexOf('\n')) >= 0) { line(buf.slice(0, i)); buf = buf.slice(i + 1); }
    });
    child.stderr.on('data', (d) => { err += d; if (err.length > 20000) err = err.slice(-10000); });
    child.on('error', (e) => fail(new Error(`could not run ${bin}: ${e.message}. Install the Codex CLI and sign in (codex login), or set STRATA_APPS_CODEX`)));
    child.on('close', (code) => {
      line(buf);
      if (settled) return;
      settled = true;
      clearTimeout(cap);
      clearTimeout(idle);
      signal?.removeEventListener('abort', onStop);
      if (failed) return reject(Object.assign(new Error(`codex: ${failed.slice(0, 600)}`), { usage }));
      if (code !== 0) return reject(Object.assign(new Error(`codex exited ${code}: ${(warning || err || text).slice(0, 600)}`), { usage }));
      resolve({ text, usage, warning });
    });
    child.stdin.end(prompt);
  });
}

// Antigravity's CLI (agy) is not wired: its --help could not be read on the
// machine this was written on (the binary was killed on start), so there is
// no checked way to ask it one prompt and read only the answer. A build that
// names it fails at once with this error rather than guessing at flags.
export const AGY_UNSUPPORTED = 'the agy (Antigravity) provider is not supported yet: no checked non-interactive single-prompt mode; use claude or codex';
function askAgy() {
  return Promise.reject(Object.assign(new Error(AGY_UNSUPPORTED), { status: 400 }));
}

// The model answers with a plan, as JSON inside <plan>...</plan>, and the
// code of each capability in its own <code slug="...">...</code> block, so
// code never has to be escaped inside JSON.
export function parseAnswer(text) {
  let planText = null;
  const p = text.match(/<plan>\s*([\s\S]*?)\s*<\/plan>/);
  if (p) planText = p[1].replace(/^```(?:json)?\s*/, '').replace(/```\s*$/, '');
  else {
    const fence = text.match(/```(?:json)?\s*\n([\s\S]*?)\n```/);
    if (fence) planText = fence[1];
  }
  if (!planText) throw new Error(`the answer held no <plan>: ${text.slice(0, 300)}`);
  let plan;
  try { plan = JSON.parse(planText); } catch (e) { throw new Error(`the plan is not valid JSON (${e.message}): ${planText.slice(0, 300)}`); }
  const code = {};
  for (const m of text.matchAll(/<code\s+slug="([^"]+)"\s*>\n?([\s\S]*?)\n?<\/code>/g)) code[m[1]] = m[2];
  return { plan, code };
}

export function extractJson(text) {
  const fence = text.match(/```(?:json)?\s*\n([\s\S]*?)\n```/);
  const candidates = [];
  if (fence) candidates.push(fence[1]);
  const a = text.indexOf('{');
  const b = text.lastIndexOf('}');
  if (a >= 0 && b > a) candidates.push(text.slice(a, b + 1));
  for (const c of candidates) {
    try { return JSON.parse(c); } catch { /* next */ }
  }
  throw new Error(`the model's answer held no JSON object: ${text.slice(0, 300)}`);
}
