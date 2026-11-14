// The model's providers (engine/llm.mjs): claude, codex and agy. Their
// arguments are built by pure functions; codex runs against a fake `codex`
// that prints the events `codex exec --json` prints. No real model calls.

import test from 'node:test';
import assert from 'node:assert/strict';
import { writeFileSync, chmodSync, mkdtempSync, readFileSync } from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { ask, claudeArgs, codexArgs, codexEvent, checkProvider, withProvider, currentProvider, modelFor, PROVIDER, AGY_UNSUPPORTED, CODEX_OFF, childEnv, codexCost, buildScope, spend } from '../engine/llm.mjs';

// Codex is off unless the engine's runner turns it on (audit 4, X-G6); the
// tests below that use it turn it on, each for itself.
const codexOn = (t) => { const was = process.env.STRATA_APPS_CODEX_ON; process.env.STRATA_APPS_CODEX_ON = '1'; t.after(() => { if (was === undefined) delete process.env.STRATA_APPS_CODEX_ON; else process.env.STRATA_APPS_CODEX_ON = was; }); };

test('claude: the same arguments as before', () => {
  assert.deepEqual(claudeArgs({ system: 'S', model: 'claude-x' }), ['-p', '--output-format', 'stream-json', '--verbose', '--include-partial-messages', '--model', 'claude-x', '--system-prompt', 'S',
    '--tools', '', '--setting-sources', '', '--strict-mcp-config', '--disable-slash-commands', '--no-session-persistence']);
  const web = claudeArgs({ system: 'S', model: 'm', tools: ['WebSearch', 'WebFetch'], maxBudgetUsd: 0.5, effort: 'low', images: [{}] });
  assert.deepEqual(web.slice(-8), ['--allowedTools', 'WebSearch,WebFetch', '--max-budget-usd', '0.5', '--effort', 'low', '--input-format', 'stream-json']);
  assert.equal(web[web.indexOf('--tools') + 1], 'WebSearch,WebFetch');
});

test('codex: exec, read only, no tools, no user config, answer only', () => {
  const a = codexArgs({ system: 'Be "brief"\nnow', model: 'gpt-5.5', effort: 'high', images: ['/t/a.png'], outFile: '/t/out.txt', cwd: '/t/w' });
  assert.equal(a[0], 'exec');
  assert.equal(a.at(-1), '-', 'the prompt is read from stdin');
  const val = (f) => a[a.indexOf(f) + 1];
  assert.ok(a.includes('--json') && a.includes('--ephemeral') && a.includes('--ignore-user-config') && a.includes('--ignore-rules') && a.includes('--skip-git-repo-check'));
  assert.equal(val('--sandbox'), 'read-only');
  assert.equal(val('--model'), 'gpt-5.5');
  assert.equal(val('--cd'), '/t/w');
  assert.equal(val('--output-last-message'), '/t/out.txt');
  assert.equal(val('--image'), '/t/a.png');
  const disabled = a.flatMap((x, i) => (x === '--disable' ? [a[i + 1]] : []));
  for (const f of ['shell_tool', 'apps', 'browser_use', 'computer_use', 'image_generation']) assert.ok(disabled.includes(f), f);
  const conf = a.flatMap((x, i) => (x === '-c' ? [a[i + 1]] : []));
  assert.ok(conf.includes('web_search="disabled"'));
  assert.ok(conf.includes('approval_policy="never"'));
  assert.ok(conf.includes('model_reasoning_effort="high"'));
  assert.ok(conf.includes('developer_instructions="Be \\"brief\\"\\nnow"'), 'the system prompt as a TOML string');
  // No model: the CLI's own; Claude's efforts it does not know are left out.
  const b = codexArgs({ system: 's', effort: 'max' });
  assert.ok(!b.includes('--model'));
  assert.ok(!b.some((x) => x.startsWith('model_reasoning_effort')));
});

test('codex: its JSONL events', () => {
  assert.deepEqual(codexEvent('{"type":"item.completed","item":{"id":"i","type":"agent_message","text":"hi"}}'), { text: 'hi', done: true });
  assert.deepEqual(codexEvent('{"type":"item.completed","item":{"type":"reasoning","text":"hmm"}}'), { thinking: 'hmm' });
  assert.deepEqual(codexEvent('{"type":"turn.completed","usage":{"input_tokens":5,"output_tokens":2}}'), { usage: { input_tokens: 5, output_tokens: 2 } });
  assert.deepEqual(codexEvent('{"type":"turn.failed","error":{"message":"401 Unauthorized"}}'), { error: '401 Unauthorized' });
  assert.equal(codexEvent('not json'), null);
});

test('codex: off by default, refused with what turns it on', async () => {
  assert.equal(process.env.STRATA_APPS_CODEX_ON, undefined);
  assert.throws(() => checkProvider('codex'), (e) => e.status === 400 && e.message === CODEX_OFF);
  await assert.rejects(ask({ provider: 'codex', bin: '/nonexistent', system: 's', prompt: 'p' }), (e) => e.status === 400 && /STRATA_APPS_CODEX_ON=1/.test(e.message));
});

