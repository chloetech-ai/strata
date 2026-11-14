// Reading the sessions: a stronger model reviews each session's steps,
// thoughts, answer and interview and names the problems it shows, each
// with the step it happened at, a category and a severity, telling the
// product's problems from the agent's own mistakes. Then one call groups
// the problems of a whole run into findings: one problem seen by many
// sessions is one finding, with how many sessions and which personas hit it.

import { ask, JUDGE_MODEL } from './model.mjs';
import { personaText } from './agent.mjs';

export const CATEGORIES = ['bug', 'error_message', 'confusing_ui', 'missing_feature', 'docs', 'performance', 'agent_mistake'];

const REVIEW = {
  name: 'review',
  description: 'Your review of the session.',
  input_schema: {
    type: 'object',
    properties: {
      summary: { type: 'string', description: 'Two sentences: what the person tried and how it went.' },
      verdict: { type: 'string', enum: ['worked', 'product_problem', 'agent_mistake', 'mixed'] },
      problems: {
        type: 'array',
        items: {
          type: 'object',
          properties: {
            step: { type: 'integer', description: 'The step where it shows, or 0 for the session as a whole.' },
            category: { type: 'string', enum: CATEGORIES },
            severity: { type: 'integer', minimum: 1, maximum: 3, description: '3: blocks the task or loses data; 2: costs real time or trust; 1: friction.' },
            title: { type: 'string', description: 'The problem in under 12 words, about the product, not the person.' },
            detail: { type: 'string', description: 'What happened and what the person expected, with the evidence from the steps.' },
          },
          required: ['step', 'category', 'severity', 'title', 'detail'],
        },
      },
      strengths: {
        type: 'array',
        description: 'What worked well, that the makers should keep: a screen, an error message, a tool or a document that got the person to their goal faster than they expected.',
        items: {
          type: 'object',
          properties: {
            step: { type: 'integer', description: 'The step where it shows, or 0 for the session as a whole.' },
            title: { type: 'string', description: 'The strength in under 12 words, about the product.' },
            detail: { type: 'string', description: 'What helped, with the evidence from the steps.' },
          },
          required: ['step', 'title', 'detail'],
        },
      },
    },
    required: ['summary', 'verdict', 'problems', 'strengths'],
  },
};

const REVIEW_RULES = `You review one session of a usability study of ChloeDB (a database with search and SQL) and its tools. A model played the person described, doing the task, and wrote a thought at every step. Name the problems a maker of the product should know about: bugs, unhelpful error messages, confusing screens or wording, missing features, gaps in documentation, slowness. Each needs evidence in the steps. Tell them apart from the agent's own mistakes (misreading a page that was clear, inventing things, not trying the obvious): those get the category agent_mistake. Report nothing you cannot point at. A session that went well can have no problems. Also name the strengths: what in the product worked well and helped the person (an error message that said what to do, a page that was where they looked, a tool that answered in one step), with the same evidence; a strength is about the product, not about the agent playing well. Use the review tool.`;

/** A session as text for the reviewer. */
export function digest(s, steps) {
  const lines = [
    `Person:\n${personaText(s.persona)}`,
    `Task (${s.task.surface}): ${s.task.prompt}`,
    `Outcome: ${s.outcome}${s.success ? ' (checks passed)' : ''}; ${s.steps} steps, ${s.seconds} s`,
    'Steps:',
  ];
  for (const st of steps) {
    const input = st.input && Object.keys(st.input).length ? ` ${JSON.stringify(st.input).slice(0, 300)}` : '';
    const ev = st.events?.length ? `\n     browser: ${st.events.map((e) => `${e.kind}: ${e.text}`).join(' | ').slice(0, 400)}` : '';
    lines.push(`${st.step}. ${st.tool}${input}\n     thought: ${st.thought || '-'}\n     ${st.ok ? 'result' : 'FAILED'}: ${String(st.error || st.result || '').replace(/\s+/g, ' ').slice(0, st.ok ? 600 : 900)}${ev}`);
  }
  lines.push(`Final answer: ${s.answer || '-'}`);
  if (s.checks?.length) lines.push(`Checks: ${s.checks.map((c) => `${c.name} ${c.ok ? 'passed' : 'failed'} (${c.detail})`).join('; ')}`);
  if (s.interview) lines.push(`Exit interview: ${JSON.stringify(s.interview)}`);
  if (s.error) lines.push(`Runner error: ${s.error}`);
  return lines.join('\n');
}

/** Review one session; answers the review, with its cost. */
export async function reviewSession(spend, session, steps, { model = JUDGE_MODEL } = {}) {
  const res = await ask(spend, {
    model, max_tokens: 8000, system: REVIEW_RULES, tools: [REVIEW],
    output_config: { effort: 'medium' },
    messages: [{ role: 'user', content: digest(session, steps) }],
  });
  const r = res.content.find((b) => b.type === 'tool_use' && b.name === 'review');
  return { ...(r?.input || { summary: 'no review', verdict: 'mixed', problems: [] }), cost: res.cost };
}

