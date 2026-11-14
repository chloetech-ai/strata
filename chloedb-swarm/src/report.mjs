// A run as Markdown: what succeeded, by persona and task, what it cost, and
// the findings with their evidence. The command line writes it beside the
// run; the Swarm page shows the same numbers.

const pct = (x) => (x == null ? '–' : `${Math.round(x * 100)}%`);
const usd = (x) => `$${(x || 0).toFixed(2)}`;

export function markdown({ run, sessions, findings }) {
  const t = run.totals || {};
  const out = [`# Swarm run ${run.id}: ${run.name}`, ''];
  out.push(`${run.status}, ${run.started}${run.ended ? ` to ${run.ended}` : ''}. ${t.sessions ?? sessions.length} sessions, ${pct(t.rate)} succeeded, ${t.findings ?? findings.length} findings, ${usd(run.cost?.usd)} (users ${usd(run.cost?.users?.usd)}, judge ${usd(run.cost?.judge?.usd)}).`, '');
  const personas = run.set?.personas || [];
  const tasks = run.set?.tasks || [];
  if (personas.length && tasks.length) {
    out.push('## Success by persona and task', '', `| persona | ${tasks.join(' | ')} | all |`, `| --- | ${tasks.map(() => '---').join(' | ')} | --- |`);
    for (const p of personas) {
      const cells = tasks.map((k) => {
        const ss = sessions.filter((s) => s.persona.id === p && s.task.id === k);
        return ss.length ? `${ss.filter((s) => s.success).length}/${ss.length}` : '–';
      });
      out.push(`| ${p} | ${cells.join(' | ')} | ${pct(t.by_persona?.[p]?.rate)} |`);
    }
    out.push(`| all | ${tasks.map((k) => pct(t.by_task?.[k]?.rate)).join(' | ')} | ${pct(t.rate)} |`, '');
  }
  if (t.by_model) {
    out.push('## By the model that played the user', '', '| model | sessions | success | mean steps | mean ease (1-7) |', '| --- | --- | --- | --- | --- |');
    for (const [m, b] of Object.entries(t.by_model)) out.push(`| ${m} | ${b.n} | ${pct(b.rate)} | ${b.steps} | ${b.ease ?? '–'} |`);
    out.push('');
  }
  const good = findings.filter((f) => f.category === 'strength');
  const bad = findings.filter((f) => f.category !== 'strength');
  out.push('## Findings', '');
  if (!bad.length) out.push('None.', '');
  for (const f of bad) {
    out.push(`### ${f.rank}. ${f.title}`, '', `${f.category}, severity ${f.severity}; ${f.sessions.length} sessions (${pct(f.share)}); personas: ${f.personas.join(', ')}; tasks: ${f.tasks.join(', ')}.`, '', f.summary, '', `Fix: ${f.fix}`, '');
    for (const e of f.evidence || []) out.push(`- ${e.session} step ${e.step}: ${e.detail}`);
    out.push('');
  }
  if (good.length) {
    out.push('## Strengths', '');
    for (const f of good) {
      out.push(`### ${f.title}`, '', `${f.sessions.length} sessions (${pct(f.share)}); personas: ${f.personas.join(', ')}; tasks: ${f.tasks.join(', ')}.`, '', f.summary, '');
      for (const e of f.evidence || []) out.push(`- ${e.session} step ${e.step}: ${e.detail}`);
      out.push('');
    }
  }
  out.push('## Sessions', '', '| session | persona | task | seed | model | outcome | steps | checks | ease |', '| --- | --- | --- | --- | --- | --- | --- | --- | --- |');
  for (const s of sessions) out.push(`| ${s.id} | ${s.persona.id} | ${s.task.id} | ${s.seed} | ${s.model} | ${s.outcome}${s.success ? ' ✓' : ''} | ${s.steps} | ${(s.checks || []).map((c) => (c.ok ? 'pass' : 'fail')).join(', ')} | ${s.interview?.ease ?? '–'} |`);
  return `${out.join('\n')}\n`;
}