test('providers: chosen by name, per build, with models of their own', async (t) => {
  codexOn(t);
  assert.equal(checkProvider(undefined), null);
  assert.equal(checkProvider('Codex'), 'codex');
  assert.throws(() => checkProvider('gpt'), (e) => e.status === 400 && /claude, codex, agy/.test(e.message));
  assert.equal(PROVIDER, 'claude', 'claude unless STRATA_APPS_PROVIDER says otherwise');
  assert.equal(currentProvider(), 'claude');
  const inside = await withProvider('codex', async () => { await new Promise((r) => setTimeout(r, 5)); return currentProvider(); });
  assert.equal(inside, 'codex', 'kept across awaits');
  assert.equal(currentProvider(), 'claude', 'and only inside');
  assert.equal(await withProvider(null, async () => currentProvider()), 'claude');
  assert.equal(modelFor('claude', 'claude-haiku-5-5'), 'claude-haiku-5-5');
  assert.equal(modelFor('codex', 'claude-haiku-5-5'), null, 'Claude\'s names are not sent to codex');
  assert.equal(modelFor('codex', 'gpt-5.5'), 'gpt-5.5');
});

test('agy: refused with a clear error', async () => {
  await assert.rejects(ask({ system: 's', prompt: 'p', provider: 'agy' }), (e) => e.message === AGY_UNSUPPORTED && e.status === 400);
  await assert.rejects(withProvider('agy', () => ask({ system: 's', prompt: 'p' })), /not supported/);
});

// A fake `codex`: writes its arguments and stdin, prints events like codex.
function fakeCodex({ fail = false, hang = false } = {}) {
  const dir = mkdtempSync(path.join(os.tmpdir(), 'sa-fake-codex-'));
  const bin = path.join(dir, 'codex');
  writeFileSync(bin, `#!${process.execPath}
const fs = require('fs');
const args = process.argv.slice(2);
let input = '';
process.stdin.on('data', (d) => { input += d; });
process.stdin.on('end', () => {
  fs.writeFileSync(${JSON.stringify(path.join(dir, 'seen.json'))}, JSON.stringify({ args, input, cwd: process.cwd(), env: process.env, pid: process.pid }));
  const out = (o) => process.stdout.write(JSON.stringify(o) + '\\n');
  out({ type: 'thread.started', thread_id: 't' });
  out({ type: 'turn.started' });
  if (${hang}) { out({ type: 'item.completed', item: { id: 'r', type: 'reasoning', text: 'thinking' } }); setInterval(() => out({ type: 'item.completed', item: { id: 'r', type: 'reasoning', text: '.' } }), 50); return; }
  if (${fail}) { out({ type: 'turn.failed', error: { message: 'unexpected status 401 Unauthorized' } }); process.exit(1); }
  out({ type: 'item.completed', item: { id: 'r', type: 'reasoning', text: 'thinking' } });
  out({ type: 'item.completed', item: { id: 'm', type: 'agent_message', text: '<plan>{}</plan>' } });
  const o = args[args.indexOf('--output-last-message') + 1];
  fs.writeFileSync(o, '<plan>{}</plan>');
  out({ type: 'turn.completed', usage: { input_tokens: 10, output_tokens: 4 } });
});
`);
  chmodSync(bin, 0o755);
  return { bin, seen: () => JSON.parse(readFileSync(path.join(dir, 'seen.json'), 'utf8')) };
}

test('codex: an answer through a fake codex, its cost worked out from its tokens', async (t) => {
  codexOn(t);
  const before = spend.usd;
  const fake = fakeCodex();
  const writes = [];
  const out = await ask({ provider: 'codex', bin: fake.bin, system: 'SYS', prompt: 'build a counter', model: 'claude-sonnet-5-5', onWrite: (w) => writes.push({ ...w }) });
  assert.equal(out.text, '<plan>{}</plan>');
  assert.equal(out.cost, codexCost({ input_tokens: 10, output_tokens: 4 }));
  assert.ok(out.cost > 0, 'a Codex answer is never free');
  assert.ok(spend.usd > before, 'and counts against the cap');
  assert.equal(out.costReported, false);
  assert.equal(out.provider, 'codex');
  assert.deepEqual(out.usage, { input_tokens: 10, output_tokens: 4 });
  const seen = fake.seen();
  assert.equal(seen.input, 'build a counter');
  assert.ok(!seen.args.includes('claude-sonnet-5-5'));
  assert.ok(seen.args.includes('developer_instructions="SYS"'));
  assert.ok(writes.length >= 2 && writes.at(-1).chars === out.text.length);
  // Through a build's provider.
  const again = await withProvider('codex', () => ask({ bin: fake.bin, system: 's', prompt: 'p' }));
  assert.equal(again.provider, 'codex');
  // Tools (an app's web ask) need claude.
  await assert.rejects(ask({ provider: 'codex', bin: fake.bin, system: 's', prompt: 'p', tools: ['WebSearch'] }), /needs the claude provider/);
});

