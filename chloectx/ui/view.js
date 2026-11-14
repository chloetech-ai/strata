// ChloeCtx's page in ChloeDB's command center (#/app/chloectx). The command
// center loads this file through chloedb-server, which forwards
// /v1/apps/chloectx/... to `chloectx app`, and runs it with its own helpers:
// render(el, ctx) gets ctx.core (h, card, table, fmt, ...), ctx.api (the
// app's API) and ctx.core.api (the server's). It imports nothing, so the app
// serves it as one file.
//
// A space is picked first, then inspected. A space is any memory ChloeCtx
// keeps: a team space of the cluster (table ctx_<name>), a Claude Code
// project chloectx sync copied into the cluster (table chloectx_memory), or
// a Claude Code store of the machine the app runs on (chloectx app
// --local-stores). Each answers the same calls (sourceOf), so its page is
// drawn alike, with the tabs its kind has.
//
//   #/app/chloectx                      Spaces: every space this key reads, to pick one
//   #/app/chloectx/<kind>/<id>[/<tab>]  one space (kind: space, project or store), on a tab:
//       overview                        what it knows now: rules, decisions, facts, open to-dos, changes, sessions
//       ask                             a question answered from its memory, with why and what was left out
//       memories[/<kind>]               every note, current or with history
//       sessions[/<session>]            the sessions; one read as its conversation; or every record, searched
//       receipts[/<id>]                 (store) what each recall showed and why, replayed on its snapshot
//       blame[/<path:line>]             (store) from a line to the edit, its prompt and the records recalled before it
//       keep                            (store) kept snapshots, and capsules cut from it
//       connect                         Claude Code and other agents: the space's MCP command, its API, apps from it
//   #/app/chloectx/seen                 Seen before: firing alerts beside past incidents
// (The pages before spaces were picked first, #/app/chloectx/inspector,
// /receipts and /blame, open the matching tab.)

export default { render };

const CSS = `
.cx-top { display: flex; gap: 12px; align-items: center; flex-wrap: wrap; margin-bottom: 16px; }
.cx-top .grow { min-width: 0; }
.cx-top h2 { margin: 0; font-size: 18px; }
.cx-lede { color: var(--text-2); font-size: 13px; margin: 2px 0 0; max-width: 760px; }
.cx-crumbs { font-size: 12.5px; color: var(--muted); margin-bottom: 4px; }
.cx-crumbs a { color: var(--accent); text-decoration: none; }
.cx-status { display: flex; gap: 6px 16px; flex-wrap: wrap; font-size: 12px; color: var(--muted); margin: -6px 0 14px; }
.cx-status b { color: var(--text-2); font-weight: 600; }
.cx-spaces { display: grid; grid-template-columns: repeat(auto-fill, minmax(260px, 1fr)); gap: 12px; margin-bottom: 16px; }
.cx-space { display: block; padding: 14px 16px; text-decoration: none; color: inherit; }
.cx-space:hover { border-color: var(--line-2); box-shadow: var(--shadow-2); }
.cx-space h4, .cx-alert h4, .cx-inc h4, .cx-match h5 { text-transform: none; letter-spacing: 0; }
.cx-space h4 { margin: 0; font-size: 14.5px; }
.cx-ask textarea.input, .cx-form textarea.input, .cx-form input.input { font-family: var(--sans); }
.cx-space p { margin: 6px 0 10px; color: var(--text-2); font-size: 12.5px; line-height: 1.45; min-height: 18px; }
.cx-counts { display: flex; gap: 4px 12px; flex-wrap: wrap; font-size: 12px; color: var(--muted); }
.cx-counts b { color: var(--text); font-weight: 600; }
.cx-split { display: grid; grid-template-columns: minmax(0, 1.45fr) minmax(0, 1fr); gap: 16px; align-items: start; }
.cx-stack { display: grid; gap: 16px; }
.cx-ask { display: grid; gap: 10px; }
.cx-ask textarea.input { min-height: 64px; resize: vertical; font-size: 14px; }
.cx-askrow { display: flex; gap: 12px; align-items: center; flex-wrap: wrap; }
.cx-askrow input[type=range] { flex: 1 1 160px; min-width: 120px; accent-color: var(--accent); }
.cx-askrow .cx-bud { font-variant-numeric: tabular-nums; font-size: 12.5px; color: var(--text-2); min-width: 120px; }
.cx-meter { display: flex; height: 14px; border-radius: 7px; overflow: hidden; background: var(--surface-3); margin: 4px 0 6px; }
.cx-meter > i { display: block; height: 100%; border-right: 1px solid var(--surface); min-width: 2px; }
.cx-meter > i:last-child { border-right: 0; }
.cx-legend { display: flex; gap: 4px 14px; flex-wrap: wrap; font-size: 12px; color: var(--muted); }
.cx-legend span { display: inline-flex; align-items: center; gap: 6px; }
.cx-legend i { width: 10px; height: 10px; border-radius: 3px; display: inline-block; }
.cx-facts { display: flex; gap: 4px 18px; flex-wrap: wrap; margin: 12px 0 4px; font-size: 12.5px; color: var(--text-2); }
.cx-facts b { font-size: 15px; color: var(--text); font-variant-numeric: tabular-nums; margin-right: 4px; }
.cx-sub { margin: 16px 0 6px; font-size: 12px; font-weight: 600; color: var(--text-2); text-transform: uppercase; letter-spacing: .04em; }
.cx-list { list-style: none; margin: 0; padding: 0; }
.cx-list > li { display: grid; grid-template-columns: 8px minmax(0, 1fr) auto; gap: 10px; padding: 9px 0; border-top: 1px solid var(--line); align-items: start; }
.cx-list > li:first-child { border-top: 0; }
.cx-list > li.plain { grid-template-columns: minmax(0, 1fr) auto; }
.cx-sw { width: 8px; height: 100%; min-height: 32px; border-radius: 4px; }
.cx-text { font-size: 13.5px; line-height: 1.45; overflow-wrap: anywhere; }
.cx-text.old { color: var(--muted); text-decoration: line-through; text-decoration-color: var(--line-2); }
.cx-why { display: flex; gap: 4px 10px; flex-wrap: wrap; margin-top: 4px; font-size: 11.5px; color: var(--muted); }
.cx-why .was { color: var(--text-2); }
.cx-why .was s { color: var(--muted); }
.cx-tok { font-size: 12px; color: var(--muted); font-variant-numeric: tabular-nums; white-space: nowrap; text-align: right; }
.cx-acts { display: flex; gap: 4px; }
.cx-acts .btn { opacity: .75; }
.cx-acts .btn:hover { opacity: 1; }
.cx-context { margin-top: 12px; }
.cx-context summary { cursor: pointer; font-size: 12.5px; color: var(--text-2); }
.cx-context pre { white-space: pre-wrap; overflow-wrap: anywhere; max-height: 360px; overflow: auto; margin-top: 8px; font-size: 12px; }
.cx-form { display: grid; gap: 10px; }
.cx-form label { display: grid; gap: 4px; font-size: 12px; color: var(--text-2); }
.cx-form textarea.input { min-height: 60px; resize: vertical; }
.cx-form .cx-two { display: grid; grid-template-columns: 1fr 1fr; gap: 10px; }
.cx-hint { font-size: 12px; color: var(--muted); }
.cx-code { white-space: pre-wrap; overflow-wrap: anywhere; font-size: 12px; margin: 6px 0 12px; }
.cx-alert { display: grid; grid-template-columns: minmax(0, 1fr) minmax(0, 1.25fr); gap: 18px; padding: 16px 0; border-top: 1px solid var(--line); }
.cx-alert:first-child { border-top: 0; padding-top: 4px; }
.cx-alert h4 { margin: 6px 0 2px; font-size: 15px; }
.cx-alert .cx-desc { font-size: 12.5px; color: var(--text-2); line-height: 1.45; margin: 6px 0; }
.cx-val { font-size: 12.5px; font-variant-numeric: tabular-nums; }
.cx-val b { font-size: 16px; }
.cx-where { font-size: 12px; color: var(--muted); margin-top: 4px; }
.cx-match { padding: 10px 12px; border: 1px solid var(--line); border-radius: var(--r-sm); background: var(--surface-2); margin-bottom: 8px; }
.cx-match.best { border-color: var(--accent); background: var(--accent-soft); }
.cx-match h5 { margin: 0; font-size: 13.5px; display: flex; gap: 8px; align-items: center; flex-wrap: wrap; }
.cx-match dl { display: grid; grid-template-columns: 62px minmax(0, 1fr); gap: 3px 8px; margin: 8px 0 0; font-size: 12.5px; line-height: 1.4; }
.cx-match dt { color: var(--muted); }
.cx-match dd { margin: 0; overflow-wrap: anywhere; }
.cx-match dd.fix { font-weight: 600; }
.cx-pct { display: inline-flex; align-items: center; gap: 6px; font-size: 11.5px; color: var(--text-2); font-variant-numeric: tabular-nums; }
.cx-pct > span { width: 46px; height: 5px; border-radius: 3px; background: var(--surface-3); overflow: hidden; display: inline-block; }
.cx-pct > span > i { display: block; height: 100%; background: var(--accent); }
.cx-hand { display: flex; gap: 8px; align-items: center; flex-wrap: wrap; margin-top: 10px; }
.cx-inc { padding: 12px 0; border-top: 1px solid var(--line); }
.cx-inc:first-child { border-top: 0; padding-top: 0; }
.cx-inc h4 { margin: 0 0 4px; font-size: 14px; display: flex; gap: 8px; align-items: center; flex-wrap: wrap; }
.cx-inc dl { display: grid; grid-template-columns: 62px minmax(0, 1fr); gap: 3px 8px; margin: 6px 0 0; font-size: 12.5px; line-height: 1.4; }
.cx-inc dt { color: var(--muted); }
.cx-inc dd { margin: 0; overflow-wrap: anywhere; }
.cx-inc dd s { color: var(--muted); }
.cx-drafts { display: flex; gap: 6px; flex-wrap: wrap; }
.cx-make { margin-top: 16px; }
.cx-ideas { display: grid; grid-template-columns: repeat(auto-fill, minmax(260px, 1fr)); gap: 12px; margin-top: 12px; }
.cx-idea { border: 1px solid var(--line); border-radius: var(--r-sm); padding: 12px 14px; background: var(--surface-2); display: grid; gap: 6px; align-content: start; }
.cx-idea h4 { margin: 0; font-size: 14.5px; text-transform: none; letter-spacing: 0; }
.cx-idea p { margin: 0; font-size: 13px; line-height: 1.45; color: var(--text-2); }
.cx-idea .cx-why { margin: 0; }
.cx-idea .btn { justify-self: start; margin-top: 4px; }
.cx-builds { list-style: none; margin: 0 0 4px; padding: 0; }
.cx-builds > li { display: grid; grid-template-columns: minmax(0, 1fr) auto; gap: 10px; padding: 10px 0; border-top: 1px solid var(--line); align-items: center; }
.cx-builds > li:first-child { border-top: 0; padding-top: 2px; }
.cx-builds b { font-size: 13.5px; }
.cx-pick { list-style: none; margin: 0; padding: 0; max-height: 300px; overflow: auto; border: 1px solid var(--line); border-radius: var(--r-sm); }
.cx-pick > li { display: grid; grid-template-columns: 18px minmax(0, 1fr); gap: 8px; padding: 7px 10px; border-top: 1px solid var(--line); font-size: 12.5px; line-height: 1.4; }
.cx-pick > li:first-child { border-top: 0; }
.cx-pick input { margin-top: 2px; accent-color: var(--accent); }
.cx-kinds { display: flex; gap: 6px 14px; flex-wrap: wrap; font-size: 12.5px; }
.cx-kinds label { display: inline-flex; gap: 6px; align-items: center; color: var(--text); }
.cx-kinds input { accent-color: var(--accent); }
.cx-pickrow { display: flex; gap: 8px 12px; align-items: center; flex-wrap: wrap; margin: -4px 0 14px; font-size: 12.5px; color: var(--text-2); }
.cx-pickrow select.input { min-width: 260px; max-width: 100%; }
.cx-dec { display: inline-block; padding: 1px 7px; border-radius: 9px; font-size: 11.5px; white-space: nowrap; background: var(--surface-3); color: var(--text-2); }
.cx-dec.shown { background: var(--accent-soft); color: var(--accent); font-weight: 600; }
.cx-dec.know { background: color-mix(in srgb, var(--s7) 18%, transparent); color: var(--text); font-weight: 600; }
.cx-bar { display: inline-block; width: 56px; height: 5px; border-radius: 3px; background: var(--surface-3); overflow: hidden; vertical-align: middle; margin-left: 6px; }
.cx-bar > i { display: block; height: 100%; background: var(--accent); }
.cx-flag { color: var(--bad, #c0392b); font-size: 12px; }
.cx-verdict { display: grid; grid-template-columns: auto minmax(0, 1fr); gap: 8px 12px; align-items: start; padding: 10px 0; border-top: 1px solid var(--line); font-size: 13px; }
.cx-verdict:first-child { border-top: 0; padding-top: 2px; }
.cx-verdict ul { margin: 6px 0 0; padding-left: 18px; color: var(--text-2); font-size: 12.5px; }
.cx-kv { display: grid; grid-template-columns: 110px minmax(0, 1fr); gap: 6px 12px; font-size: 13px; line-height: 1.45; }
.cx-kv dt { color: var(--muted); }
.cx-kv dd { margin: 0; overflow-wrap: anywhere; }
.cx-blameq { display: flex; gap: 8px; flex-wrap: wrap; align-items: center; }
.cx-blameq input.input { flex: 1 1 320px; font-family: var(--mono); }
.cx-causes { margin: 0; padding-left: 18px; font-size: 13px; line-height: 1.5; }
.cx-cost { padding: 10px 12px; border-radius: var(--r-sm); background: var(--accent-soft); font-size: 13px; line-height: 1.45; }
.cx-insp { display: grid; grid-template-columns: minmax(220px, 300px) minmax(0, 1fr); gap: 16px; align-items: start; }
.cx-sess { list-style: none; margin: 0; padding: 0; max-height: 70vh; overflow: auto; }
.cx-sess > li > a { display: grid; gap: 2px; padding: 8px 10px; border-radius: var(--r-sm); text-decoration: none; color: inherit; border: 1px solid transparent; }
.cx-sess > li > a:hover { background: var(--surface-2); }
.cx-sess > li > a.on { border-color: var(--accent); background: var(--accent-soft); }
.cx-sess .mono { font-size: 11.5px; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.cx-sess .cx-why { margin: 0; }
.cx-filter { display: flex; gap: 8px; flex-wrap: wrap; align-items: center; margin-bottom: 10px; }
.cx-filter input.input { flex: 1 1 220px; min-width: 0; }
.cx-rec { display: grid; grid-template-columns: 8px minmax(0, 1fr); gap: 10px; padding: 9px 0; border-top: 1px solid var(--line); cursor: pointer; }
.cx-rec:first-child { border-top: 0; }
.cx-rec:hover .cx-text { color: var(--accent); }
.cx-rec .cx-text { display: -webkit-box; -webkit-line-clamp: 4; -webkit-box-orient: vertical; overflow: hidden; white-space: pre-wrap; }
.cx-rec.tool .cx-text { font-family: var(--mono); font-size: 12px; -webkit-line-clamp: 2; color: var(--text-2); }
.cx-rec.turn { margin-top: 10px; border-top: 1px solid var(--line-2); }
.cx-full { font-family: var(--sans); white-space: pre-wrap; overflow-wrap: anywhere; font-size: 13px; line-height: 1.5; margin: 0; }
.cx-dl { display: grid; grid-template-columns: 90px minmax(0, 1fr); gap: 4px 10px; font-size: 12.5px; margin: 12px 0; }
.cx-dl dt { color: var(--muted); }
.cx-dl dd { margin: 0; overflow-wrap: anywhere; }
/* Numbered steps and copyable commands, as the command center's "Start here" draws them. */
.cx-steps { list-style: none; margin: 0; padding: 0; display: flex; flex-direction: column; gap: 10px; }
.cx-steps > li { display: flex; gap: 14px; align-items: flex-start; padding: 14px 16px; border: 1px solid var(--line); border-radius: var(--r); background: var(--surface); font-size: 13px; line-height: 1.5; color: var(--text-2); }
.cx-steps > li > div { flex: 1; min-width: 0; display: flex; flex-direction: column; gap: 6px; }
.cx-steps > li b { color: var(--text); }
.cx-steps > li.done { background: var(--surface-2); }
.cx-n { flex: none; width: 26px; height: 26px; border-radius: 50%; display: inline-flex; align-items: center; justify-content: center; font-weight: 700; background: var(--accent-soft); color: var(--accent); }
.cx-steps > li.done .cx-n { background: var(--good-soft); color: var(--good-ink); }
.cx-wrap { position: relative; }
.cx-wrap .cx-copy { position: absolute; top: 6px; right: 6px; }
pre.code.cx-cmd { margin: 0; white-space: pre-wrap; overflow-wrap: anywhere; font-size: 12px; padding-right: 72px; }
.cx-sphd { display: flex; gap: 8px; align-items: center; justify-content: space-between; flex-wrap: wrap; }
.cx-sphd h4 { margin: 0; flex: 1 1 auto; min-width: 0; overflow-wrap: anywhere; }
.cx-path { font-size: 11.5px !important; overflow-wrap: anywhere; }
.cx-group { display: flex; gap: 10px; align-items: baseline; flex-wrap: wrap; margin: 6px 0 10px; }
.cx-group h3 { margin: 0; font-size: 14px; }
.cx-none { margin: 0 0 16px; }
.cx-more { margin: 4px 0 16px; }
.cx-more > summary { cursor: pointer; font-size: 12.5px; color: var(--text-2); margin-bottom: 10px; }
.cx-badges { display: flex; gap: 6px; flex-wrap: wrap; margin: 6px 0 0; }
.cx-badges a.badge { text-decoration: none; }
.cx-switchl { display: grid; gap: 3px; }
.cx-switch { min-width: 220px; max-width: 320px; }
.cx-tabs { margin-top: -4px; }
.cx-kpis { display: grid; grid-template-columns: repeat(6, minmax(0, 1fr)); gap: 10px; margin-bottom: 12px; }
.cx-kpi { text-decoration: none; color: inherit; }
a.cx-kpi:hover { border-color: var(--line-2); box-shadow: var(--shadow-2); }
.cx-ov { display: grid; grid-template-columns: minmax(0, 1.3fr) minmax(0, 1fr); gap: 16px; align-items: start; }
.cx-note { cursor: pointer; }
.cx-note:hover .cx-text { color: var(--accent); }
.cx-note .cx-text.old { color: var(--muted); }
.cx-slink { text-decoration: none; color: inherit; display: block; }
.cx-slink:hover .cx-text { color: var(--accent); }
.cx-answer { margin-top: 14px; }
.cx-lead { border-left: 3px solid var(--accent); background: var(--accent-soft); border-radius: 0 var(--r-sm) var(--r-sm) 0; padding: 12px 14px; display: grid; gap: 6px; }
.cx-leadk { display: flex; gap: 8px; align-items: center; flex-wrap: wrap; font-size: 12px; color: var(--text-2); }
.cx-leadt { font-size: 16px; line-height: 1.45; color: var(--text); font-weight: 500; overflow-wrap: anywhere; }
.cx-leadw { font-size: 12.5px; color: var(--text-2); }
.cx-leadw s { color: var(--muted); }
.cx-on { font-variant-numeric: tabular-nums; }
.cx-cell { white-space: normal; line-height: 1.4; min-width: 240px; max-width: 560px; overflow-wrap: anywhere; }
.cx-stitle { font-size: 13px; line-height: 1.35; display: -webkit-box; -webkit-line-clamp: 2; -webkit-box-orient: vertical; overflow: hidden; }
.cx-turn { padding: 12px 0; border-top: 1px solid var(--line); display: grid; gap: 8px; }
.cx-turn:first-child { border-top: 0; padding-top: 0; }
.cx-turnh { display: flex; gap: 10px; font-size: 11.5px; font-weight: 600; color: var(--text-2); text-transform: uppercase; letter-spacing: .04em; }
.cx-turnh .muted { font-weight: 400; text-transform: none; letter-spacing: 0; }
.cx-msg { display: grid; grid-template-columns: 56px minmax(0, 1fr); gap: 10px; cursor: pointer; }
.cx-who { font-size: 11.5px; font-weight: 600; color: var(--muted); padding-top: 2px; }
.cx-body { font-size: 13.5px; line-height: 1.5; white-space: pre-wrap; overflow-wrap: anywhere; }
.cx-msg.you .cx-body { background: var(--surface-2); border: 1px solid var(--line); border-radius: var(--r-sm); padding: 8px 10px; }
.cx-msg.you .cx-who { color: var(--accent); }
.cx-tools { margin-left: 66px; font-size: 12px; }
.cx-tools > summary { cursor: pointer; color: var(--text-2); }
.cx-tools > div { display: grid; gap: 4px; margin-top: 6px; }
.cx-tool { font-family: var(--mono); font-size: 11.5px; white-space: pre-wrap; overflow-wrap: anywhere; color: var(--text-2); padding: 4px 8px; border-left: 2px solid var(--line-2); cursor: pointer; max-height: 4.5em; overflow: hidden; }
.cx-tool.res { color: var(--muted); border-left-color: var(--line); }
.cx-tool.err { margin-left: 66px; color: var(--crit-ink, #b42318); border-left-color: var(--crit, #d92d20); }
.cx-wrote { display: grid; grid-template-columns: 56px minmax(0, 1fr); gap: 10px; padding: 6px 0 0; }
.cx-wrote .cx-who { color: var(--s4); }
.cx-wrote .cx-list > li { padding: 5px 0; }
.cx-caps { display: grid; gap: 10px; }
.cx-cap { border: 1px solid var(--line); border-radius: var(--r-sm); padding: 10px 12px; background: var(--surface-2); display: grid; gap: 6px; }
.cx-graph { display: grid; grid-template-columns: minmax(0, 1fr) 300px; gap: 12px; align-items: start; }
.cx-gbar { gap: 10px; align-items: center; }
.cx-gopts { display: flex; flex-wrap: wrap; gap: 4px 12px; font-size: 13px; color: var(--text-2); }
.cx-gopts label { display: inline-flex; gap: 5px; align-items: center; }
.cx-gwrap { position: relative; border: 1px solid var(--line); border-radius: var(--r-sm); background: var(--surface-2); overflow: hidden; }
.cx-gsvg { display: block; width: 100%; height: min(72vh, 700px); touch-action: none; cursor: grab; user-select: none; }
.cx-gsvg .e { stroke: var(--line-2); stroke-width: 1.1; }
.cx-gsvg .e.from { stroke-dasharray: 1 3; }
.cx-gsvg .e.sup { stroke: var(--muted); stroke-dasharray: 5 3; stroke-width: 1.4; }
.cx-gsvg .e.hl { stroke: var(--accent); stroke-width: 2; stroke-dasharray: none; }
.cx-gsvg .n { cursor: pointer; stroke-width: 1.5; }
.cx-gsvg .n.hl { stroke: var(--accent); stroke-width: 3; }
.cx-gsvg .dim { opacity: .14; }
.cx-gsvg text { font-size: 10.5px; fill: var(--text-2); paint-order: stroke; stroke: var(--surface-2); stroke-width: 3px; stroke-linejoin: round; pointer-events: none; }
.cx-gsvg text.big { font-size: 12.5px; font-weight: 600; fill: var(--text); }
.cx-gsvg text.minor { display: none; }
.cx-gsvg.near text.minor, .cx-gsvg text.minor.on { display: inline; }
.cx-glegend { position: absolute; left: 8px; bottom: 6px; display: flex; flex-wrap: wrap; gap: 4px 10px; font-size: 11px; color: var(--text-2); background: var(--surface-2); padding: 2px 6px; border-radius: 4px; pointer-events: none; }
.cx-gdot { display: inline-block; width: 8px; height: 8px; border-radius: 50%; margin-right: 5px; vertical-align: 0; flex: none; }
.cx-gside { max-height: min(72vh, 700px); overflow: auto; font-size: 13px; }
.cx-gtitle { font-size: 14px; line-height: 1.45; margin-top: 6px; color: var(--text); overflow-wrap: anywhere; }
.cx-gtitle.old { text-decoration: line-through; color: var(--muted); }
.cx-gnear { margin-top: 10px; }
.cx-gnear a.cx-gold { text-decoration: line-through; color: var(--muted); }
.cx-gh { font-size: 11px; text-transform: uppercase; letter-spacing: .04em; color: var(--muted); margin-bottom: 4px; }
.cx-gnear ul { list-style: none; margin: 0; padding: 0; display: grid; gap: 4px; }
.cx-gnear li { display: flex; align-items: baseline; line-height: 1.35; }
@media (max-width: 900px) {
  .cx-kpis { grid-template-columns: repeat(3, minmax(0, 1fr)); }
  .cx-ov { grid-template-columns: minmax(0, 1fr); }
  .cx-switch { min-width: 0; max-width: 100%; }
  .cx-tools, .cx-tool.err { margin-left: 0; }
  .cx-msg, .cx-wrote { grid-template-columns: minmax(0, 1fr); gap: 2px; }
  .cx-insp { grid-template-columns: minmax(0, 1fr); }
  .cx-graph { grid-template-columns: minmax(0, 1fr); }
  .cx-gsvg { height: 62vh; }
  .cx-gside { max-height: none; }
  .cx-split, .cx-alert { grid-template-columns: minmax(0, 1fr); }
  .cx-form .cx-two { grid-template-columns: minmax(0, 1fr); }
}`;