const FINDINGS = {
  name: 'findings',
  description: 'The findings of the run.',
  input_schema: {
    type: 'object',
    properties: {
      findings: {
        type: 'array',
        items: {
          type: 'object',
          properties: {
            title: { type: 'string', description: 'The problem in under 12 words.' },
            category: { type: 'string', enum: CATEGORIES },
            severity: { type: 'integer', minimum: 1, maximum: 3 },
            summary: { type: 'string', description: 'What goes wrong, for whom, and why, in two or three sentences.' },
            fix: { type: 'string', description: 'The change that would most likely fix it, in one sentence.' },
            problems: { type: 'array', items: { type: 'string' }, description: 'The ids of the problems this finding groups.' },
          },
          required: ['title', 'category', 'severity', 'summary', 'fix', 'problems'],
        },
      },
    },
    required: ['findings'],
  },
};

const STRENGTHS = {
  name: 'strengths',
  description: 'The strengths of the run.',
  input_schema: {
    type: 'object',
    properties: {
      strengths: {
        type: 'array',
        items: {
          type: 'object',
          properties: {
            title: { type: 'string', description: 'The strength in under 12 words.' },
            summary: { type: 'string', description: 'What works, for whom, and why it matters, in one or two sentences.' },
            problems: { type: 'array', items: { type: 'string' }, description: 'The ids of the strengths this groups.' },
          },
          required: ['title', 'summary', 'problems'],
        },
      },
    },
    required: ['strengths'],
  },
};

/**
 * Group the problems of a run's sessions into findings, and their
 * strengths likewise. `problems` (and `strengths`) are { id, session,
 * persona, task, step, title, detail } (problems with category and
 * severity too). A strength comes back as a finding of category
 * `strength`, severity 0, after the problems.
 */
export async function groupFindings(spend, problems, sessions, { model = JUDGE_MODEL, strengths = [] } = {}) {
  const byId = new Map([...problems, ...strengths].map((p) => [p.id, p]));
  const total = sessions.length || 1;
  const shape = (f, i, extra) => {
    const ps = (f.problems || []).map((id) => byId.get(id)).filter(Boolean);
    const sess = [...new Set(ps.map((p) => p.session))];
    return {
      rank: i + 1, title: f.title, ...extra, summary: f.summary,
      problems: ps.map((p) => p.id), sessions: sess, share: Number((sess.length / total).toFixed(3)),
      personas: [...new Set(ps.map((p) => p.persona))], tasks: [...new Set(ps.map((p) => p.task))],
      evidence: ps.slice(0, 6).map((p) => ({ problem: p.id, session: p.session, step: p.step, detail: p.detail })),
    };
  };
  // One call for the problems and one for the strengths, each streamed with
  // room to think: a run of 60 sessions has well over a hundred problems,
  // and an answer cut at max_tokens has no findings in it.
  const group = async (tool, key, system, text) => {
    const res = await ask(spend, { model, max_tokens: 48000, tools: [tool], output_config: { effort: 'medium' }, system, messages: [{ role: 'user', content: text }] }, { stream: true });
    if (res.stop_reason === 'max_tokens') throw new Error(`grouping the ${key} was cut at max_tokens`);
    const r = res.content.find((b) => b.type === 'tool_use' && b.name === tool.name);
    if (!r) throw new Error(`grouping the ${key} answered without the ${tool.name} tool`);
    return { items: r.input?.[key] || [], cost: res.cost };
  };
  let cost = 0;
  let findings = [];
  if (problems.length) {
    const g = await group(FINDINGS, 'findings', 'You turn the problems that reviewers found in the sessions of a usability study into findings for the product\'s makers. Group problems that are the same underlying issue, even when worded differently or seen by different personas; keep different issues apart. Every problem id goes into exactly one finding. Keep agent_mistake problems in findings of their own, so the makers can see how often the simulated people, not the product, were at fault. Order findings by how much they matter: severity, then how many sessions hit them. Use the findings tool.',
      `The problems:\n${problems.map((p) => `[${p.id}] (${p.category}, severity ${p.severity}; ${p.persona} on ${p.task}) ${p.title}: ${p.detail}`).join('\n')}`);
    cost += g.cost;
    findings = g.items.map((f, i) => shape(f, i, { category: f.category, severity: f.severity, fix: f.fix }));
  }
  let goods = [];
  if (strengths.length) {
    const g = await group(STRENGTHS, 'strengths', 'You turn the strengths that reviewers found in the sessions of a usability study into a short list for the product\'s makers: what works and should be kept. Group strengths that are the same thing, even when worded differently or seen by different personas. Every strength id goes into exactly one group. Order them by how many sessions they helped. Use the strengths tool.',
      `The strengths:\n${strengths.map((p) => `[${p.id}] (${p.persona} on ${p.task}) ${p.title}: ${p.detail}`).join('\n')}`);
    cost += g.cost;
    goods = g.items.map((f, i) => shape(f, findings.length + i, { category: 'strength', severity: 0, fix: '' }));
  }
  return { findings: [...findings, ...goods], cost };
}