test('codex: a failed turn is the error', async (t) => {
  codexOn(t);
  const fake = fakeCodex({ fail: true });
  await assert.rejects(ask({ provider: 'codex', bin: fake.bin, system: 's', prompt: 'p' }), /codex: unexpected status 401/);
});

test('model children: a reduced environment, not the engine\'s', async (t) => {
  codexOn(t);
  const from = { PATH: '/bin', HOME: '/home/u', LANG: 'C', HTTPS_PROXY: 'http://p', ANTHROPIC_API_KEY: 'a', CLAUDE_CONFIG_DIR: '/c', OPENAI_API_KEY: 'o', CODEX_HOME: '/x',
    STRATA_TOKEN: 'secret', STRATA_APP_TOKEN: 'secret', CHLOEDB: 'secret', STRATA_APPS_SEAL_KEY: 'secret', GITHUB_TOKEN: 'secret', AWS_SECRET_ACCESS_KEY: 'secret', MY_VAR: 'v', STRATA_APPS_CHILD_ENV: 'MY_VAR' };
  const c = childEnv('claude', from);
  for (const k of ['PATH', 'HOME', 'LANG', 'HTTPS_PROXY', 'ANTHROPIC_API_KEY', 'CLAUDE_CONFIG_DIR', 'MY_VAR']) assert.equal(c[k], from[k], k);
  for (const k of ['STRATA_TOKEN', 'STRATA_APP_TOKEN', 'CHLOEDB', 'STRATA_APPS_SEAL_KEY', 'GITHUB_TOKEN', 'AWS_SECRET_ACCESS_KEY', 'OPENAI_API_KEY', 'CODEX_HOME']) assert.equal(c[k], undefined, k);
  assert.equal(c.CHLOECTX_DISABLE, '1');
  assert.equal(c.CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC, '1');
  // Bedrock's sign-in only when Claude Code is told to use it.
  assert.equal(childEnv('claude', { ...from, CLAUDE_CODE_USE_BEDROCK: '1' }).AWS_SECRET_ACCESS_KEY, 'secret');
  const x = childEnv('codex', from);
  for (const k of ['PATH', 'HOME', 'OPENAI_API_KEY', 'CODEX_HOME']) assert.equal(x[k], from[k], k);
  for (const k of ['STRATA_TOKEN', 'ANTHROPIC_API_KEY', 'CLAUDE_CONFIG_DIR', 'AWS_SECRET_ACCESS_KEY']) assert.equal(x[k], undefined, k);
  // What a real child sees.
  const fake = fakeCodex();
  process.env.SA_TEST_SECRET_TOKEN = 'do-not-pass';
  t.after(() => { delete process.env.SA_TEST_SECRET_TOKEN; });
  await ask({ provider: 'codex', bin: fake.bin, system: 's', prompt: 'p' });
  const seen = fake.seen();
  assert.equal(seen.env.SA_TEST_SECRET_TOKEN, undefined);
  assert.equal(seen.env.PATH, process.env.PATH);
});

test('codex: its cost, from tokens or from lengths', () => {
  assert.equal(codexCost({ input_tokens: 1e6, cached_input_tokens: 0, output_tokens: 1e6 }, {}, '1,0.1,10'), 11);
  assert.equal(codexCost({ input_tokens: 1e6, cached_input_tokens: 1e6, output_tokens: 0 }, {}, '1,0.1,10'), 0.1);
  assert.equal(codexCost({}, { promptChars: 4e6, answerChars: 4e6 }, '1,0.1,10'), 11, 'no usage: four characters a token');
  assert.ok(codexCost(null, { promptChars: 400 }) > 0);
});

test('codex: Stop kills the child at once', async (t) => {
  codexOn(t);
  const fake = fakeCodex({ hang: true });
  const ctl = new AbortController();
  const t0 = Date.now();
  const run = buildScope.run({ signal: ctl.signal }, () => ask({ provider: 'codex', bin: fake.bin, system: 's', prompt: 'p', idleMs: 30000, timeoutMs: 60000 }));
  setTimeout(() => ctl.abort(), 300);
  await assert.rejects(run, (e) => e.stopped === true);
  assert.ok(Date.now() - t0 < 5000, 'not at the idle or hard timeout');
  const { pid } = fake.seen();
  await new Promise((r) => setTimeout(r, 100));
  assert.throws(() => process.kill(pid, 0), 'the child is gone');
  // Already stopped: refused before a child starts.
  await assert.rejects(buildScope.run({ signal: AbortSignal.abort() }, () => ask({ provider: 'codex', bin: fake.bin, system: 's', prompt: 'p' })), (e) => e.stopped === true);
});
