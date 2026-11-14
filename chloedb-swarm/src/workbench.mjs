#!/usr/bin/env node
// A persona run for ChloeDB Workbench's Preview lab (chloedb-workbench's
// SPEC.md, "Fourth build", use case 2): one simulated person doing one task
// in the app an agent is previewing, through Workbench's preview proxy, in
// Chromium (the browser surface of this package). Workbench starts it, one
// process per run, and reads what it does as lines of JSON on stdout:
//
//   {"type":"start","adapter":"claude -p"}
//   {"type":"step","step":1,"action":"click","input":{"ref":"e12","name":"Pay"},"thought":"…","ok":true,"url":"…","events":[…],"ms":812}
//   {"type":"finding","kind":"bug","severity":3,"title":"…","detail":"…","step":4,"url":"…"}
//   {"type":"done","outcome":"done"|"gave_up"|"steps"|"time","answer":"…","cost_usd":0.03}
//   {"type":"error","message":"…"}
//
// The job comes as one JSON object on stdin: { url (the preview, such as
// http://127.0.0.1:7777/preview/<agent>), token (Workbench's, traded once
// for the preview's cookie), persona, task, model, max_steps, max_seconds,
// claude_bin, workdir }.
//
// `--check --claude BIN` prints {ok, why, adapter} and exits: whether a
// run could start here (Playwright and Chromium, and a model).
//
// The model: the Anthropic API when CHLOEDB_SWARM_ANTHROPIC_KEY or
// ANTHROPIC_API_KEY is set and the SDK is installed, else `claude -p` on
// the user's own login. Each step is one stateless call that sees the
// persona, the task, the steps so far and the page, and answers one action.
// The model's module and the SDK are imported only when the API is used,
// so this file runs without `npm install`.

import { spawn, spawnSync } from 'node:child_process';
import { existsSync, realpathSync } from 'node:fs';
import { pathToFileURL } from 'node:url';
import { BrowserSurface, playwright, closeBrowser } from './surfaces/browser.mjs';

const ACTIONS = ['look', 'click', 'type', 'choose', 'press', 'scroll', 'back', 'go', 'wait', 'finish'];

/** The JSON schema of one answer of the model: a single action. */
export const ACTION_SCHEMA = {
  type: 'object',
  properties: {
    note: { type: 'string', description: 'What you are doing now, in a few words, as you would say it aloud.' },
    action: { type: 'string', enum: ACTIONS },
    ref: { type: 'string', description: 'click, type, choose: the element\'s reference from the page, such as e12.' },
    text: { type: 'string', description: 'type: the text.' },
    submit: { type: 'boolean', description: 'type: press Enter afterwards.' },
    option: { type: 'string', description: 'choose: the option\'s label.' },
    key: { type: 'string', description: 'press: a key or combination, such as Enter, Tab or Escape.' },
    direction: { type: 'string', enum: ['up', 'down'] },
    path: { type: 'string', description: 'go: a path of this site, such as /cart.' },
    seconds: { type: 'number' },
    outcome: { type: 'string', enum: ['done', 'gave_up'], description: 'finish: whether you did the task.' },
    answer: { type: 'string', description: 'finish: your answer to the task, or what stopped you.' },
  },
  required: ['note', 'action'],
};

/** The JSON schema of the review's answer. */
export const REVIEW_SCHEMA = {
  type: 'object',
  properties: {
    findings: {
      type: 'array',
      items: {
        type: 'object',
        properties: {
          kind: { type: 'string', enum: ['bug', 'confusing', 'accessibility', 'slow', 'copy'] },
          severity: { type: 'integer', minimum: 1, maximum: 3, description: '3: it stopped the task; 2: it cost real effort or trust; 1: a nuisance.' },
          title: { type: 'string', description: 'The problem in one short sentence, as a ticket title.' },
          detail: { type: 'string', description: 'What happened, at which step, and what the person expected instead.' },
          step: { type: 'integer' },
        },
        required: ['kind', 'severity', 'title', 'detail'],
      },
    },
  },
  required: ['findings'],
};