const SECTION = {
  rule: ["var(--s7)", "rule", "Rules that apply go in first: close in meaning to the question, found by its words, or every rule of a small space"],
  note: ["var(--s1)", "note", "A note the searches found for the question"],
  timeline: ["var(--s3)", "timeline", "Every note, oldest first: the question asks about the whole history"],
  reply: ["var(--s4)", "earlier reply", "A past exchange whose reply answers something like this"],
  excerpt: ["var(--s5)", "excerpt", "A passage of a past exchange the searches found"],
};
const KINDS = [["decision", "Decision"], ["rule", "Rule"], ["fact", "Fact"], ["todo", "To do"], ["note", "Note"]];
const KIND_LABEL = Object.fromEntries(KINDS.map(([k, l]) => [k, l]));
const LEFT = { superseded: ["Replaced", "A later note replaced it; the context shows the change on the note that replaced it"], retired: ["Retired", "Retired by hand, with nothing in its place"], uncertain: ["Uncertain", "Marked uncertain: kept out unless asked for"], budget: ["No room", "Found, but the budget was spent on closer matches"] };
// A note that no longer holds: replaced by a later one, or retired by hand
// (tagged "retired"; before, a retired note was "superseded" with nothing
// that replaced it).
const closed = (n) => n.tag === "superseded" || n.tag === "retired";
const closedWord = (n) => (n.tag === "superseded" && n.superseded_by?.length ? "replaced" : "retired");
const day = (secs) => (secs ? new Date(secs * 1000).toLocaleDateString(undefined, { year: "numeric", month: "short", day: "numeric" }) : "–");

function render(el, ctx) {
  // A drawer closes when the page changes under it (Back, or a link inside it).
  const core = ctx.core;
  ctx = { ...ctx, core: { ...core, drawer: (o) => {
    const off = () => d.close();
    const d = core.drawer({ ...o, onClose: () => { removeEventListener("hashchange", off); o.onClose?.(); } });
    addEventListener("hashchange", off);
    return d;
  } } };
  const { h } = ctx.core;
  el.append(h("style", {}, CSS));
  const root = h("div");
  el.append(root);
  let stop = null;
  const show = (path) => {
    try { stop?.(); } catch (e) { console.warn(e); }
    stop = null;
    root.replaceChildren();
    const main = el.closest("main");
    if (main) main.scrollTop = 0;
    // The pages before spaces were picked first: each now opens a space's tab.
    const [a, b, c] = path;
    if (a === "inspector") { location.replace(b ? ctx.href("project", b, "sessions", ...(c ? [c] : [])) : ctx.href()); return; }
    if ((a === "receipts" || a === "blame") && (b || pickedStore())) { location.replace(ctx.href("store", b || pickedStore(), a, ...(c ? [c] : []))); return; }
    if (a === "receipts" || a === "blame") { location.replace(ctx.href()); return; }
    stop = a === "seen" ? seenPage(root, ctx)
      : (a === "space" || a === "project" || a === "store") && b ? spacePage(root, ctx, sourceOf(ctx, a, b), path.slice(2))
      : spacesPage(root, ctx);
  };
  ctx.onPath(show);
  show(ctx.path());
  return () => stop?.();
}

// ---------------------------------------------------------------- shared bits

// Whether this chloectx app reads its machine's stores (--local-stores):
// null until /v1/info says.
let LOCAL = null;
let INFO = null;
function info(ctx) {
  if (!INFO) INFO = ctx.api("/v1/info").then((i) => { LOCAL = !!i.local; return i; }).catch((e) => { INFO = null; throw e; });
  return INFO;
}

/** The page's top: its title, a line of what it is, its tools, and the two pages of the app. */
function header(ctx, on, title, lede, tools, crumbs) {
  const { h, seg } = ctx.core;
  const top = seg([["spaces", "Spaces"], ["seen", "Seen before", "Firing alerts beside the past incidents they look like"]], on, (v) => { location.hash = v === "spaces" ? ctx.href() : ctx.href(v); });
  return h("div", { class: "cx-top" },
    h("div", { class: "grow" }, crumbs ? h("div", { class: "cx-crumbs" }, crumbs) : null, h("h2", {}, title), lede ? h("p", { class: "cx-lede" }, lede) : null),
    tools, on ? top : null);
}

function kindBadge(ctx, kind) {
  const k = kind === "preference" ? "rule" : kind;
  return ctx.core.h("span", { class: "badge" + (k === "rule" ? " accent" : "") }, KIND_LABEL[k] || k);
}

function modelLine(core, info) {
  const m = info?.model || {};
  if (m.how === "api") return `distilled by ${m.model} through Anthropic's API`;
  if (m.how === "cli") return `distilled by the claude CLI (${m.model})`;
  return "not distilled: start chloectx app with ANTHROPIC_API_KEY, or with the claude CLI on its PATH";
}

const base = (p) => (p || "").replace(/\/+$/, "").split("/").pop() || p;
const kindOf = (k) => (k === "preference" ? "rule" : k);

/**
 * A space to inspect, whatever keeps it: a team space of the cluster
 * (`space`), a Claude Code project chloectx sync copied into the cluster
 * (`project`), or a Claude Code store on the machine this app runs on
 * (`store`, with --local-stores). Each answers the same calls, so the
 * space's page draws them alike; `tabs` are those its kind has.
 */
function sourceOf(ctx, type, id) {
  const e = encodeURIComponent(id);
  const api = ctx.api;
  if (type === "space") {
    const b = `/v1/spaces/${e}`;
    return {
      type, id, writes: true, label: "Team space",
      overview: () => api(`${b}/overview`),
      notes: (all) => api(`${b}/notes?limit=1000${all ? "&all=1" : ""}`).then((j) => j.notes || []),
      history: (n) => api(`${b}/notes/${encodeURIComponent(n)}`),
      sessions: () => api(`${b}/sessions`),
      records: (qs) => api(`${b}/records?${qs}`),
      graph: () => api(`${b}/graph`),
      ask: (question, budget) => api(`${b}/ask`, { body: { question, budget } }),
      tabs: ["overview", "ask", "memories", "sessions", "graph", "connect"],
    };
  }
  if (type === "store") {
    const b = `/v1/local/${e}`;
    return {
      type, id, label: "Claude Code store on this machine",
      overview: () => api(`${b}/overview`, { timeout: 60000 }),
      notes: (all) => api(`${b}/notes?limit=1000${all ? "&all=1" : ""}`).then((j) => j.notes || []),
      history: (n) => api(`${b}/notes/${encodeURIComponent(n)}`),
      sessions: () => api(`${b}/sessions`),
      records: (qs) => api(`${b}/records?${qs}`),
      graph: () => api(`${b}/graph`, { timeout: 60000 }),
      ask: (question, budget) => api(`${b}/ask`, { body: { question, budget } }),
      tabs: ["overview", "ask", "memories", "sessions", "graph", "receipts", "blame", "keep", "connect"],
    };
  }
  const p = `project=${e}`;
  let cache = null;
  const notes = (all) => api(`/v1/memory/notes?${p}${all ? "&all=1" : ""}`).then((j) => j.notes || []);
  return {
    type, id, label: "Claude Code project, synced to this cluster",
    overview: () => api(`/v1/memory/overview?${p}`),
    notes,
    // The synced copy keeps a note's links: its history is walked here.
    history: async (n) => {
      cache = cache || await notes(true);
      const by = new Map(cache.map((x) => [x.id, x]));
      const walk = (start, key) => { const out = [], seen = new Set([start.id]); let next = [...(start[key] || [])]; while (next.length && out.length < 50) { const id = next.shift(); const x = by.get(id); if (x && !seen.has(id)) { seen.add(id); out.push(x); next.push(...(x[key] || [])); } } return out; };
      const note = by.get(n);
      if (!note) throw new Error(`no note ${n}`);
      return { note, earlier: walk(note, "supersedes"), later: walk(note, "superseded_by") };
    },
    sessions: () => api(`/v1/memory/sessions?${p}`),
    records: (qs) => api(`/v1/memory/records?${p}&${qs}`),
    graph: () => api(`/v1/memory/graph?${p}`),
    // No store to build a context from: the synced rows, ranked by BM25.
    ask: async (question) => {
      const t0 = performance.now();
      const j = await api(`/v1/memory/records?${p}&q=${encodeURIComponent(question)}&limit=40`);
      // A note that was replaced or done is history, not an answer.
      cache = cache || await notes(true);
      const past = new Set(cache.filter((x) => closed(x) || x.tag === "done").map((x) => x.id));
      const keep = (j.records || []).filter((r) => r.kind !== "entity" && r.kind !== "tool_call" && r.kind !== "tool_result" && !past.has(r.id) && !closed(r) && r.tag !== "done");
      const items = keep.slice(0, 14).map((r, i) => ({ ...r, kind: r.kind === "prompt" ? "user" : r.kind === "answer" ? "assistant" : kindOf(r.kind), section: r.kind === "preference" ? "rule" : r.kind === "prompt" || r.kind === "answer" ? "excerpt" : "note", rank: i, tokens: Math.ceil((r.text || "").length / 4) }));
      return { question, items, left_out: [], tokens: items.reduce((a, x) => a + x.tokens, 0), budget: null, ms: performance.now() - t0, words_only: true, context: items.map((x) => x.text).join("\n\n"), chars: items.reduce((a, x) => a + x.text.length, 0) };
    },
    tabs: ["overview", "ask", "memories", "sessions", "graph", "connect"],
  };
}

const TABS = {
  overview: ["Overview", "What it knows now"],
  ask: ["Ask", "A question, answered from its memory"],
  memories: ["Memories", "Every rule, decision, fact and to-do, with its history"],
  sessions: ["Sessions", "Each session read as the conversation it recorded"],
  graph: ["Graph", "How its notes, subjects, files and sessions are connected"],
  receipts: ["Receipts", "What each recall showed and why, replayed"],
  blame: ["Blame", "From a line of code to the memory behind it"],
  keep: ["Snapshots and capsules", "Copies of the store, and capsules cut from it"],
  connect: ["Connect", "Use it from Claude Code or another agent"],
};

/**
 * Every space this key can inspect, for the picker and the switcher:
 * the team spaces, then the Claude Code projects (a store of this machine
 * and its synced copy are one, by the repository's name).
 */
async function catalog(ctx) {
  const i = await info(ctx).catch(() => null);
  const [sp, mem, loc] = await Promise.allSettled([ctx.api("/v1/spaces"), ctx.api("/v1/memory"), i?.local ? ctx.api("/v1/local") : Promise.resolve(null)]);
  const out = { team: [], incidents: [], claude: [], memory: mem.value || null, errors: {} };
  if (sp.status === "rejected") out.errors.spaces = sp.reason;
  if (mem.status === "rejected") out.errors.memory = mem.reason;
  if (loc.status === "rejected") out.errors.local = loc.reason;
  for (const s of sp.value?.spaces || []) (s.kind === "incidents" ? out.incidents : out.team).push({ type: "space", id: s.name, title: s.title || s.name, s });
  const synced = (mem.value?.projects || []).map((p) => ({ type: "project", id: p.project, title: p.project, p }));
  const stores = (loc.value?.stores || []).map((s) => ({ type: "store", id: s.id, title: base(s.project) || s.id, s }));
  for (const st of stores) {
    const twin = synced.find((x) => !x.merged && base(x.id) === st.title);
    if (twin) { twin.merged = true; st.synced = twin.id; }
  }
  // Stores whose folder is gone or temporary come last.
  const tmp = (s) => !s.s.project_exists || /^\/(tmp|var\/folders|private\/var)\//.test(s.s.project || "");
  stores.sort((a, b) => (tmp(a) - tmp(b)) || (b.s.modified || 0) - (a.s.modified || 0));
  out.claude = [...stores, ...synced.filter((x) => !x.merged)];
  out.home = loc.value?.home;
  return out;
}

function knows(ctx, by) {
  const { h, fmt } = ctx.core;
  const parts = [["preference", "rule"], ["decision", "decision"], ["fact", "fact"], ["todo", "to-do"]].filter(([k]) => by[k]).map(([k, w]) => h("span", {}, h("b", {}, fmt.n(by[k])), " ", by[k] === 1 ? w : w + "s"));
  return parts.length ? parts : [h("span", {}, "no notes yet")];
}

// ---------------------------------------------------------------- the spaces

