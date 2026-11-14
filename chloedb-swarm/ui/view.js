// Swarm's page in ChloeDB's command center (#/app/swarm). The command
// center loads this file through chloedb-server, which forwards
// /v1/apps/swarm/... to chloedb-swarm, and runs it with its own helpers:
// render(el, ctx) gets ctx.core (h, card, table, fmt, ...) and ctx.api (the
// swarm's API). It imports nothing, so the swarm serves it as one file.
//
//   #/app/swarm                 start a run from a scenario set; the runs
//   #/app/swarm/<run>           a run: success by persona and task, the
//                               findings with their evidence, the sessions
//   #/app/swarm/<run>/<session> one session, step by step: what the person
//                               did, thought and saw, the checks, the review

export default { render };

const CSS = `
.swm-head { display: flex; align-items: baseline; gap: 10px; flex-wrap: wrap; margin: 0 0 14px; }
.swm-head h2 { margin: 0; font-size: 18px; }
.swm-head .sub { color: var(--muted); font-size: 13px; }
.swm-start { display: grid; grid-template-columns: minmax(0, 2fr) 110px 110px auto; gap: 10px; align-items: end; }
.swm-start label { display: grid; gap: 4px; font-size: 12px; color: var(--text-2); }
.swm-note { font-size: 12.5px; color: var(--muted); margin-top: 10px; line-height: 1.5; }
.swm-set { font-size: 12.5px; color: var(--text-2); margin-top: 10px; display: grid; gap: 4px; }
.swm-kpis { display: grid; grid-template-columns: repeat(5, minmax(0, 1fr)); gap: 12px; margin-bottom: 14px; }
.swm-bar { height: 6px; background: var(--surface-3); border-radius: 3px; overflow: hidden; margin: 0 0 14px; }
.swm-bar > i { display: block; height: 100%; background: var(--accent); transition: width .5s; }
.swm-matrix { border-collapse: collapse; width: 100%; font-size: 12.5px; }
.swm-matrix th, .swm-matrix td { padding: 6px 8px; text-align: center; border-bottom: 1px solid var(--line); }
.swm-matrix th:first-child, .swm-matrix td:first-child { text-align: left; }
.swm-cell { display: inline-block; min-width: 46px; padding: 3px 6px; border-radius: 6px; font-variant-numeric: tabular-nums; }
.swm-f { border-top: 1px solid var(--line); padding: 12px 0; display: grid; gap: 6px; }
.swm-f:first-child { border-top: 0; padding-top: 2px; }
.swm-f .t { display: flex; gap: 8px; align-items: baseline; flex-wrap: wrap; }
.swm-f .t b { font-size: 14px; }
.swm-f .meta { font-size: 12px; color: var(--muted); }
.swm-f .fix { font-size: 12.5px; }
.swm-f ul { margin: 2px 0 0; padding-left: 18px; font-size: 12.5px; color: var(--text-2); }
.swm-chip { display: inline-block; font-size: 11.5px; padding: 1px 7px; border-radius: 10px; background: var(--surface-3); color: var(--text-2); margin-right: 4px; }
.swm-sev3 { background: var(--crit-soft); color: var(--crit-ink); }
.swm-sev2 { background: var(--warn-soft); color: var(--text); }
.swm-sev1 { background: var(--surface-3); color: var(--text-2); }
.swm-sev0 { background: var(--good-soft); color: var(--good-ink); }
.swm-steps { list-style: none; margin: 0; padding: 0; }
.swm-steps li { display: grid; grid-template-columns: 34px minmax(0, 1fr); gap: 10px; padding: 10px 0; border-top: 1px solid var(--line); }
.swm-steps li:first-child { border-top: 0; }
.swm-steps .n { font-weight: 650; color: var(--muted); font-variant-numeric: tabular-nums; }
.swm-steps .act { font-family: var(--mono); font-size: 12.5px; overflow-wrap: anywhere; }
.swm-steps .th { font-style: italic; color: var(--text-2); margin: 3px 0; }
.swm-steps .bad { color: var(--crit-ink); font-size: 12.5px; }
.swm-steps li.target { background: var(--accent-soft); }
.swm-steps details summary { cursor: pointer; font-size: 12px; color: var(--muted); }
.swm-steps pre { white-space: pre-wrap; overflow-wrap: anywhere; max-height: 320px; overflow: auto; font-size: 11.5px; margin: 4px 0 0; }
.swm-kv { display: grid; grid-template-columns: 140px minmax(0, 1fr); gap: 6px 12px; font-size: 13px; }
.swm-kv dt { color: var(--muted); }
.swm-kv dd { margin: 0; overflow-wrap: anywhere; }
.swm-cols { display: grid; grid-template-columns: minmax(0, 3fr) minmax(0, 2fr); gap: 14px; align-items: start; }
@media (max-width: 900px) {
  .swm-start { grid-template-columns: minmax(0, 1fr) minmax(0, 1fr); }
  .swm-start .wide { grid-column: 1 / -1; }
  .swm-kpis { grid-template-columns: repeat(2, minmax(0, 1fr)); }
  .swm-cols { grid-template-columns: minmax(0, 1fr); }
  .swm-kv { grid-template-columns: minmax(0, 1fr); }
}`;