const RULES = `You are taking part in a usability test of a web app that is being built. You play one person, described below, doing one task in it, in a browser. The developers will read what you did, step by step, to find what works and what does not. Nobody answers questions.

How to play:
- Be this person: their knowledge, habits, patience and vocabulary, and not more. If they would give up after a few failed tries, give up.
- Each answer is exactly one action on the page you are shown, with a "note": what you are doing, in a few words, as this person would say it aloud.
- Act only on elements of the page as shown, by their reference (such as e12). Do not invent pages, elements or results you have not seen.
- Do the task's main action (Pay, Submit, Send, Save: whatever the task is for) before you finish, once, as this person would. A problem you notice on the way is not a reason to stop: say it in your note and carry on. Give up only when the page truly will not let you go on.
- Before you call a price, total or count wrong, check your arithmetic against everything the page shows: a total can include tax, shipping, a fee or a discount listed elsewhere, and a percentage can apply before or after tax. Read the whole summary first. If it still does not add up, say exactly which numbers the page shows and which you expected, not a guess at the cause.
- When you are done, answer action "finish" with outcome "done" and your answer to the task in your own words. If you are stuck and this person would stop, answer "finish" with outcome "gave_up" and what stopped you.
- Use the app as this person would: no guessing internal URLs, no unrelated sites.`;

const REVIEW = `You review one session of a usability test of a web app being built. You get the person, the task, every step they took with their notes, what the browser showed (errors, failed requests) and how it ended. List the problems the developers should fix: each one that got in the person's way, a bug the browser showed, or a place where the app did not do what a person would expect. Merge repeats of the same problem into one. Leave out what is only the person's own slip with nothing the app could do better. No problems: an empty list.
- A wrong number is a bug only when you checked it against every amount the page showed (tax, shipping, fees, discounts) and it still does not add up; then give the numbers. A number that only looked wrong because a line was missing or unlabelled (tax not shown) is "confusing", severity 1 or 2, not a bug.
- Severity 3 only when the person could not finish the task because of the app, not because they misread it.
- If the person never did the task's main action (pressed Pay, submitted the form), say so in one finding ("Pay was never pressed") with why, so nobody takes it as tested.`;

/** The persona as the prompts state it. */
export function personaText(p = {}) {
  if (p.text) return p.text;
  const lines = [`You are ${p.name || 'someone'}${p.role ? `, ${p.role}` : ''}.`];
  if (p.background) lines.push(p.background);
  for (const t of p.traits || []) lines.push(`- ${t}`);
  return lines.join('\n');
}

/** One step as the next prompt recalls it. */
export function recall(s) {
  const i = s.input || {};
  const what = i.name ? `"${i.name}"` : i.ref || '';
  const act = {
    click: `clicked ${what}`, type: `typed "${String(i.text ?? '').slice(0, 80)}" into ${what}${i.submit ? ' and pressed Enter' : ''}`,
    choose: `chose "${i.option}" in ${what}`, press: `pressed ${i.key}`, scroll: `scrolled ${i.direction || 'down'}`,
    back: 'went back', go: `opened ${i.path}`, wait: `waited ${i.seconds ?? 1} s`, look: 'looked at the page',
  }[s.action] || s.action;
  const seen = (s.events || []).map((e) => `${e.kind}: ${e.text}`).join(' | ');
  return `${s.step}. ${act} (“${s.thought || ''}”)${s.ok === false ? ` — it did not work: ${s.error}` : ''}${seen ? ` — the browser showed ${seen}` : ''}`;
}

/** The prompt of one step. */
export function stepPrompt(job, steps, page) {
  const done = steps.length ? steps.map(recall).join('\n') : '(none yet)';
  return `Who you are:\n${personaText(job.persona)}\n\nYour task: ${job.task}\n\nWhat you did so far:\n${done}\n\nThe page now (step ${steps.length + 1} of at most ${job.max_steps || 25}):\n${page}\n\nAnswer your next action.`;
}

/**
 * What the browser showed that is a bug by itself, as findings: uncaught
 * exceptions and 5xx answers, one finding per message.
 */