function spacesPage(root, ctx) {
  const { h, empty, failure, fmt, icon } = ctx.core;
  const filter = h("input", { class: "input", type: "search", placeholder: "Find a space", style: { width: "200px" } });
  const newBtn = h("button", { class: "btn primary", onclick: () => newSpace(ctx) }, icon("plus"), "New space");
  root.append(header(ctx, "spaces", "Context spaces",
    "Every memory ChloeCtx keeps that this key may read: the cluster's team spaces, and the memory of Claude Code projects. Pick one to see what it knows, ask it a question, read its sessions and, for a Claude Code store, what each recall showed and why.",
    h("div", { class: "row", style: { gap: "8px" } }, filter, newBtn)));
  const status = h("div", { class: "cx-status" });
  const body = h("div", {}, h("div", { class: "muted small" }, "Loading the spaces…"));
  root.append(status, body);
  let cat = null, all = false;
  const card = (sp) => {
    const href = sp.type === "space" && sp.s.kind === "incidents" ? ctx.href("seen") : ctx.href(sp.type, sp.id);
    if (sp.type === "space") {
      const s = sp.s, all = s.by_kind || {}, by = s.live_by_kind || all;
      const said = (all.prompt || 0) + (all.answer || 0);
      return h("a", { class: "card cx-space", href },
        h("div", { class: "cx-sphd" }, h("h4", {}, sp.title), h("span", { class: "badge mono" }, s.table)),
        h("p", {}, s.description || ""),
        h("div", { class: "cx-counts" }, s.kind === "incidents" ? h("span", {}, h("b", {}, fmt.n(by.note || 0)), " incidents") : knows(ctx, by),
          said ? h("span", {}, fmt.plural(said, "message")) : null, s.created ? h("span", {}, "since " + day(s.created)) : null));
    }
    if (sp.type === "store") {
      const s = sp.s;
      return h("a", { class: "card cx-space", href },
        h("div", { class: "cx-sphd" }, h("h4", {}, sp.title), h("span", { class: "badge" }, "this machine"), sp.synced ? h("span", { class: "badge accent", title: `chloectx sync copies it into this cluster as ${sp.synced}` }, "synced") : null),
        h("p", { class: "mono cx-path" }, s.project || s.dir || s.id),
        h("div", { class: "cx-counts" }, h("span", {}, "last used ", h("b", {}, s.modified ? fmt.ago(s.modified * 1000) : "–")), h("span", {}, fmt.bytes(s.bytes)),
          s.running ? null : h("span", { title: "Opening it starts its daemon" }, "daemon stopped"), s.project_exists ? null : h("span", {}, "folder gone")));
    }
    const p = sp.p;
    return h("a", { class: "card cx-space", href },
      h("div", { class: "cx-sphd" }, h("h4", { class: "mono" }, sp.title), h("span", { class: "badge accent" }, "synced")),
      h("p", {}, "Claude Code's memory of this repository, as chloectx sync copies it into the table chloectx_memory"),
      h("div", { class: "cx-counts" }, h("span", {}, h("b", {}, fmt.n(p.sessions)), p.sessions === 1 ? " session" : " sessions"), h("span", {}, fmt.plural(p.records, "record")), p.last ? h("span", {}, "last " + when(p.last)) : null));
  };
  const draw = () => {
    if (!cat) return;
    const q = filter.value.trim().toLowerCase();
    const hit = (sp) => !q || `${sp.id} ${sp.title} ${sp.s?.description || ""} ${sp.s?.project || ""}`.toLowerCase().includes(q);
    const team = [...cat.team, ...cat.incidents].filter(hit);
    const claude = cat.claude.filter(hit);
    const shown = all || q ? claude : claude.slice(0, 9);
    const parts = [];
    parts.push(h("div", { class: "cx-group" }, h("h3", {}, "Team spaces"), h("span", { class: "muted small" }, "tables of this cluster that agents write notes to and ask for context")));
    if (cat.errors.spaces) parts.push(failure(cat.errors.spaces, "The spaces"));
    else if (team.length) parts.push(h("div", { class: "cx-spaces" }, team.map(card)));
    else if (!q) parts.push(empty("layers", "No team space yet", "A space holds one team's or one agent's notes: make one, then connect Claude Code to it.", h("button", { class: "btn sm primary", onclick: () => newSpace(ctx) }, "New space")));
    else parts.push(h("div", { class: "muted small cx-none" }, "No team space matches."));
    parts.push(h("div", { class: "cx-group" }, h("h3", {}, "Claude Code projects"), h("span", { class: "muted small" }, LOCAL ? "the stores of the machine this app runs on, and the projects synced to this cluster" : "the projects chloectx sync copied into this cluster")));
    if (claude.length) {
      parts.push(h("div", { class: "cx-spaces" }, shown.map(card)));
      if (shown.length < claude.length) parts.push(h("button", { class: "btn sm", style: { marginBottom: "16px" }, onclick: () => { all = true; draw(); } }, `Show all ${claude.length}`));
    } else if (q) parts.push(h("div", { class: "muted small cx-none" }, "No project matches."));
    else if (cat.errors.memory) parts.push(failure(cat.errors.memory, "The synced projects"));
    if (!q && cat.memory && (!cat.memory.exists || !cat.memory.projects.length)) parts.push(syncSetup(ctx, cat.memory, location.origin));
    else if (!q && cat.memory) parts.push(h("details", { class: "cx-more" }, h("summary", {}, "Sync another machine's Claude Code memory here"), syncSetup(ctx, cat.memory, location.origin)));
    body.replaceChildren(...parts);
  };
  filter.oninput = draw;
  (async () => {
    const i = await info(ctx).catch(() => null);
    if (i) status.replaceChildren(
      h("span", {}, h("b", {}, "Search "), `BM25 and ${i.embedder} vectors (${i.dim} dimensions)`),
      h("span", {}, h("b", {}, "Conversations "), modelLine(ctx.core, i)),
      ...(i.distill?.running ? [h("span", {}, `${i.distill.running} distilling now`)] : []));
    try { cat = await catalog(ctx); } catch (e) { body.replaceChildren(failure(e, "The spaces")); return; }
    draw();
  })();
  return null;
}

function newSpace(ctx) {
  const { h, modal, toast } = ctx.core;
  const name = h("input", { class: "input", placeholder: "billing", autocomplete: "off", spellcheck: "false" });
  const title = h("input", { class: "input", placeholder: "Billing service" });
  const desc = h("textarea", { class: "input", rows: 2, placeholder: "What the agents of the billing service know" });
  const go = h("button", { class: "btn primary" }, "Make it");
  const m = modal({
    title: "New space",
    body: h("div", { class: "cx-form" },
      h("label", {}, "Name (lowercase letters, digits and _; its table is ctx_<name>)", name),
      h("label", {}, "Title", title), h("label", {}, "What it holds", desc)),
    foot: [h("button", { class: "btn", onclick: () => m.close() }, "Cancel"), go],
  });
  go.onclick = async () => {
    go.disabled = true;
    try {
      const s = await ctx.api("/v1/spaces", { body: { name: name.value.trim(), title: title.value.trim(), description: desc.value.trim() } });
      m.close();
      location.hash = ctx.href("space", s.name, "connect");
    } catch (e) { toast(e.message, "bad"); go.disabled = false; }
  };
}

// ---------------------------------------------------------------- one space

/**
 * A space's page: its header (what it is, a switcher to another space), its
 * tabs, and the tab `rest[0]` (with what follows it: a session, a receipt,
 * a line to blame).
 */
function spacePage(root, ctx, src, rest) {
  const { h, icon } = ctx.core;
  const tab = src.tabs.includes(rest[0]) ? rest[0] : "overview";
  const arg = rest.slice(1).join("/");
  const title = h("h2", {}, src.type === "store" ? src.id : src.id);
  const lede = h("p", { class: "cx-lede" });
  const badges = h("div", { class: "cx-badges" }, h("span", { class: "badge" }, src.label));
  const switcher = h("select", { class: "input cx-switch", "aria-label": "Another space" }, h("option", {}, "Loading…"));
  root.append(h("div", { class: "cx-top" },
    h("div", { class: "grow" }, h("div", { class: "cx-crumbs" }, h("a", { href: ctx.href() }, "Spaces"), " / ", src.id), title, badges, lede),
    h("label", { class: "cx-switchl" }, h("span", { class: "muted small" }, "Space"), switcher)));
  const tabs = h("div", { class: "tabs cx-tabs", role: "tablist" }, src.tabs.map((t) => h("button", {
    role: "tab", "aria-selected": String(t === tab), title: TABS[t][1],
    onclick: () => { location.hash = ctx.href(src.type, src.id, ...(t === "overview" ? [] : [t])); },
  }, TABS[t][0])));
  const page = h("div");
  root.append(tabs, page);
  catalog(ctx).then((cat) => {
    const all = [...cat.team, ...cat.claude];
    const me = all.find((x) => x.type === src.type && x.id === src.id) || cat.claude.find((x) => x.type === "project" && src.type === "project" && x.id === src.id);
    if (me) {
      title.textContent = me.title;
      if (me.type === "space") {
        lede.textContent = me.s.description || "";
        badges.append(h("span", { class: "badge mono" }, me.s.table));
      } else if (me.type === "store") {
        lede.replaceChildren(h("span", { class: "mono" }, me.s.project || me.s.dir));
        if (me.synced) badges.append(h("a", { class: "badge accent", href: ctx.href("project", me.synced), title: "The copy chloectx sync keeps in this cluster" }, `synced as ${me.synced}`));
        if (!me.s.running) badges.append(h("span", { class: "badge", title: "Reading it started its daemon" }, "daemon started for this page"));
      } else {
        lede.textContent = "Claude Code's memory of this repository, as chloectx sync copies it from each machine into the table chloectx_memory. Read only.";
      }
    }
    const opt = (x) => h("option", { value: `${x.type}/${x.id}`, selected: x.type === src.type && x.id === src.id }, x.type === "store" ? `${x.title} (this machine)` : x.title);
    switcher.replaceChildren(
      ...(cat.team.length ? [h("optgroup", { label: "Team spaces" }, cat.team.map(opt))] : []),
      ...(cat.claude.length ? [h("optgroup", { label: "Claude Code projects" }, cat.claude.slice(0, 60).map(opt))] : []));
    if (!me) switcher.prepend(h("option", { selected: true }, src.id));
    switcher.onchange = () => {
      const [t, ...id] = switcher.value.split("/");
      const next = sourceOf(ctx, t, id.join("/"));
      location.hash = ctx.href(t, id.join("/"), ...(next.tabs.includes(tab) && tab !== "overview" ? [tab] : []));
    };
  }).catch(() => switcher.replaceChildren(h("option", {}, src.id)));
  const draw = {
    overview: () => overviewTab(page, ctx, src),
    ask: () => askTab(page, ctx, src, arg),
    memories: () => memoriesTab(page, ctx, src, arg),
    sessions: () => sessionsTab(page, ctx, src, arg),
    graph: () => graphTab(page, ctx, src, arg),
    receipts: () => (arg ? receiptPage(page, ctx, src, arg) : receiptsTab(page, ctx, src)),
    blame: () => blameTab(page, ctx, src, arg),
    keep: () => keepTab(page, ctx, src),
    connect: () => connectTab(page, ctx, src),
  }[tab];
  return draw() || null;
}

/** A note as a line of a list: its text (struck through when replaced), kind, date, and what it replaced. */
function noteLine(ctx, src, n, opts = {}) {
  const { h } = ctx.core;
  const old = closed(n);
  return h("li", { class: "plain cx-note", onclick: () => historyDrawer(ctx, src, n.id) },
    h("div", {},
      h("div", { class: "cx-text" + (old ? " old" : "") }, n.text),
      h("div", { class: "cx-why" }, opts.kind === false ? null : kindBadge(ctx, n.kind), h("span", {}, day(n.ts)),
        n.was?.length ? h("span", { class: "was" }, "was: ", h("s", {}, n.was[0].text)) : n.supersedes?.length ? h("span", { class: "was" }, "updates an earlier note") : null,
        old ? h("span", {}, closedWord(n)) : n.tag === "done" ? h("span", {}, "done") : null)),
    opts.tools || h("span"));
}

function overviewTab(page, ctx, src) {
  const { h, card, fmt, failure, empty } = ctx.core;
  page.append(h("div", { class: "muted small" }, src.type === "store" ? "Reading the store (its daemon starts if it is stopped)…" : "Loading…"));
  (async () => {
    let o;
    try { o = await src.overview(); } catch (e) { page.replaceChildren(failure(e, "The space")); return; }
    const live = o.live || {};
    const stat = (n, label, href) => h(href ? "a" : "div", { class: "card kpi cx-kpi", href }, h("div", { class: "lbl" }, label), h("div", { class: "val" }, fmt.n(n || 0)));
    const tab = (t, ...a) => ctx.href(src.type, src.id, t, ...a);
    const kpis = h("div", { class: "cx-kpis" },
      stat(live.rule, "Rules", tab("memories", "rule")), stat(live.decision, "Decisions", tab("memories", "decision")), stat(live.fact, "Facts", tab("memories", "fact")),
      stat(live.todo, "Open to-dos", tab("memories", "todo")), stat(o.sessions, "Sessions", tab("sessions")), stat(o.records, "Records", null));
    const span = o.first ? h("div", { class: "cx-status" }, h("span", {}, h("b", {}, "Recorded "), `${day(o.first)} to ${day(o.last)}`),
      Object.values(live).reduce((a, b) => a + b, 0) < Object.entries(o.by_kind || {}).filter(([k]) => NOTE_KINDS.has(k)).reduce((a, [, n]) => a + n, 0) ? h("span", {}, h("b", {}, "History "), "replaced and done notes are kept: Memories shows them") : null) : null;
    const list = (ns, none, k) => ns.length ? h("ul", { class: "cx-list" }, ns.map((n) => noteLine(ctx, src, n, { kind: k }))) : h("div", { class: "muted small" }, none);
    const more = (k, n, all) => (all > n ? h("a", { class: "btn xs", href: tab("memories", k) }, `All ${fmt.n(all)}`) : null);
    if (!o.records) {
      page.replaceChildren(kpis, empty("note", "Nothing in this space yet", src.type === "space" ? "Connect Claude Code (or any agent) to it, or add a note under Memories." : "It fills as Claude Code sessions run in the project.",
        h("a", { class: "btn sm primary", href: tab(src.type === "space" ? "connect" : "sessions") }, src.type === "space" ? "Connect Claude Code" : "Sessions")));
      return;
    }
    const sessions = (o.recent_sessions || []).length ? h("ul", { class: "cx-list" }, o.recent_sessions.map((s) => h("li", { class: "plain" },
      h("a", { class: "cx-slink", href: tab("sessions", s.session) }, h("div", { class: "cx-text" }, s.title || h("span", { class: "mono" }, s.session)),
        h("div", { class: "cx-why" }, h("span", {}, when(s.last)), h("span", {}, fmt.plural(s.turns, "turn")), h("span", {}, fmt.plural(s.records, "record")))), h("span")))) : h("div", { class: "muted small" }, "No session yet.");
    const changes = (o.changes || []).length ? h("ul", { class: "cx-list" }, o.changes.map((n) => noteLine(ctx, src, n))) : h("div", { class: "muted small" }, "No note was replaced yet: when one is, the change shows here.");
    page.replaceChildren(kpis, span || "",
      h("div", { class: "cx-ov" },
        h("div", { class: "cx-stack" },
          card({ title: "Rules", sub: "go into every context, whatever the question", tools: more("rule", o.rules.length, live.rule), body: list(o.rules, "No rule yet.", false) }),
          card({ title: "Decisions", sub: "the latest first", tools: more("decision", o.decisions.length, live.decision), body: list(o.decisions, "No decision yet.", false) }),
          card({ title: "Facts", tools: more("fact", o.facts.length, live.fact), body: list(o.facts, "No fact yet.", false) })),
        h("div", { class: "cx-stack" },
          card({ title: "Open to-dos", sub: "planned, and not done yet", tools: more("todo", o.todos.length, live.todo), body: list(o.todos, "Nothing open.", false) }),
          card({ title: "What changed", sub: "notes that replaced an earlier one", body: changes }),
          card({ title: "Latest sessions", tools: h("a", { class: "btn xs", href: tab("sessions") }, "All sessions"), body: sessions }))));
  })();
  return null;
}

const NOTE_KINDS = new Set(["decision", "preference", "fact", "todo", "note"]);

function askTab(page, ctx, src, asked) {
  const { h, card, fmt, failure } = ctx.core;
  const q = h("textarea", { class: "input", placeholder: "What an agent would ask before a task: \"Who approves a refund of 700 EUR?\"" });
  const budget = h("input", { type: "range", min: 500, max: 16000, step: 500, value: 4000, "aria-label": "Token budget" });
  const budLabel = h("span", { class: "cx-bud" });
  const setBud = () => { budLabel.textContent = `budget ${fmt.n(+budget.value)} tokens`; };
  setBud();
  const askBtn = h("button", { class: "btn primary" }, "Ask");
  const out = h("div", {}, h("div", { class: "cx-hint", style: { marginTop: "6px" } },
    src.type === "project" ? "The answer comes from the synced copy's records, ranked by their words (BM25)." : "The answer is what an agent gets in its context: found by search alone, within the budget, with no model call."));
  const examples = h("div", { class: "cx-drafts" });
  let last = "";
  const ask = async () => {
    const question = q.value.trim();
    if (!question) { q.focus(); return; }
    askBtn.disabled = true;
    out.replaceChildren(h("div", { class: "muted small", style: { marginTop: "8px" } }, "Asking…"));
    if (question !== asked) history.replaceState(null, "", ctx.href(src.type, src.id, "ask", question));
    try { last = question; drawAnswer(ctx, out, await src.ask(question, +budget.value), src); }
    catch (e) { out.replaceChildren(failure(e, "The answer")); }
    askBtn.disabled = false;
  };
  askBtn.onclick = ask;
  q.onkeydown = (e) => { if (e.key === "Enter" && (e.metaKey || e.ctrlKey || !e.shiftKey)) { e.preventDefault(); ask(); } };
  budget.oninput = setBud;
  budget.onchange = () => { if (last && q.value.trim() === last) ask(); };
  // Questions worth asking: what its rules and decisions are about.
  src.notes(false).then((ns) => {
    const qs = suggestions(ns);
    if (qs.length) examples.replaceChildren(h("span", { class: "muted small" }, "Try:"), ...qs.map((t) => h("button", { class: "chip", onclick: () => { q.value = t; ask(); } }, t)));
  }).catch(() => {});
  page.append(card({ body: h("div", { class: "cx-ask" }, q, examples, h("div", { class: "cx-askrow" }, src.type === "project" ? h("span", { class: "grow" }) : [budget, budLabel], askBtn)) }), out);
  // A link can carry the question (#…/ask/<question>), and asking puts it there.
  if (asked) { q.value = asked; ask(); } else setTimeout(() => q.focus(), 0);
  return null;
}

/** A few questions from a space's notes: the subjects its decisions and rules name. */
function suggestions(ns) {
  const out = [];
  const seen = new Set();
  for (const n of ns) {
    if (out.length >= 4) break;
    if (n.kind !== "decision" && n.kind !== "rule" && n.kind !== "fact") continue;
    const m = (n.text || "").match(/\b(refunds?|webhooks?|migrations?|rate limit\w*|retries|currency|logging|tests?|releases?|deploys?|approv\w+|idempoten\w+|timezones?|reports?|flags?|invoices?|payments?|caching|auth\w*)\b/i);
    const w = m?.[1]?.toLowerCase();
    if (!w || seen.has(w)) continue;
    seen.add(w);
    out.push({ refund: "Who approves a large refund?", refunds: "Who approves a large refund?", webhook: "How do we verify Stripe webhooks?", webhooks: "How do we verify Stripe webhooks?", migration: "What are the rules for database migrations?", migrations: "What are the rules for database migrations?", currency: "How are amounts and currencies stored?", logging: "What may we log?", test: "How do I run the tests?", tests: "How do I run the tests?", release: "How do we cut a release?", releases: "How do we cut a release?" }[w] || `What did we decide about ${w}?`);
  }
  return [...new Set(out)];
}