const pct = (x) => (x == null ? '–' : `${Math.round(x * 100)}%`);
const usd = (x) => `$${(x || 0).toFixed(2)}`;
const tone = (r) => (r == null ? 'var(--surface-3)' : r >= 0.8 ? 'var(--good-soft)' : r >= 0.5 ? 'var(--warn-soft)' : 'var(--crit-soft)');

function render(el, ctx) {
  const { h } = ctx.core;
  el.append(h('style', {}, CSS));
  const root = h('div');
  el.append(root);
  let stop = null;
  const show = (path) => {
    try { stop?.(); } catch (e) { console.warn(e); }
    root.replaceChildren();
    const main = el.closest('main');
    if (main) main.scrollTop = 0;
    stop = path[1] ? sessionPage(root, ctx, path[0], path[1], path[2]) : path[0] ? runPage(root, ctx, path[0]) : homePage(root, ctx);
  };
  ctx.onPath(show);
  show(ctx.path());
  return () => stop?.();
}

const kpi = (h, label, val, sub) => h('div', { class: 'card kpi' }, h('div', { class: 'lbl' }, label), h('div', { class: 'val' }, val), sub ? h('div', { class: 'delta' }, sub) : null);

function homePage(root, ctx) {
  const { h, card, table, fmt, empty, failure, toast, every, pill } = ctx.core;
  root.append(h('div', { class: 'swm-head' }, h('h2', {}, 'Swarm'), h('span', { class: 'sub' }, 'Simulated people, each with a persona and a task, try ChloeDB in sandboxes of their own. Read what worked and what did not.')));
  const startBody = h('div');
  const runsBody = h('div');
  root.append(card({ title: 'Start a run', body: startBody }), h('div', { style: { height: '14px' } }), card({ title: 'Runs', flush: true, body: runsBody }));

  const runs = table([
    { key: 'name', label: 'Run', render: (r) => h('div', {}, h('b', {}, r.name), h('div', { class: 'small dim' }, r.id)) },
    { key: 'started', label: 'Started', render: (r) => fmt.ago ? fmt.ago(Date.parse(r.started)) : r.started.replace('T', ' ').slice(0, 16) },
    { key: 'status', label: 'Status', render: (r) => pill(r.status === 'done' ? 'ok' : r.status === 'running' || r.status === 'judging' ? 'pending' : 'warn', r.status === 'running' ? `running ${r.done}/${r.planned}` : r.status) },
    { key: 'sessions', label: 'Sessions', num: true, sort: (r) => r.planned, render: (r) => fmt.n(r.totals?.sessions ?? r.done) },
    { key: 'rate', label: 'Succeeded', num: true, sort: (r) => r.totals?.rate ?? -1, render: (r) => pct(r.totals?.rate) },
    { key: 'findings', label: 'Findings', num: true, sort: (r) => r.totals?.findings ?? -1, render: (r) => fmt.n(r.totals?.findings) },
    { key: 'usd', label: 'Cost', num: true, sort: (r) => r.cost?.usd || 0, render: (r) => usd(r.cost?.usd) },
  ], { sortKey: 'started', desc: true, onRow: (r) => { location.hash = ctx.href(r.id); }, empty: empty('users', 'No runs yet', 'Start one above: a starter set plays five personas on four tasks.') });
  runsBody.append(runs);

  let info = null;
  const loadInfo = async () => {
    try { info = await ctx.api('/v1/info', { timeout: 10000 }); } catch (e) { startBody.replaceChildren(failure(e, 'Swarm')); return; }
    const sets = info.scenarios.filter((s) => !s.error);
    if (!sets.length) { startBody.replaceChildren(empty('info', 'No scenario sets', 'Put a set (JSON) in the swarm\'s scenarios folder.')); return; }
    const sel = h('select', { class: 'input' }, sets.map((s) => h('option', { value: s.file }, `${s.name}: ${s.personas.length} personas × ${s.tasks.length} tasks × ${s.seeds} seeds`)));
    const budget = h('input', { class: 'input', type: 'number', min: '1', step: '1', value: String(sets[0].budget_usd) });
    const limit = h('input', { class: 'input', type: 'number', min: '0', step: '1', placeholder: 'all' });
    const go = h('button', { class: 'btn primary', disabled: !info.model_key || !!info.running }, 'Start');
    const desc = h('div', { class: 'swm-set' });
    const describe = () => {
      const s = sets.find((x) => x.file === sel.value);
      budget.value = String(s.budget_usd);
      desc.replaceChildren(
        h('div', {}, s.description),
        h('div', {}, h('b', {}, 'People: '), s.personas.map((p) => h('span', { class: 'swm-chip', title: p.role }, `${p.name} (${p.id}${p.device === 'phone' ? ', phone' : ''})`))),
        h('div', {}, h('b', {}, 'Tasks: '), s.tasks.map((t) => h('span', { class: 'swm-chip', title: t.prompt }, `${t.title} · ${t.surface}`))),
        h('div', {}, h('b', {}, 'Users played by: '), s.models.join(', '), ' (one per seed)'));
    };
    sel.onchange = describe;
    go.onclick = async () => {
      go.disabled = true;
      try {
        const r = await ctx.api('/v1/runs', { body: { scenario: sel.value, budget: Number(budget.value) || undefined, limit: Number(limit.value) || undefined } });
        toast('Run started', 'ok');
        if (r.run?.id) location.hash = ctx.href(r.run.id);
      } catch (e) { toast(e.message, 'bad'); go.disabled = false; }
    };
    startBody.replaceChildren(
      h('div', { class: 'swm-start' },
        h('label', { class: 'wide' }, 'Scenario set', sel),
        h('label', {}, 'Budget (USD)', budget),
        h('label', {}, 'Sessions', limit),
        go),
      desc,
      h('div', { class: 'swm-note' }, info.running ? `Run ${info.running.id} is playing (${info.running.done}/${info.running.planned}).` : !info.model_key ? 'The swarm has no model key: start it with ANTHROPIC_API_KEY set.' : 'Each session gets a chloedb-server of its own, its data and an API key that dies with it; nothing touches this cluster\'s tables except the swarm_* tables where results go. The budget caps what the models may cost; a quarter of it is kept for reviewing the sessions.'));
    describe();
  };
  const loadRuns = async () => {
    try { runs.update((await ctx.api('/v1/runs', { timeout: 10000 })).runs); } catch (e) { runsBody.replaceChildren(failure(e, 'Runs')); }
  };
  loadInfo();
  return every(5000, loadRuns);
}