export function browserFindings(steps) {
  const seen = new Map();
  for (const s of steps) {
    for (const e of s.events || []) {
      const is5xx = e.kind === 'http' && /^5\d\d /.test(e.text);
      if (e.kind !== 'exception' && !is5xx) continue;
      const key = `${e.kind}:${e.text.replace(/\d+/g, '#')}`;
      if (seen.has(key)) { seen.get(key).count++; continue; }
      seen.set(key, {
        count: 1,
        f: {
          kind: 'bug', severity: is5xx ? 3 : 2, step: s.step, url: s.url,
          title: is5xx ? `The server answered ${e.text.split(' ')[0]} to ${e.text.split(' ').slice(1).join(' ')}` : `The page threw: ${e.text.slice(0, 140)}`,
          detail: `At step ${s.step} (${recall(s).replace(/^\d+\. /, '')}), the browser showed ${e.kind === 'http' ? 'a failed request' : 'an uncaught exception'}: ${e.text}.`,
        },
      });
    }
  }
  return [...seen.values()].map(({ count, f }) => (count > 1 ? { ...f, detail: `${f.detail} It happened ${count} times.` } : f));
}

/** The JSON of a model's text answer: the whole text, or its first {...}. */
export function parseJson(text) {
  const t = String(text ?? '').trim();
  try { return JSON.parse(t); } catch { /* look inside */ }
  const fence = t.match(/```(?:json)?\s*([\s\S]*?)```/);
  if (fence) { try { return JSON.parse(fence[1]); } catch { /* go on */ } }
  const a = t.indexOf('{'); const b = t.lastIndexOf('}');
  if (a >= 0 && b > a) { try { return JSON.parse(t.slice(a, b + 1)); } catch { /* none */ } }
  return null;
}

// ---------------------------------------------------------------- models

/** `claude -p` on the user's login: stateless, no tools, no hooks. */
export function claudeAdapter(bin, workdir) {
  let cost = 0;
  const env = { ...process.env };
  for (const k of Object.keys(env)) if (k.startsWith('CHLOEDB_WORKBENCH_') || k === 'CLAUDECODE') delete env[k];
  return {
    name: 'claude -p',
    get cost() { return cost; },
    ask({ system, prompt, schema, model }) {
      const args = ['-p', '--output-format', 'json', '--json-schema', JSON.stringify(schema), '--tools', '', '--strict-mcp-config',
        '--settings', '{"disableAllHooks":true}', '--no-session-persistence', '--system-prompt', system];
      if (model) args.push('--model', model);
      return new Promise((resolve, reject) => {
        const p = spawn(bin, args, { cwd: workdir || process.cwd(), env, stdio: ['pipe', 'pipe', 'pipe'] });
        let out = ''; let err = '';
        p.stdout.on('data', (d) => { out += d; });
        p.stderr.on('data', (d) => { err += d; });
        p.on('error', (e) => reject(new Error(`cannot run ${bin}: ${e.message}`)));
        p.on('close', (code) => {
          const res = parseJson(out);
          if (!res) return reject(new Error(`claude -p ended (${code}) without JSON: ${(err || out).trim().split('\n').pop()?.slice(0, 300) || 'no output'}`));
          cost += Number(res.total_cost_usd) || 0;
          if (res.is_error) return reject(new Error(`claude -p: ${String(res.result || res.subtype || 'error').slice(0, 300)}`));
          const v = res.structured_output ?? parseJson(res.result);
          if (!v) return reject(new Error(`claude -p answered no action: ${String(res.result).slice(0, 200)}`));
          resolve(v);
        });
        p.stdin.end(prompt);
      });
    },
  };
}