/**
 * An answer to a question: the note that answers it best first, the rules
 * that apply with it, then what else bears on it; why each was picked (its
 * scores), what was left out and the context as the agent gets it are
 * folded below.
 */
function drawAnswer(ctx, out, a, src) {
  const { h, fmt, empty, table } = ctx.core;
  const items = a.items || [];
  if (!items.length) { out.replaceChildren(empty("search", "Nothing found", "The space has nothing that bears on this: an agent gets an empty context.")); return; }
  // How well each matched: its words (BM25, scaled to the best) and its
  // meaning (cosine), together, so a note both find comes first; a to-do is
  // a plan, so an answer of the same fit goes before it.
  const maxB = Math.max(1e-9, ...items.map((x) => x.bm25 || 0));
  const fit = (x) => (x.rank != null ? 1 - x.rank / 100 : (Math.max(0, x.bm25 || 0) / maxB + Math.max(0, x.cos || 0)) / 2 * (x.kind === "todo" ? 0.9 : 1));
  const notes = items.filter((x) => NOTE_KINDS.has(x.kind) || x.kind === "rule");
  const ranked = [...notes].sort((x, y) => fit(y) - fit(x));
  const lead = ranked.find((x) => x.section !== "rule" && (x.bm25 > 0 || x.cos > 0.25 || x.rank != null)) || ranked.find((x) => x.bm25 > 0 || x.cos > 0.3) || ranked[0];
  const rules = notes.filter((x) => x !== lead && x.section === "rule" && (x.bm25 > 0 || x.cos > 0.3));
  const others = ranked.filter((x) => x !== lead && !rules.includes(x) && x.section !== "rule").slice(0, 6);
  const talk = items.filter((x) => x.kind === "user" || x.kind === "assistant" || x.section === "reply" || x.section === "excerpt").filter((x) => !notes.includes(x));
  const when = (x) => h("span", { class: "cx-on" }, day(x.ts));
  const line = (x) => h("li", { class: "plain cx-note", onclick: x.id && src.history ? () => historyDrawer(ctx, src, x.id) : null },
    h("div", {}, h("div", { class: "cx-text" }, x.text), h("div", { class: "cx-why" }, kindBadge(ctx, x.kind), when(x),
      x.replaced ? h("span", { class: "was" }, "was: ", h("s", {}, x.replaced.replace(/^changed [0-9-]+; was: /, ""))) : null)), h("span"));
  const leadEl = lead ? h("div", { class: "cx-lead" },
    h("div", { class: "cx-leadk" }, kindBadge(ctx, lead.kind), h("span", {}, `noted ${day(lead.ts)}`), lead.session && lead.session !== "-" ? h("a", { class: "mono", href: ctx.href(src.type, src.id, "sessions", lead.session) }, `session ${short(lead.session)}`) : null),
    h("div", { class: "cx-leadt" }, lead.text),
    lead.replaced ? h("div", { class: "cx-leadw" }, "This replaced an earlier note: ", h("s", {}, lead.replaced.replace(/^changed [0-9-]+; was: /, ""))) : null,
    lead.id ? h("a", { href: "javascript:void 0", class: "small", onclick: () => historyDrawer(ctx, src, lead.id) }, "Its history") : null) : null;
  const facts = h("div", { class: "cx-facts" },
    a.budget ? h("span", {}, h("b", {}, fmt.n(a.tokens)), `of ${fmt.n(a.budget)} tokens`) : h("span", {}, h("b", {}, fmt.n(a.tokens)), "tokens"),
    h("span", {}, h("b", {}, fmt.n(items.length)), items.length === 1 ? "record" : "records"),
    h("span", {}, h("b", {}, fmt.ms(a.ms)), "to build"),
    h("span", {}, h("b", {}, "0"), "model calls"));
  // Why: every record with its section and scores, as the context holds them.
  const why = table([
    { key: "text", label: "Record", render: (x) => h("div", { class: "cx-cell" }, x.text) },
    { key: "section", label: "Why it is in", render: (x) => h("span", { title: (SECTION[x.section] || SECTION.note)[2] }, (SECTION[x.section] || SECTION.note)[1]) },
    { key: "bm25", label: "Words", num: true, render: (x) => (x.bm25 == null ? "–" : x.bm25.toFixed(2)) },
    { key: "cos", label: "Meaning", num: true, render: (x) => (x.cos == null ? "–" : x.cos.toFixed(2)) },
    { key: "tokens", label: "Tokens", num: true, render: (x) => fmt.n(x.tokens) },
  ], { rows: items, compact: true, sortKey: null });
  const left = (a.left_out || []).length ? h("ul", { class: "cx-list" }, a.left_out.map((r) => {
    const [label, w] = LEFT[r.why] || [r.why, ""];
    return h("li", { class: "plain" }, h("div", {}, h("div", { class: "cx-text old" }, r.text), h("div", { class: "cx-why" }, h("span", { title: w }, label + (w ? ": " + w.toLowerCase() : "")), h("span", {}, day(r.ts)))), h("span"));
  })) : null;
  out.replaceChildren(h("div", { class: "card cx-answer" }, h("div", { class: "bd" },
    h("div", { class: "cx-sub", style: { marginTop: 0 } }, "The answer from its memory"),
    leadEl || h("div", { class: "muted" }, "No note answers it; the context below holds what the searches found."),
    rules.length ? [h("div", { class: "cx-sub" }, "Rules that apply"), h("ul", { class: "cx-list" }, rules.map(line))] : null,
    others.length ? [h("div", { class: "cx-sub" }, "Also relevant"), h("ul", { class: "cx-list" }, others.map(line))] : null,
    talk.length ? h("details", { class: "cx-context" }, h("summary", {}, `From past conversations (${talk.length})`), h("ul", { class: "cx-list" }, talk.map((x) => h("li", { class: "plain" }, h("div", {}, h("div", { class: "cx-text" }, h("span", { class: "muted" }, x.kind === "user" ? "Asked: " : "Answered: "), x.text), h("div", { class: "cx-why" }, when(x))), h("span"))))) : null,
    facts,
    a.words_only ? null : h("details", { class: "cx-context" }, h("summary", {}, "Why these, with their scores"), h("div", { class: "cx-hint", style: { margin: "8px 0" } }, "Rules that apply go in first; then the notes the searches found, by their words (BM25) and their meaning (cosine of the vectors), dated and in order until the budget is spent."), why),
    left ? h("details", { class: "cx-context" }, h("summary", {}, `Left out (${a.left_out.length})`), left) : null,
    a.context ? h("details", { class: "cx-context" }, h("summary", {}, `The context as the agent gets it (${fmt.n(a.chars)} characters)`), h("pre", { class: "code" }, a.context)) : null)));
}

/** A note's history (what it replaced and what replaced it), in a drawer. */
async function historyDrawer(ctx, src, id) {
  const { h, drawer, toast } = ctx.core;
  let j;
  try { j = await src.history(id); }
  catch (e) { toast(e.message, "bad"); return; }
  const row = (n, now) => h("li", { class: "plain" },
    h("div", {}, h("div", { class: "cx-text" + (closed(n) && !now ? " old" : "") }, n.text),
      h("div", { class: "cx-why" }, kindBadge(ctx, n.kind), h("span", {}, day(n.ts)), n.session ? h("a", { class: "mono", href: ctx.href(src.type, src.id, "sessions", n.session) }, `session ${short(n.session)}`) : null, closed(n) ? h("span", {}, closedWord(n)) : n.tag === "done" ? h("span", {}, "done") : h("span", {}, "current"))),
    h("span"));
  drawer({
    title: "A note's history",
    sub: "Each version is kept; a context shows only the current one, with what it changed",
    body: h("div", {},
      j.later.length ? [h("div", { class: "cx-sub" }, "Replaced by"), h("ul", { class: "cx-list" }, j.later.map((n) => row(n, !closed(n))))] : null,
      h("div", { class: "cx-sub" }, "This note"), h("ul", { class: "cx-list" }, row(j.note, true)),
      j.earlier.length ? [h("div", { class: "cx-sub" }, "It replaced"), h("ul", { class: "cx-list" }, j.earlier.map((n) => row(n, false)))] : null),
  });
}

function memoriesTab(page, ctx, src, only) {
  const { h, card, fmt, toast, empty, failure, icon } = ctx.core;
  let kind = KIND_LABEL[only] ? only : "", all = false, notes = [];
  const q = h("input", { class: "input", type: "search", placeholder: "Filter by words" });
  const chips = h("div", { class: "cx-kinds" });
  const body = h("div", {}, h("div", { class: "muted small" }, "Loading…"));
  const which = ctx.core.seg([["live", "Current"], ["all", "With history"]], "live", (v) => { all = v === "all"; load(); });
  const tools = [which];
  if (src.writes) tools.push(h("button", { class: "btn sm", onclick: () => noteForm(ctx, src.id, null, load) }, icon("plus"), "Add"));
  page.append(card({ title: "Memories", sub: src.type === "space" ? "what agents wrote down, and what was distilled from conversations" : "what the distiller wrote down from each turn, and what was remembered by hand", tools, body: h("div", {}, h("div", { class: "cx-filter" }, q, chips), body) }));
  const draw = () => {
    const t = q.value.trim().toLowerCase();
    const counts = {};
    for (const n of notes) counts[n.kind] = (counts[n.kind] || 0) + 1;
    chips.replaceChildren(...[["", "All", notes.length], ...KINDS.filter(([k]) => counts[k]).map(([k, l]) => [k, l, counts[k]])].map(([k, l, n]) =>
      h("button", { class: "chip", "aria-pressed": String(kind === k), onclick: () => { kind = k; draw(); } }, `${l} ${fmt.n(n)}`)));
    const ns = notes.filter((n) => (!kind || n.kind === kind) && (!t || n.text.toLowerCase().includes(t)));
    if (!ns.length) {
      body.replaceChildren(notes.length ? empty("search", "Nothing matches", "No note of this kind has these words.") : empty("note", all ? "No notes yet" : "No current notes", src.writes ? "Add one here, or write to the space from an agent." : "The distiller writes notes down after each turn of a session."));
      return;
    }
    body.replaceChildren(h("div", { class: "cx-hint", style: { marginBottom: "4px" } }, `${fmt.plural(ns.length, "note")}, the newest first. Open one for its history.`), h("ul", { class: "cx-list" }, ns.slice(0, 400).map((n) => noteLine(ctx, src, n, {
      tools: src.writes && !closed(n) ? h("div", { class: "cx-acts", onclick: (e) => e.stopPropagation() },
        h("button", { class: "btn ghost icon sm", title: "Replace with a new version", "aria-label": "Replace", onclick: () => noteForm(ctx, src.id, n, load) }, icon("edit")),
        h("button", { class: "btn ghost icon sm", title: "Retire: leave it out of every context", "aria-label": "Retire", onclick: async () => {
          if (!(await ctx.core.confirm({ title: "Retire this note?", body: `“${n.text}” stays in the space's history but no context shows it again.`, ok: "Retire", danger: true }))) return;
          try { await ctx.api(`/v1/spaces/${encodeURIComponent(src.id)}/notes/${encodeURIComponent(n.id)}`, { method: "DELETE" }); load(); } catch (e) { toast(e.message, "bad"); }
        } }, icon("x"))) : null,
    }))));
  };
  async function load() {
    try { notes = (await src.notes(all)).map((n) => ({ ...n, kind: kindOf(n.kind) })); }
    catch (e) { body.replaceChildren(e.status === 404 ? empty("layers", "No such space", e.message) : failure(e, "The notes")); return; }
    draw();
  }
  q.oninput = draw;
  load();
  return null;
}

// What each kind of record is, and its colour: as `chloectx ui` shows a local store.
const REC = {
  prompt: ["var(--s1)", "Prompt"], answer: ["var(--s2)", "Answer"], tool_call: ["var(--line-2)", "Tool call"], tool_result: ["var(--line-2)", "Tool result"],
  error: ["var(--crit)", "Error"], episode: ["var(--s3)", "Episode"], summary: ["var(--s3)", "Summary"], decision: ["var(--s4)", "Decision"],
  preference: ["var(--s7)", "Rule"], fact: ["var(--s5)", "Fact"], todo: ["var(--s6)", "To do"], note: ["var(--s5)", "Note"], entity: ["var(--muted)", "Entity"],
};
const when = (secs) => (secs ? new Date(secs * 1000).toLocaleString(undefined, { month: "short", day: "numeric", hour: "2-digit", minute: "2-digit" }) : "–");

// ---------------------------------------------------------------- graph

const G_COLOR = { decision: "#3b6fd8", rule: "#8b5cf6", fact: "#16a34a", todo: "#e08a0b", note: "#64748b", entity: "#0d9488", file: "#8a94a6", session: "#b45309" };
const G_KINDS = [["decision", "Decision"], ["rule", "Rule"], ["fact", "Fact"], ["todo", "To do"], ["entity", "Subject"], ["file", "File"], ["session", "Session"]];
const G_EDGE = { about: "about", touches: "touched", from: "came from", supersedes: "replaced" };

/**
 * How a space's memory is connected: each note (a circle, by kind), the
 * subjects it is about (entities, sized by how many notes name them), the
 * files it touched, the session it came from and the note it replaced.
 * A click lights up a node's neighbours and lists them; `focusId` (the
 * link's last part) selects one when the page opens.
 */