function runPage(root, ctx, id) {
  const { h, card, table, fmt, empty, failure, toast, every, pill } = ctx.core;
  const head = h('div', { class: 'swm-head' }, h('a', { href: ctx.href(), class: 'small' }, '← Swarm'));
  const body = h('div');
  root.append(head, body);
  let last = '';
  const draw = (d) => {
    const { run, sessions, findings } = d;
    const sig = JSON.stringify([run.status, run.done, sessions.length, findings.length, run.cost?.usd]);
    if (sig === last) return;
    last = sig;
    const t = run.totals || {};
    const done = sessions.filter((s) => s.outcome !== 'running');
    const ok = done.filter((s) => s.success).length;
    const playing = run.status === 'running' || run.status === 'judging';
    const stopBtn = playing ? h('button', { class: 'btn sm', onclick: async () => { try { await ctx.api(`/v1/runs/${id}/stop`, { body: {} }); toast('Stopping after the sessions playing now', 'ok'); } catch (e) { toast(e.message, 'bad'); } } }, 'Stop') : null;
    head.replaceChildren(...[h('a', { href: ctx.href(), class: 'small' }, '← Swarm'), h('h2', {}, run.name), h('span', { class: 'sub' }, `${run.id} · started ${run.started.replace('T', ' ').slice(0, 16)} UTC`), pill(run.status === 'done' ? 'ok' : playing ? 'pending' : 'warn', run.status), stopBtn].filter(Boolean));
    const parts = [];
    if (playing) parts.push(h('div', { class: 'swm-bar' }, h('i', { style: { width: `${Math.round(100 * run.done / Math.max(1, run.planned))}%` } })));
    parts.push(h('div', { class: 'swm-kpis' },
      kpi(h, 'Succeeded', pct(t.rate ?? (done.length ? ok / done.length : null)), `${ok} of ${done.length} sessions`),
      kpi(h, 'Sessions', fmt.n(done.length), `of ${run.planned} planned`),
      kpi(h, 'Findings', fmt.n(findings.filter((f) => f.category !== 'strength').length), t.problems != null ? `from ${t.problems} problems` : 'after the review'),
      kpi(h, 'Agent mistakes', t.problems ? pct(t.agent_mistakes / t.problems) : '–', 'of the problems found'),
      kpi(h, 'Cost', usd(run.cost?.usd), `users ${usd(run.cost?.users?.usd)}, review ${usd(run.cost?.judge?.usd)}`)));

    // Success by persona and task.
    const personas = run.set.personas;
    const tasks = run.set.tasks;
    const cell = (ss) => {
      if (!ss.length) return h('span', { class: 'dim' }, '–');
      const fin = ss.filter((s) => s.outcome !== 'running');
      const r = fin.length ? fin.filter((s) => s.success).length / fin.length : null;
      return h('span', { class: 'swm-cell', style: { background: tone(r) }, title: ss.map((s) => `seed ${s.seed} (${s.model}): ${s.outcome}`).join('\n') }, `${fin.filter((s) => s.success).length}/${ss.length}`);
    };
    const matrix = h('table', { class: 'swm-matrix' },
      h('thead', {}, h('tr', {}, h('th', {}, 'Persona'), tasks.map((k) => h('th', {}, k)))),
      h('tbody', {}, personas.map((p) => h('tr', {}, h('td', {}, p), tasks.map((k) => h('td', {}, cell(sessions.filter((s) => s.persona.id === p && s.task.id === k))))))));
    const byModel = t.by_model ? h('div', { class: 'small dim', style: { marginTop: '10px' } }, 'By the model playing the user: ', Object.entries(t.by_model).map(([m, b]) => `${m} ${pct(b.rate)} of ${b.n}, ${b.steps} steps, ease ${b.ease ?? '–'}`).join(' · ')) : null;

    const bad = findings.filter((f) => f.category !== 'strength');
    const good = findings.filter((f) => f.category === 'strength');
    const glist = h('div', {}, good.map((f) => h('div', { class: 'swm-f' },
      h('div', { class: 't' }, h('span', { class: 'swm-chip swm-sev0' }, 'strength'), h('b', {}, f.title)),
      h('div', { class: 'meta' }, `${f.sessions.length} sessions (${pct(f.share)}) · `, f.personas.map((p) => h('span', { class: 'swm-chip' }, p)), f.tasks.map((k) => h('span', { class: 'swm-chip' }, k))),
      h('div', {}, f.summary),
      h('ul', {}, (f.evidence || []).map((e) => h('li', {}, h('a', { href: ctx.href(id, e.session, String(e.step)) }, `${e.session.replace(`${id}-`, '')} step ${e.step}`), ': ', e.detail))))));
    const flist = h('div', {}, bad.length ? bad.map((f) => h('div', { class: 'swm-f' },
      h('div', { class: 't' }, h('span', { class: `swm-chip swm-sev${f.severity}` }, ['', 'friction', 'costly', 'blocking'][f.severity] || f.severity), h('b', {}, `${f.rank}. ${f.title}`)),
      h('div', { class: 'meta' }, `${f.category.replace('_', ' ')} · ${f.sessions.length} sessions (${pct(f.share)}) · `, f.personas.map((p) => h('span', { class: 'swm-chip' }, p)), f.tasks.map((k) => h('span', { class: 'swm-chip' }, k))),
      h('div', {}, f.summary),
      h('div', { class: 'fix' }, h('b', {}, 'Fix: '), f.fix),
      h('ul', {}, (f.evidence || []).map((e) => h('li', {}, h('a', { href: ctx.href(id, e.session, String(e.step)) }, `${e.session.replace(`${id}-`, '')} step ${e.step}`), ': ', e.detail))))) : [empty('info', playing ? 'Findings come after the review' : 'No findings', playing ? 'Each session is reviewed when all have played.' : 'The reviews found nothing to report.')]);

    const stable = table([
      { key: 'persona', label: 'Persona', sort: (s) => s.persona.id, render: (s) => s.persona.id },
      { key: 'task', label: 'Task', sort: (s) => s.task.id, render: (s) => s.task.id },
      { key: 'seed', label: 'Seed', num: true },
      { key: 'model', label: 'User model', render: (s) => s.model.replace('claude-', '') },
      { key: 'outcome', label: 'Outcome', render: (s) => pill(s.success ? 'ok' : s.outcome === 'running' ? 'pending' : s.outcome === 'done' ? 'warn' : 'bad', s.success ? 'succeeded' : s.outcome === 'done' ? 'done, checks failed' : s.outcome.replace('_', ' ')) },
      { key: 'steps', label: 'Steps', num: true },
      { key: 'ease', label: 'Ease', num: true, sort: (s) => s.interview?.ease ?? -1, render: (s) => s.interview?.ease ?? '–' },
      { key: 'problems', label: 'Problems', num: true, sort: (s) => s.review?.problems?.length ?? -1, render: (s) => (s.review ? String(s.review.problems.length) : '–') },
      { key: 'cost', label: 'Cost', num: true, render: (s) => usd(s.cost) },
    ], { rows: sessions, sortKey: 'persona', desc: false, compact: true, onRow: (s) => { location.hash = ctx.href(id, s.id); } });

    body.replaceChildren(...parts,
      h('div', { class: 'swm-cols' },
        card({ title: 'Findings', sub: 'grouped from every session\'s review, most important first', body: flist }),
        h('div', { class: 'grid' }, card({ title: 'Success by persona and task', body: h('div', {}, matrix, byModel) }), good.length ? card({ title: 'Strengths', sub: 'what worked, to keep', body: glist }) : null)),
      h('div', { style: { height: '14px' } }),
      card({ title: 'Sessions', flush: true, body: stable }));
  };
  const load = async () => {
    try { draw(await ctx.api(`/v1/runs/${encodeURIComponent(id)}`, { timeout: 15000 })); } catch (e) { body.replaceChildren(failure(e, 'This run')); }
  };
  return every(4000, load);
}