/** The Anthropic API, through this package's model module. */
async function apiAdapter() {
  const m = await import('./model.mjs');
  const spend = new m.Spend(0);
  return {
    name: 'api',
    get cost() { return spend.usd; },
    async ask({ system, prompt, schema, model }) {
      const res = await m.ask(spend, {
        model: m.modelId(model), max_tokens: 1500, system,
        tools: [{ name: 'answer', description: 'Your answer.', input_schema: schema }],
        tool_choice: { type: 'tool', name: 'answer' },
        messages: [{ role: 'user', content: prompt }],
      });
      const t = res.content.find((b) => b.type === 'tool_use');
      if (!t) throw new Error('the model answered no action');
      return t.input;
    },
  };
}

const apiKey = () => process.env.CHLOEDB_SWARM_ANTHROPIC_KEY || process.env.ANTHROPIC_API_KEY;

async function sdkInstalled() {
  try { await import('@anthropic-ai/sdk'); return true; } catch { return false; }
}

/** Whether a run can start here: {ok, why, adapter}. */
export async function check(claudeBin) {
  try {
    const { chromium } = playwright();
    const exe = process.env.CHLOEDB_SWARM_CHROMIUM || chromium.executablePath();
    if (!exe || !existsSync(exe)) return { ok: false, why: 'Playwright has no Chromium here: run npx playwright install chromium', adapter: null };
  } catch (e) {
    return { ok: false, why: e.message, adapter: null };
  }
  if (apiKey() && await sdkInstalled()) return { ok: true, why: null, adapter: 'api' };
  const bin = claudeBin || 'claude';
  const v = spawnSync(bin, ['--version'], { encoding: 'utf8', timeout: 10000 });
  if (v.error || v.status !== 0) return { ok: false, why: `${bin} does not run (${v.error?.message || `exit ${v.status}`}): persona tests use claude -p on your login, or an API key`, adapter: null };
  return { ok: true, why: null, adapter: 'claude -p' };
}

// ------------------------------------------------------------ the browser

/** The browser surface on a preview: the token is traded for its cookie. */
export class PreviewSurface extends BrowserSurface {
  async start() {
    await this.page.goto(`${this.url}/?token=${encodeURIComponent(this.key)}`, { waitUntil: 'domcontentloaded' });
    await this.settle();
    return `You opened the app in a browser.\n\n${await this.snapshot()}`;
  }

  /** The words of an element a step acts on, for the record. */
  async nameOf(ref) {
    if (!ref) return null;
    try {
      const n = await this.el(ref).evaluate((e) => e.getAttribute('aria-label') || e.labels?.[0]?.innerText || e.getAttribute('placeholder') || e.innerText || e.value || e.getAttribute('title') || '', null, { timeout: 1000 });
      return String(n).replace(/\s+/g, ' ').trim().slice(0, 80) || null;
    } catch { return null; }
  }
}

// ---------------------------------------------------------------- the run

const emit = (o) => process.stdout.write(`${JSON.stringify(o)}\n`);

/**
 * Plays a job on a surface with a model; `out` gets each line. Answers the
 * `done` line. Exported for the tests, which give a fake surface and model.
 */