function graphTab(page, ctx, src, focusId) {
  const { h, card, failure, empty } = ctx.core;
  const NS = "http://www.w3.org/2000/svg";
  const S = (tag, attrs) => { const e = document.createElementNS(NS, tag); for (const [k, v] of Object.entries(attrs || {})) e.setAttribute(k, v); return e; };
  const show = { entity: true, file: true, session: false, replaced: true };
  const find = h("input", { class: "input", type: "search", placeholder: "Find a subject, file or note", "aria-label": "Find a node", style: { maxWidth: "280px" } });
  const opts = h("div", { class: "cx-gopts" }, [["entity", "Subjects"], ["file", "Files"], ["session", "Sessions"], ["replaced", "Replaced notes"]].map(([k, l]) => {
    const b = h("input", { type: "checkbox", checked: show[k] });
    b.onchange = () => { show[k] = b.checked; build(); };
    return h("label", {}, b, l);
  }));
  const fitBtn = h("button", { class: "btn sm" }, "Fit");
  const info = h("span", { class: "muted small" }, "Loading…");
  const svg = S("svg", { class: "cx-gsvg", role: "img", "aria-label": "How the space's notes, subjects, files and sessions are connected" });
  const side = h("div", { class: "cx-gside" });
  const hint = () => side.replaceChildren(h("div", { class: "muted small" }, "Click a node to see what it is connected to. Drag the background to move, scroll or pinch to zoom, drag a node to place it."));
  hint();
  const legend = h("div", { class: "cx-glegend" }, G_KINDS.map(([k, l]) => h("span", {}, h("i", { class: "cx-gdot", style: { background: G_COLOR[k], borderRadius: k === "file" ? "1px" : k === "session" ? "1px" : "50%", transform: k === "session" ? "rotate(45deg)" : "" } }), l)));
  const body = h("div", { class: "cx-graph" }, h("div", { class: "cx-gwrap" }, svg, legend), side);
  page.append(card({ body: h("div", { class: "stack", style: { gap: "8px" } },
    h("div", { class: "row wrap cx-gbar" }, find, opts, h("span", { class: "grow" }), info, fitBtn), body) }));

  let data = null, nodes = [], edges = [], byId = new Map(), raf = 0, alpha = 0, gone = false, sel = null;
  const view = { x: 0, y: 0, k: 1 };
  const pos = new Map();
  const gView = S("g"), gE = S("g"), gN = S("g"), gL = S("g");
  gView.append(gE, gN, gL);
  svg.append(gView);

  const size = () => [svg.clientWidth || 800, svg.clientHeight || 600];
  const build = () => {
    if (!data) return;
    const keep = (n) => n.type === "note" ? (n.current || show.replaced) : show[n.type];
    nodes = data.nodes.filter(keep).map((n) => ({ ...n }));
    byId = new Map(nodes.map((n) => [n.id, n]));
    edges = data.edges.filter((e) => byId.has(e.s) && byId.has(e.t)).map((e) => ({ ...e, a: byId.get(e.s), b: byId.get(e.t) }));
    // A subject or file no shown note names is left out.
    const deg = new Map();
    for (const e of edges) { deg.set(e.s, (deg.get(e.s) || 0) + 1); deg.set(e.t, (deg.get(e.t) || 0) + 1); }
    nodes = nodes.filter((n) => n.type === "note" || deg.get(n.id));
    byId = new Map(nodes.map((n) => [n.id, n]));
    const [W, H] = size();
    nodes.forEach((n, i) => {
      const p = pos.get(n.id);
      n.x = p ? p.x : W / 2 + Math.cos(i * 2.4) * (40 + (i * 7) % 260);
      n.y = p ? p.y : H / 2 + Math.sin(i * 2.4) * (40 + (i * 11) % 220);
      n.vx = n.vy = 0;
      n.deg = deg.get(n.id) || 0;
      n.r = n.type === "entity" ? 5 + Math.min(13, n.deg * 1.3) : n.type === "note" ? (n.kind === "rule" ? 6 : 5) : 5.5;
    });
    gE.replaceChildren(...edges.map((e) => (e.el = S("line", { class: "e" + (e.label === "supersedes" ? " sup" : e.label === "from" ? " from" : "") }))));
    gN.replaceChildren(...nodes.map((n) => {
      const c = G_COLOR[n.type === "note" ? n.kind : n.type] || G_COLOR.note;
      const el = n.type === "note" ? S("circle", { r: n.r, fill: n.current ? c : "var(--surface)", stroke: n.current ? "var(--surface)" : c, "stroke-dasharray": n.current ? "" : "2 2" })
        : n.type === "entity" ? S("circle", { r: n.r, fill: c, "fill-opacity": "0.85" })
        : S("rect", { width: n.r * 2, height: n.r * 2, rx: 1.5, fill: c, transform: "" });
      el.classList.add("n");
      const t = S("title"); t.textContent = (n.type === "note" ? `${KIND_LABEL[n.kind] || n.kind}${n.current ? "" : " (replaced)"}: ` : n.type === "entity" ? "Subject: " : n.type === "file" ? "File: " : "Session: ") + n.label;
      el.append(t);
      el.addEventListener("pointerdown", (ev) => { ev.stopPropagation(); grab = { n, moved: false, x: ev.clientX, y: ev.clientY }; svg.setPointerCapture?.(ev.pointerId); });
      n.el = el;
      return el;
    }));
    // Subjects, files and sessions carry their names; a note shows its text on hover and on a click.
    gL.replaceChildren(...nodes.filter((n) => n.type !== "note").map((n) => {
      const t = S("text", { class: n.type === "entity" && n.deg >= 3 ? "big" : n.type === "entity" && n.deg < 2 || n.type === "session" ? "minor" : "" });
      t.textContent = n.label.length > 34 ? n.label.slice(0, 33) + "…" : n.label;
      n.tl = t;
      return t;
    }));
    const notes = nodes.filter((n) => n.type === "note").length;
    info.textContent = `${notes} notes · ${nodes.length - notes} ${show.session ? "subjects, files and sessions" : "subjects and files"} · ${edges.length} links`;
    // Laid out before it is shown (new nodes from where they start, the
    // others from where they were), then fitted to the view.
    alpha = pos.size ? 0.3 : 1;
    for (let i = 0; i < 400 && alpha > 0.01; i++) step();
    alpha = 0;
    if (!fit()) requestAnimationFrame(() => fit());
    if (sel && byId.has(sel)) select(sel, false); else { sel = null; mark(null); }
  };

  // One step of the layout: nodes push each other apart (on a coarse grid,
  // so a few thousand stay quick), links pull their ends together, and a
  // weak pull keeps the whole near the middle.
  const step = () => {
    const [W, H] = size();
    const cell = 150, grid = new Map();
    for (const n of nodes) { const k = Math.floor(n.x / cell) + "," + Math.floor(n.y / cell); (grid.get(k) || grid.set(k, []).get(k)).push(n); }
    for (const n of nodes) {
      const cx = Math.floor(n.x / cell), cy = Math.floor(n.y / cell);
      for (let dx = -1; dx <= 1; dx++) for (let dy = -1; dy <= 1; dy++) for (const m of grid.get((cx + dx) + "," + (cy + dy)) || []) {
        if (m === n) continue;
        const x = n.x - m.x, y = n.y - m.y, d2 = x * x + y * y + 0.01;
        if (d2 > cell * cell) continue;
        const d = Math.sqrt(d2), f = (2600 + 80 * (n.r + m.r)) / d2 * alpha;
        n.vx += x / d * f; n.vy += y / d * f;
      }
    }
    for (const e of edges) {
      const x = e.b.x - e.a.x, y = e.b.y - e.a.y, d = Math.sqrt(x * x + y * y) + 0.01;
      const want = e.label === "from" ? 110 : 60 + e.a.r + e.b.r;
      const f = (d - want) * 0.03 * alpha;
      e.a.vx += x / d * f; e.a.vy += y / d * f; e.b.vx -= x / d * f; e.b.vy -= y / d * f;
    }
    for (const n of nodes) {
      n.vx += (W / 2 - n.x) * 0.004 * alpha; n.vy += (H / 2 - n.y) * 0.004 * alpha;
      if (grab && grab.n === n) { n.vx = n.vy = 0; continue; }
      n.vx = Math.max(-25, Math.min(25, n.vx)); n.vy = Math.max(-25, Math.min(25, n.vy));
      n.x += n.vx; n.y += n.vy; n.vx *= 0.6; n.vy *= 0.6;
      pos.set(n.id, { x: n.x, y: n.y });
    }
    alpha *= 0.985;
  };
  // While a node is dragged, the others make room.
  const tick = () => {
    raf = 0;
    if (gone) return;
    if (alpha > 0.004 && nodes.length) { step(); draw(); }
    if (alpha > 0.004) raf = requestAnimationFrame(tick);
  };

  const draw = () => {
    gView.setAttribute("transform", `translate(${view.x},${view.y}) scale(${view.k})`);
    svg.classList.toggle("near", view.k >= 1.4);
    for (const e of edges) { e.el.setAttribute("x1", e.a.x); e.el.setAttribute("y1", e.a.y); e.el.setAttribute("x2", e.b.x); e.el.setAttribute("y2", e.b.y); }
    for (const n of nodes) {
      if (n.el.tagName === "circle") { n.el.setAttribute("cx", n.x); n.el.setAttribute("cy", n.y); }
      else { n.el.setAttribute("x", n.x - n.r); n.el.setAttribute("y", n.y - n.r); if (n.type === "session") n.el.setAttribute("transform", `rotate(45 ${n.x} ${n.y})`); }
      if (n.tl) { n.tl.setAttribute("x", n.x + n.r + 3); n.tl.setAttribute("y", n.y + 4); }
    }
  };

  const fit = () => {
    const [W, H] = size();
    if (!nodes.length || !svg.clientWidth) return false;
    const xs = nodes.map((n) => n.x), ys = nodes.map((n) => n.y);
    const [x0, x1, y0, y1] = [Math.min(...xs) - 20, Math.max(...xs) + 90, Math.min(...ys) - 20, Math.max(...ys) + 20];
    const k = Math.min(1.6, 0.92 * Math.min(W / (x1 - x0), H / (y1 - y0)));
    Object.assign(view, { k, x: W / 2 - k * (x0 + x1) / 2, y: H / 2 - k * (y0 + y1) / 2 });
    draw();
    return true;
  };
  const center = (n) => {
    if (!n) return;
    const [W, H] = size();
    const k = Math.max(view.k, 1);
    Object.assign(view, { k, x: W / 2 - k * n.x, y: H / 2 - k * n.y });
    draw();
  };
  fitBtn.onclick = () => { fit(); };

  // Lights up a set of nodes (and the links between them), dims the rest.
  const mark = (ids) => {
    const on = ids ? new Set(ids) : null;
    for (const n of nodes) { n.el.classList.toggle("hl", !!on && n.id === sel); n.el.classList.toggle("dim", !!on && !on.has(n.id)); n.tl?.classList.toggle("dim", !!on && !on.has(n.id)); n.tl?.classList.toggle("on", !!on && on.has(n.id)); }
    for (const e of edges) { const lit = !!on && on.has(e.s) && on.has(e.t) && (e.s === sel || e.t === sel || !sel); e.el.classList.toggle("hl", lit); e.el.classList.toggle("dim", !!on && !lit); }
  };

  const select = (id, move = true) => {
    const n = byId.get(id);
    if (!n) return;
    sel = id;
    history.replaceState(null, "", ctx.href(src.type, src.id, "graph", id));
    const near = edges.filter((e) => e.s === id || e.t === id);
    mark([id, ...near.map((e) => (e.s === id ? e.t : e.s))]);
    if (move) center(n);
    const kind = n.type === "note" ? kindBadge(ctx, n.kind) : h("span", { class: "badge" }, n.type === "entity" ? "Subject" : n.type === "file" ? "File" : "Session");
    const link = (m) => h("a", { href: "#", class: m.type === "note" && !m.current ? "cx-gold" : "", title: m.type === "note" && !m.current ? "replaced" : null, onclick: (ev) => { ev.preventDefault(); select(m.id); } }, m.label.length > 120 ? m.label.slice(0, 119) + "…" : m.label);
    // Its neighbours, by what links them: the notes about a subject, a note's subjects and files...
    const groups = new Map();
    for (const e of near) {
      const m = e.s === id ? e.b : e.a;
      const name = e.label === "supersedes" ? (e.s === id ? "Replaced" : "Replaced by") : e.s === id ? ({ about: "About", touches: "Touched", from: "From the session" }[e.label] || e.label) : m.type === "note" ? "Notes" : e.label;
      (groups.get(name) || groups.set(name, []).get(name)).push(m);
    }
    const acts = [];
    if (n.type === "note") acts.push(h("button", { class: "btn sm", onclick: () => historyDrawer(ctx, src, n.id) }, "Its history"));
    if (n.type === "note" && n.session && n.session !== "-") acts.push(h("a", { class: "btn sm", href: ctx.href(src.type, src.id, "sessions", n.session) }, "Its session"));
    if (n.type === "session") acts.push(h("a", { class: "btn sm", href: ctx.href(src.type, src.id, "sessions", n.session) }, "Read the session"));
    if (n.type === "entity") acts.push(h("a", { class: "btn sm", href: ctx.href(src.type, src.id, "ask", `What do we know about ${n.label}?`) }, "Ask about it"));
    if (n.type === "file" && src.tabs.includes("blame")) acts.push(h("a", { class: "btn sm", href: ctx.href(src.type, src.id, "blame", n.label) }, "Blame it"));
    side.replaceChildren(
      h("div", { class: "cx-why" }, kind, n.type === "note" && !n.current ? h("span", {}, n.tag === "retired" ? "retired" : n.tag === "done" ? "done" : "replaced") : null, n.ts ? h("span", {}, day(n.ts)) : null),
      h("div", { class: "cx-gtitle" + (n.type === "note" && !n.current ? " old" : "") }, n.type === "note" ? n.text : n.label),
      acts.length ? h("div", { class: "row wrap", style: { gap: "6px", margin: "8px 0" } }, acts) : null,
      ...[...groups].map(([name, ms]) => h("div", { class: "cx-gnear" }, h("div", { class: "cx-gh" }, `${name} · ${ms.length}`),
        h("ul", {}, ms.slice(0, 40).map((m) => h("li", {}, m.type === "note" ? h("i", { class: "cx-gdot", style: { background: G_COLOR[m.kind] || G_COLOR.note } }) : null, link(m)))))),
      h("button", { class: "btn ghost sm", style: { marginTop: "8px" }, onclick: () => { sel = null; mark(null); hint(); history.replaceState(null, "", ctx.href(src.type, src.id, "graph")); } }, "Clear"));
  };

  // Finding: the nodes whose names hold the words light up; Enter picks the first.
  const matches = () => { const q = find.value.trim().toLowerCase(); return q ? nodes.filter((n) => (n.label || "").toLowerCase().includes(q) || (n.text || "").toLowerCase().includes(q)) : []; };
  find.oninput = () => { const m = matches(); sel = null; mark(find.value.trim() ? m.map((n) => n.id) : null); };
  find.onkeydown = (e) => { if (e.key === "Enter") { const m = matches().sort((a, b) => (a.type === "note") - (b.type === "note") || b.deg - a.deg); if (m[0]) select(m[0].id); } };

  // Pan, zoom and drag, with a mouse, a pen or fingers.
  let grab = null, pan = null;
  const pts = new Map();
  svg.addEventListener("pointerdown", (ev) => { pts.set(ev.pointerId, { x: ev.clientX, y: ev.clientY }); if (!grab) pan = { x: ev.clientX - view.x, y: ev.clientY - view.y, moved: false }; svg.setPointerCapture?.(ev.pointerId); });
  svg.addEventListener("pointermove", (ev) => {
    const prev = pts.get(ev.pointerId);
    if (prev && pts.size === 2) {
      const [a, b] = [...pts.values()];
      const d0 = Math.hypot(a.x - b.x, a.y - b.y);
      pts.set(ev.pointerId, { x: ev.clientX, y: ev.clientY });
      const [c, d] = [...pts.values()];
      const d1 = Math.hypot(c.x - d.x, c.y - d.y);
      if (d0 > 0) zoomAt((c.x + d.x) / 2, (c.y + d.y) / 2, d1 / d0);
      return;
    }
    if (prev) pts.set(ev.pointerId, { x: ev.clientX, y: ev.clientY });
    if (grab) {
      const b = svg.getBoundingClientRect();
      if (Math.abs(ev.clientX - grab.x) + Math.abs(ev.clientY - grab.y) > 3) grab.moved = true;
      grab.n.x = (ev.clientX - b.left - view.x) / view.k; grab.n.y = (ev.clientY - b.top - view.y) / view.k;
      pos.set(grab.n.id, { x: grab.n.x, y: grab.n.y });
      if (grab.moved) { alpha = Math.max(alpha, 0.15); if (!raf) raf = requestAnimationFrame(tick); draw(); }
    } else if (pan) {
      if (Math.abs(ev.clientX - view.x - pan.x) + Math.abs(ev.clientY - view.y - pan.y) > 3) pan.moved = true;
      view.x = ev.clientX - pan.x; view.y = ev.clientY - pan.y; draw();
    }
  });
  const up = (ev) => {
    pts.delete(ev.pointerId);
    if (grab && !grab.moved) select(grab.n.id, false);
    else if (pan && !pan.moved && !grab && sel) { sel = null; mark(null); hint(); history.replaceState(null, "", ctx.href(src.type, src.id, "graph")); }
    grab = null; pan = null;
  };
  svg.addEventListener("pointerup", up);
  svg.addEventListener("pointercancel", up);
  const zoomAt = (cx, cy, f) => {
    const b = svg.getBoundingClientRect(), mx = cx - b.left, my = cy - b.top;
    const k = Math.min(6, Math.max(0.15, view.k * f));
    view.x = mx - (mx - view.x) * k / view.k; view.y = my - (my - view.y) * k / view.k; view.k = k;
    draw();
  };
  svg.addEventListener("wheel", (ev) => { ev.preventDefault(); zoomAt(ev.clientX, ev.clientY, Math.exp(-ev.deltaY * 0.0015)); }, { passive: false });

  src.graph().then((j) => {
    if (gone) return;
    if (!j.nodes?.length) { body.replaceWith(empty("note", "Nothing to draw yet", "The graph links the space's notes to what they are about: it fills as decisions, rules and facts are written down.")); info.textContent = ""; return; }
    data = j;
    // A link to a session, file or replaced note shows it, whatever the boxes say.
    const f = focusId && j.nodes.find((n) => n.id === focusId);
    if (f?.type === "session") { show.session = true; opts.querySelectorAll("input")[2].checked = true; }
    build();
    if (f) select(f.id, false);
  }).catch((e) => { if (!gone) { body.replaceWith(failure(e, "The graph")); info.textContent = ""; } });

  return () => { gone = true; cancelAnimationFrame(raf); };
}

/**
 * Sessions: the list, the latest first, each titled by its first prompt;
 * one session read as the conversation it recorded, turn by turn; or, with
 * none picked, every record, filtered by kind and searched by its words.
 */
function sessionsTab(page, ctx, src, session) {
  const { h, empty, failure, fmt, card } = ctx.core;
  const list = h("ul", { class: "cx-sess" }, h("li", { class: "muted small" }, "Loading…"));
  const right = h("div");
  page.append(h("div", { class: "cx-insp" }, card({ title: "Sessions", sub: "the latest first", body: list }), right));
  (async () => {
    let j;
    try { j = await src.sessions(); }
    catch (e) { list.replaceChildren(h("li", {}, failure(e, "The sessions"))); return; }
    const ss = j.sessions || [];
    list.replaceChildren(
      h("li", {}, h("a", { href: ctx.href(src.type, src.id, "sessions"), class: session ? "" : "on" }, h("b", {}, "Every record"), h("div", { class: "cx-why" }, fmt.plural(ss.length, "session"), " · search and filter"))),
      ...ss.map((s) => h("li", {}, h("a", { href: ctx.href(src.type, src.id, "sessions", s.session), class: s.session === session ? "on" : "" },
        h("span", { class: "cx-stitle" }, s.title || s.session), h("div", { class: "cx-why" }, h("span", {}, when(s.first)), h("span", {}, fmt.plural(s.turns, "turn")), h("span", {}, fmt.plural(s.records, "record")))))));
    // Side by side, the list keeps the session in view; stacked (a phone), the conversation is what was asked for.
    const stacked = matchMedia("(max-width: 900px)").matches;
    if (session && !stacked) list.querySelector("a.on")?.scrollIntoView({ block: "nearest" });
    // Once it is drawn, so that the page is long enough to scroll to it.
    if (session) conversation(right, ctx, src, ss.find((s) => s.session === session) || { session }).then(() => { if (stacked) right.scrollIntoView({ block: "start" }); });
    else everyRecord(right, ctx, src, j.by_kind || {});
  })();
  return null;
}

/** One session as the conversation it recorded: each turn's prompt, the tool calls it made, its answer and the notes written from it. */
async function conversation(el, ctx, src, s) {
  const { h, card, failure, fmt, empty } = ctx.core;
  const body = h("div", {}, h("div", { class: "muted small" }, "Loading the session…"));
  el.append(card({ title: s.title || `Session ${short(s.session, 12)}`, sub: [h("span", { class: "mono" }, s.session), s.first ? ` · ${when(s.first)}` : "", s.turns ? ` · ${fmt.plural(s.turns, "turn")}` : ""], body }));
  let rs = [];
  // Its first 500 records, in the order they were recorded.
  try { rs = (await src.records(`session=${encodeURIComponent(s.session)}&limit=500`)).records || []; }
  catch (e) { body.replaceChildren(failure(e, "The session")); return; }
  if (!rs.length) { body.replaceChildren(empty("note", "Nothing recorded", "This session has no records.")); return; }
  const turns = new Map();
  for (const r of rs) {
    if (r.kind === "entity") continue;
    if (!turns.has(r.turn)) turns.set(r.turn, []);
    turns.get(r.turn).push(r);
  }
  const out = [];
  let n = 0, last = null;
  for (const [t, recs] of turns) {
    // Notes written after a turn (a team space's, through its API) go with it.
    if (last && !recs.some((r) => r.kind === "prompt" || r.kind === "answer") && recs.every((r) => NOTE_KINDS.has(r.kind))) {
      last.append(h("div", { class: "cx-wrote" }, h("div", { class: "cx-who" }, "Written down"), h("ul", { class: "cx-list" }, recs.map((x) => noteLine(ctx, src, { ...x, kind: kindOf(x.kind) })))));
      continue;
    }
    if (recs.some((r) => r.kind === "prompt")) n++;
    const prompt = recs.filter((r) => r.kind === "prompt");
    const answer = recs.filter((r) => r.kind === "answer");
    const tools = recs.filter((r) => r.kind === "tool_call" || r.kind === "tool_result");
    const errors = recs.filter((r) => r.kind === "error");
    const notes = recs.filter((r) => NOTE_KINDS.has(r.kind));
    const other = recs.filter((r) => r.kind === "summary");
    const at = recs[0]?.ts;
    last = h("div", { class: "cx-turn" },
      h("div", { class: "cx-turnh" }, h("span", {}, n ? `Turn ${n}` : "Before the first turn"), h("span", { class: "muted" }, when(at))),
      prompt.map((r) => h("div", { class: "cx-msg you", onclick: () => recordDrawer(ctx, src, r) }, h("div", { class: "cx-who" }, "You"), h("div", { class: "cx-body" }, r.text))),
      tools.length ? h("details", { class: "cx-tools" }, h("summary", {}, `${fmt.plural(tools.filter((r) => r.kind === "tool_call").length || tools.length, "tool call")}: ${[...new Set(tools.map((r) => r.tag).filter(Boolean))].slice(0, 5).join(", ")}`),
        h("div", {}, tools.map((r) => h("div", { class: "cx-tool" + (r.kind === "tool_result" ? " res" : ""), onclick: () => recordDrawer(ctx, src, r) }, r.text.split("\n").slice(0, r.kind === "tool_call" ? 3 : 2).join("\n"))))) : null,
      errors.map((r) => h("div", { class: "cx-tool err", onclick: () => recordDrawer(ctx, src, r) }, r.text.split("\n").slice(0, 4).join("\n"))),
      answer.map((r) => h("div", { class: "cx-msg ai", onclick: () => recordDrawer(ctx, src, r) }, h("div", { class: "cx-who" }, "Claude"), h("div", { class: "cx-body" }, r.text))),
      other.map((r) => h("div", { class: "cx-hint" }, r.text)),
      notes.length ? h("div", { class: "cx-wrote" }, h("div", { class: "cx-who" }, "Written down"), h("ul", { class: "cx-list" }, notes.map((x) => noteLine(ctx, src, { ...x, kind: kindOf(x.kind) })))) : null);
    out.push(last);
  }
  body.replaceChildren(...out, rs.length >= 500 ? h("div", { class: "cx-hint" }, "Showing its first 500 records.") : "");
}

