// The models that play the users and judge them: one Anthropic client, a
// retry on overload, and what every call cost, from the list prices below
// (US dollars per million tokens, as published on 2026-11-03; a cache read
// and a cache write priced from the input price as the API bills them).

import Anthropic from '@anthropic-ai/sdk';

export const PRICES = {
  'claude-haiku-5-5': { input: 0.10, output: 0.50, cacheRead: 0.01, cacheWrite: 0.125 },
  'claude-sonnet-5-5': { input: 2.00, output: 10.00, cacheRead: 0.10, cacheWrite: 2.50 },
  'claude-opus-5-5': { input: 4.00, output: 20.00, cacheRead: 0.20, cacheWrite: 5.00 },
};

// The model that plays users by default, the one that judges, and the
// short names a scenario may use for them.
export const USER_MODEL = 'claude-haiku-5-5';
export const JUDGE_MODEL = 'claude-opus-5-5';
const SHORT = { haiku: 'claude-haiku-5-5', sonnet: 'claude-sonnet-5-5', opus: 'claude-opus-5-5' };
export const modelId = (m) => SHORT[m] || m || USER_MODEL;

/** What one answer cost, in dollars, from its usage. */
export function costOf(model, usage = {}) {
  const p = PRICES[model];
  if (!p) return 0;
  return ((usage.input_tokens || 0) * p.input
    + (usage.output_tokens || 0) * p.output
    + (usage.cache_read_input_tokens || 0) * p.cacheRead
    + (usage.cache_creation_input_tokens || 0) * p.cacheWrite) / 1e6;
}

/**
 * A spending account shared by everything a run does. `cap` in dollars (0:
 * none). `charge` adds a call's cost; `over` says whether the cap is spent.
 */
export class Spend {
  constructor(cap = 0) {
    this.cap = cap;
    this.usd = 0;
    this.calls = 0;
    this.tokens = { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 };
  }

  charge(model, usage) {
    const c = costOf(model, usage);
    this.usd += c;
    this.calls++;
    this.tokens.input += usage?.input_tokens || 0;
    this.tokens.output += usage?.output_tokens || 0;
    this.tokens.cacheRead += usage?.cache_read_input_tokens || 0;
    this.tokens.cacheWrite += usage?.cache_creation_input_tokens || 0;
    return c;
  }

  get over() { return this.cap > 0 && this.usd >= this.cap; }

  toJSON() { return { usd: Number(this.usd.toFixed(4)), cap: this.cap, calls: this.calls, tokens: this.tokens }; }
}

const FALLBACK = /^claude-(sonnet|opus)-5-5$/;

let shared = null;

/**
 * The client: CHLOEDB_SWARM_ANTHROPIC_KEY or ANTHROPIC_API_KEY, and the
 * API's own address unless CHLOEDB_SWARM_ANTHROPIC_URL names another, so
 * that a base URL set for some other program does not redirect the swarm.
 */
export function client() {
  if (shared) return shared;
  const apiKey = process.env.CHLOEDB_SWARM_ANTHROPIC_KEY || process.env.ANTHROPIC_API_KEY;
  if (!apiKey) throw new Error('no model key: set ANTHROPIC_API_KEY (or CHLOEDB_SWARM_ANTHROPIC_KEY)');
  shared = new Anthropic({ apiKey, baseURL: process.env.CHLOEDB_SWARM_ANTHROPIC_URL || 'https://api.anthropic.com', maxRetries: 4, timeout: 180_000 });
  return shared;
}

/** Tests put a scripted client in place of the API. */
export function useClient(c) { shared = c; }

/**
 * One request; `spend` is charged and the answer carries its `cost`. A
 * spent budget and a refusal are errors, which the caller records.
 */
export async function ask(spend, params, { stream = false } = {}) {
  if (spend?.over) throw Object.assign(new Error(`the run's budget of $${spend.cap} is spent`), { budget: true });
  // Sonnet 5.5 and Opus 5.5 decline some requests by classifier; a server-side
  // fallback retries those it can on the model Anthropic recommends. A long
  // answer (`stream`) is streamed, so a large max_tokens does not time out.
  const fb = FALLBACK.test(params.model);
  const api = fb ? client().beta.messages : client().messages;
  const body = fb ? { ...params, betas: ['server-side-fallback-2026-07-01'], fallbacks: 'default' } : params;
  const res = stream && api.stream ? await api.stream(body).finalMessage() : await api.create(body);
  const priced = PRICES[res.model] ? res.model : params.model;
  const cost = spend ? spend.charge(priced, res.usage) : costOf(priced, res.usage);
  if (res.stop_reason === 'refusal') throw Object.assign(new Error(`the model declined (${res.stop_details?.category || 'no category'})`), { cost, refusal: true });
  return Object.assign(res, { cost });
}