export async function play(job, surface, model, out = emit) {
  const t0 = Date.now();
  const maxSteps = job.max_steps || 25;
  const maxMs = (job.max_seconds || 900) * 1000;
  const steps = [];
  out({ type: 'start', adapter: model.name });
  let page = await surface.start();
  let end = null;
  while (!end) {
    if (steps.length >= maxSteps) { end = { outcome: 'steps', answer: `Stopped after ${maxSteps} steps without finishing.` }; break; }
    if (Date.now() - t0 > maxMs) { end = { outcome: 'time', answer: `Stopped after ${Math.round(maxMs / 1000)} s without finishing.` }; break; }
    let a;
    try {
      a = await model.ask({ system: RULES, prompt: stepPrompt(job, steps, page), schema: ACTION_SCHEMA, model: job.model });
    } catch (e) {
      out({ type: 'error', message: e.message });
      end = { outcome: 'error', answer: null };
      break;
    }
    const n = steps.length + 1;
    if (!ACTIONS.includes(a.action)) a = { ...a, action: 'look' };
    const input = {};
    for (const k of ['ref', 'text', 'submit', 'option', 'key', 'direction', 'path', 'seconds', 'outcome']) if (a[k] !== undefined && a[k] !== null && a[k] !== '') input[k] = a[k];
    if (a.action === 'finish') {
      const s = { step: n, action: 'finish', input, thought: a.note || '', ok: true, url: surface.page?.url?.() ?? null, events: [], ms: 0 };
      steps.push(s);
      out({ type: 'step', ...s });
      end = { outcome: a.outcome === 'gave_up' ? 'gave_up' : 'done', answer: a.answer || '' };
      break;
    }
    if (input.ref && surface.nameOf) { const nm = await surface.nameOf(input.ref); if (nm) input.name = nm; }
    const t = Date.now();
    const r = await surface.call(a.action, input);
    const s = { step: n, action: a.action, input, thought: a.note || '', ok: r.ok, error: r.error || undefined, url: r.url, events: r.events || [], ms: Date.now() - t };
    steps.push(s);
    out({ type: 'step', ...s });
    page = `${r.text}${r.note || ''}`;
  }
  // What the browser showed, then what a reviewer reads in the session.
  const found = browserFindings(steps);
  if (end.outcome !== 'error') {
    try {
      const v = await model.ask({
        system: REVIEW,
        schema: REVIEW_SCHEMA,
        model: job.model,
        prompt: `The person:\n${personaText(job.persona)}\n\nThe task: ${job.task}\n\nThe steps:\n${steps.map(recall).join('\n')}\n\nHow it ended: ${end.outcome}${end.answer ? `: ${end.answer}` : ''}\n\nAlready reported from the browser: ${found.map((f) => f.title).join('; ') || 'nothing'}. Do not repeat those.`,
      });
      for (const f of v?.findings || []) if (f?.title) found.push({ kind: f.kind || 'confusing', severity: f.severity || 2, title: f.title, detail: f.detail || '', step: f.step ?? null, url: steps.find((s) => s.step === f.step)?.url ?? null });
    } catch (e) {
      out({ type: 'error', message: `the review failed: ${e.message}` });
    }
  }
  if (end.outcome === 'gave_up' && !found.some((f) => f.severity >= 3)) {
    found.push({ kind: 'confusing', severity: 3, title: `${job.persona?.name || 'The persona'} gave up on the task`, detail: end.answer || 'They stopped without saying why.', step: steps.length, url: steps.at(-1)?.url ?? null });
  }
  for (const f of found) out({ type: 'finding', ...f });
  const done = { type: 'done', outcome: end.outcome, answer: end.answer, cost_usd: Math.round(model.cost * 10000) / 10000 };
  out(done);
  return done;
}

async function readStdin() {
  let s = '';
  for await (const c of process.stdin) s += c;
  return s;
}

async function main(argv) {
  if (argv.includes('--check')) {
    const i = argv.indexOf('--claude');
    emit(await check(i >= 0 ? argv[i + 1] : 'claude'));
    return 0;
  }
  const job = parseJson(await readStdin());
  if (!job?.url) { emit({ type: 'error', message: 'no job on stdin' }); return 2; }
  let surface = null;
  const stop = async () => { await surface?.close(); await closeBrowser().catch(() => {}); process.exit(143); };
  process.on('SIGTERM', stop);
  process.on('SIGINT', stop);
  try {
    const model = apiKey() && await sdkInstalled() ? await apiAdapter() : claudeAdapter(job.claude_bin || 'claude', job.workdir);
    surface = new PreviewSurface({ url: job.url, key: job.token || '', device: job.persona?.device === 'phone' ? 'phone' : 'desktop' });
    await surface.open();
    await play(job, surface, model);
    return 0;
  } catch (e) {
    emit({ type: 'error', message: e.message });
    emit({ type: 'done', outcome: 'error', answer: null });
    return 1;
  } finally {
    await surface?.close();
    await closeBrowser().catch(() => {});
  }
}

const isMain = (() => { try { return import.meta.url === pathToFileURL(realpathSync(process.argv[1])).href; } catch { return false; } })();
if (isMain) {
  main(process.argv.slice(2)).then((code) => process.exit(code));
}