/** Every record of the space, the latest first, filtered by kind or ranked by BM25 for some words. */
function everyRecord(el, ctx, src, by) {
  const { h, empty, failure, fmt, card } = ctx.core;
  const q = h("input", { class: "input", type: "search", placeholder: "Search every record (BM25), and press Enter" });
  const kinds = h("div", { class: "cx-kinds" });
  const out = h("div", {}, h("div", { class: "muted small" }, "Loading…"));
  const more = h("div", { style: { marginTop: "8px" } });
  const title = h("span", {}, "The latest records");
  let kind = "", oldest = null;
  el.append(card({ title, body: h("div", {}, h("div", { class: "cx-filter" }, q, kinds), out, more) }));
  const chip = (k, label) => h("button", { class: "chip", "aria-pressed": String(kind === k), "data-k": k, onclick: () => { kind = k; oldest = null; for (const c of kinds.children) c.setAttribute("aria-pressed", String(c.dataset.k === k)); records(false); } }, label);
  kinds.replaceChildren(chip("", "All"), ...Object.keys(REC).filter((k) => by[k]).map((k) => chip(k, `${REC[k][1]} ${fmt.n(by[k])}`)));
  async function records(append) {
    const text = q.value.trim();
    title.textContent = text ? `Best matches for “${text}”` : kind ? `The latest ${(REC[kind] || [, kind])[1].toLowerCase()} records` : "The latest records";
    let j;
    try { j = await src.records((kind ? `kind=${kind}&` : "") + (text ? `q=${encodeURIComponent(text)}&` : "") + (append && oldest != null ? `before=${oldest}&` : "") + "limit=200"); }
    catch (e) { out.replaceChildren(failure(e, "The records")); return; }
    const rs = j.records || [];
    if (!append) out.replaceChildren();
    if (!rs.length && !append) out.append(empty("search", "Nothing", text ? "No record matches." : "No record of this kind."));
    for (const r of rs) {
      const [color, label] = REC[r.kind] || ["var(--muted)", r.kind];
      const tool = r.kind === "tool_call" || r.kind === "tool_result";
      out.append(h("div", { class: "cx-rec" + (tool ? " tool" : ""), onclick: () => recordDrawer(ctx, src, r) },
        h("i", { class: "cx-sw", style: { background: color } }),
        h("div", {}, h("div", { class: "cx-text" + (closed(r) ? " old" : "") }, r.text),
          h("div", { class: "cx-why" }, h("span", { class: "badge" }, label), h("span", {}, when(r.ts)), h("a", { href: ctx.href(src.type, src.id, "sessions", r.session), onclick: (e) => e.stopPropagation(), class: "mono" }, short(r.session)),
            r.tag && r.tag !== "-" ? h("span", {}, r.tag) : null, r.files ? h("span", { class: "mono" }, r.files.split(/[,\n]/)[0]) : null))));
    }
    oldest = rs.length ? rs.reduce((m, r) => Math.min(m, r.seq), Infinity) : oldest;
    more.replaceChildren(j.more && !text ? h("button", { class: "btn sm", onclick: () => records(true) }, "Older records") : j.more ? h("div", { class: "cx-hint" }, "Showing the first 200.") : "");
  }
  q.onkeydown = (e) => { if (e.key === "Enter") { oldest = null; records(false); } };
  q.onsearch = () => { if (!q.value) { oldest = null; records(false); } };
  records(false);
}

/** One record in full, in a drawer. */
function recordDrawer(ctx, src, r) {
  const { h, drawer } = ctx.core;
  const [, label] = REC[r.kind] || [null, r.kind];
  const row = (k, v) => (v || v === 0 ? [h("dt", {}, k), h("dd", {}, v)] : null);
  drawer({
    title: label,
    sub: `${src.id} · ${when(r.ts)}`,
    body: h("div", {},
      h("pre", { class: "cx-full" }, r.text + (r.cut ? "\n…" : "")),
      h("dl", { class: "cx-dl" },
        row("Session", h("a", { href: ctx.href(src.type, src.id, "sessions", r.session), class: "mono" }, r.session)),
        row("Turn", `${r.turn} (record ${r.seq})`), row("Tag", r.tag && r.tag !== "-" ? r.tag : null), row("Importance", r.importance ? r.importance.toFixed(2) : null),
        row("Files", r.files ? h("span", { class: "mono" }, r.files) : null), row("Id", h("span", { class: "mono" }, r.id)),
        row("Links", r.links?.length ? r.links.map((l) => `${l.label} ${l.to}`).join(", ") : null)),
      r.context ? [h("div", { class: "cx-sub" }, "Context"), h("pre", { class: "cx-full", style: { color: "var(--text-2)", fontSize: "12px" } }, r.context)] : null),
  });
}

/** Connect: how an agent uses the space (team), or how this store reaches Claude Code and the cluster (Claude Code). */
function connectTab(page, ctx, src) {
  const { h, card, sqlBlock } = ctx.core;
  if (src.type === "space") {
    const name = src.id;
    const b = `${location.origin}/v1/apps/chloectx/v1/spaces/${name}`;
    const make = makeApps(ctx, name);
    page.append(connectAgent(ctx, name), h("div", { class: "cx-split" },
      card({
        title: "From any agent", sub: "over HTTP, with a key that may write the space",
        body: h("div", {},
          h("div", { class: "cx-hint" }, "Write down what it learned (replaces names the note it updates):"),
          h("pre", { class: "code cx-code" }, `curl -s ${b}/notes -H "authorization: Bearer $KEY" \\\n  -d '{"kind": "decision", "text": "Refunds over 1000 EUR need a second approver.", "entities": ["refunds"]}'`),
          h("div", { class: "cx-hint" }, "Ask for context before a task, within a budget in tokens:"),
          h("pre", { class: "code cx-code" }, `curl -s ${b}/ask -H "authorization: Bearer $KEY" \\\n  -d '{"question": "Who approves a 700 EUR refund?", "budget": 2000}'`),
          h("div", { class: "cx-hint" }, "The records are rows of the space's table, so SQL reads them too:"),
          sqlBlock(`SELECT kind, text, ts FROM ctx_${name} WHERE kind IN ('decision', 'preference') AND tag NOT IN ('superseded', 'retired') ORDER BY ts DESC LIMIT 20`)),
      }),
      handOver(ctx, src)), make.el);
    return make.stop;
  }
  if (src.type === "store") {
    page.append(h("div", { class: "cx-split" },
      card({
        title: "In Claude Code", sub: "already connected: this store is the project's memory",
        body: h("div", { class: "stack", style: { gap: "8px" } },
          h("div", { class: "cx-hint" }, "Every Claude Code session in the project records into it through ChloeCtx's hooks, and gets what bears on each prompt in its context. In a session, the chloectx MCP server's tools (memory_ask, memory_search, memory_remember, memory_forget) ask it directly, or ask in words:"),
          copyable(ctx, "What did we decide about refund approvals? Ask your memory."),
          h("div", { class: "cx-hint" }, "From a shell in the project's folder:"),
          copyable(ctx, "chloectx ask \"who approves a large refund?\"\nchloectx blame app/payments/refunds.py:42\nchloectx receipts")),
      }),
      card({
        title: "Share it", sub: "with the cluster, or as a capsule",
        body: h("div", { class: "stack", style: { gap: "8px" } },
          h("div", { class: "cx-hint" }, "Copy it into this cluster, where the Spaces page lists it as a synced project, for teammates and other machines:"),
          copyable(ctx, `printf %s "$KEY" | chloectx sync setup --url ${location.origin} --token-stdin\nchloectx sync push`),
          h("div", { class: "cx-hint" }, "Or cut a capsule (under Snapshots and capsules) and mount it in another agent: a read-only copy of the notes and the conversation, redacted again.")),
      })));
    return null;
  }
  page.append(card({
    title: "Where it comes from", sub: "chloectx sync, on each machine that records the project",
    body: h("div", { class: "stack", style: { gap: "8px" } },
      h("div", { class: "cx-hint" }, `Each machine's ChloeCtx daemon sends what it records to this cluster every 30 seconds. The page only reads; every row of ${src.id} is in the table chloectx_memory:`),
      sqlBlock(`SELECT session, kind, content, ts FROM chloectx_memory WHERE project = '${src.id.replace(/'/g, "''")}' AND kind IN ('decision', 'preference', 'fact') ORDER BY ts DESC LIMIT 20`)),
  }));
  return null;
}

/** A conversation handed over to a team space: kept as two messages, and distilled into notes when a model is set. */
function handOver(ctx, src) {
  const { h, card, toast } = ctx.core;
  const user = h("textarea", { class: "input", rows: 3, placeholder: "What the user said" });
  const asst = h("textarea", { class: "input", rows: 3, placeholder: "What the agent answered" });
  const send = h("button", { class: "btn" }, "Hand it over");
  const said = h("div", { class: "cx-hint" });
  info(ctx).then((i) => { said.textContent = `Kept as two messages, and ${modelLine(ctx.core, i)}.`; }).catch(() => {});
  send.onclick = async () => {
    if (!user.value.trim() && !asst.value.trim()) return;
    send.disabled = true;
    try {
      const j = await ctx.api(`/v1/spaces/${encodeURIComponent(src.id)}/exchanges`, { body: { user: user.value, assistant: asst.value } });
      user.value = ""; asst.value = "";
      toast(j.distilling ? "Kept; its notes follow in a few seconds" : "Kept as messages", "ok");
    } catch (e) { toast(e.message, "bad"); }
    send.disabled = false;
  };
  return card({ title: "Hand over a conversation", sub: "a model writes down its decisions, rules and facts", body: h("div", { class: "cx-form" }, user, asst, h("div", { class: "row", style: { justifyContent: "space-between", gap: "10px" } }, said, send)) });
}