function sessionPage(root, ctx, runId, sid, target) {
  const { h, card, empty, failure, pill } = ctx.core;
  const body = h('div');
  root.append(h('div', { class: 'swm-head' }, h('a', { href: ctx.href(runId), class: 'small' }, '← The run')), body);
  (async () => {
    try {
      const [d, st] = await Promise.all([ctx.api(`/v1/runs/${encodeURIComponent(runId)}`, { timeout: 15000 }), ctx.api(`/v1/sessions/${encodeURIComponent(sid)}/steps`, { timeout: 15000 })]);
      const s = d.sessions.find((x) => x.id === sid);
      if (!s) { body.replaceChildren(empty('info', 'No such session')); return; }
      const kv = (pairs) => h('dl', { class: 'swm-kv' }, pairs.filter(([, v]) => v != null && v !== '').flatMap(([k, v]) => [h('dt', {}, k), h('dd', {}, v)]));
      const iv = s.interview;
      const review = s.review;
      const steps = h('ol', { class: 'swm-steps' }, (st.steps || []).map((x) => {
        const input = x.input && Object.keys(x.input).length ? JSON.stringify(x.input) : '';
        return h('li', { id: `step-${x.step}`, class: String(x.step) === target ? 'target' : '' },
          h('span', { class: 'n' }, x.step),
          h('div', {},
            h('div', { class: 'act' }, x.tool, input ? ` ${input.length > 300 ? `${input.slice(0, 300)}…` : input}` : ''),
            x.thought ? h('div', { class: 'th' }, `“${x.thought}”`) : null,
            x.ok ? null : h('div', { class: 'bad' }, x.error || 'failed'),
            x.events?.length ? h('div', { class: 'bad' }, x.events.map((e) => `${e.kind}: ${e.text}`).join(' · ')) : null,
            x.result ? h('details', {}, h('summary', {}, x.tool === 'finish' ? 'answer' : 'what it saw'), h('pre', { class: 'code' }, x.result)) : null));
      }));
      body.replaceChildren(
        h('div', { class: 'swm-head' }, h('h2', {}, `${s.persona.name} · ${s.task.title}`), pill(s.success ? 'ok' : s.outcome === 'done' ? 'warn' : 'bad', s.success ? 'succeeded' : s.outcome.replace('_', ' ')), h('span', { class: 'sub' }, `seed ${s.seed} · ${s.model} · ${s.steps} steps · ${s.seconds} s · ${usd(s.cost)}`)),
        h('div', { class: 'swm-cols' },
          card({ title: 'Steps', sub: 'what the person did, thought and saw', body: steps.children.length ? steps : empty('info', 'No steps') }),
          h('div', { class: 'grid' },
            card({ title: 'Task', body: kv([['Person', `${s.persona.name}, ${s.persona.role}`], ['Device', s.persona.device], ['Surface', s.task.surface], ['Asked', h('div', { style: { whiteSpace: 'pre-wrap' } }, s.task.prompt.length > 600 ? `${s.task.prompt.slice(0, 600)}…` : s.task.prompt)]]) }),
            card({ title: 'Result', body: kv([['Answer', s.answer], ['Checks', h('div', {}, (s.checks || []).map((c) => h('div', {}, `${c.ok ? '✓' : '✗'} ${c.name}: ${c.detail}`)))], ['Error', s.error], ['Errors seen', `${s.errors.failed_steps} failed steps, ${s.errors.http5xx} server errors, ${s.errors.http4xx} refused calls, ${s.errors.console} console errors`]]) }),
            iv ? card({ title: 'Exit interview', body: kv([['Ease (1-7)', String(iv.ease)], ['Hardest', iv.hardest], ['Expected', iv.expected], ['One change', iv.one_change], ['Would use', iv.would_use]]) }) : null,
            review ? card({ title: 'Review', sub: review.verdict.replace('_', ' '), body: h('div', {}, h('p', { style: { marginTop: 0 } }, review.summary), review.problems.length ? h('ul', { style: { paddingLeft: '18px', margin: 0 } }, review.problems.map((p) => h('li', {}, h('span', { class: `swm-chip swm-sev${p.severity}` }, p.category.replace('_', ' ')), h('a', { href: ctx.href(runId, sid, String(p.step)) }, `step ${p.step}`), `: ${p.title}. `, h('span', { class: 'dim' }, p.detail)))) : h('div', { class: 'dim' }, 'No problems.'), (review.strengths || []).length ? h('ul', { style: { paddingLeft: '18px', margin: '8px 0 0' } }, review.strengths.map((p) => h('li', {}, h('span', { class: 'swm-chip swm-sev0' }, 'strength'), h('a', { href: ctx.href(runId, sid, String(p.step)) }, `step ${p.step}`), `: ${p.title}. `, h('span', { class: 'dim' }, p.detail)))) : null) }) : null)));
      if (target) document.getElementById(`step-${target}`)?.scrollIntoView({ block: 'center' });
    } catch (e) { body.replaceChildren(failure(e, 'This session')); }
  })();
  return () => {};
}