/** A command in a code block, with a Copy button on it. */
function copyable(ctx, text) {
  const { h, icon } = ctx.core;
  const pre = h("pre", { class: "code cx-cmd" }, text);
  const el = h("div", { class: "cx-wrap" }, pre, h("button", { class: "btn xs cx-copy", title: "Copy", onclick: () => ctx.core.copy(pre.textContent.replace(/ +#.*$/gm, ""), "Command copied") }, icon("copy"), "Copy"));
  el.set = (t) => { pre.textContent = t; };
  return el;
}

/**
 * Connect an agent to a space: the space is an MCP server (tools ask,
 * remember, notes), and a key limited to its table is all an agent needs.
 * The key is made here and shown once, in the command to paste.
 */
function connectAgent(ctx, name) {
  const { h, card, toast } = ctx.core;
  const url = `${location.origin}/v1/apps/chloectx/v1/spaces/${encodeURIComponent(name)}/mcp`;
  const cmd = (secret) => `claude mcp add --transport http ctx-${name.replace(/_/g, "-")} ${url} \\\n  --header "Authorization: Bearer ${secret || "$KEY"}"`;
  const code = copyable(ctx, cmd(null));
  const said = h("div", { class: "cx-hint" }, "Then, in Claude Code: “check the ", name, " space before you start” or “remember that …”. Any MCP client connects the same way.");
  const make = h("button", { class: "btn sm primary" }, "Make a key for this space");
  make.onclick = async () => {
    make.disabled = true;
    try {
      // A key that may write this space's table alone: chloedb-server
      // lets it call the app, which holds it to that grant.
      const k = await ctx.core.api("/v1/admin/keys", { body: { name: `ctx-${name}-agent-${new Date().toISOString().slice(0, 10)}`, scope: "write", tables: { [`ctx_${name}`]: "write" }, note: `MCP access to the ChloeCtx space ${name}` } });
      code.set(cmd(k.secret));
      said.replaceChildren(h("b", {}, "Shown once: "), "this key reaches only the table ", h("span", { class: "mono" }, `ctx_${name}`), " (revoke it under Access and audit). Paste the command where Claude Code runs.");
      make.remove();
    } catch (e) { toast(`The key: ${e.message}`, "bad", 8000); make.disabled = false; }
  };
  if (!ctx.core.can("admin")) { make.disabled = true; make.title = ctx.core.needs("admin"); }
  return card({
    title: "Connect Claude Code",
    sub: "this space as an MCP server: ask, remember, notes",
    tools: make,
    attrs: { style: { marginBottom: "16px" } },
    body: h("div", { class: "stack", style: { gap: "8px" } }, code, said),
  });
}

// ---------------------------------------------------------------- apps from a space

const usd = (v) => (v == null ? "?" : v < 0.01 ? "under $0.01" : `$${v.toFixed(2)}`);

function costLine(e) {
  if (!e) return "";
  const built = e.usd != null ? `About ${usd(e.usd)} a build, the median of the ${e.builds} first ${e.builds === 1 ? "build" : "builds"} the app engine made here` : "No build here to measure a cost from yet";
  const left = e.left != null ? `; it may spend ${usd(e.left)} more of its ${usd(e.cap)} cap` : "";
  return `${built}${left}. Loading the notes takes a call to ${e.loading_model?.model || "the model"} for every 30 of them.`;
}

/** Apps made from a space: suggested from its notes, built by the cluster's app engine, loaded with the notes the person picks. */
function makeApps(ctx, space) {
  const { h, card, toast, pill, failure, every } = ctx.core;
  const sp = `/v1/spaces/${encodeURIComponent(space)}`;
  const builds = h("div");
  const ideas = h("div");
  const suggestBtn = h("button", { class: "btn primary" }, "Suggest apps");
  const lead = h("p", { class: "cx-hint", style: { margin: "0 0 10px" } }, "ChloeCtx reads this space's notes and suggests apps they could become (one model call). You pick one, choose which notes go into it, and see what it costs before the cluster's app engine builds it. The app gets copies of the notes you pick and nothing else.");
  let estimate = null;
  ctx.api(`${sp}/apps/estimate`).then((e) => { estimate = e; }).catch((e) => {
    if (e.status === 503) { suggestBtn.disabled = true; lead.textContent = e.message; }
  });
  const open = async (b) => {
    try { const j = await ctx.api(`${sp}/apps/${encodeURIComponent(b.id)}/open`, { method: "POST", body: {} }); window.open(j.url, "_blank", "noopener"); }
    catch (e) { toast(e.status === 403 ? "Opening the studio needs an admin key" : e.message, "bad"); }
  };
  let running = false;
  const loadBuilds = async () => {
    let j;
    try { j = await ctx.api(`${sp}/apps`); } catch (e) { builds.replaceChildren(failure(e, "The builds")); return; }
    const bs = j.builds || [];
    running = bs.some((b) => b.status === "building" || b.status === "loading");
    if (!bs.length) { builds.replaceChildren(); return; }
    builds.replaceChildren(h("div", { class: "cx-sub", style: { marginTop: "4px" } }, "Built from this space"), h("ul", { class: "cx-builds" }, bs.map((b) => {
      const state = b.status === "ready" ? pill("ok", "ready")
        : b.status === "failed" ? pill("bad", "failed")
        : b.status === "loading" ? pill("pending", `loading notes, ${b.loaded} rows so far`)
        : pill("pending", "the app engine is building it");
      const facts = [`${b.items} notes picked`, b.status === "ready" || b.status === "loading" ? `${b.loaded} rows loaded` : null, b.skipped ? `${b.skipped} fit no table` : null, b.cost ? `build ${usd(b.cost)}` : null, day(b.created)].filter(Boolean);
      return h("li", {},
        h("div", {}, h("b", {}, b.title), " ", state,
          h("div", { class: "cx-why" }, facts.map((f) => h("span", {}, f))),
          b.error ? h("div", { class: "cx-hint", style: { color: b.status === "failed" ? "var(--crit-ink)" : undefined } }, b.error) : null),
        b.status === "ready" ? h("button", { class: "btn sm", onclick: () => open(b) }, "Open in the studio") : h("span"));
    })));
  };
  suggestBtn.onclick = async () => {
    suggestBtn.disabled = true;
    ideas.replaceChildren(h("div", { class: "muted small", style: { marginTop: "10px" } }, "Reading the notes…"));
    try {
      const j = await ctx.api(`${sp}/apps/suggest`, { body: {} });
      ideas.replaceChildren(h("div", { class: "cx-ideas" }, j.apps.map((a) => h("div", { class: "cx-idea" },
        h("h4", {}, a.title), h("p", {}, a.pitch),
        a.why ? h("p", { class: "cx-hint" }, a.why) : null,
        h("div", { class: "cx-why" }, a.kinds.map((k) => kindBadge(ctx, k)), h("span", {}, `${a.items} notes`)),
        h("button", { class: "btn sm", onclick: () => buildForm(ctx, space, a, estimate, () => { loadBuilds(); }) }, "Build this…")))));
    } catch (e) { ideas.replaceChildren(failure(e, "The suggestions")); }
    suggestBtn.disabled = false;
  };
  const el = card({ cls: "cx-make", title: "Create an app from this context", sub: "built by the cluster's app engine", tools: [suggestBtn], body: h("div", {}, lead, builds, ideas) });
  loadBuilds();
  const stop = every(5000, () => { if (running) loadBuilds(); }, false);
  return { el, stop };
}

/** The build, asked for: what the app is, which notes go in, what it costs. */
async function buildForm(ctx, space, idea, estimate, done) {
  const { h, drawer, toast } = ctx.core;
  const sp = `/v1/spaces/${encodeURIComponent(space)}`;
  const title = h("input", { class: "input", value: idea.title });
  const desc = h("textarea", { class: "input", rows: 7 }, idea.description);
  const picked = new Set(idea.kinds);
  const out = new Set();
  const list = h("ul", { class: "cx-pick" });
  const count = h("span", { class: "cx-hint" });
  const go = h("button", { class: "btn primary" }, "Build it");
  const cost = h("div", { class: "cx-cost" }, costLine(estimate));
  let notes = [];
  const draw = () => {
    const n = notes.filter((x) => !out.has(x.id)).length;
    count.textContent = `${n} of ${notes.length} notes go into the app`;
    go.textContent = estimate?.usd != null ? `Build it, about ${usd(estimate.usd)}` : "Build it";
    go.disabled = n === 0;
  };
  const load = async () => {
    list.replaceChildren(h("li", { class: "muted" }, h("span"), "Loading…"));
    try { notes = (await ctx.api(`${sp}/apps/items`, { body: { kinds: [...picked] } })).notes || []; }
    catch (e) { list.replaceChildren(h("li", {}, h("span"), e.message)); return; }
    list.replaceChildren(...(notes.length ? notes.map((n) => {
      const box = h("input", { type: "checkbox", checked: !out.has(n.id), "aria-label": "Put this note in the app" });
      box.onchange = () => { if (box.checked) out.delete(n.id); else out.add(n.id); draw(); };
      return h("li", {}, box, h("div", {}, n.text, h("div", { class: "cx-why" }, kindBadge(ctx, n.kind), h("span", {}, day(n.ts)))));
    }) : [h("li", {}, h("span"), "No notes of these kinds.")]));
    draw();
  };
  const kinds = h("div", { class: "cx-kinds" }, KINDS.map(([k, l]) => {
    const box = h("input", { type: "checkbox", checked: picked.has(k) });
    box.onchange = () => { if (box.checked) picked.add(k); else picked.delete(k); load(); };
    return h("label", {}, box, l);
  }));
  const d = drawer({
    title: "Build an app from this space",
    sub: "The app engine builds it from the request, then the notes you keep ticked become its first rows",
    wide: true,
    body: h("div", { class: "cx-form" },
      h("label", {}, "Name", title),
      h("label", {}, "What to build (the request the app engine gets; change it as you like)", desc),
      h("div", { class: "cx-sub", style: { margin: "6px 0 0" } }, "The notes that go in"),
      kinds, list, count,
      h("div", { class: "cx-sub", style: { margin: "6px 0 0" } }, "What it costs"), cost),
    foot: [h("button", { class: "btn", onclick: () => d.close() }, "Cancel"), go],
  });
  go.onclick = async () => {
    go.disabled = true;
    try {
      await ctx.api(`${sp}/apps`, { body: { title: title.value, description: desc.value, kinds: [...picked], exclude: [...out], confirm: true } });
      d.close();
      toast("The app engine is building it; the notes go in once it is built", "ok", 6000);
      done();
    } catch (e) { toast(e.message, "bad", 8000); go.disabled = false; }
  };
  load();
}

function noteForm(ctx, space, old, done) {
  const { h, modal, toast } = ctx.core;
  const kind = h("select", { class: "select" }, KINDS.map(([k, l]) => h("option", { value: k, selected: old ? old.kind === k : k === "decision" }, l)));
  const text = h("textarea", { class: "input", rows: 3, placeholder: "One sentence that stands on its own, with its reason" }, old ? old.text : "");
  const ents = h("input", { class: "input", placeholder: "refunds, invoices (what it is about; optional)" });
  const go = h("button", { class: "btn primary" }, old ? "Replace" : "Add");
  const m = modal({
    title: old ? "A new version of the note" : "Add a note",
    body: h("div", { class: "cx-form" },
      old ? h("div", { class: "cx-hint" }, "The old version stays in the history; contexts show this one, with what changed.") : null,
      h("label", {}, "Kind", kind), h("label", {}, "Note", text), h("label", {}, "About", ents)),
    foot: [h("button", { class: "btn", onclick: () => m.close() }, "Cancel"), go],
  });
  go.onclick = async () => {
    go.disabled = true;
    try {
      const entities = ents.value.split(",").map((s) => s.trim()).filter(Boolean);
      await ctx.api(`/v1/spaces/${encodeURIComponent(space)}/notes`, { body: { kind: kind.value, text: text.value, entities, replaces: old?.id } });
      m.close();
      done();
    } catch (e) { toast(e.message, "bad"); go.disabled = false; }
  };
}

// ---------------------------------------------------------------- Seen before

function pct(ctx, v) {
  const { h } = ctx.core;
  const word = v >= 0.5 ? "close" : v >= 0.3 ? "similar" : "related";
  return h("span", { class: "cx-pct", title: `How close it is in meaning: cosine similarity ${v.toFixed(2)}` }, h("span", {}, h("i", { style: { width: Math.round(Math.max(0, Math.min(1, v)) * 100) + "%" } })), word);
}

function incidentFields(ctx, i) {
  const { h } = ctx.core;
  return h("dl", {},
    h("dt", {}, "Symptom"), h("dd", {}, i.symptom || "–"),
    h("dt", {}, "Cause"), h("dd", {}, i.cause || "–"),
    h("dt", {}, "Fix"), h("dd", { class: "fix" }, i.fix || "–"),
    (i.replaced || []).map((e) => [h("dt", {}, "Before"), h("dd", {}, h("s", {}, e.fix), h("span", { class: "muted" }, ` (until ${day(e.ts)})`))]));
}

function seenPage(root, ctx) {
  const { h, card, fmt, toast, empty, failure, every, icon } = ctx.core;
  const rec = h("button", { class: "btn primary", onclick: () => incidentForm(ctx, null, load) }, icon("plus"), "Record an incident");
  root.append(header(ctx, "seen", "Seen before",
    "The cluster's firing alerts, each beside the past incidents it looks like and the fix that worked. Hand one to the on-call agent with those incidents as its context, and keep what it found as a new incident.", rec));
  const kpis = h("div", { class: "grid g3", style: { marginBottom: "16px" } });
  const firing = h("div", {}, h("div", { class: "muted small" }, "Reading the alerts…"));
  const incidents = h("div", {}, h("div", { class: "muted small" }, "Loading…"));
  const probe = h("input", { class: "input", style: { width: "100%" }, placeholder: "Or describe what you see, and press Enter: \"p99 doubled on one shard after a deploy\"" });
  const probeOut = h("div");
  probe.onkeydown = async (e) => {
    if (e.key !== "Enter" || !probe.value.trim()) return;
    try {
      const j = await ctx.api(`/v1/incidents/match?q=${encodeURIComponent(probe.value.trim())}&n=3`);
      probeOut.replaceChildren(j.matches.length ? h("div", { style: { marginTop: "10px" } }, j.matches.map((m, i) => matchCard(ctx, m, i === 0))) : empty("search", "Nothing like it", "No recorded incident looks like this."));
    } catch (err) { probeOut.replaceChildren(failure(err, "The match")); }
  };
  root.append(kpis,
    card({ title: "Firing now", sub: "with the closest past incidents", body: h("div", {}, firing, h("div", { style: { marginTop: "12px" } }, probe, probeOut)) }),
    h("div", { style: { height: "16px" } }),
    card({ title: "Incidents", sub: "kept in the space incidents (table ctx_incidents)", body: incidents }));
  const kpi = (label, val, sub) => h("div", { class: "card kpi" }, h("div", { class: "lbl" }, label), h("div", { class: "val" }, val), h("div", { class: "delta" }, sub || ""));
  let incs = [];
  const open = new Set(), handed = new Map();
  async function load() {
    let f, l;
    try { [f, l] = await Promise.all([ctx.api("/v1/incidents/firing"), ctx.api("/v1/incidents")]); }
    catch (e) { firing.replaceChildren(failure(e, "The alerts")); return; }
    incs = l.incidents || [];
    const alerts = f.alerts || [];
    const nFiring = alerts.filter((a) => a.state === "firing").length;
    const matched = alerts.filter((a) => a.matches?.length).length;
    kpis.replaceChildren(
      kpi("Firing", fmt.n(nFiring), alerts.length > nFiring ? `${alerts.length - nFiring} pending` : "alerts of this cluster"),
      kpi("Seen before", alerts.length ? `${matched} of ${alerts.length}` : "–", `alerts with a past incident like them · matched in ${fmt.ms(f.ms)}`),
      kpi("Incidents kept", fmt.n(incs.length), fmt.plural(incs.reduce((a, i) => a + (i.replaced?.length || 0), 0), "fix", "fixes") + " replaced by a better one"));
    firing.replaceChildren(alerts.length ? h("div", {}, alerts.map((a) => alertBlock(ctx, a, open, handed))) : empty("okc", "No alert is firing", "When one fires, it shows here with the incidents it looks like."));
    incidents.replaceChildren(incs.length ? h("div", {}, incs.map((i) => h("div", { class: "cx-inc" },
      h("h4", {}, i.title, i.alert ? h("span", { class: "badge mono" }, i.alert) : null, h("span", { class: "small muted", style: { fontWeight: 400 } }, day(i.ts)),
        h("span", { class: "grow" }),
        h("button", { class: "btn xs", onclick: () => incidentForm(ctx, i, load), title: "A better fix, or a correction: the old one stays as history" }, "Better fix")),
      incidentFields(ctx, i)))) : empty("note", "No incident recorded yet", "Record one by hand, paste a postmortem for a model to draft it, or draft it from an on-call run.", h("button", { class: "btn sm primary", onclick: () => incidentForm(ctx, null, load) }, "Record an incident")));
  }
  return every(15000, load);
}

function matchCard(ctx, m, best) {
  const { h } = ctx.core;
  return h("div", { class: "cx-match" + (best ? " best" : "") },
    h("h5", {}, m.title, pct(ctx, m.match || 0), m.same_alert ? h("span", { class: "badge accent", title: "Recorded for this same alert rule" }, "same alert") : null, h("span", { class: "small muted", style: { fontWeight: 400 } }, day(m.ts))),
    incidentFields(ctx, m));
}

function alertBlock(ctx, a, open, handed) {
  const { h, fmt, pill, toast } = ctx.core;
  const since = a.since_ms ? fmt.ago(a.since_ms).replace(" ago", "") : null;
  const where = (a.where || []).slice(0, 3).map((w) => w.short || w.name || w.url || "").filter(Boolean);
  const hand = h("button", { class: "btn primary sm" }, ctx.core.icon("bolt"), "Hand to the on-call agent");
  const handOut = h("span", { class: "small muted" });
  const shown = (id) => {
    hand.disabled = true;
    handOut.replaceChildren("The agent is looking into it: ", h("a", { href: "#/oncall?run=" + encodeURIComponent(id) }, "follow the run"), " · when it ends, ", h("a", { href: "javascript:void 0", onclick: () => incidentForm(ctx, null, null, { run: id, alert: a.rule }) }, "keep what it found"));
  };
  if (handed.has(a.rule)) shown(handed.get(a.rule));
  hand.onclick = async () => {
    hand.disabled = true;
    try {
      const j = await ctx.api("/v1/incidents/handoff", { body: { alert: a.rule } });
      handed.set(a.rule, j.run?.run);
      shown(j.run?.run);
    } catch (e) { toast(e.message, "bad", 8000); hand.disabled = false; }
  };
  const ctxBox = h("details", { class: "cx-context", open: open.has(a.rule), ontoggle: (e) => { if (e.target.open) open.add(a.rule); else open.delete(a.rule); } },
    h("summary", {}, `What the agent gets with it (${fmt.n(a.context_tokens)} tokens)`),
    h("pre", { class: "code" }, a.context || "Nothing: no incident looks like it, so the agent starts from the alert alone."));
  return h("div", { class: "cx-alert" },
    h("div", {},
      h("div", { class: "row", style: { gap: "8px" } }, pill(a.state === "firing" ? "firing" : "pending", a.state), a.severity ? h("span", { class: "badge" }, a.severity) : null, since ? h("span", { class: "small muted" }, "for " + since) : null),
      h("h4", {}, a.name || a.rule),
      h("div", { class: "cx-val" }, h("b", {}, a.value == null ? "–" : fmt.si(a.value)), h("span", { class: "muted" }, ` ${a.metric} ${a.op || ""} ${a.threshold ?? ""}`)),
      a.description ? h("p", { class: "cx-desc" }, a.description) : null,
      where.length ? h("div", { class: "cx-where" }, "on " + where.join(", ")) : null,
      h("div", { class: "cx-hand" }, hand, handOut), ctxBox),
    h("div", {},
      h("div", { class: "cx-sub", style: { marginTop: 0 } }, "Seen before"),
      a.matches?.length ? a.matches.map((m, i) => matchCard(ctx, m, i === 0)) : h("div", { class: "cx-hint" }, "No recorded incident looks like this one. When it is over, keep what was found so that the next one is.")));
}

function incidentForm(ctx, old, done, from) {
  const { h, modal, toast, icon } = ctx.core;
  const f = (ph, rows) => rows ? h("textarea", { class: "input", rows, placeholder: ph }) : h("input", { class: "input", placeholder: ph });
  const title = f("Search p99 jumped during a large merge"), alert = f("p99 (the alert rule, if one fired)");
  const symptom = f("What was seen first: the metric, the shard, the numbers", 2), cause = f("Why it happened", 2), fix = f("What made it stop, and what changed so it does not come back", 2);
  const set = (i) => { title.value = i.title || ""; alert.value = i.alert || ""; symptom.value = i.symptom || ""; cause.value = i.cause || ""; fix.value = i.fix || ""; };
  if (old) set(old);
  if (from?.alert) alert.value = from.alert;
  const src = h("textarea", { class: "input", rows: 5, placeholder: "Paste a postmortem, an incident thread or notes" });
  const run = h("input", { class: "input", placeholder: "an on-call run's id" }, );
  if (from?.run) run.value = from.run;
  const drafts = h("div", { class: "cx-drafts" });
  const draftBtn = h("button", { class: "btn sm" }, icon("sparkle"), "Draft it");
  draftBtn.onclick = async () => {
    const body = src.value.trim() ? { text: src.value } : run.value.trim() ? { run: run.value.trim() } : null;
    if (!body) { src.focus(); return; }
    draftBtn.disabled = true;
    try {
      const j = await ctx.api("/v1/incidents/draft", { body, timeout: 150000 });
      const ds = j.drafts || [];
      if (!ds.length) toast("The model found no incident in it", "info");
      else set({ ...ds[0], alert: ds[0].alert || alert.value });
      drafts.replaceChildren(...(ds.length > 1 ? ds.map((d, i) => h("button", { class: "chip", onclick: () => set(d) }, `${i + 1}. ${d.title}`)) : []));
    } catch (e) { toast(e.message, "bad", 8000); }
    draftBtn.disabled = false;
  };
  const go = h("button", { class: "btn primary" }, old ? "Keep the better fix" : "Record it");
  const m = modal({
    title: old ? "A better fix for: " + old.title : "Record an incident",
    wide: true,
    body: h("div", { class: "cx-form" },
      old ? h("div", { class: "cx-hint" }, "The old fix stays as history: a match shows it struck through, under this one.") :
        h("details", { open: !!from?.run }, h("summary", { class: "small", style: { cursor: "pointer" } }, "Draft it with a model, from a postmortem or an on-call run"),
          h("div", { class: "cx-form", style: { marginTop: "8px" } }, src, h("div", { class: "row", style: { gap: "8px" } }, run, draftBtn), drafts,
            h("div", { class: "cx-hint" }, "Nothing is kept until you record it; check what the model wrote first."))),
      h("div", { class: "cx-two" }, h("label", {}, "Title", title), h("label", {}, "Alert", alert)),
      h("label", {}, "Symptom", symptom), h("label", {}, "Cause", cause), h("label", {}, "Fix", fix)),
    foot: [h("button", { class: "btn", onclick: () => m.close() }, "Cancel"), go],
  });
  go.onclick = async () => {
    go.disabled = true;
    try {
      await ctx.api("/v1/incidents", { body: { title: title.value, alert: alert.value, symptom: symptom.value, cause: cause.value, fix: fix.value, replaces: old?.id } });
      m.close();
      toast(old ? "Kept; the old fix is history now" : "Recorded", "ok");
      done?.();
    } catch (e) { toast(e.message, "bad"); go.disabled = false; }
  };
}

// ---------------------------------------------------------------- bringing Claude Code memory here

/**
 * How to fill the Inspector: a key that may write only the memory table,
 * made here, and the one command that points a machine's ChloeCtx at
 * this server with it.
 */
function syncSetup(ctx, j, server) {
  const { h, card, toast } = ctx.core;
  const cmd = (secret) => `printf %s ${secret ? `'${secret}'` : '"$KEY"'} | chloectx sync setup --url ${server} --token-stdin\nchloectx sync push    # in a project's folder: now, rather than within 30 s`;
  const code = copyable(ctx, cmd(null));
  const said = h("span", {}, "Any key that may write ", h("span", { class: "mono" }, j.table), " works; an admin's key also creates the table.");
  const make = h("button", { class: "btn sm primary" }, "Make a sync key");
  make.onclick = async () => {
    make.disabled = true;
    try {
      // The table first, so that a key limited to it can write.
      if (!j.exists && j.create) await ctx.core.api("/v1/sql", { body: { sql: j.create } });
      const k = await ctx.core.api("/v1/admin/keys", { body: { name: `chloectx-sync-${new Date().toISOString().slice(0, 10)}`, scope: "write", tables: { [j.table]: "write" }, note: "chloectx sync, made on the Inspector page" } });
      code.set(cmd(k.secret));
      keyStep.classList.add("done");
      said.replaceChildren(h("b", {}, "Shown once: "), "this key may only write ", h("span", { class: "mono" }, j.table), ". Revoke it under Access and audit.");
      make.remove();
    } catch (e) { toast(`The key: ${e.message}`, "bad", 8000); make.disabled = false; }
  };
  if (!ctx.core.can("admin")) { make.disabled = true; make.title = ctx.core.needs("admin"); }
  const keyStep = h("li", {}, h("span", { class: "cx-n" }, "1"), h("div", {}, h("b", {}, "A key that may write only the memory table"), h("div", {}, make), said));
  return card({
    title: j.exists ? "No project synced yet" : "Nothing synced to this cluster yet",
    sub: "three steps, on a machine where ChloeCtx records Claude Code",
    body: h("ol", { class: "cx-steps" },
      keyStep,
      h("li", {}, h("span", { class: "cx-n" }, "2"), h("div", {}, h("b", {}, "ChloeCtx 0.2.0 or later"), h("span", {}, h("span", { class: "mono" }, "chloectx doctor"), " says which you have; an older one has no ", h("span", { class: "mono" }, "sync"), ": run ", h("span", { class: "mono" }, "git pull && ./install.sh"), " in its checkout."))),
      h("li", {}, h("span", { class: "cx-n" }, "3"), h("div", {}, h("b", {}, "Point it here, once"), h("span", {}, "Each project's daemon then sends what it records every 30 seconds, and the projects appear on this page."), code))),
  });
}

// ---------------------------------------------------------------- Receipts, Blame, snapshots and capsules (a store of this machine)

const STORE_KEY = "chloectx.store";
function pickedStore() { try { return localStorage.getItem(STORE_KEY) || ""; } catch { return ""; } }
function pickStore(id) { try { localStorage.setItem(STORE_KEY, id); } catch { /* private window */ } }
const short = (s, n = 8) => (s || "").slice(0, n);
const SHOWN = new Set(["take", "excerpt", "knowledge", "listed", "briefing"]);
const DEC_TITLE = {
  take: "Shown whole", excerpt: "Shown as a snippet", knowledge: "A rule, decision or fact shown on its own text, ahead of the gate",
  listed: "Named by id (under Further candidates, or a search result)", briefing: "Shown in the session's briefing",
};

function decBadge(ctx, d) {
  const cls = d === "knowledge" ? "know" : SHOWN.has(d) ? "shown" : "";
  return ctx.core.h("span", { class: "cx-dec " + cls, title: DEC_TITLE[d] || "Left out" }, d);
}

const SOURCE_LABEL = { prompt: "Prompt", briefing: "Session start", compact: "After a compaction", search: "memory_search", ask: "memory_ask" };

function receiptsTab(page, ctx, src) {
  const { h, card, table, failure, empty, fmt } = ctx.core;
  pickStore(src.id);
  const st = `/v1/local/${encodeURIComponent(src.id)}`;
  const session = h("input", { class: "input", placeholder: "session id (optional)", style: { maxWidth: "220px" } });
  const listBody = h("div", {}, h("div", { class: "muted small" }, "Loading…"));
  const load = async () => {
    let j;
    try { j = await ctx.api(`${st}/receipts?limit=200&session=${encodeURIComponent(session.value.trim())}`); }
    catch (e) { listBody.replaceChildren(failure(e, "The receipts")); return; }
    const rs = j.receipts || [];
    if (!rs.length) { listBody.replaceChildren(empty("note", "No receipts yet", "A receipt is written with each recall once ChloeCtx 0.3 runs; start a new Claude Code session in the project.")); return; }
    listBody.replaceChildren(table([
      { key: "ts", label: "When", num: true, render: (r) => when(r.ts) },
      { key: "source", label: "Recall", render: (r) => SOURCE_LABEL[r.source] || r.source },
      { key: "query", label: "For", render: (r) => h("div", { class: "cx-cell" }, r.query || r.inputs?.query || (r.source === "briefing" ? "the session's briefing" : "–")) },
      { key: "shown", label: "Shown", num: true, sort: (r) => r.shown.length, render: (r) => fmt.n(r.shown.length) },
      { key: "tokens", label: "Tokens", num: true, render: (r) => fmt.n(r.tokens) },
      { key: "decision", label: "Gate", render: (r) => r.decision || "–" },
      { key: "session", label: "Session", render: (r) => h("span", { class: "mono" }, `${short(r.session)} · ${r.turn}`) },
    ], { rows: rs, sortKey: "ts", desc: true, compact: true, onRow: (r) => { location.hash = ctx.href(src.type, src.id, "receipts", r.id); } }));
  };
  session.onchange = load;
  page.append(h("p", { class: "cx-lede", style: { margin: "0 0 12px" } },
    "Each time memory put records into a Claude Code session's context (a prompt's recall, the briefing at the start, the recall after a compaction, a memory_search), it wrote a receipt: the store's commit it read, each candidate's scores and tokens, why it was taken or left out, and the text the agent was shown. Open one to replay it on the snapshot kept at that commit, and on today's store."),
    card({ title: "Receipts", sub: "newest first; open one to replay it", tools: session, body: listBody }));
  load();
  return null;
}

async function drawSnapshots(ctx, el, store) {
  const { h, table, failure, toast, confirm, fmt } = ctx.core;
  el.replaceChildren(h("div", { class: "muted small" }, "Loading…"));
  let j;
  try { j = await ctx.api(`/v1/local/${encodeURIComponent(store)}/snapshots`); }
  catch (e) { el.replaceChildren(failure(e, "The snapshots")); return; }
  const name = h("input", { class: "input", placeholder: "before-release", style: { maxWidth: "220px" } });
  const keep = h("button", { class: "btn sm primary" }, "Keep one now");
  keep.onclick = async () => {
    const n = name.value.trim();
    if (!n) { name.focus(); return; }
    keep.disabled = true;
    try { const r = await ctx.api(`/v1/local/${encodeURIComponent(store)}/snapshots`, { body: { name: n } }); toast(r.text, "good"); drawSnapshots(ctx, el, store); }
    catch (e) { toast(e.message, "bad"); keep.disabled = false; }
  };
  const ks = (j.snapshots || []).slice().reverse();
  const auto = ks.filter((k) => k.auto).length;
  el.replaceChildren(
    h("div", { class: "cx-hint", style: { marginBottom: "8px" } },
      `${fmt.plural(auto, "automatic snapshot")}, kept after recalls that showed records (at most one an hour) and pruned by age; named ones stay until dropped. Each hard-links the files of the one before it, so it costs about what changed.`),
    h("div", { class: "cx-blameq", style: { marginBottom: "10px" } }, name, keep),
    ks.length ? table([
      { key: "name", label: "Name", render: (k) => h("span", { class: "mono" }, k.name) },
      { key: "at", label: "Commit", num: true },
      { key: "ts", label: "Kept", num: true, render: (k) => when(k.ts) },
      { key: "records", label: "Records", num: true, render: (k) => fmt.n(k.records) },
      { key: "auto", label: "", sortable: false, render: (k) => k.auto ? h("span", { class: "muted small" }, "automatic") : h("button", { class: "btn sm ghost", onclick: async (ev) => {
        ev.stopPropagation();
        if (!(await confirm({ title: `Drop ${k.name}?`, body: "Receipts read at its commit can then replay only on later snapshots and today's store.", ok: "Drop", danger: true }))) return;
        try { await ctx.api(`/v1/local/${encodeURIComponent(store)}/snapshots/${encodeURIComponent(k.name)}`, { method: "DELETE" }); drawSnapshots(ctx, el, store); }
        catch (e) { toast(e.message, "bad"); }
      } }, "Drop") },
    ], { rows: ks, sortKey: "at", desc: true, compact: true, maxHeight: "320px" }) : h("div", { class: "muted small" }, "None kept yet."));
}

/** The capsules cut from a store, and a form to cut one. */
async function drawCapsules(ctx, el, store) {
  const { h, failure, toast, fmt, empty } = ctx.core;
  const st = `/v1/local/${encodeURIComponent(store)}`;
  el.replaceChildren(h("div", { class: "muted small" }, "Loading…"));
  let j;
  try { j = await ctx.api(`${st}/capsules`); }
  catch (e) { el.replaceChildren(failure(e, "The capsules")); return; }
  const title = h("input", { class: "input", placeholder: "Refunds and payments, for the support agent", style: { flex: "1 1 260px" } });
  const PACK = [["decision", "Decisions"], ["preference", "Rules"], ["fact", "Facts"], ["todo", "To-dos"], ["note", "Notes"], ["prompt", "Prompts"], ["answer", "Answers"], ["episode", "Episodes"]];
  const picked = new Set(["decision", "preference", "fact", "todo", "note", "prompt", "answer"]);
  const kinds = h("div", { class: "cx-kinds" }, PACK.map(([k, l]) => {
    const box = h("input", { type: "checkbox", checked: picked.has(k) });
    box.onchange = () => { if (box.checked) picked.add(k); else picked.delete(k); };
    return h("label", {}, box, l);
  }));
  const cut = h("button", { class: "btn sm primary" }, "Cut a capsule");
  cut.onclick = async () => {
    cut.disabled = true;
    try {
      const m = await ctx.api(`${st}/capsules`, { body: { title: title.value.trim(), kinds: [...picked] }, timeout: 600000 });
      toast(`Cut: ${fmt.plural(m.records, "record")}, ${fmt.bytes(m.bytes)}`, "good");
      drawCapsules(ctx, el, store);
    } catch (e) { toast(e.message, "bad", 8000); cut.disabled = false; }
  };
  const cs = j.capsules || [];
  el.replaceChildren(
    h("div", { class: "cx-hint", style: { marginBottom: "8px" } }, "A capsule is a read-only ChloeDB file of the records you pick (no raw tool output by default), redacted again, with a manifest. Another agent mounts it as an MCP server with capsule_ask, capsule_search, capsule_timeline and capsule_sql."),
    h("div", { class: "cx-form", style: { marginBottom: "12px" } }, h("div", { class: "cx-blameq" }, title, cut), kinds),
    cs.length ? h("div", { class: "cx-caps" }, cs.map((c) => h("div", { class: "cx-cap" },
      h("div", { class: "cx-sphd" }, h("b", {}, c.title), h("span", { class: "muted small" }, when(c.created_ts))),
      h("div", { class: "cx-why" }, h("span", {}, fmt.plural(c.records, "record")), h("span", {}, fmt.bytes(c.bytes)), c.redacted ? h("span", {}, `${fmt.n(c.redacted)} redacted`) : null,
        ...Object.entries(c.by_kind || {}).sort((a, b) => b[1] - a[1]).slice(0, 5).map(([k, n]) => h("span", {}, `${fmt.n(n)} ${(REC[k] || [, k])[1].toLowerCase()}`))),
      c.topics?.length ? h("div", { class: "cx-why" }, h("span", {}, "about: " + c.topics.slice(0, 8).join(", "))) : null,
      copyable(ctx, `claude mcp add ${(c.title || "capsule").toLowerCase().replace(/[^a-z0-9]+/g, "-").replace(/^-|-$/g, "").slice(0, 30) || "capsule"} -- chloectx mount ${c.dir}`)))) : empty("box", "No capsule cut yet", "Cut one above: it takes a few seconds."));
}

/** Snapshots and capsules: what receipts replay on, and what is shared from the store. */
function keepTab(page, ctx, src) {
  const { h, card } = ctx.core;
  const snaps = h("div"), caps = h("div");
  page.append(h("div", { class: "cx-split" },
    card({ title: "Kept snapshots", sub: "copies of the store that receipts replay on", body: snaps }),
    card({ title: "Capsules", sub: "cut from this store, to mount in another agent", body: caps })));
  drawSnapshots(ctx, snaps, src.id);
  drawCapsules(ctx, caps, src.id);
  return null;
}

function rowsTable(ctx, rows, now, texts) {
  const { h, table, fmt } = ctx.core;
  const max = Math.max(1e-9, ...rows.map((r) => r.score || 0));
  return table([
    { key: "rank", label: "#", num: true, render: (r) => (r.rank === 0 ? "k" : r.rank) },
    { key: "kind", label: "Kind" },
    { key: "id", label: "Record", render: (r) => h("div", { style: { minWidth: "220px" } }, texts?.[r.id] ? h("div", { class: "cx-cell" }, texts[r.id]) : null, h("span", { class: "mono muted small" }, r.id)) },
    { key: "score", label: "Score", num: true, render: (r) => h("span", {}, (r.score || 0).toFixed(3), h("span", { class: "cx-bar" }, h("i", { style: { width: `${Math.round(100 * (r.score || 0) / max)}%` } }))) },
    { key: "bm25", label: "BM25", num: true, render: (r) => (r.bm25 == null ? "–" : r.bm25.toFixed(2)) },
    { key: "cos", label: "Cosine", num: true, render: (r) => (r.cos == null ? "–" : r.cos.toFixed(3)) },
    { key: "tokens", label: "Tokens", num: true, render: (r) => fmt.n(r.tokens) },
    { key: "decision", label: "Decision", render: (r) => decBadge(ctx, r.decision) },
    ...(now ? [{ key: "now", label: "Now", sortable: false, render: (r) => (now[r.id]?.length ? h("span", { class: "cx-flag" }, now[r.id].join(", ")) : "") }] : []),
  ], { rows, sortKey: "rank", desc: false, compact: true });
}

function replayCard(ctx, replays) {
  const { h, card, pill } = ctx.core;
  if (!replays?.length) return null;
  return card({
    title: "Replay", sub: "the same retrieval, run again with the receipt's inputs",
    body: h("div", {}, replays.map((r) => {
      const v = r.verdict || {};
      const on = r.on === "kept snapshot" ? `On the snapshot kept at commit ${r.txn}` : r.on === "later snapshot" ? `On the nearest later snapshot (commit ${r.txn}; none was kept at the receipt's)` : `On today's store (commit ${r.txn})`;
      const state = v.identical ? pill("good", "identical") : v.same_shown ? pill("warn", "same records") : pill("bad", "different");
      return h("div", { class: "cx-verdict" }, state, h("div", {}, h("b", {}, on),
        h("div", { class: "muted small" }, v.identical ? "The same rows, scores and decisions." : v.same_shown ? "The same records shown with the same decisions; scores or lower ranks moved." : "What it shows now differs:"),
        !v.identical && v.changes?.length ? h("ul", {}, v.changes.map((c) => h("li", {}, c))) : null));
    })),
  });
}

function receiptPage(page, ctx, src, id) {
  const { h, card, failure } = ctx.core;
  const store = src.id;
  const body = h("div", {}, h("div", { class: "muted small" }, "Loading the receipt and replaying it…"));
  page.append(h("div", { class: "cx-crumbs", style: { marginBottom: "10px" } }, h("a", { href: ctx.href(src.type, src.id, "receipts") }, "Receipts"), " / ", h("span", { class: "mono" }, id)), body);
  (async () => {
    let j;
    try { j = await ctx.api(`/v1/local/${encodeURIComponent(store)}/receipts/${encodeURIComponent(id)}`, { timeout: 120000 }); }
    catch (e) { body.replaceChildren(failure(e, "The receipt")); return; }
    const r = j.receipt;
    const ev = r.evidence || {};
    const facts = card({
      title: `${SOURCE_LABEL[r.source] || r.source} recall`, sub: [`${when(r.ts)} · `, h("a", { href: ctx.href(src.type, src.id, "sessions", r.session) }, `session ${short(r.session)}`), ` turn ${r.turn}`],
      body: h("dl", { class: "cx-kv" },
        h("dt", {}, "Query"), h("dd", {}, r.inputs?.query || "–"),
        h("dt", {}, "Store"), h("dd", {}, `commit ${r.snapshot.txn}${r.snapshot.seq_no != null ? ` (seq_no ${r.snapshot.seq_no})` : ""} · ${r.snapshot.records} records · embedder ${r.embedder}`),
        r.decision ? [h("dt", {}, "Gate"), h("dd", {}, `${r.decision} at ${r.gate.toFixed(3)} · ${r.candidates} candidates · best cosine ${(ev.max_cos || 0).toFixed(3)}`)] : null,
        h("dt", {}, "Shown"), h("dd", {}, `${r.shown.length} records, about ${r.tokens} tokens`),
        h("dt", {}, "Already shown"), h("dd", {}, r.inputs?.skip?.length ? `${r.inputs.skip.length} records shown to the session before, left out` : "none")),
    });
    const rows = card({ title: "Candidates", sub: "every record ranked, with why it was taken or left out", flush: true, body: rowsTable(ctx, r.rows || [], null, Object.fromEntries(Object.entries(j.records || {}).map(([k, v]) => [k, v.text]))) });
    const text = h("details", { class: "cx-context" }, h("summary", {}, "What the agent was shown"), h("pre", { class: "code" }, r.text || "(nothing)"));
    body.replaceChildren(h("div", { class: "cx-stack" },
      h("div", { class: "cx-split" }, facts, replayCard(ctx, j.replays) || h("div")),
      rows, card({ body: text })));
  })();
  return null;
}

function blameTab(page, ctx, src, target) {
  const { h, card, failure, empty } = ctx.core;
  pickStore(src.id);
  const input = h("input", { class: "input", placeholder: "app/payments/refunds.py:42", value: target || "", spellcheck: "false", autocomplete: "off" });
  const go = h("button", { class: "btn primary" }, "Blame");
  const out = h("div");
  const files = h("div", { class: "cx-drafts", style: { marginTop: "8px" } });
  const run = async () => {
    const t = input.value.trim();
    if (!t) { input.focus(); return; }
    if (t !== (target || "")) { location.hash = ctx.href(src.type, src.id, "blame", t); return; }
    go.disabled = true;
    out.replaceChildren(h("div", { class: "muted small" }, "Looking for the edit and replaying its recall…"));
    let b;
    try { b = await ctx.api(`/v1/local/${encodeURIComponent(src.id)}/blame?target=${encodeURIComponent(t)}`, { timeout: 120000 }); }
    catch (e) { out.replaceChildren(failure(e, "Blame")); go.disabled = false; return; }
    go.disabled = false;
    drawBlame(ctx, out, b, src);
  };
  go.onclick = run;
  input.onkeydown = (e) => { if (e.key === "Enter") run(); };
  page.append(h("p", { class: "cx-lede", style: { margin: "0 0 12px" } }, "Name a line of code (path:line, relative to the project) or a file. Blame finds the recorded edit that wrote it, the prompt and tool calls behind it, and what memory had put in the agent's context before, each record checked against the store today, with the recall replayed."),
    card({ body: h("div", {}, h("div", { class: "cx-blameq" }, input, go), files) }), h("div", { style: { height: "16px" } }), out);
  // The files sessions edited most, to start from.
  src.records("kind=tool_call&limit=500").then((j) => {
    const n = new Map();
    for (const r of j.records || []) if (/^(Edit|Write|MultiEdit) /.test(r.text)) { const f = r.text.split(/[ :\n]/)[1]; if (f) n.set(f, (n.get(f) || 0) + 1); }
    const top = [...n.entries()].sort((a, b) => b[1] - a[1]).slice(0, 6);
    if (top.length) files.replaceChildren(h("span", { class: "muted small" }, "Edited most:"), ...top.map(([f]) => {
      const rel = src.type === "store" ? f.replace(/^.*?\/(?=(app|src|tests|docs|lib|migrations)\/)/, "") : f;
      return h("button", { class: "chip", onclick: () => { location.hash = ctx.href(src.type, src.id, "blame", rel); } }, rel);
    }));
  }).catch(() => {});
  if (target) run();
  else out.replaceChildren(empty("search", "Name a line", "For example app/payments/refunds.py:42, or pick a file above. Lines written outside a recorded Claude Code session have no edit to find."));
  return null;
}

function drawBlame(ctx, out, b, src) {
  const { h, card, empty } = ctx.core;
  const g = b.git;
  const head = card({
    title: b.file + (b.line ? `:${b.line}` : ""), sub: b.how || "",
    body: h("dl", { class: "cx-kv" },
      b.line_text != null ? [h("dt", {}, "Line"), h("dd", {}, h("code", {}, b.line_text))] : null,
      g ? [h("dt", {}, "Commit"), h("dd", {}, g.uncommitted ? "not committed yet" : `${(g.commit || "").slice(0, 10)} · ${g.summary || ""}${g.time ? ` · ${when(g.time)}` : ""}`)] : null,
      b.edit ? [h("dt", {}, "Edit"), h("dd", {}, `${when(b.edit.ts)} · `, h("a", { href: ctx.href(src.type, src.id, "sessions", b.edit.session) }, `session ${short(b.edit.session)}`), ` turn ${b.edit.turn} · `, h("span", { class: "mono" }, b.edit.id))] : null,
      b.prompt ? [h("dt", {}, "Prompt"), h("dd", {}, b.prompt.text)] : null,
      b.calls?.length ? [h("dt", {}, "Before it"), h("dd", {}, h("ul", { class: "cx-causes" }, b.calls.slice(-8).map((c) => h("li", {}, h("code", {}, c.text.split("\n")[0].slice(0, 160))))))] : null,
      b.notes?.length ? [h("dt", {}, "From the turn"), h("dd", {}, h("ul", { class: "cx-causes" }, b.notes.map((n) => h("li", {}, kindBadge(ctx, n.kind), " ", n.text))))] : null),
  });
  if (!b.edit) {
    out.replaceChildren(head, h("div", { style: { height: "16px" } }), empty("search", "No recorded edit", `ChloeCtx recorded no edit of ${b.file}: it was written outside a recorded session, or before ChloeCtx was installed.`));
    return;
  }
  const parts = [head];
  if (b.causes?.length) parts.push(card({ title: "Worth a look", body: h("ul", { class: "cx-causes" }, b.causes.map((c) => h("li", {}, c))) }));
  for (const rc of b.receipts || []) {
    const now = Object.fromEntries(rc.rows.map((x) => [x.row.id, x.now]));
    parts.push(card({
      title: rc.turn === b.edit.turn ? "Recalled in the edit's turn" : `Recalled earlier in the session (turn ${rc.turn})`,
      sub: `${SOURCE_LABEL[rc.source] || rc.source} · receipt ${rc.receipt}`,
      tools: h("a", { class: "btn sm", href: ctx.href(src.type, src.id, "receipts", rc.receipt) }, "Open the receipt"),
      flush: true,
      body: rowsTable(ctx, rc.rows.map((x) => x.row), now, Object.fromEntries(rc.rows.map((x) => [x.row.id, x.text]))),
    }));
  }
  if (!(b.receipts || []).length) parts.push(card({ body: h("div", { class: "muted small" }, "Memory recalled nothing into the session before the edit (no receipt shows a record).") }));
  const rp = replayCard(ctx, b.replays);
  if (rp) parts.push(rp);
  out.replaceChildren(h("div", { class: "cx-stack" }, parts));
}
