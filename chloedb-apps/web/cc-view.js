// The app creator, inside ChloeDB's command center (#/app/apps). The command
// center loads this file through chloedb-server, which forwards
// /v1/apps/apps/... to the engine, and runs render(el, ctx) with its own
// helpers: ctx.core (h, card, table, toast, modal, drawer, ...) and ctx.api
// (the engine's API, called with the signed-in key: read scope for GET, admin
// for the rest). The pieces of the studio it shares (the design tokens, the
// frame policy) come from the engine as text (GET /api/kit) and are imported
// from blobs, as the command center imports this file.
//
// Pages: #/app/apps (describe a new app; the apps), #/app/apps/a/<id>[/<screen>]
// (build it: the conversation, the live app, and what it is made of) and
// #/app/apps/library (the shared library, searched).
//
// A view is code a model wrote, and it runs here as it does in the studio:
// in a frame with the studio's sandbox (no same origin, so it reads nothing
// of this page, its key included), a Content-Security-Policy with no network
// (web/policy.js), and every read and write sent to this page as a message,
// which calls the engine's API for that one app (the engine scopes app.sql
// to the app's tables). The broker below answers only the methods the
// studio's does. What differs: people's sign-in (a cookie of the engine's)
// does not pass through chloedb-server, so the creator shows an app as a
// visitor sees it; its own page (/a/<id>) is where people sign in.
//
// chloedb-server forwards whole answers, so live updates come by long polls
// of GET /api/events (the same events the studio's stream carries).

export default { render };

const CSS = `
.ac { --ac-c: #2f5bd3; --ac-s: #12a37a; --ac-r: #9aa3ae; --ac-p: #d9822b; display: flex; flex-direction: column; gap: 16px; min-width: 0; }
@media (prefers-color-scheme: dark) { :root:not([data-theme="light"]) .ac { --ac-c: #7aa2ff; --ac-s: #3fcf9a; --ac-r: #6f7782; --ac-p: #f0a35c; } }
:root[data-theme="dark"] .ac { --ac-c: #7aa2ff; --ac-s: #3fcf9a; --ac-r: #6f7782; --ac-p: #f0a35c; }
.ac .muted { color: var(--muted); } .ac .dim { color: var(--text-2); } .ac .mono { font-family: var(--mono); }
.ac .row { display: flex; align-items: center; gap: 8px; min-width: 0; } .ac .grow { flex: 1; min-width: 0; }
.ac .spin { display: inline-block; width: 12px; height: 12px; border: 2px solid var(--line-2); border-top-color: var(--accent); border-radius: 50%; animation: ac-spin .8s linear infinite; vertical-align: -2px; }
@keyframes ac-spin { to { transform: rotate(360deg); } }
.ac .ac-chip { display: inline-flex; align-items: center; gap: 6px; border: 1px solid var(--line); background: var(--surface); color: var(--text-2); border-radius: 999px; padding: 5px 11px; font: inherit; font-size: 12.5px; cursor: pointer; text-align: left; }
.ac .ac-chip:hover { border-color: var(--accent); color: var(--text); }
.ac .tag { display: inline-flex; align-items: center; gap: 4px; font-size: 11px; font-weight: 600; letter-spacing: .02em; padding: 1px 7px; border-radius: 999px; background: var(--surface-3); color: var(--text-2); white-space: nowrap; }
.ac .tag.live { background: var(--good-soft); color: var(--good-ink); } .ac .tag.try { background: var(--warn-soft); color: var(--cc-warn-ink, #8a5b00); }
.ac .tag.bad { background: var(--crit-soft); color: var(--crit-ink); }
.ac .appcard .cardacts { display: flex; gap: 6px; margin-top: 2px; }
.ac .tag.k-create { background: color-mix(in srgb, var(--ac-c) 16%, transparent); color: var(--ac-c); }
.ac .tag.k-scratch { background: color-mix(in srgb, var(--ac-s) 18%, transparent); color: var(--ac-s); }
.ac .tag.k-reused { background: color-mix(in srgb, var(--ac-r) 22%, transparent); color: var(--text-2); }
.ac .tag.k-hand { background: color-mix(in srgb, var(--ac-p) 18%, transparent); color: var(--ac-p); }

/* home */
.ac .hero { position: relative; overflow: hidden; border: 1px solid var(--line); border-radius: 14px; background: var(--surface); padding: 26px 26px 20px; }
.ac .hero::before { content: ""; position: absolute; inset: -40% -10% auto auto; width: 60%; height: 180%; background: radial-gradient(closest-side, color-mix(in srgb, var(--accent) 14%, transparent), transparent); pointer-events: none; }
.ac .hero h1 { margin: 0 0 4px; font-size: 26px; letter-spacing: -.01em; }
.ac .hero p.lead { margin: 0 0 16px; color: var(--text-2); max-width: 52rem; }
.ac .prompt { position: relative; border: 1px solid var(--line-2); border-radius: 12px; background: var(--bg); transition: border-color .15s, box-shadow .15s; }
.ac .prompt:focus-within { border-color: var(--accent); box-shadow: 0 0 0 4px var(--focus); }
.ac .prompt textarea { display: block; width: 100%; min-height: 96px; resize: vertical; border: 0; outline: 0; background: transparent; color: var(--text); font: inherit; font-size: 15px; line-height: 1.5; padding: 14px 16px 4px; box-sizing: border-box; }
.ac .prompt .bar { display: flex; align-items: center; gap: 10px; padding: 8px 10px 10px 16px; }
.ac .examples { display: flex; flex-wrap: wrap; gap: 8px; margin-top: 12px; }
.ac .facts { display: flex; flex-wrap: wrap; gap: 6px 18px; margin-top: 14px; color: var(--muted); font-size: 12.5px; }
.ac .facts b { color: var(--text); font-variant-numeric: tabular-nums; }
.ac .gallery { display: grid; grid-template-columns: repeat(auto-fill, minmax(260px, 1fr)); gap: 12px; }
.ac .appcard { display: flex; flex-direction: column; gap: 8px; border: 1px solid var(--line); border-radius: 12px; background: var(--surface); padding: 14px; text-decoration: none; color: inherit; transition: border-color .15s, transform .15s, box-shadow .15s; min-width: 0; }
.ac .appcard:hover { border-color: var(--accent); transform: translateY(-1px); box-shadow: 0 6px 18px rgba(0,0,0,.06); }
.ac .appcard .ico { width: 36px; height: 36px; border-radius: 9px; display: grid; place-items: center; font-weight: 700; color: #fff; flex: none; }
.ac .appcard .nm { font-weight: 650; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.ac .appcard .ds { color: var(--text-2); font-size: 12.5px; display: -webkit-box; -webkit-line-clamp: 2; -webkit-box-orient: vertical; overflow: hidden; min-height: 2.6em; }
.ac .appcard .meta { display: flex; align-items: center; gap: 8px; font-size: 12px; color: var(--muted); }
.ac .appgrp { display: flex; flex-direction: column; gap: 4px; min-width: 0; }
.ac .appgrp .earlier { display: flex; flex-wrap: wrap; gap: 4px 10px; padding: 0 6px; font-size: 12px; color: var(--muted); }
.ac .appgrp .earlier a { color: var(--text-2); }
.ac .sectionhd { display: flex; align-items: baseline; gap: 10px; margin: 4px 0 -4px; } .ac .sectionhd h2 { margin: 0; font-size: 15px; }

/* the made-of meter */
.ac .ac-meter { display: flex; height: 8px; border-radius: 999px; overflow: hidden; background: var(--surface-3); }
.ac .ac-meter.big { height: 14px; }
.ac .ac-meter span { display: block; min-width: 0; }
.ac .ac-meter .k-create, .ac i.k-create { background: var(--ac-c); } .ac .ac-meter .k-scratch, .ac i.k-scratch { background: var(--ac-s); }
.ac .ac-meter .k-reused, .ac i.k-reused { background: var(--ac-r); } .ac .ac-meter .k-hand, .ac i.k-hand { background: var(--ac-p); }
.ac .ac-legend { display: flex; flex-wrap: wrap; gap: 4px 14px; font-size: 12px; color: var(--text-2); }
.ac .ac-legend i { display: inline-block; width: 9px; height: 9px; border-radius: 3px; margin-right: 5px; vertical-align: -1px; }
.ac .headline { display: flex; align-items: baseline; gap: 10px; }
.ac .headline .big { font-size: 34px; font-weight: 700; letter-spacing: -.02em; color: var(--ac-c); font-variant-numeric: tabular-nums; }
.ac .tiles { display: grid; grid-template-columns: repeat(2, minmax(0, 1fr)); gap: 8px; }
.ac .tile { border: 1px solid var(--line); border-radius: 9px; padding: 8px 10px; background: var(--surface); }
.ac .tile b { display: block; font-size: 17px; font-variant-numeric: tabular-nums; } .ac .tile span { color: var(--muted); font-size: 11.5px; }
.ac .pieces { display: flex; flex-direction: column; }
.ac .piece { display: grid; grid-template-columns: minmax(0, 1fr) auto; gap: 2px 10px; padding: 8px 4px; border-top: 1px solid var(--line); cursor: pointer; border-radius: 6px; }
.ac .piece:hover { background: var(--surface-2); }
.ac .piece .nm { font-family: var(--mono); font-size: 12.5px; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.ac .piece .how { grid-column: 1 / -1; color: var(--muted); font-size: 11.5px; }
.ac .piece .ln { font-size: 11.5px; color: var(--text-2); font-variant-numeric: tabular-nums; text-align: right; white-space: nowrap; }
.ac .piece .gen { grid-column: 1 / -1; display: flex; height: 4px; border-radius: 3px; overflow: hidden; background: var(--surface-3); }
.ac .piece .gen .w { background: color-mix(in srgb, var(--text) 55%, transparent); }

/* the workspace */
.ac.ws { gap: 10px; }
.ac .wsbar { display: flex; align-items: center; gap: 10px; flex-wrap: wrap; }
.ac .wsbar h1 { margin: 0; font-size: 18px; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; max-width: 40vw; }
.ac .crumb { color: var(--muted); text-decoration: none; font-size: 13px; display: inline-flex; align-items: center; gap: 4px; }
.ac .crumb:hover { color: var(--text); }
.ac .grid3 { display: grid; grid-template-columns: minmax(280px, 340px) minmax(0, 1fr) minmax(300px, 380px); gap: 12px; height: calc(100dvh - var(--ac-top, 150px)); min-height: 560px; }
.ac .col { display: flex; flex-direction: column; min-height: 0; border: 1px solid var(--line); border-radius: 12px; background: var(--surface); overflow: hidden; }
.ac .colhd { display: flex; align-items: center; gap: 8px; padding: 10px 12px; border-bottom: 1px solid var(--line); min-height: 42px; box-sizing: border-box; }
.ac .colhd h3 { margin: 0; font-size: 13px; }
.ac .scroll { flex: 1; overflow: auto; min-height: 0; }
.ac .convo { display: flex; flex-direction: column; gap: 10px; padding: 12px; }
.ac .msg { max-width: 92%; border-radius: 12px; padding: 8px 11px; font-size: 13px; line-height: 1.45; overflow-wrap: anywhere; }
.ac .msg.you { align-self: flex-end; background: var(--accent); color: var(--accent-ink); border-bottom-right-radius: 4px; }
.ac .msg.you .src { display: block; font-size: 10.5px; opacity: .8; margin-top: 2px; }
.ac .msg.it { align-self: flex-start; background: var(--surface-2); border: 1px solid var(--line); border-bottom-left-radius: 4px; }
.ac .msg.it.bad { background: var(--crit-soft); border-color: transparent; }
.ac .msg.sys { align-self: stretch; max-width: none; background: transparent; border: 1px dashed var(--line-2); color: var(--text-2); font-size: 12px; }
.ac .msg.sys .i { width: 13px; height: 13px; vertical-align: -2px; }
.ac-down { position: fixed; left: 50%; top: 64px; transform: translateX(-50%); z-index: 40; background: var(--surface); border: 1px solid var(--line-2); border-radius: 999px; padding: 6px 14px; font-size: 12.5px; color: var(--text-2); box-shadow: 0 6px 20px rgba(0,0,0,.12); }
.ac .prog .stopbar { display: flex; gap: 6px; align-items: center; }
.ac .msg .m { display: flex; flex-wrap: wrap; align-items: center; gap: 4px 10px; margin-top: 4px; font-size: 11.5px; color: var(--muted); }
.ac .linkbtn { border: 0; background: none; padding: 0; color: var(--accent); font: inherit; cursor: pointer; }
.ac .linkbtn:hover { text-decoration: underline; }
.ac .composer { border-top: 1px solid var(--line); padding: 10px; background: var(--surface); }
.ac .composer .prompt textarea { min-height: 64px; font-size: 13.5px; padding: 10px 12px 2px; }
.ac .composer .prompt .bar { padding: 6px 8px 8px 12px; }
.ac .sugs { display: flex; flex-direction: column; gap: 6px; }
.ac .sug { border: 1px dashed var(--line-2); border-radius: 10px; padding: 8px 10px; font-size: 12.5px; }
.ac .sug .why { color: var(--muted); font-size: 11.5px; margin-top: 2px; }
.ac .sug .bar { display: flex; gap: 6px; align-items: center; margin-top: 6px; }
.ac .lbl { font-size: 11px; font-weight: 650; text-transform: uppercase; letter-spacing: .06em; color: var(--muted); margin: 4px 0 0; }
.ac .prog { border: 1px solid var(--line); border-radius: 12px; padding: 10px 12px; background: var(--surface); display: flex; flex-direction: column; gap: 8px; }
.ac .pbar { height: 6px; border-radius: 999px; background: var(--surface-3); overflow: hidden; }
.ac .pbar > div { height: 100%; background: linear-gradient(90deg, var(--accent), var(--ac-s)); transition: width .25s linear; }
.ac .steps { list-style: none; margin: 0; padding: 0; display: flex; flex-direction: column; gap: 4px; font-size: 12.5px; }
.ac .steps li { display: flex; align-items: center; gap: 8px; color: var(--muted); } .ac .steps li.now { color: var(--text); font-weight: 600; } .ac .steps li.done { color: var(--text-2); }
.ac .steps .mk { width: 14px; text-align: center; } .ac .steps .took { margin-left: auto; font-size: 11px; color: var(--muted); font-weight: 400; }
.ac .bits { display: flex; flex-wrap: wrap; gap: 4px; }
.ac .bit { font-size: 11px; border-radius: 6px; padding: 2px 6px; background: var(--surface-2); border: 1px solid var(--line); display: inline-flex; gap: 5px; align-items: center; }
.ac .bit.done { color: var(--good-ink); }
.ac .now { font-size: 12px; color: var(--text-2); }
.ac details.log summary { cursor: pointer; font-size: 11.5px; color: var(--muted); }
.ac details.log div { font-family: var(--mono); font-size: 11px; color: var(--text-2); max-height: 160px; overflow: auto; margin-top: 4px; white-space: pre-wrap; }

/* the stage */
.ac .ac-stage { background: var(--sunken); }
.ac .screens { display: flex; gap: 2px; overflow-x: auto; scrollbar-width: none; flex: 1; min-width: 0; }
.ac .screens button { border: 0; background: none; color: var(--text-2); font: inherit; font-size: 12.5px; padding: 5px 10px; border-radius: 7px; cursor: pointer; white-space: nowrap; display: inline-flex; gap: 6px; align-items: center; }
.ac .screens button[aria-pressed="true"] { background: var(--surface-3); color: var(--text); font-weight: 600; }
.ac .screens .dot { width: 6px; height: 6px; border-radius: 50%; background: var(--crit); }
.ac .screens .usage-tab { margin-left: auto; }
.ac .screens .usage-tab .pulse { width: 7px; height: 7px; border-radius: 50%; background: var(--good); box-shadow: 0 0 0 3px color-mix(in srgb, var(--good) 22%, transparent); }
/* the Usage tab: the studio's Activity view (web/activity.css), under the
   command center's tokens, given the names the view reads */
.ac .ac-usage { flex: 1; min-height: 0; position: relative; background: var(--sunken);
  --fg: var(--text); --bg: var(--sunken); --card: var(--surface); --soft: var(--surface-2); --ok: var(--good); --bad: var(--crit);
  --accent-fg: var(--accent-ink); --radius: 12px; --radius-control: 8px; --card-border: var(--line); --card-shadow: none; }
.ac .ac-usage[hidden] { display: none; }
.ac .device[hidden] { display: none; }
/* Usage takes the inspector's column too: its charts and visits need the width */
.ac .grid3.usage-on { grid-template-columns: minmax(280px, 340px) minmax(0, 1fr); }
.ac .grid3.usage-on > .inspect { display: none; }
.ac .ac-usage .sep { flex: none; }
.ac .device { flex: 1; min-height: 0; display: flex; align-items: flex-start; justify-content: center; padding: 16px; overflow: auto; position: relative; }
.ac .device .shell { position: relative; background: var(--bg); width: 100%; height: 100%; border-radius: 10px; overflow: hidden; box-shadow: 0 1px 0 var(--line), 0 10px 30px rgba(0,0,0,.08); }
.ac .device.phone { padding: 12px 8px; }
.ac .device.phone .shell { box-sizing: border-box; width: 408px; max-width: 100%; height: min(800px, 100%); border-radius: 34px; border: 9px solid #15171b; box-shadow: 0 18px 40px rgba(0,0,0,.18); }
.ac .device.tablet .shell { width: 820px; max-width: 100%; height: min(1000px, 100%); border-radius: 22px; border: 9px solid #15171b; }
@media (prefers-color-scheme: dark) { :root:not([data-theme="light"]) .ac .device.phone .shell, :root:not([data-theme="light"]) .ac .device.tablet .shell { border-color: #3a3f48; } }
:root[data-theme="dark"] .ac .device.phone .shell, :root[data-theme="dark"] .ac .device.tablet .shell { border-color: #3a3f48; }
.ac .device iframe { position: absolute; inset: 0; width: 100%; height: 100%; border: 0; background: #fff; }
/* Desktop: drawn 1280 px wide (or the stage's width, when wider), scaled to fit down to MIN_SCALE, then scrolled (fitDevice) */
.ac .device .sizer { display: none; }
.ac .device.desktop .sizer { display: block; flex: none; }
.ac .device.desktop { padding: 0; overflow: auto hidden; justify-content: flex-start; }
.ac .device.desktop .shell { position: absolute; left: 0; top: 0; border-radius: 0; transform-origin: 0 0; }
.ac .home-name { border: 0; outline: 0; background: transparent; color: var(--text); font: inherit; font-size: 13px; min-width: 0; flex: 0 1 200px; padding: 4px 0; border-bottom: 1px dashed var(--line-2); }
.ac .pubchoices { display: flex; flex-direction: column; gap: 6px; }
.ac .pubchoice { display: flex; gap: 10px; align-items: flex-start; padding: 8px 10px; border: 1px solid var(--line); border-radius: 10px; cursor: pointer; }
.ac .pubchoice.on { border-color: var(--accent); background: color-mix(in srgb, var(--accent) 6%, transparent); }
.ac .pubchoice input { margin-top: 3px; }
.ac .pubchoice .what { display: block; font-size: 12.5px; color: var(--text-2); }
.ac .pubwarn { border-radius: 10px; padding: 8px 10px; font-size: 12.5px; background: var(--warn-soft, var(--surface-2)); border: 1px solid color-mix(in srgb, var(--warn, #b7791f) 30%, transparent); }
.ac .device iframe[hidden] { display: none; }
.ac .viewerr { position: absolute; left: 12px; right: 12px; bottom: 12px; display: flex; gap: 8px; align-items: center; background: var(--crit-soft); border: 1px solid color-mix(in srgb, var(--crit) 30%, transparent); border-radius: 10px; padding: 8px 10px; font-size: 12.5px; }
.ac .building { position: absolute; inset: 0; display: grid; place-items: center; padding: 20px; text-align: center; }
.ac .building .box { max-width: 420px; display: flex; flex-direction: column; gap: 10px; align-items: stretch; text-align: left; }
.ac .building h2 { margin: 0; font-size: 17px; text-align: center; }
.ac .previewbar { position: absolute; left: 0; right: 0; top: 0; z-index: 2; display: flex; gap: 8px; align-items: center; padding: 6px 10px; background: color-mix(in srgb, var(--warn-soft) 92%, transparent); font-size: 12px; }
.ac .trybar { display: flex; gap: 8px; align-items: center; padding: 8px 12px; background: var(--warn-soft); border-bottom: 1px solid var(--line); font-size: 12.5px; }

/* the inspector */
.ac .itabs { display: flex; flex-wrap: wrap; gap: 2px; padding: 6px; border-bottom: 1px solid var(--line); }
.ac .itabs button { border: 0; background: none; color: var(--text-2); font: inherit; font-size: 12.5px; padding: 6px 9px; border-radius: 7px; cursor: pointer; white-space: nowrap; }
.ac .itabs button[aria-pressed="true"] { background: var(--accent-soft); color: var(--accent); font-weight: 600; }
.ac .ibody { padding: 12px; display: flex; flex-direction: column; gap: 12px; }
.ac .note { font-size: 12px; color: var(--muted); line-height: 1.45; }
.ac .list { display: flex; flex-direction: column; flex: none; border: 1px solid var(--line); border-radius: 10px; overflow: hidden; }
.ac .ibody > *, .acr-tl > * { flex: none; }
.ac .li { display: flex; align-items: center; gap: 8px; padding: 8px 10px; border-top: 1px solid var(--line); font-size: 12.5px; cursor: pointer; background: var(--surface); }
.ac .li:first-child { border-top: 0; } .ac .li:hover { background: var(--surface-2); } .ac .li.on { background: var(--accent-soft); }
.ac .li .nm { font-family: var(--mono); overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.ac pre.src { margin: 0; max-height: 52vh; overflow: auto; font-size: 11.5px; line-height: 1.5; white-space: pre; }
.ac textarea.stmt { width: 100%; box-sizing: border-box; min-height: 220px; font-family: var(--mono); font-size: 11.5px; line-height: 1.5; border: 1px solid var(--line-2); border-radius: 8px; padding: 8px; background: var(--code); color: var(--text); resize: vertical; }
.ac .ver { display: grid; grid-template-columns: 38px minmax(0, 1fr); gap: 2px 8px; padding: 9px 10px; border-top: 1px solid var(--line); font-size: 12.5px; background: var(--surface); }
.ac .ver:first-child { border-top: 0; } .ac .ver .n { font-weight: 700; color: var(--muted); } .ac .ver.on .n { color: var(--good-ink); }
.ac .ver .q { color: var(--muted); font-size: 11.5px; font-style: italic; }
.ac .ver .m { display: flex; flex-wrap: wrap; gap: 4px 10px; align-items: center; font-size: 11.5px; color: var(--muted); margin-top: 3px; }
.ac .design { display: flex; gap: 10px; align-items: center; border: 1px solid var(--line); border-radius: 10px; padding: 10px; cursor: pointer; background: var(--surface); text-align: left; font: inherit; color: inherit; width: 100%; }
.ac .design.on { border-color: var(--accent); box-shadow: 0 0 0 3px var(--focus); }
.ac .ac-sw { display: inline-block; width: 22px; height: 22px; border-radius: 6px; border: 1px solid var(--line); flex: none; }
.ac .reach { display: flex; gap: 10px; align-items: flex-start; padding: 10px; border-top: 1px solid var(--line); background: var(--surface); }
.ac .reach:first-child { border-top: 0; } .ac .reach.wants { background: var(--warn-soft); }
.ac .reach .what { display: block; color: var(--muted); font-size: 11.5px; }
.ac .pubtag { display: inline-flex; align-items: center; gap: 6px; border: 1px solid var(--line); background: var(--surface); color: var(--text-2); border-radius: 999px; padding: 3px 10px; font: inherit; font-size: 12px; cursor: pointer; }
.ac .pubtag i { width: 7px; height: 7px; border-radius: 50%; background: var(--muted); }
.ac .pubtag.on { color: var(--text); border-color: color-mix(in srgb, var(--good) 45%, var(--line)); }
.ac .pubtag.on i { background: var(--good); box-shadow: 0 0 0 3px color-mix(in srgb, var(--good) 22%, transparent); }
.ac .pubtag[hidden] { display: none; }
.ac .rows { overflow: auto; max-height: 340px; border: 1px solid var(--line); border-radius: 8px; }
.ac .rows table { font-size: 11.5px; }

/* a build's timeline */
.acr-tl { display: flex; flex-direction: column; gap: 14px; }
.acr-tl h3 { margin: 0 0 6px; font-size: 13px; }
.acr-tl .split { display: flex; height: 12px; border-radius: 6px; overflow: hidden; }
.acr-tl .k-strata { background: #6b7a90; } .acr-tl .k-plan { background: #7b3fc4; } .acr-tl .k-model { background: #9d5ce0; } .acr-tl .k-write { background: #2f5bd3; } .acr-tl .k-check { background: #12a37a; } .acr-tl .k-fix { background: #d9822b; }
.acr-tl .key { display: flex; flex-wrap: wrap; gap: 4px 14px; font-size: 12px; color: var(--text-2); margin-top: 6px; }
.acr-tl .key i { display: inline-block; width: 9px; height: 9px; border-radius: 3px; margin-right: 5px; }
.acr-tl .facts { display: flex; flex-wrap: wrap; gap: 4px 16px; font-size: 12.5px; color: var(--text-2); }
.acr-tl .gantt { display: grid; grid-template-columns: minmax(120px, 34%) minmax(0, 1fr); gap: 3px 10px; font-size: 11.5px; }
.acr-tl .gantt .lab { overflow: hidden; text-overflow: ellipsis; white-space: nowrap; color: var(--text-2); }
.acr-tl .gantt .track { position: relative; height: 16px; background: var(--surface-2); border-radius: 4px; }
.acr-tl .gantt .track span { position: absolute; top: 2px; bottom: 2px; border-radius: 3px; min-width: 2px; }
.acr-tl .bars { display: flex; flex-direction: column; gap: 4px; }
.acr-tl .brow { display: grid; grid-template-columns: 44px minmax(0, 1fr) 50px 44px; gap: 8px; align-items: center; border: 0; background: none; padding: 3px 4px; border-radius: 6px; cursor: pointer; font: inherit; color: inherit; text-align: left; }
.acr-tl .brow:hover { background: var(--surface-2); }
.acr-tl .bn { font-size: 11.5px; font-weight: 650; color: var(--text-2); } .acr-tl .bn.bad { color: var(--crit-ink); }
.acr-tl .btrack { height: 12px; background: var(--surface-2); border-radius: 4px; overflow: hidden; }
.acr-tl .bfill { display: flex; height: 100%; border-radius: 4px; overflow: hidden; }
.acr-tl .bt, .acr-tl .bc { font-size: 11px; color: var(--muted); text-align: right; font-variant-numeric: tabular-nums; }
.acr-tl .dec { display: grid; grid-template-columns: auto minmax(0, 1fr) auto; gap: 6px 10px; font-size: 12px; align-items: baseline; }
.acr-tl .badge { font-size: 10.5px; font-weight: 650; padding: 1px 6px; border-radius: 999px; background: var(--surface-3); white-space: nowrap; }
.acr-tl .d-new { background: color-mix(in srgb, #12a37a 18%, transparent); } .acr-tl .d-change, .acr-tl .d-fork { background: color-mix(in srgb, #2f5bd3 16%, transparent); }
.acr-tl .d-use, .acr-tl .d-copy { background: color-mix(in srgb, #9aa3ae 25%, transparent); }

@media (max-width: 1180px) {
  .ac .grid3 { grid-template-columns: minmax(260px, 320px) minmax(0, 1fr); height: auto; }
  .ac .grid3 > .inspect { grid-column: 1 / -1; max-height: none; }
  .ac .grid3 > .col { height: calc(100dvh - var(--ac-top, 150px)); min-height: 520px; }
  .ac .grid3 > .inspect { height: auto; }
}
@media (max-width: 760px) {
  .ac .grid3.usage-on { grid-template-columns: minmax(0, 1fr); }
  .ac .grid3 { grid-template-columns: minmax(0, 1fr); }
  .ac .grid3 > .col { height: auto; min-height: 0; }
  .ac .grid3 > .ac-stage { height: 640px; }
  /* a phone has no keyboard shortcut to show, and the hint wrapped a letter a line */
  .ac .kbd-hint { visibility: hidden; min-width: 0; overflow: hidden; }
  /* on a phone the app is drawn at the phone's own width, without a frame */
  .ac .device.phone { padding: 0; }
  .ac .device.phone .shell { width: 100%; height: 100%; border: 0; border-radius: 0; box-shadow: none; }
  .ac .grid3 > .builder .scroll { max-height: 55vh; }
  .ac .wsbar h1 { max-width: 70vw; }
  .ac .hero { padding: 18px; } .ac .hero h1 { font-size: 21px; }
}
`;

const EXAMPLES = [
  ['Reading list', 'A reading list: I paste a link with a title and a note, tag it, and mark it read. Show what is unread first.'],
  ['Habit tracker', 'A habit tracker: my daily habits, a check per day, and my current streak for each.'],
  ['Consulting CRM', 'A tiny CRM for my consulting: people, companies, and notes from each call, with a follow-up date.'],
  ['Kids\' books', 'Track the books my kids read: who read it, when they finished, and a rating out of 5.'],
  ['Shared groceries', 'A shared grocery list for our home, grouped by aisle, where ticking an item moves it to the bottom.'],
  ['Team retro', 'A retro board for my team: cards in went well, to improve and actions, with votes, and the actions carried to next time.'],
];

const ORIGIN_ORDER = ['create', 'scratch', 'hand', 'reused'];
const USAGE = '~usage';
// The hosts an app's owner let it read from (engine/reach.mjs), for its frames' policy.
const webHosts = (st) => (st?.reach?.on?.fetch ? (st.reach.on.hosts || []).map((x) => x.host) : []); // the stage's Usage tab, where a screen's slug goes in the address
const ORIGIN_SHORT = { create: 'CREATE statements', scratch: 'written from scratch', hand: 'edited by hand', reused: 'reused from the library' };
const KIND_LABEL = { strata: 'ChloeDB', plan: 'Model planning', model: 'Model planning and writing', write: 'Model writing pieces', fix: 'Model fixing', check: 'Checks' };
const DECISION = {
  use: ['Used', 'as it is'], copy: ['Copied', 'as it is'], keep: ['Kept', 'as it was'], new: ['New', 'written from scratch'],
  change: ['Rewrote', 'changed'], fork: ['Forked', 'started from another'], version: ['New version', 'of a shared piece'],
  upgrade: ['Newer', 'moved to a newer version'], remove: ['Removed', ''],
};
const STEP_LABELS = {
  read: () => 'Read the app and the library',
  plan: (b) => (b.first ? 'Plan the app' : stepKeys(b).includes('write') ? 'Plan the change' : 'Write the change'),
  write: (b) => (b.first ? 'Write the screens and rules' : 'Write the pieces'),
  fix: () => 'Fix what the checks found',
  store: () => 'Check it and store it in ChloeDB',
};
const SOURCE_LABEL = { suggestion: 'a suggestion', edit: 'edited by hand' };
// Requests the engine made itself.
const AUTO = new Set(['repair', 'look', 'use']);
const AUTO_LABEL = { repair: 'The engine repairs a piece that failed', look: 'The engine fixes what its screen check found', use: 'The engine builds a change from how the app is used' };

// A call that failed for want of an answer, not because of what it asked:
// the network, a timeout, the engine restarting behind chloedb-server (502,
// 503, 504), too many requests (429).
const transientError = (err) => !!err && (err.answered === false || [0, 408, 429, 502, 503, 504].includes(Number(err.status)));
const DESKTOP_W = 1280; // the Desktop stage's width, as a laptop's window
const MIN_SCALE = 0.6; // the smallest the Desktop stage is drawn: 13 px text at about 8 px
// Columns whose values look like people's contact details (Publish warns of them).
const CONTACT = /(^|_)(e_?mail|phone|mobile|cell|tel|telephone|whatsapp|address|postcode|zip)(_|$)/i;
const secs = (ms) => (ms < 1000 ? `${(ms / 1000).toFixed(1)} s` : ms < 100000 ? `${(ms / 1000).toFixed(ms < 10000 ? 1 : 0)} s` : `${Math.floor(ms / 60000)} min ${Math.round((ms % 60000) / 1000)} s`);
const bsecs = (ms) => (ms < 950 ? 'under 1 s' : ms < 10000 ? `${Math.max(1, Math.round(ms / 1000))} s` : ms < 90000 ? `${Math.round(ms / 5000) * 5} s` : `${Math.round(ms / 60000)} min`);
const money = (c) => `$${(c || 0).toFixed(2)}`;
const kTok = (n) => (n >= 1000 ? `${(n / 1000).toFixed(n >= 10000 ? 0 : 1)}k` : String(n || 0));
const num = (n) => Number(n || 0).toLocaleString('en-US');
const pct = (f) => `${Math.round((f || 0) * 100)}%`;
const noClose = (code) => String(code).replace(/<\/(script)/gi, '<\\/$1');
const hue = (s) => { let x = 0; for (const c of String(s)) x = (x * 31 + c.charCodeAt(0)) % 360; return x; };

function stepKeys(b) {
  const seen = new Set((b?.steps || []).map((x) => x.key));
  // fix: a round of fixes the checks asked for, counted once it starts
  return ['read', 'plan', 'write', 'fix', 'store'].filter((k) => (k !== 'write' || seen.has(k) || b?.split) && (k !== 'fix' || seen.has(k)));
}
// How far a build is (0..1) and about how long is left, as the studio
// reckons it: each step's expected time from the last builds' timelines.
function buildPace(b) {
  const elapsed = (b.elapsed || 0) + (Date.now() - b.at);
  const keys = stepKeys(b);
  const exp = (k) => Math.max(200, b.expect?.[k] || (k === 'write' ? 9000 : k === 'plan' || k === 'fix' ? 15000 : 1500));
  const total = keys.reduce((a, k) => a + exp(k), 0);
  const cur = b.steps.at(-1);
  if (!cur) return { f: 0.01, elapsed, left: total, slow: false, late: elapsed > 2 * total };
  const i = Math.max(0, keys.indexOf(cur.key));
  const before = keys.slice(0, i).reduce((a, k) => a + exp(k), 0);
  const after = keys.slice(i + 1).reduce((a, k) => a + exp(k), 0);
  const e = exp(cur.key);
  const t = Math.max(0, elapsed - cur.t);
  let within = t < e ? 0.85 * t / e : 0.85 + 0.15 * (1 - Math.exp(-(t - e) / e));
  if (cur.key === 'write' && b.pieces?.length) within = Math.max(within, b.pieces.filter((p) => p.state === 'done').length / b.pieces.length * 0.95);
  // late: twice what the whole build usually takes (offer to start it again)
  return { f: Math.min(0.97, (before + within * e) / total), elapsed, left: Math.max(0, e - t) + after, slow: t > e * 1.6, late: elapsed > 2 * total };
}
const paceText = (b) => {
  if (!b) return 'Waiting for its turn';
  const p = buildPace(b);
  return `${bsecs(p.elapsed)} so far · ${p.slow ? 'taking longer than usual' : p.left < 2000 ? 'almost done' : `about ${bsecs(p.left)} left`}`;
};

function render(el, ctx) {
  const core = ctx.core;
  const { h, toast, icon } = core;
  const api = (p, o) => ctx.api(p, o);
  const post = (p, body) => api(p, { method: 'POST', body: body ?? {} });
  el.append(h('style', {}, CSS));
  const root = h('div', { class: 'ac' });
  el.append(root);

  let gone = false;
  let page = null; // { kind, onEvent(type, data), stop() }
  let kit = null;
  let mods = null;

  // ---- the frames' runtime and the studio's modules, from the engine

  async function loadKit() {
    if (kit) return kit;
    kit = await api('/api/kit');
    const imp = async (src) => {
      const url = URL.createObjectURL(new Blob([src], { type: 'text/javascript' }));
      try { return await import(url); } finally { URL.revokeObjectURL(url); }
    };
    const [design, policy, activity] = await Promise.all([imp(kit.design), imp(kit.policy), imp(kit.activity)]);
    mods = { design, policy, activity };
    el.append(h('style', {}, kit.activityCss || ''));
    // Where a frame loads files and map tiles from: the engine's public
    // address (no key needed there for those), else through this server.
    kit.base = kit.publicUrl || new URL(core.at('/v1/apps/apps'), location.href).href.replace(/\/$/, '');
    return kit;
  }

  // ---- live updates: long polls of the engine's events

  // While the engine does not answer its events (restarting, or down), a
  // note says so; it goes once the engine answers again.
  const downNote = h('div', { class: 'ac-down', role: 'status', style: { display: 'none' } }, h('span', { class: 'spin' }), ' The app engine is not answering (restarting?). Trying again…');
  el.append(downNote);
  let seq = null;
  (async () => {
    let fails = 0;
    while (!gone) {
      try {
        const out = await api(`/api/events?after=${seq ?? 0}&wait=${seq === null ? 0 : 20000}`, { timeout: 45000 });
        const first = seq === null;
        seq = out.seq;
        if (fails) { downNote.style.display = 'none'; if (!first) page?.onEvent?.('app', {}); }
        fails = 0;
        if (!first) for (const e of out.events) { try { page?.onEvent?.(e.type, e.data || {}); } catch (err) { console.warn(err); } }
      } catch (e) {
        if (gone) return;
        // An engine from before /api/events: the page polls on its own.
        if (e.status === 404) { page?.noEvents?.(); return; }
        fails++;
        if (transientError(e)) downNote.style.display = '';
        await new Promise((r) => setTimeout(r, Math.min(15000, 1000 * 2 ** Math.min(fails, 4))));
      }
    }
  })();

  // ---- routing

  const draw = (path) => {
    if (gone) return;
    try { page?.stop?.(); } catch (e) { console.warn(e); }
    page = null;
    root.replaceChildren();
    root.className = 'ac';
    if (path[0] === 'a' && path[1]) page = workspace(path[1], path[2] || null);
    else if (path[0] === 'library') page = library();
    else page = home();
  };
  ctx.onPath(draw);
  draw(ctx.path());

  // ======================================================== home

  function home() {
    const ta = h('textarea', { placeholder: 'Describe the first version: what it keeps, what you do with it, what you want to see.', 'aria-label': 'Describe your app' });
    const nameIn = h('input', { class: 'home-name', placeholder: 'Name (optional)', 'aria-label': 'The app’s name (optional; kept as you write it)', maxlength: 60 });
    const go = h('button', { class: 'btn primary', type: 'button', onclick: () => create() }, icon('sparkle'), 'Build it');
    const err = h('span', { class: 'small', style: { color: 'var(--crit-ink)' } });
    const tip = h('span', { class: 'small muted grow kbd-hint' }, '⌘/Ctrl + Enter');
    async function create() {
      const description = ta.value.trim();
      if (description.length < 5) { ta.focus(); return; }
      go.disabled = true;
      go.replaceChildren(h('span', { class: 'spin' }), 'Starting');
      err.textContent = '';
      try {
        const out = await post('/api/apps', { description, ...(nameIn.value.trim() ? { name: nameIn.value.trim() } : {}) });
        location.hash = ctx.href('a', out.app);
      } catch (e) {
        err.textContent = e.status === 403 ? 'Building an app needs an admin key' : e.message;
        go.disabled = false;
        go.replaceChildren(icon('sparkle'), 'Build it');
      }
    }
    ta.addEventListener('keydown', (e) => { if (e.key === 'Enter' && (e.metaKey || e.ctrlKey)) create(); });
    const facts = h('div', { class: 'facts' });
    const hero = h('section', { class: 'hero' },
      h('h1', {}, 'What should we build?'),
      h('p', { class: 'lead' }, 'Describe an app in a sentence or two. The agent plans it, declares its tables, screens and rules as CREATE statements, writes JavaScript only where a statement cannot say it, checks every piece, and keeps the app in this cluster as rows. Then change it by asking.'),
      h('div', { class: 'prompt' }, ta, h('div', { class: 'bar' }, nameIn, tip, err, go)),
      h('div', { class: 'examples' }, EXAMPLES.map(([t, d]) => h('button', { class: 'ac-chip', type: 'button', title: d, onclick: () => { ta.value = d; ta.focus(); } }, icon('plus'), t))),
      facts);
    const gallery = h('div', { class: 'gallery' }, h('div', { class: 'small muted' }, 'Loading the apps...'));
    const libLink = h('a', { class: 'btn sm', href: ctx.href('library') }, icon('layers'), 'Library');
    root.append(hero, h('div', { class: 'sectionhd' }, h('h2', {}, 'Your apps'), h('span', { class: 'small muted grow' }), libLink), gallery);
    setTimeout(() => ta.focus(), 0);

    const made = new Map();
    // An app's state in a word: its live version, building, or "did not
    // build" (its first build failed; the engine says so, status).
    const stateTag = (a) => (Number(a.head) ? h('span', { class: 'tag live' }, `v${a.head}`)
      : a.status === 'failed' ? h('span', { class: 'tag bad', title: 'Its first build did not go through: open it to see why, or try again' }, 'did not build')
        : h('span', { class: 'tag try' }, 'building'));
    const stateWord = (a) => (Number(a.head) ? `v${a.head}` : a.status === 'failed' ? 'did not build' : 'building');
    const again = async (e, a) => {
      e.preventDefault(); e.stopPropagation();
      try { await post(`/api/apps/${a.id}/requests`, { text: a.description }); location.hash = ctx.href('a', a.id); } catch (err) { toast(err.status === 403 ? 'Building an app needs an admin key' : err.message, 'bad'); }
    };
    const drop = async (e, a) => {
      e.preventDefault(); e.stopPropagation();
      const ok = await core.confirm({ title: `Delete “${String(a.name || a.id).trim()}”?`, ok: 'Delete app', danger: true, body: 'Its first version never built, so it has no screens or rows: this removes its request and what it cost from the gallery.' });
      if (!ok) return;
      try { await post(`/api/apps/${a.id}/delete`, {}); toast('Deleted', 'ok'); refresh(); } catch (err) { toast(err.status === 403 ? 'Deleting an app needs an admin key' : err.message, 'bad'); }
    };
    const card = (a) => {
      const m = made.get(a.id);
      const initials = String(a.name || a.id).split(/\s+/).filter(Boolean).slice(0, 2).map((w) => w[0].toUpperCase()).join('');
      return h('a', { class: 'appcard', href: ctx.href('a', a.id) },
        h('div', { class: 'row' }, h('div', { class: 'ico', style: { background: `hsl(${hue(a.id)} 55% 48%)` } }, initials || '·'),
          h('div', { class: 'grow' }, h('div', { class: 'nm' }, a.name || a.id), h('div', { class: 'meta' }, stateTag(a), a.created ? core.ago(new Date(a.created).getTime()) : null, a.cost?.builds ? h('span', { title: `What its ${a.cost.builds} build${a.cost.builds === 1 ? '' : 's'} cost` }, money(a.cost.usd)) : null))),
        h('div', { class: 'ds' }, a.description || ''),
        !Number(a.head) && a.status === 'failed' ? h('div', { class: 'cardacts' }, h('button', { class: 'btn sm', type: 'button', onclick: (e) => again(e, a) }, 'Try again'), h('button', { class: 'btn sm ghost', type: 'button', onclick: (e) => drop(e, a) }, 'Delete')) : null,
        m && m.runs ? [meter(m, false), h('div', { class: 'meta' }, h('span', {}, h('b', { style: { color: 'var(--ac-c)' } }, pct(m.totals.create.share)), ' from CREATE'), h('span', {}, `${num(m.runs)} lines`))] : h('div', { class: 'ac-meter' }));
    };
    let apps = [];
    // Apps of one name are one card: the newest, with the earlier ones
    // linked under it (asking for "the same app again" makes a new app,
    // and the gallery showed each as if it were a different one).
    const groups = () => {
      const by = new Map();
      for (const a of apps) {
        const k = String(a.name || a.id).trim().toLowerCase();
        if (!by.has(k)) by.set(k, []);
        by.get(k).push(a);
      }
      return [...by.values()];
    };
    const group = ([a, ...older]) => (older.length
      ? h('div', { class: 'appgrp' }, card(a), h('div', { class: 'earlier', title: 'Other apps with this name, newest first' }, h('span', {}, `${older.length} earlier:`),
        older.map((o) => h('a', { href: ctx.href('a', o.id) }, stateWord(o), o.created ? ` · ${core.ago(new Date(o.created).getTime())}` : ''))))
      : card(a));
    const paint = () => {
      gallery.replaceChildren(...(apps.length ? groups().map(group) : [core.empty('box', 'No apps yet', 'Describe one above, or start from an example.')]));
    };
    const refresh = async () => {
      try {
        const [list, status] = await Promise.all([api('/api/apps'), api('/api/status')]);
        apps = (list.apps || []).sort((x, y) => String(y.created || '').localeCompare(String(x.created || '')));
        paint();
        const s = status.spend;
        const named = groups().length;
        // (replaceChildren prints a null as "null": only what is there)
        facts.replaceChildren(...[
          h('span', {}, h('b', {}, String(named)), ` app${named === 1 ? '' : 's'}`, named < apps.length ? ` (${apps.length} with the earlier ones)` : ''),
          status.model && status.model !== 'null' ? h('span', {}, 'built by ', h('b', {}, String(status.model))) : null,
          h('span', {}, s && s.cap != null ? [h('b', {}, money(s.usd)), ` spent of the ${money(s.cap)} limit`] : 'no spending limit'),
          status.feed ? h('span', {}, h('b', {}, 'live'), ': following the cluster’s changes') : null].filter(Boolean));
        // What each app is made of, a few at a time.
        for (const a of apps.slice(0, 24)) {
          if (made.has(a.id) || !a.head) continue;
          api(`/api/apps/${a.id}/made`).then((m) => { made.set(a.id, m); if (!gone) paint(); }).catch(() => made.set(a.id, null));
        }
      } catch (e) {
        gallery.replaceChildren(core.failure(e, 'The app engine'));
      }
    };
    const stop = core.every(20000, refresh);
    return { kind: 'home', stop, onEvent: (type) => { if (type === 'app' || type === 'done') refresh(); } };
  }

  // A bar of where the lines that run came from.
  function meter(m, big = true) {
    return h('div', { class: `ac-meter${big ? ' big' : ''}`, title: ORIGIN_ORDER.filter((k) => m.totals[k].runs).map((k) => `${ORIGIN_SHORT[k]}: ${pct(m.totals[k].share)} (${num(m.totals[k].runs)} lines)`).join('\n') },
      ORIGIN_ORDER.map((k) => (m.totals[k].runs ? h('span', { class: `k-${k}`, style: { flex: String(m.totals[k].runs) } }) : null)));
  }

  // ======================================================== library

  function library() {
    const q = h('input', { class: 'input', type: 'search', placeholder: 'Search the library, or ask: "which screens use the calendar", "what runs when a vote is added"', style: { width: '100%' } });
    const out = h('div', { class: 'list' });
    root.append(h('div', { class: 'wsbar' }, h('a', { class: 'crumb', href: ctx.href() }, icon('left'), 'Apps'), h('h1', {}, 'Library')),
      h('p', { class: 'small dim', style: { margin: 0, maxWidth: '52rem' } }, 'Every piece any app runs, and the shared components and helpers they call. The agent searches this before it writes anything new: words and meaning, fused with the graph of what goes with what.'),
      q, out);
    const show = async () => {
      out.replaceChildren(h('div', { class: 'li' }, h('span', { class: 'spin' }), 'Searching'));
      try {
        const r = await api(`/api/library${q.value.trim() ? `?q=${encodeURIComponent(q.value.trim())}` : ''}`);
        if (r.question) {
          // A question about how pieces fit, answered from the graph (engine/ask.mjs).
          const a = r.question;
          const found = Array.isArray(a.answer) ? a.answer : [];
          out.replaceChildren(h('div', { class: 'li', style: { cursor: 'default' } }, icon('route'), h('b', { class: 'grow' }, a.understood || a.question), h('span', { class: 'small muted' }, `${found.length} found`)),
            ...found.map((x) => h('div', { class: 'li', onclick: () => (x.kind === 'app' ? (location.hash = ctx.href('a', x.app)) : x.live && showCode(x.live)) },
              h('span', { class: 'tag' }, x.kind), h('span', { class: 'nm' }, x.title || x.slug), h('span', { class: 'small muted grow' }, x.app_name || ''), x.hops > 1 ? h('span', { class: 'small muted' }, `${x.hops} steps away`) : null)));
          return;
        }
        const rows = r.capabilities || [];
        out.replaceChildren(...(rows.length ? rows.slice(0, 200).map((c) => h('div', { class: 'li', onclick: () => showCode(c.id) },
          h('span', { class: 'tag' }, c.kind), h('span', { class: 'nm' }, c.kind === 'component' ? `ui.${c.slug}` : c.kind === 'helper' ? `lib.${c.slug}` : c.slug),
          h('span', { class: 'small muted grow', style: { overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap' } }, c.title !== c.slug ? c.title : c.description || ''),
          h('span', { class: 'small muted' }, c.used_by ? `${c.used_by.length} app${c.used_by.length === 1 ? '' : 's'}` : c.app_name || ''))) : [h('div', { class: 'li' }, 'Nothing found')]));
      } catch (e) { out.replaceChildren(h('div', { class: 'li' }, e.message)); }
    };
    q.addEventListener('input', core.debounce(show, 300));
    show();
    return { kind: 'library' };
  }

  // ======================================================== code and timelines

  async function showCode(id) {
    let cap;
    try { cap = await api(`/api/capabilities/${encodeURIComponent(id)}`); } catch (e) { toast(e.message, 'bad'); return; }
    const sql = cap.sql || cap.spec?.statement || cap.code;
    const compiled = (cap.kind === 'view' && cap.spec?.screen) || (cap.kind === 'function' && cap.spec?.rule) ? prettyJson(cap.code) : null;
    const body = h('div', {}, codeBlock(sql));
    const seg = compiled ? core.seg([['sql', 'As written'], ['compiled', 'What runs']], 'sql', (v) => body.replaceChildren(v === 'sql' ? codeBlock(sql) : h('pre', { class: 'code src' }, compiled))) : null;
    core.drawer({
      title: `${cap.title} · v${cap.version}`, wide: true,
      sub: [cap.kind, cap.id, `by ${cap.author || 'forge'}`, cap.forked_from ? `from ${cap.forked_from}` : null].filter(Boolean).join(' · '),
      body: h('div', { class: 'ac', style: { gap: '10px' } }, cap.description ? h('div', { class: 'small dim' }, cap.description) : null,
        h('div', { class: 'row' }, seg, h('span', { class: 'grow' }), h('button', { class: 'btn sm', onclick: () => core.copy(body.textContent) }, icon('copy'), 'Copy')), body),
    });
  }
  const prettyJson = (s) => { try { return JSON.stringify(JSON.parse(s), null, 2); } catch { return String(s || ''); } };
  // SQL highlighted; a body in $$ ... $$ (JavaScript or JSON) shown as it is.
  function codeBlock(text) {
    const s = String(text || '');
    const m = s.match(/^([\s\S]*?\bAS\s+(\$[A-Za-z0-9_]*\$)\n)([\s\S]*)(\n\2\s*;?\s*)$/);
    if (!m) return /^\s*(--[^\n]*\n\s*)*CREATE\b/i.test(s) ? core.sqlBlock(s, { class: 'code src' }) : h('pre', { class: 'code src' }, s);
    return h('pre', { class: 'code src' }, core.highlightSql(m[1]), m[3], core.highlightSql(m[4]));
  }

  async function showTimeline(appId, requestId, heading) {
    const body = h('div', { class: 'acr-tl' }, h('div', { class: 'small muted' }, 'Loading...'));
    core.drawer({ title: heading, wide: true, body });
    try {
      const { trace } = await api(`/api/apps/${appId}/traces/${encodeURIComponent(requestId)}`);
      body.replaceChildren(...timelineBody(trace));
    } catch (e) { body.replaceChildren(h('div', { class: 'small' }, e.message)); }
  }
  function timeSplit(trace) {
    const by = {};
    const model = { ms: 0, calls: 0, cost: 0, in: 0, out: 0, cached: 0 };
    for (const s of trace.spans || []) {
      (by[s.kind] ||= []).push([s.start, s.end]);
      if (s.tokens) { model.ms += s.end - s.start; model.calls++; model.cost += s.cost || 0; model.in += (s.tokens.in || 0) + (s.tokens.cacheWrite || 0) + (s.tokens.cached || 0); model.cached += s.tokens.cached || 0; model.out += s.tokens.out || 0; }
    }
    const wall = {};
    for (const [k, list] of Object.entries(by)) {
      list.sort((a, b) => a[0] - b[0]);
      let total = 0, cs = -1, ce = -1;
      for (const [a, b] of list) { if (a > ce) { if (ce > cs) total += ce - cs; cs = a; ce = b; } else ce = Math.max(ce, b); }
      if (ce > cs) total += ce - cs;
      wall[k] = total;
    }
    return { wall, model };
  }
  function whereFrom(d) {
    const f = d.from || {};
    if (d.decision === 'use') return f.builtin ? 'built in, used as it is' : 'from the library, used as it is';
    if (d.decision === 'copy') return f.starter ? 'built-in starter, copied as it is' : `copied from ${f.title}`;
    if (d.decision === 'fork') return `forked from ${f.title}`;
    if (d.decision === 'version') return `new version of the shared ${d.kind}`;
    if (d.decision === 'change' || d.decision === 'new') {
      if (d.kind === 'table') return d.detail || '';
      const verb = d.decision === 'new' ? 'written' : 'rewritten';
      if (d.patched) return `edited in place: ${d.patched} change${d.patched === 1 ? '' : 's'}`;
      if (d.statement) return `${verb} as a CREATE SCREEN statement${d.patterns ? ` from ${d.patterns.join(', ')}` : ''}`;
      if (d.screen) return `${verb} as screen JSON`;
      return `${verb} from scratch`;
    }
    return DECISION[d.decision]?.[1] || '';
  }
  function timelineBody(trace) {
    const { wall, model } = timeSplit(trace);
    const kinds = ['strata', 'plan', 'model', 'write', 'check', 'fix'].filter((k) => wall[k]);
    const sum = kinds.reduce((a, k) => a + wall[k], 0) || 1;
    const total = Math.max(trace.ms || 1, 1);
    const at = (t) => `${Math.min(100, (t / total) * 100).toFixed(2)}%`;
    const pieceName = (d) => `${d.kind === 'component' ? 'ui.' : d.kind === 'helper' ? 'lib.' : ''}${d.name}`;
    const order = ['new', 'change', 'fork', 'version', 'copy', 'use', 'upgrade', 'remove', 'keep'];
    const decisions = (trace.decisions || []).slice().sort((a, b) => order.indexOf(a.decision) - order.indexOf(b.decision));
    return [
      h('div', {}, h('div', { class: 'dim', style: { fontStyle: 'italic' } }, `“${trace.text}”`),
        h('div', { class: 'facts', style: { marginTop: '6px' } }, h('span', {}, h('b', {}, secs(trace.ms || 0)), ' in all'), h('span', {}, h('b', {}, money(trace.cost ?? model.cost)), ` for ${model.calls} model call${model.calls === 1 ? '' : 's'}`),
          h('span', {}, h('b', {}, kTok(model.in)), ` tokens in (${kTok(model.cached)} cached), `, h('b', {}, kTok(model.out)), ' out'),
          trace.status === 'failed' ? h('span', { style: { color: 'var(--crit-ink)' } }, `Failed: ${trace.error}`) : null)),
      h('div', {}, h('h3', {}, 'Where the time went'), h('div', { class: 'split' }, kinds.map((k) => h('span', { class: `k-${k}`, style: { flex: String(wall[k]) }, title: `${KIND_LABEL[k]}: ${secs(wall[k])}` }))),
        h('div', { class: 'key' }, kinds.map((k) => h('span', {}, h('i', { class: `k-${k}` }), `${KIND_LABEL[k]} ${secs(wall[k])} (${Math.round((wall[k] / sum) * 100)}%)`)))),
      h('div', {}, h('h3', {}, 'Timeline'), h('div', { class: 'gantt' }, (trace.spans || []).flatMap((s) => [
        h('div', { class: 'lab', title: s.label }, s.label),
        h('div', { class: 'track' }, h('span', { class: `k-${s.kind}`, style: { left: at(s.start), width: at(s.end - s.start) }, title: `${s.label}: ${secs(s.end - s.start)}${s.cost ? ` · ${money(s.cost)}` : ''}` }))]))),
      decisions.length ? h('div', {}, h('h3', {}, 'What was decided for each piece'), h('div', { class: 'dec' }, decisions.flatMap((d) => [
        h('span', { class: `badge d-${d.decision}` }, DECISION[d.decision]?.[0] || d.decision),
        h('div', {}, h('span', { class: 'mono' }, pieceName(d)), h('span', { class: 'muted' }, ` · ${whereFrom(d)}`)),
        h('span', { class: 'muted small' }, d.chars ? `${num(d.chars)} chars` : '')]))) : null,
    ].filter(Boolean);
  }

  // ======================================================== an app

  function workspace(appId, firstScreen) {
    root.classList.add('ws');
    const W = {
      st: null, view: firstScreen, frames: new Map(), errors: new Map(), left: new Map(),
      lines: [], build: null, runningText: '', runningId: null, ended: new Set(), failed: null, previews: new Map(), previewView: null,
      // The device the stage draws: the owner's choice, else (auto) the one
      // the stage's width shows at its real size (fitDevice).
      tab: 'data', device: (() => { try { return localStorage.getItem('ac-device') || 'auto'; } catch { return 'auto'; } })(),
      tryV: null, pick: null, designCss: '', polling: false, stopped: false, logOpen: false,
    };
    const atV = () => (W.tryV ? `?v=${W.tryV}` : '');
    const base = `/api/apps/${appId}`;

    // The frame: the studio's srcdoc, with the frame's origin pointed at the
    // engine (its files and map tiles) and the studio's policy.
    function srcdoc(v, params) {
      const { frameCsp } = mods.policy;
      const compiled = v.spec && v.spec.screen ? compileScreen(v.code) : null;
      const code = compiled
        ? (compiled.error ? `throw new Error(${JSON.stringify(compiled.error)});` : `async function render(root, app) { await __sa_screen(root, app, ${JSON.stringify(compiled.spec).replace(/</g, '\\u003c')}); }`)
        : noClose(v.code);
      const boot = JSON.stringify({ slug: v.slug, cap: v.id, app: appId, origin: kit.base, params: params || {} }).replace(/</g, '\\u003c');
      const st = W.st;
      const comps = (st.components || []).map((c) => `<script>__sa_component(${JSON.stringify(c.slug)}, ${JSON.stringify(c.id)}, function () {\n${noClose(c.code)}\n;return typeof component === 'function' ? component : null; });</script>`).join('\n');
      const libs = (st.helpers || []).map((c) => `<script>__sa_helper(${JSON.stringify(c.slug)}, ${JSON.stringify(c.id)}, function () {\n${noClose(c.code)}\n;return helper(); });</script>`).join('\n');
      const src = kit.publicUrl || `${kit.base}/`;
      // The hosts the owner let the app read from (app.fetch): its frames may show images from them.
      return `<!doctype html><html><head><meta charset="utf-8"><meta name="viewport" content="width=device-width, initial-scale=1">
<meta http-equiv="Content-Security-Policy" content="${frameCsp(src, { hosts: webHosts(st) })}">
<style>${kit.kit}</style><style id="__design">${W.designCss || ''}</style></head><body><div id="root"></div>
<script>window.__SA = ${boot};</script>
<script>${kit.frame}</script>
${compiled ? `<script>${kit.front}</script>\n<script>${kit.screen}</script>\n${compiled.scripts || ''}` : ''}
${libs}
${comps}
<script>
(async () => { try {
${code}
;if (typeof render !== 'function') throw new Error('the view defines no render(root, app)');
await render(document.getElementById('root'), app);
__sa_ready();
} catch (e) { __sa_report(e); } })();
</script></body></html>`;
    }
    function compileScreen(text) {
      let spec;
      try { spec = JSON.parse(text); } catch (e) { return { error: `the screen is not JSON: ${e.message}` }; }
      const exprs = [];
      const codes = [];
      const walk = (v, key) => {
        if (key === 'code' && typeof v === 'string') { codes.push(v); return { $c: codes.length - 1 }; }
        if (typeof v === 'string' && v.startsWith('=')) { exprs.push(v.slice(1)); return { $e: exprs.length - 1 }; }
        if (Array.isArray(v)) return v.map((x) => walk(x));
        if (v && typeof v === 'object') return Object.fromEntries(Object.entries(v).map(([k, x]) => [k, walk(x, k)]));
        return v;
      };
      const compiled = walk(spec);
      const scripts = [
        `<script>window.__SA_EXPR = []; window.__SA_CODE = []; window.__SA_EXPR_SRC = ${JSON.stringify(exprs).replace(/</g, '\\u003c')};</script>`,
        ...exprs.map((e, i) => `<script>__SA_EXPR[${i}] = function (s) { with (s) { return (${noClose(e)}\n); } };</script>`),
        ...codes.map((c, i) => `<script>__SA_CODE[${i}] = function () {\n${noClose(c)}\n;return typeof render === 'function' ? render : null; };</script>`),
      ];
      return { spec: compiled, scripts: scripts.join('\n') };
    }

    // ---- the layout

    const title = h('h1', {}, '…');
    const verTag = h('span');
    const status = h('span', { class: 'small muted' });
    const devices = core.seg([['phone', 'Phone', 'A phone, 390 px wide'], ['tablet', 'Tablet', 'A tablet, 820 px wide'], ['desktop', 'Desktop', `A computer, ${DESKTOP_W} px wide: drawn smaller when the stage is narrower, down to ${Math.round(MIN_SCALE * 100)}%, then scrolled`]], W.device === 'auto' ? null : W.device, (v) => {
      W.device = v; try { localStorage.setItem('ac-device', v); } catch { /* this visit only */ }
      fitDevice();
    });
    const ownLink = h('a', { class: 'btn sm', target: '_blank', rel: 'noopener', title: 'The app on its own, as people use it: sign-in, a phone', hidden: true }, icon('ext'), 'Its own page');
    // Open to the public, or not: the tag opens the Publish tab.
    const pubTag = h('button', { class: 'pubtag', type: 'button', onclick: () => { W.tab = 'publish'; paintInspector(); } });
    const studioBtn = h('button', { class: 'btn sm ghost', type: 'button', title: 'Open the standalone studio in a tab of its own', onclick: () => openStudio(`#/app/${appId}`) }, 'Studio');
    const moreBtn = h('button', { class: 'btn sm ghost icon', type: 'button', title: 'Rename or delete this app', 'aria-label': 'Rename or delete this app',
      onclick: (e) => core.menu(e.currentTarget, [{ label: 'Rename…', icon: 'edit', onclick: renameApp }, { label: 'Duplicate', icon: 'copy', onclick: duplicateApp },
        { label: 'Export its data (JSON)', icon: 'download', onclick: () => exportData() }, '-', { label: 'Delete app…', icon: 'trash', danger: true, onclick: deleteApp }]) }, icon('dots'));
    const costTag = h('span', { class: 'small muted', title: 'What the model cost for every build of this app' });
    const bar = h('div', { class: 'wsbar' }, h('a', { class: 'crumb', href: ctx.href() }, icon('left'), 'Apps'), title, verTag, status, costTag, h('span', { class: 'grow' }), devices, pubTag, ownLink, studioBtn, moreBtn);

    function renameApp() {
      const input = h('input', { class: 'input', value: W.st?.app.name || '', maxlength: 60, 'aria-label': 'The app’s name', style: { width: '100%' } });
      const save = h('button', { class: 'btn primary' }, 'Rename');
      const cancel = h('button', { class: 'btn' }, 'Cancel');
      const m = core.modal({ title: 'Rename this app', body: [h('p', { class: 'dim', style: { marginTop: 0 } }, 'The name in the gallery and on its screens. It stays as you write it: later changes do not rename it.'), input], foot: [cancel, save] });
      const go = async () => {
        const name = input.value.trim();
        if (!name) { input.focus(); return; }
        save.disabled = true;
        try { await post(`${base}/rename`, { name }); m.close(); toast('Renamed', 'ok'); refresh(); } catch (e) { save.disabled = false; toast(e.status === 403 ? 'Renaming an app needs an admin key' : e.message, 'bad'); }
      };
      save.onclick = go;
      cancel.onclick = () => m.close();
      input.onkeydown = (e) => { if (e.key === 'Enter') { e.preventDefault(); go(); } };
    }
    async function duplicateApp() {
      try {
        const out = await post(`${base}/duplicate`, {});
        toast('Copied, with its data', 'ok');
        location.hash = ctx.href('a', out.app);
      } catch (e) { toast(e.status === 403 ? 'Copying an app needs an admin key' : e.message, 'bad'); }
    }
    // Every table's rows as one JSON file, or one table's as CSV.
    const save = (text, file, type) => { const url = URL.createObjectURL(new Blob([text], { type })); const a = h('a', { href: url, download: file }); document.body.append(a); a.click(); a.remove(); setTimeout(() => URL.revokeObjectURL(url), 1000); };
    const fileName = () => String(W.st?.app.name || appId).toLowerCase().replace(/[^a-z0-9]+/g, '-').replace(/^-+|-+$/g, '') || appId;
    async function exportData(table = null) {
      try {
        const out = await api(`${base}/export`);
        if (!table) { save(JSON.stringify(out, null, 2), `${fileName()}.json`, 'application/json'); return; }
        const rows = out.tables[table] || [];
        const cols = [...new Set(rows.flatMap((r) => Object.keys(r)))];
        const cell = (v) => { const t = v == null ? '' : typeof v === 'object' ? JSON.stringify(v) : String(v); return /[",\n\r]/.test(t) ? `"${t.replace(/"/g, '""')}"` : t; };
        save([cols.join(','), ...rows.map((r) => cols.map((c) => cell(r[c])).join(','))].join('\r\n'), `${fileName()}-${table}.csv`, 'text/csv');
      } catch (e) { toast(e.message, 'bad'); }
    }
    async function deleteApp() {
      const st = W.st;
      const name = st?.app.name || appId;
      const rows = (st?.tables || []).reduce((a, t) => a + (t.rows || 0), 0);
      const ok = await core.confirm({
        title: `Delete ${name}?`, ok: 'Delete app', danger: true, typed: appId,
        body: `This removes the app for good: its screens and versions, its ${(st?.tables || []).length} table${(st?.tables || []).length === 1 ? '' : 's'} (app_${appId}_*, ${num(rows)} row${rows === 1 ? '' : 's'}), its files and what people did in it${st?.publish?.on ? ', and its public address' : ''}. Other apps and the shared library stay as they are.`,
      });
      if (!ok) return;
      try {
        await post(`${base}/delete`, {});
        toast(`${name} deleted`, 'ok');
        location.hash = ctx.href();
      } catch (e) { toast(e.status === 403 ? 'Deleting an app needs an admin key' : e.message, 'bad'); }
    }

    const convo = h('div', { class: 'convo' });
    const convoScroll = h('div', { class: 'scroll' }, convo);
    const ta = h('textarea', { placeholder: 'Ask for a change: "add a due date and sort by it", "a chart of this week", "let me import a CSV"…', 'aria-label': 'Ask for a change' });
    const send = h('button', { class: 'btn primary sm', type: 'button', onclick: () => submit() }, 'Send');
    const hint = h('span', { class: 'small muted grow' }, '⌘/Ctrl + Enter');
    ta.addEventListener('keydown', (e) => { if (e.key === 'Enter' && (e.metaKey || e.ctrlKey)) submit(); });
    const builder = h('section', { class: 'col builder' }, h('div', { class: 'colhd' }, icon('sparkle'), h('h3', {}, 'Build'), h('span', { class: 'grow' })),
      convoScroll, h('div', { class: 'composer' }, h('div', { class: 'prompt' }, ta, h('div', { class: 'bar' }, hint, send))));

    const screens = h('div', { class: 'screens', role: 'tablist' });
    const reload = h('button', { class: 'btn ghost icon sm', type: 'button', title: 'Start the screen again', onclick: () => { for (const f of W.frames.values()) f.iframe.remove(); W.frames.clear(); W.errors.clear(); W.left.clear(); paintStage(); } }, icon('refresh'));
    const tryBar = h('div');
    const deviceBox = h('div', { class: 'device' });
    const shell = h('div', { class: 'shell' });
    // What the stage scrolls over when the desktop is wider than it, drawn smaller.
    const sizer = h('div', { class: 'sizer' });
    deviceBox.append(sizer, shell);
    // The device drawn: the owner's choice, else the largest the stage shows
    // at its own size (a computer from 900 px, a tablet from 640 px, else a
    // phone), so the app's text is never drawn too small to read. A computer
    // keeps its width (a table stays a table, its columns all there), drawn
    // smaller when the stage is narrower, but never under MIN_SCALE: the
    // stage scrolls sideways instead.
    function fitDevice() {
      const w = deviceBox.clientWidth, ht = deviceBox.clientHeight;
      const dev = W.device !== 'auto' ? W.device : !w ? 'phone' : w >= 900 ? 'desktop' : w >= 640 ? 'tablet' : 'phone';
      deviceBox.className = `device ${dev}`;
      devices.set(dev);
      if (dev !== 'desktop') { shell.style.width = shell.style.height = shell.style.transform = shell.style.left = sizer.style.width = sizer.style.height = ''; return; }
      if (!w || !ht) return;
      const width = Math.max(DESKTOP_W, w);
      const k = Math.max(MIN_SCALE, w / width);
      shell.style.width = `${width}px`;
      shell.style.height = `${Math.round(ht / k)}px`;
      shell.style.transform = k < 1 ? `scale(${k})` : '';
      sizer.style.width = `${Math.round(width * k)}px`;
      sizer.style.height = `${ht}px`;
    }
    const fitter = typeof ResizeObserver === 'function' ? new ResizeObserver(() => fitDevice()) : null;
    fitter?.observe(deviceBox);
    // Usage: the Activity view (web/activity.js, the studio's), over the whole stage.
    const usageBox = h('div', { class: 'ac-usage', hidden: true });
    const stage = h('section', { class: 'col ac-stage' }, h('div', { class: 'colhd', style: { background: 'var(--surface)' } }, screens, reload), tryBar, deviceBox, usageBox);

    const itabs = h('div', { class: 'itabs', role: 'tablist' });
    const ibody = h('div', { class: 'ibody' });
    const inspect = h('section', { class: 'col inspect' }, itabs, h('div', { class: 'scroll' }, ibody));

    root.append(bar, h('div', { class: 'grid3' }, builder, stage, inspect));
    const place = () => { const top = root.querySelector('.grid3')?.getBoundingClientRect().top; if (top) root.style.setProperty('--ac-top', `${Math.round(top + window.scrollY + 18)}px`); };
    requestAnimationFrame(place);

    async function openStudio(next) {
      // Without a login (no app token), the studio's address opens as it is.
      if (!kit.login && kit.publicUrl) { window.open(`${kit.publicUrl}/${next}`, '_blank', 'noopener'); return; }
      try {
        const { url } = await post('/api/handoff', { next });
        window.open(url, '_blank', 'noopener');
      } catch (e) {
        toast(e.status === 403 ? 'Opening the studio needs an admin key' : e.status === 409 ? 'The studio has no address of its own here (STRATA_APPS_PUBLIC_URL)' : e.message, 'bad');
      }
    }

    // ---- state

    let refreshing = null;
    let again = false;
    async function refresh() {
      if (W.stopped) return;
      if (refreshing) { again = true; return refreshing; }
      refreshing = (async () => {
        try {
          await loadKit();
          const st = await api(`${base}${atV()}`);
          if (W.stopped) return;
          W.st = st;
          const run = st.running && !W.ended.has(st.running.id) ? st.running : null;
          if (run && !W.lines.length) { W.runningText = run.text; W.runningSource = run.source; W.lines = run.lines; W.runningId = run.id; }
          if (run?.steps && W.runningText === run.text) W.build = { first: run.first, split: run.split, steps: run.steps, pieces: run.pieces || [], elapsed: run.elapsed || 0, expect: run.expect || {}, at: Date.now() };
          if (!run && W.lines.length && (!W.runningText || W.ended.has(W.runningId))) { W.lines = []; W.build = null; W.runningText = ''; }
          paint();
        } catch (e) {
          if (!W.st) root.replaceChildren(bar, core.failure(e, 'This app'));
          else toast(e.message, 'bad');
        }
      })();
      await refreshing;
      refreshing = null;
      if (again) { again = false; await refresh(); }
    }

    function applyDesign() {
      const { designCss, checkDesign, PRESETS } = mods.design;
      const d = W.st.design;
      const chosen = d ? (d.designs[d.chosen] || d.designs[0]) : PRESETS[0];
      const css = designCss(checkDesign(chosen, chosen));
      if (css === W.designCss) return;
      W.designCss = css;
      for (const f of W.frames.values()) f.iframe.contentWindow?.postMessage({ __sa: 1, kind: 'design', css }, '*');
    }

    function paint() {
      const st = W.st;
      title.textContent = st.app.name || appId;
      costTag.textContent = st.cost?.builds ? `${money(st.cost.usd)} in ${st.cost.builds} build${st.cost.builds === 1 ? '' : 's'}` : '';
      const firstFailed = !st.version.n && !st.running && !W.lines.length && ['failed', 'stopped'].includes((st.requests || [])[0]?.status);
      verTag.replaceChildren(st.version.n ? h('span', { class: `tag ${st.version.trying ? 'try' : 'live'}` }, st.version.trying ? `trying v${st.version.trying} · live v${st.version.live}` : `v${st.version.n} live`)
        : firstFailed ? h('span', { class: 'tag bad' }, 'did not build') : h('span', { class: 'tag try' }, 'first version'));
      status.replaceChildren(...(W.lines.length ? [h('span', { class: 'spin' }), ' building'] : []));
      const pub = st.publish;
      pubTag.hidden = !pub || !st.views.length;
      if (pub) {
        pubTag.className = `pubtag${pub.on ? ' on' : ''}`;
        pubTag.title = pub.on ? `Open to the public${pub.urls[0] ? ` at ${pub.urls[0]}` : ''}` : 'Only you and other admins can open it; publish it under Publish';
        pubTag.replaceChildren(h('i'), pub.on ? 'Public' : 'Private');
      }
      if (kit.publicUrl && st.views.length) { ownLink.hidden = false; ownLink.href = `${kit.publicUrl}/a/${appId}${atV()}`; } else ownLink.hidden = true;
      studioBtn.hidden = !kit.publicUrl;
      applyDesign();
      if (W.view !== USAGE && !st.views.find((v) => v.slug === W.view)) W.view = st.views[0]?.slug || null;
      paintConvo();
      paintStage();
      paintInspector();
    }

    // ---- the conversation

    function paintConvo() {
      const st = W.st;
      const nearBottom = convoScroll.scrollHeight - convoScroll.scrollTop - convoScroll.clientHeight < 60;
      const items = [];
      const reqs = (st.requests || []).slice().reverse().filter((r) => r.id !== W.runningId || !W.lines.length);
      if (!reqs.length && !W.lines.length && st.app.description) items.push(h('div', { class: 'msg you' }, st.app.description));
      for (const r of reqs) {
        if (r.status === 'running' || r.status === 'queued') continue;
        items.push(asked(r.text, r.source));
        if (r.status === 'done') {
          items.push(h('div', { class: 'msg it' }, h('div', {}, r.comp ? h('b', {}, `Version ${r.comp} is live. `) : null, r.summary || 'Done.'),
            h('div', { class: 'm' }, r.ms ? h('span', {}, secs(r.ms)) : null, r.cost ? h('span', {}, money(r.cost)) : null, r.created ? h('span', {}, core.ago(new Date(r.created).getTime())) : null,
              r.source !== 'edit' ? h('button', { class: 'linkbtn', onclick: () => showTimeline(appId, r.id, r.comp ? `How version ${r.comp} was built` : 'This build') }, 'Timeline') : null)));
        } else if (r.status === 'failed' || r.status === 'stopped') {
          items.push(failure(r.said, r.error, r.text, r.status === 'failed' ? r.id : null));
        }
      }
      if (W.lines.length) {
        items.push(asked(W.runningText, W.runningSource));
        items.push(progressBox());
      } else if (W.failed && !reqs.some((r) => (r.status === 'failed' || r.status === 'stopped') && r.text === W.failed.text)) {
        items.push(failure(W.failed.said, W.failed.error, W.failed.text, null));
      }
      // Built from use: versions the engine made from patterns in what people do.
      const cands = (st.evolve?.candidates || []).filter((c) => c.status === 'ready' || c.status === 'building');
      if (cands.length) {
        items.push(h('div', { class: 'lbl' }, 'Built from use, to try'));
        items.push(h('div', { class: 'sugs' }, cands.map((c) => h('div', { class: 'sug' }, h('div', {}, c.text), c.why ? h('div', { class: 'why' }, c.why) : null,
          h('div', { class: 'bar' }, c.status === 'building' ? h('span', { class: 'small muted' }, h('span', { class: 'spin' }), ' building beside the live version')
            : [h('button', { class: `btn sm${W.tryV === c.n ? '' : ' primary'}`, onclick: () => tryVersion(W.tryV === c.n ? null : c.n) }, W.tryV === c.n ? 'Back to live' : 'Try it'),
              h('button', { class: 'btn sm', onclick: () => decide(c, 'keep') }, 'Keep'), h('button', { class: 'btn sm ghost', onclick: () => decide(c, 'drop') }, 'Drop')])))));
      }
      if (st.version.n && (st.suggestions || []).length && !W.lines.length) {
        items.push(h('div', { class: 'lbl' }, 'Suggested next'));
        items.push(h('div', { class: 'sugs' }, st.suggestions.slice(0, 4).map((s) => h('div', { class: 'sug' }, h('div', {}, s.text), s.why ? h('div', { class: 'why' }, s.why) : null,
          h('div', { class: 'bar' }, h('button', { class: 'btn sm primary', onclick: (e) => { e.target.disabled = true; ask(s.text, { source: 'suggestion', suggestion: s.id }); } }, 'Do it'),
            h('button', { class: 'btn sm ghost', onclick: async () => { await post(`${base}/suggestions/${s.id}`, { status: 'dismissed' }).catch((e) => toast(e.message, 'bad')); refresh(); } }, 'Not now'))))));
      }
      if (st.version.n && !W.lines.length) {
        items.push(h('div', {}, h('button', { class: 'linkbtn small', onclick: async (e) => {
          e.target.disabled = true; e.target.textContent = 'Reading how the app is used…';
          try {
            const r = await post(`${base}/suggest`);
            if (r.building) toast(`Building “${r.building.text}” to try, from your use`);
            else if (!r.suggestions?.length) toast('Nothing in the app’s use points anywhere yet');
          } catch (err) { toast(err.message, 'bad'); }
          refresh();
        } }, 'Suggest from how it is used')));
      }
      convo.replaceChildren(...items);
      ta.disabled = false;
      const firstFailed = !st.version.n && !W.lines.length && ['failed', 'stopped'].includes((st.requests || [])[0]?.status);
      ta.placeholder = st.version.n ? 'Ask for a change: "add a due date and sort by it", "a chart of this week", "let me import a CSV"…'
        : firstFailed ? 'Describe the app again, in other words if you like, to build its first version…' : 'Waiting for the first version… you can queue a change now.';
      hint.textContent = W.lines.length ? 'Requests wait their turn' : '⌘/Ctrl + Enter';
      if (nearBottom || W.lines.length) convoScroll.scrollTop = convoScroll.scrollHeight;
    }

    // A build that did not go through: what went wrong and what to do, in a
    // sentence (the engine's said), the error itself folded under Details.
    function failure(said, error, text, requestId) {
      const t = said || { title: 'The build did not go through.', hint: '' };
      const stopped = t.title === 'Stopped.';
      return h('div', { class: `msg it${stopped ? '' : ' bad'}` }, h('b', {}, t.title), t.hint ? ` ${t.hint}` : null,
        error && !stopped ? h('details', { class: 'log' }, h('summary', {}, 'Details'), h('div', {}, error)) : null,
        h('div', { class: 'm' }, h('button', { class: 'linkbtn', onclick: () => ask(text) }, 'Try again'),
          requestId ? h('button', { class: 'linkbtn', onclick: () => showTimeline(appId, requestId, 'How this build failed') }, 'Timeline') : null));
    }

    // What was asked: by the owner, or by the engine itself (a repair, the
    // drawn-page check, a change from use), told apart and kept short.
    function asked(text, source) {
      if (AUTO.has(source)) {
        const short = String(text).length > 120 ? `${String(text).slice(0, 120).trim()}…` : text;
        const body = h('span', {}, short);
        return h('div', { class: 'msg sys' }, h('div', { class: 'small', style: { fontWeight: 600 } }, icon('wrench'), ' ', AUTO_LABEL[source]), body,
          short !== text ? h('div', { class: 'm' }, h('button', { class: 'linkbtn', onclick: (e) => { body.textContent = text; e.target.remove(); } }, 'All of it')) : null);
      }
      return h('div', { class: 'msg you' }, text, source && SOURCE_LABEL[source] ? h('span', { class: 'src' }, SOURCE_LABEL[source]) : null);
    }
    function progressBox() {
      const b = W.build;
      const guess = { first: !W.st?.version?.n, split: !W.st?.version?.n };
      const keys = stepKeys(b || guess);
      const at = new Map((b?.steps || []).map((x, i, all) => [x.key, { t: x.t, end: all[i + 1]?.t }]));
      const cur = b?.steps.at(-1)?.key;
      const f = b ? buildPace(b).f : 0.01;
      const nowLine = W.lines.at(-1) || '';
      const log = h('details', { class: 'log', ontoggle: (e) => { W.logOpen = e.target.open; } }, h('summary', {}, `Log (${W.lines.length} line${W.lines.length === 1 ? '' : 's'})`), h('div', {}, W.lines.join('\n')));
      log.open = W.logOpen;
      return h('div', { class: 'prog' },
        h('div', { class: 'pbar' }, h('div', { class: 'fill', style: { width: `${Math.round(f * 1000) / 10}%` } })),
        h('div', { class: 'small muted pace' }, paceText(b)),
        h('ol', { class: 'steps' }, keys.map((k) => {
          const s = at.get(k);
          const state = !s ? 'todo' : k === cur ? 'now' : 'done';
          return h('li', { class: state }, h('span', { class: 'mk' }, state === 'done' ? '✓' : state === 'now' ? h('span', { class: 'spin' }) : '·'), STEP_LABELS[k](b || guess),
            state === 'done' && s.end != null ? h('span', { class: 'took' }, bsecs(s.end - s.t)) : null);
        })),
        b?.pieces?.length ? h('div', { class: 'bits' }, b.pieces.map((p) => h('span', { class: `bit ${p.state}`, title: `${p.kind} ${p.label}` }, p.state === 'done' ? '✓' : h('span', { class: 'spin' }), p.label))) : null,
        h('div', { class: 'now' }, nowLine),
        h('div', { class: 'm stopbar' }, h('button', { class: 'btn sm ghost', type: 'button', title: 'Stop this build: the model stops at once and the app stays as it is', onclick: () => stopBuild(false) }, 'Stop'),
          h('button', { class: 'btn sm', type: 'button', style: { display: b && buildPace(b).late ? '' : 'none' }, title: 'It is taking twice as long as builds here usually do', onclick: () => stopBuild(true) }, 'Stop and try again')),
        log);
    }
    async function stopBuild(again) {
      const text = W.runningText;
      try {
        const out = await post(`${base}/stop`, W.runningId ? { request: W.runningId } : {});
        if (!out.stopped) { toast('Nothing is being built right now'); return; }
        if (again && text) { W.lines = []; await ask(text); }
      } catch (e) { toast(e.status === 403 ? 'Stopping a build needs an admin key' : e.message, 'bad'); }
    }
    const pacer = setInterval(() => {
      if (!W.lines.length || !W.build) return;
      const f = buildPace(W.build).f;
      for (const el of root.querySelectorAll('.prog .fill')) el.style.width = `${Math.round(f * 1000) / 10}%`;
      for (const el of root.querySelectorAll('.prog .pace')) el.textContent = paceText(W.build);
      const late = buildPace(W.build).late;
      for (const el of root.querySelectorAll('.prog .stopbar .btn:not(.ghost)')) el.style.display = late ? '' : 'none';
    }, 250);

    async function submit() {
      const text = ta.value.trim();
      if (!text) { ta.focus(); return; }
      send.disabled = true;
      try { await ask(text); ta.value = ''; } finally { send.disabled = false; }
    }
    async function ask(text, extra = {}) {
      try {
        await post(`${base}/requests`, { text, ...extra });
      } catch (e) { toast(e.status === 403 ? 'Changing an app needs an admin key' : e.message, 'bad'); return; }
      W.failed = null;
      W.runningText = text;
      W.lines = ['Queued'];
      W.build = null;
      W.runningId = null;
      paintConvo();
      status.replaceChildren(h('span', { class: 'spin' }), ' building');
    }
    function tryVersion(n) { W.tryV = n || null; for (const f of W.frames.values()) f.iframe.remove(); W.frames.clear(); refresh(); }
    async function decide(c, action) {
      try {
        const out = await post(`${base}/candidates/${c.id}`, { action });
        if (W.tryV === c.n) W.tryV = null;
        toast(action === 'drop' ? 'Dropped: the app stays as it is' : out.how === 'rebuilt' ? 'The live version changed the same pieces: building it again on it' : `Kept: version ${out.head} is live`, 'ok');
      } catch (e) { toast(e.message, 'bad'); }
      refresh();
    }

    // ---- the stage: the app itself

    function paintStage() {
      const st = W.st;
      if (!st) return;
      // the screens
      screens.replaceChildren(...(st.views.length ? st.views.map((v) => h('button', { type: 'button', role: 'tab', 'aria-pressed': String(v.slug === W.view), onclick: () => switchView(v.slug) },
        v.title, W.errors.has(v.slug) ? h('span', { class: 'dot', title: 'This screen hit an error' }) : null)) : [h('span', { class: 'small muted', style: { padding: '0 6px' } }, 'No screens yet')]),
        ...(st.views.length ? [h('button', { type: 'button', role: 'tab', class: 'usage-tab', title: 'What people did in the app: each visit, step by step', 'aria-pressed': String(W.view === USAGE), onclick: () => switchView(USAGE) }, h('span', { class: 'pulse' }), 'Usage')] : []));
      paintUsage();
      // trying a version
      const c = st.version.trying ? (st.evolve?.candidates || []).find((x) => x.n === st.version.trying) : null;
      tryBar.replaceChildren(...(st.version.trying ? [h('div', { class: 'trybar' }, h('span', { class: 'grow' }, h('b', {}, c ? c.text : `Version ${st.version.trying}`), h('span', { class: 'muted' }, ` · on the app's real data; everyone else sees v${st.version.live}`)),
        c?.status === 'ready' ? h('button', { class: 'btn sm primary', onclick: () => decide(c, 'keep') }, 'Keep') : null,
        h('button', { class: 'btn sm ghost', onclick: () => tryVersion(null) }, 'Back to live'))] : []));
      // screens drawn while the model writes the rest
      deviceBox.querySelector('.previewbar')?.remove();
      deviceBox.querySelector('.building')?.remove();
      if (W.previews.size && W.lines.length) {
        const list = [...W.previews.values()].sort((a, b) => (a.order ?? 50) - (b.order ?? 50));
        const cur = W.previews.get(W.previewView) || list[0];
        const key = `${cur.slug}|${JSON.stringify(cur.screen).length}`;
        for (const f of W.frames.values()) f.iframe.hidden = true;
        let pv = shell.querySelector('iframe.pv');
        if (!pv || pv.dataset.key !== key) {
          pv?.remove();
          pv = h('iframe', { class: 'pv', sandbox: 'allow-scripts', title: `Preview: ${cur.title}` });
          pv.dataset.key = key;
          pv.srcdoc = srcdoc({ slug: cur.slug, id: `preview:${cur.slug}`, title: cur.title, spec: { screen: true }, code: JSON.stringify({ ...cur.screen, preview: true }) }, {});
          shell.append(pv);
        }
        deviceBox.append(h('div', { class: 'previewbar' }, h('span', { class: 'spin' }), h('b', {}, 'Preview'), h('span', { class: 'grow muted' }, 'while the agent writes the rest'),
          list.map((p) => h('button', { class: 'ac-chip', style: p.slug === cur.slug ? { borderColor: 'var(--accent)' } : null, onclick: () => { W.previewView = p.slug; paintStage(); } }, p.title))));
        return;
      }
      shell.querySelector('iframe.pv')?.remove();
      if (!st.views.length) {
        for (const f of W.frames.values()) f.iframe.remove();
        W.frames.clear();
        const failedFirst = !st.running && !W.lines.length && ['failed', 'stopped'].includes((st.requests || [])[0]?.status) ? st.requests[0] : null;
        deviceBox.append(h('div', { class: 'building' }, h('div', { class: 'box' },
          h('h2', {}, failedFirst ? (failedFirst.status === 'stopped' ? 'The first build was stopped' : 'The first version did not build') : 'Building the first version'),
          failedFirst?.said && failedFirst.status !== 'stopped' ? h('div', { class: 'small', style: { textAlign: 'center' } }, failedFirst.said.title) : null,
          h('div', { class: 'small dim', style: { textAlign: 'center' } }, st.app.description),
          W.lines.length ? h('div', { class: 'small muted', style: { textAlign: 'center' } }, 'Follow it on the left; its screens show here as soon as they are written.')
            : failedFirst ? h('button', { class: 'btn sm', style: { alignSelf: 'center' }, onclick: () => ask(st.app.description) }, 'Try again') : h('div', { class: 'small muted', style: { textAlign: 'center' } }, h('span', { class: 'spin' }), ' Waiting for the engine…'))));
        return;
      }
      // drop the frames of views that are gone or changed, or whose shared pieces changed
      const sig = [...(st.components || []), ...(st.helpers || [])].map((x) => x.id).join(',') + `|${webHosts(st).join(' ')}` + (W.tryV || '');
      for (const [slug, f] of W.frames) {
        const v = st.views.find((x) => x.slug === slug);
        if (!v || v.id !== f.capId || f.sig !== sig) { f.iframe.remove(); W.frames.delete(slug); W.errors.delete(slug); }
      }
      const v = st.views.find((x) => x.slug === W.view);
      if (v && !W.frames.has(v.slug) && W.left.get(v.slug) !== v.id) {
        const iframe = h('iframe', { sandbox: mods.policy.FRAME_SANDBOX, title: v.title, allow: 'clipboard-write' });
        iframe.srcdoc = srcdoc(v, W.params?.[v.slug] || {});
        // A frame that navigates away from its page has sent a request out: closed.
        let loads = 0;
        iframe.addEventListener('load', () => {
          if (++loads < 2 || W.frames.get(v.slug)?.iframe !== iframe) return;
          iframe.remove();
          W.frames.delete(v.slug);
          W.left.set(v.slug, v.id);
          W.errors.set(v.slug, { message: 'the view left its page (it navigated the frame), so it was closed' });
          paintStage();
        });
        shell.append(iframe);
        W.frames.set(v.slug, { iframe, capId: v.id, sig, slug: v.slug });
      }
      for (const [slug, f] of W.frames) f.iframe.hidden = slug !== W.view;
      if (W.view === USAGE) return;
      deviceBox.querySelector('.viewerr')?.remove();
      const err = W.errors.get(W.view);
      if (err) {
        deviceBox.append(h('div', { class: 'viewerr' }, icon('bad'), h('div', { class: 'grow' }, h('b', {}, 'This screen hit an error. '), err.message,
          err.repairing ? h('div', { class: 'small muted' }, h('span', { class: 'spin' }), ' The engine is repairing it; the fixed version replaces it when ready.') : null),
          !err.repairing ? h('button', { class: 'btn sm', onclick: () => ask(`The screen "${v?.title}" shows this error: ${err.message}. Fix it.`) }, 'Ask to fix') : null,
          h('button', { class: 'btn sm ghost', onclick: () => { W.errors.delete(W.view); paintStage(); } }, 'Dismiss')));
      }
    }
    // The Usage tab: what people did, each visit step by step, drawn by the
    // studio's Activity view. It reads GET /api/apps/:id/activity.
    function paintUsage() {
      const on = W.view === USAGE && !!W.st?.views.length;
      deviceBox.hidden = on;
      usageBox.hidden = !on;
      stage.parentNode?.classList.toggle('usage-on', on);
      if (!on) { W.act?.destroy(); W.act = null; return; }
      if (W.act) { W.act.refresh(); return; }
      W.act = mods.activity.mountActivity(usageBox, {
        appId,
        api: (method, p, body) => api(p, method === 'GET' ? {} : { method, body }),
        openScreen: (slug) => switchView(slug),
      });
    }
    function switchView(slug, params) {
      if (params) {
        (W.params ||= {})[slug] = params;
        const f = W.frames.get(slug);
        if (f) { f.iframe.remove(); W.frames.delete(slug); }
      }
      W.view = slug;
      history.replaceState(null, '', ctx.href('a', appId, slug));
      paintStage();
      if (W.tab === 'code' && W.pick?.kind === 'view') { W.pick = null; paintInspector(); }
    }

    // ---- the broker: what a frame may ask for

    const onMessage = async (e) => {
      const m = e.data;
      if (!m || !m.__sa || !W.st) return;
      let entry = null;
      for (const f of W.frames.values()) if (f.iframe.contentWindow === e.source) entry = f;
      const preview = !entry && [...shell.querySelectorAll('iframe.pv')].some((f) => f.contentWindow === e.source);
      if (!entry && !preview) return;
      const reply = (value, error, refused, transient) => e.source.postMessage({ __sa: 1, kind: 'result', id: m.id, value, error, refused, transient }, '*');
      if (preview && m.kind === 'rpc' && !['sql', 'me', 'people', 'files'].includes(m.method)) { reply(undefined, 'a preview changes nothing'); return; }
      try {
        if (m.kind === 'rpc') {
          const [a, b, c] = m.args || [];
          let value;
          switch (m.method) {
            case 'sql': value = (await post(`${base}/sql${atV()}`, { sql: a, params: b })).rows; break;
            case 'insert': value = (await post(`${base}/insert${atV()}`, { table: a, row: b })).row; break;
            case 'update': value = (await post(`${base}/update${atV()}`, { table: a, id: b, fields: c })).row; break;
            case 'remove': value = await post(`${base}/remove${atV()}`, { table: a, id: b }); break;
            case 'call': value = (await post(`${base}/call${atV()}`, { name: a, input: b })).result; break;
            case 'me': value = null; break; // people sign in on the app's own page
            case 'companies': value = []; break;
            case 'company': value = null; break;
            case 'signIn': case 'signUp': case 'signOut': case 'createCompany': case 'useCompany': case 'addToCompany': case 'joinCompany': throw new Error('People sign in on the app’s own page; here you see the app as a visitor does');
            case 'people': value = (await api(`${base}/people`)).people; break;
            case 'files': value = (await post(`${base}/files/info`, { ids: a })).files.map((f) => ({ ...f, url: `${kit.base}${f.url}` })); break;
            case 'email': value = (await post(`${base}/email`, a)).email; break;
            case 'geocode': value = (await api(`/api/geocode?q=${encodeURIComponent(a || '')}`)).places; break;
            case 'ask': value = (await post(`${base}/ask`, { prompt: a, opts: b || {} })).answer; break;
            case 'fetch': value = (await post(`${base}/fetch`, { url: a, opts: b || {} })).page; break;
            case 'upload': {
              if (!(a instanceof Blob)) throw new Error('app.upload takes a File (from an <input type="file"> or a drop)');
              const q = new URLSearchParams({ name: a.name || 'file', type: a.type || 'application/octet-stream' });
              const out = await api(`${base}/files?${q}`, { method: 'POST', body: a, type: 'application/octet-stream' });
              value = { ...out.file, url: `${kit.base}${out.file.url}` };
              break;
            }
            default: throw new Error(`no app.${m.method}`);
          }
          reply(value);
        } else if (m.kind === 'track' && entry) {
          post(`${base}/track${atV()}`, { kind: m.action === 'abandon' ? 'abandon' : 'action', action: m.action, detail: m.detail, cap: entry.capId, q: m.q, results: m.results, table: m.table, ms: m.ms }).catch(() => {});
        } else if (m.kind === 'link') {
          const url = mods.policy.linkToOpen(m.href);
          if (url) window.open(url, '_blank', 'noopener,noreferrer');
        } else if (m.kind === 'open' && entry) {
          if (W.st.views.find((v) => v.slug === m.slug)) switchView(m.slug, m.params);
        } else if (m.kind === 'error' && entry) {
          const out = await post(`${base}/error`, { cap: m.cap || entry.capId, message: `${m.message}\n${m.stack || ''}`.slice(0, 1500) }).catch(() => ({}));
          W.errors.set(entry.slug, { message: m.message, repairing: !!out.repairing });
          paintStage();
        }
      } catch (err) {
        // A refusal (a rule's, or a key's: 401, 403) is an answer, and a call
        // nobody answered (the network, the engine restarting: no answer, 502
        // to 504) is no fault of the view: neither is reported as its error.
        if (m.kind === 'rpc') reply(undefined, err.message, !!err.body?.refused || err.status === 401 || err.status === 403, transientError(err));
      }
    };
    window.addEventListener('message', onMessage);

    // ---- the inspector

    // What an owner needs first (their data, its versions, its look, who can
    // use it), then how it is made.
    const TABS = [['data', 'Data'], ['versions', 'Versions'], ['design', 'Design'], ['publish', 'Publish'], ['reach', 'Reach'], ['made', 'Made of'], ['time', 'Time'], ['code', 'Code']];
    function paintInspector() {
      const st = W.st;
      itabs.replaceChildren(...TABS.map(([k, label]) => h('button', { type: 'button', role: 'tab', 'aria-pressed': String(W.tab === k), onclick: () => { W.tab = k; W.pick = null; paintInspector(); } }, label,
        k === 'versions' && st.versions?.length ? h('span', { class: 'muted' }, ` ${st.versions.length}`) : null)));
      const keepScroll = ibody.parentNode.scrollTop;
      const draw = { made: madeTab, time: timeTab, code: codeTab, data: dataTab, versions: versionsTab, design: designTab, reach: reachTab, publish: publishTab }[W.tab] || madeTab;
      // A tab with a field being typed in is left as it is.
      const typing = document.activeElement && /^(TEXTAREA|INPUT|SELECT)$/.test(document.activeElement.tagName) && !/^(checkbox|radio|button)$/.test(document.activeElement.type || '');
      if (ibody.contains(document.activeElement) && typing && ibody.dataset.tab === W.tab) return;
      ibody.dataset.tab = W.tab;
      ibody.replaceChildren(...draw(st).flat(Infinity).filter(Boolean));
      ibody.parentNode.scrollTop = keepScroll;
    }

    // Where the app's code came from (engine/provenance.mjs).
    function madeTab(st) {
      const m = st.made;
      if (!m || !m.pieces.length) return [h('div', { class: 'note' }, 'Once the first version is stored, this shows how much of the app the engine generated from the CREATE statements the agent wrote, and how much the agent wrote from scratch.')];
      const t = m.totals;
      const agentShare = m.agent.wrote ? m.agent.create / m.agent.wrote : 0;
      return [
        h('div', {}, h('div', { class: 'headline' }, h('span', { class: 'big' }, pct(t.create.share)), h('span', { class: 'dim' }, 'of the code that runs was generated from CREATE statements')),
          h('div', { class: 'small muted', style: { marginTop: '2px' } }, `${num(m.runs)} lines run in version ${m.version}: ${num(t.create.runs)} from CREATE TABLE, SCREEN and RULE, ${num(t.scratch.runs)} written from scratch${t.reused.runs ? `, ${num(t.reused.runs)} reused` : ''}${t.hand.runs ? `, ${num(t.hand.runs)} edited by hand` : ''}.`)),
        meter(m),
        h('div', { class: 'ac-legend' }, ORIGIN_ORDER.filter((k) => t[k].pieces).map((k) => h('span', {}, h('i', { class: `k-${k}` }), `${ORIGIN_SHORT[k]} ${pct(t[k].share)}`))),
        h('div', { class: 'tiles' },
          h('div', { class: 'tile' }, h('b', {}, num(m.agent.wrote)), h('span', {}, 'lines the agent wrote')),
          h('div', { class: 'tile' }, h('b', {}, pct(agentShare)), h('span', {}, 'of them as CREATE statements')),
          h('div', { class: 'tile' }, h('b', {}, m.leverage ? `${m.leverage.toFixed(1)}×` : '–'), h('span', {}, 'lines run per CREATE line')),
          h('div', { class: 'tile' }, h('b', {}, `${t.create.pieces} / ${t.scratch.pieces}`), h('span', {}, 'pieces declared / from scratch'))),
        h('div', { class: 'lbl' }, 'Each piece'),
        h('div', { class: 'pieces' }, m.pieces.map((p) => h('div', { class: 'piece', title: `${p.id}\nwritten: ${p.wrote} lines · runs: ${p.runs} lines`, onclick: () => showCode(p.id) },
          h('div', { class: 'row' }, h('span', { class: `tag k-${p.origin}` }, p.statement || (p.language === 'javascript' ? 'JS' : p.language.toUpperCase())), h('span', { class: 'nm' }, `${p.kind === 'component' ? 'ui.' : p.kind === 'helper' ? 'lib.' : ''}${p.name}`)),
          h('span', { class: 'ln' }, p.origin === 'reused' ? `${num(p.runs)} reused` : p.generated ? `${num(p.wrote)} → ${num(p.runs)}` : `${num(p.wrote)} lines`),
          h('div', { class: 'gen' }, p.origin === 'reused' ? h('span', { class: 'k-reused', style: { flex: '1', background: 'var(--ac-r)' } })
            : [h('span', { class: 'w', style: { flex: String(p.wrote || 0) } }), p.generated ? h('span', { style: { flex: String(p.generated), background: p.origin === 'create' ? 'var(--ac-c)' : 'var(--ac-s)' } }) : null]),
          h('div', { class: 'how' }, p.how)))),
        h('div', { class: 'note' }, 'Lines that run: a screen’s JSON with its patterns expanded, a rule’s compiled steps, a table’s CREATE TABLE, a JavaScript piece’s code, all without blank lines. A bar shows what the agent wrote (dark) and what the engine generated from it. Reused pieces are the library’s, shared with other apps.'),
      ];
    }

    // Where the building time went: every build of the app with a timeline
    // (sa_traces), summed up, one bar per build, and the slowest steps.
    function timeTab(st) {
      const box = h('div', { class: 'acr-tl', style: { gap: '12px' } });
      const key = (st.requests || []).map((r) => `${r.id}:${r.status}`).join(',');
      const paintTime = (traces) => box.replaceChildren(...timeBody(traces).filter(Boolean));
      if (W.traces?.key === key) paintTime(W.traces.list);
      else {
        box.append(h('div', { class: 'small muted' }, h('span', { class: 'spin' }), ' Reading the builds'));
        api(`${base}/traces`).then(({ traces }) => { W.traces = { key, list: traces || [] }; if (W.tab === 'time') paintTime(W.traces.list); })
          .catch((e) => box.replaceChildren(h('div', { class: 'small' }, e.message)));
      }
      return [box];
    }
    function timeBody(traces) {
      if (!traces.length) return [h('div', { class: 'note' }, 'No builds with a timeline yet.')];
      const KINDS = ['strata', 'plan', 'model', 'write', 'check', 'fix'];
      const builds = traces.slice().sort((a, b) => (a.t0 || 0) - (b.t0 || 0)).map((t) => ({ t, ...timeSplit(t) }));
      let ms = 0, cost = 0, model = 0, calls = 0, tin = 0, tout = 0;
      const wall = {};
      for (const b of builds) {
        ms += b.t.ms || 0; cost += b.t.cost ?? b.model.cost; model += b.model.ms; calls += b.model.calls; tin += b.model.in; tout += b.model.out;
        for (const [k, v] of Object.entries(b.wall)) wall[k] = (wall[k] || 0) + v;
      }
      const kinds = KINDS.filter((k) => wall[k]);
      const sum = kinds.reduce((a, k) => a + wall[k], 0) || 1;
      const longest = Math.max(...builds.map((b) => b.t.ms || 0), 1);
      const done = builds.filter((b) => b.t.status !== 'failed');
      const slow = builds.flatMap((b) => (b.t.spans || []).map((sp) => ({ ...sp, ms: sp.end - sp.start, build: b.t }))).sort((a, b) => b.ms - a.ms).slice(0, 6);
      const label = (t) => (t.n ? `v${t.n}` : t.status === 'failed' ? 'failed' : '–');
      const counts = {};
      for (const b of builds) for (const d of b.t.decisions || []) if (d.kind !== 'table') counts[d.decision] = (counts[d.decision] || 0) + 1;
      return [
        h('div', { class: 'tiles' },
          h('div', { class: 'tile' }, h('b', {}, String(builds.length)), h('span', {}, `build${builds.length === 1 ? '' : 's'}${builds.length > done.length ? `, ${builds.length - done.length} failed` : ''}`)),
          h('div', { class: 'tile' }, h('b', {}, secs(ms)), h('span', {}, `building in all, ${secs(ms / builds.length)} a build`)),
          h('div', { class: 'tile' }, h('b', {}, money(cost)), h('span', {}, `for ${calls} model call${calls === 1 ? '' : 's'}`)),
          h('div', { class: 'tile' }, h('b', {}, `${kTok(tin)} / ${kTok(tout)}`), h('span', {}, `tokens in / out, ${secs(model)} of model time`))),
        h('div', {}, h('h3', {}, 'Where the time went'),
          h('div', { class: 'split', style: { height: '14px' } }, kinds.map((k) => h('span', { class: `k-${k}`, style: { flex: String(wall[k]) }, title: `${KIND_LABEL[k]}: ${secs(wall[k])}` }))),
          h('div', { class: 'key' }, kinds.map((k) => h('span', {}, h('i', { class: `k-${k}` }), `${KIND_LABEL[k]} ${Math.round((wall[k] / sum) * 100)}%`)))),
        h('div', {}, h('h3', {}, 'Each build'),
          h('div', { class: 'bars' }, builds.map((b) => {
            const own = KINDS.filter((k) => b.wall[k]);
            const tot = own.reduce((a, k) => a + b.wall[k], 0) || 1;
            return h('button', { type: 'button', class: 'brow', title: `“${b.t.text}”\n${secs(b.t.ms || 0)} · ${money(b.t.cost ?? b.model.cost)}`, onclick: () => showTimeline(appId, b.t.id, b.t.n ? `How version ${b.t.n} was built` : 'This build') },
              h('span', { class: `bn${b.t.status === 'failed' ? ' bad' : ''}` }, label(b.t)),
              h('span', { class: 'btrack' }, h('span', { class: 'bfill', style: { width: `${Math.max(2, ((b.t.ms || 0) / longest) * 100)}%` } },
                own.map((k) => h('span', { class: `k-${k}`, style: { flex: String(b.wall[k] / tot) } })))),
              h('span', { class: 'bt' }, secs(b.t.ms || 0)), h('span', { class: 'bc' }, money(b.t.cost ?? b.model.cost)));
          })),
          h('div', { class: 'note', style: { marginTop: '4px' } }, 'One bar per build, as long as it took; click one for its timeline.')),
        h('div', {}, h('h3', {}, 'Slowest steps'), h('div', { class: 'list' }, slow.map((sp) => h('div', { class: 'li', onclick: () => showTimeline(appId, sp.build.id, sp.build.n ? `How version ${sp.build.n} was built` : 'This build') },
          h('i', { class: `k-${sp.kind}`, style: { width: '9px', height: '9px', borderRadius: '3px', flex: 'none' } }), h('span', { class: 'grow', style: { overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap' } }, sp.label),
          h('span', { class: 'small muted' }, label(sp.build)), h('b', { class: 'small' }, secs(sp.ms)))))),
        Object.keys(counts).length ? h('div', {}, h('h3', {}, 'What the builds did with each piece'), h('div', { class: 'key' }, Object.entries(DECISION).filter(([k]) => counts[k]).map(([k, [l]]) => h('span', { class: `badge d-${k}` }, `${l} ${counts[k]}`)))) : null,
      ];
    }

    // How people use the app: the engine's record of what they did
    // (sa_events, GET /api/apps/:id/activity), over 7, 30 or 90 days.
    function codeTab(st) {
      if (W.pick) return codeDetail(W.pick);
      const group = (label, list) => (list.length ? [h('div', { class: 'lbl' }, label), h('div', { class: 'list' }, list.map((c) => h('div', { class: 'li', onclick: () => { W.pick = c; paintInspector(); } },
        h('span', { class: 'tag' }, c.kind === 'view' ? (c.spec?.screen ? (c.spec.statement ? 'SCREEN' : 'JSON') : 'JS') : c.kind === 'function' ? (c.spec?.rule ? 'RULE' : 'JS') : c.kind === 'table' ? 'TABLE' : 'JS'),
        h('span', { class: 'nm grow' }, `${c.kind === 'component' ? 'ui.' : c.kind === 'helper' ? 'lib.' : ''}${c.kind === 'table' ? c.spec?.name || c.slug : c.slug}`),
        h('span', { class: 'small muted' }, `v${c.version}`))))] : []);
      const tables = st.tables.map((t) => ({ ...t, kind: 'table', slug: t.name, spec: { name: t.name }, version: Number(String(t.id).split('@').pop()) || 1 }));
      return [
        h('div', { class: 'row' }, h('span', { class: 'note grow' }, 'Each piece is a row of sa_capabilities, stored as its SQL. Click one to read it; a CREATE SCREEN can be edited here, with no model.'),
          h('button', { class: 'btn sm', onclick: () => wholeApp() }, icon('download'), 'Whole app')),
        ...group('Screens', st.views), ...group('Rules and functions', st.functions), ...group('Tables', tables),
        ...group('Shared components', st.components || []), ...group('Shared helpers', st.helpers || []),
      ];
    }
    function codeDetail(c) {
      const box = h('div', {}, h('div', { class: 'small muted' }, h('span', { class: 'spin' }), ' Loading'));
      const back = h('button', { class: 'btn sm ghost', onclick: () => { W.pick = null; paintInspector(); } }, icon('left'), 'All pieces');
      api(`/api/capabilities/${encodeURIComponent(c.id)}`).then((cap) => {
        const sql = cap.sql || cap.spec?.statement || cap.code;
        const compiled = (cap.kind === 'view' && cap.spec?.screen) || (cap.kind === 'function' && cap.spec?.rule) ? prettyJson(cap.code) : null;
        const editable = cap.kind === 'view' && cap.spec?.statement && !W.tryV;
        const shown = h('div', {}, codeBlock(sql));
        const err = h('div', { class: 'small', style: { color: 'var(--crit-ink)' } });
        const edit = () => {
          const t = h('textarea', { class: 'stmt', spellcheck: 'false' });
          t.value = cap.sql || cap.spec.statement;
          const save = h('button', { class: 'btn sm primary', onclick: async () => {
            save.disabled = true; err.textContent = '';
            try {
              const r = await post(`${base}/screens/${cap.slug}`, { statement: t.value, note: `Edited the statement of ${cap.title} in the command center` });
              toast(r.unchanged ? 'Nothing changed' : `Version ${r.n} is live`, r.unchanged ? 'info' : 'ok');
              W.pick = null; refresh();
            } catch (e) { err.textContent = e.message; save.disabled = false; }
          } }, 'Save as a new version');
          shown.replaceChildren(t, h('div', { class: 'row', style: { marginTop: '6px' } }, save, h('button', { class: 'btn sm ghost', onclick: () => shown.replaceChildren(codeBlock(sql)) }, 'Cancel')), err);
          t.focus();
        };
        box.replaceChildren(
          h('div', { class: 'row' }, h('b', { class: 'grow' }, cap.title), h('span', { class: 'small muted' }, `v${cap.version} · ${cap.author || 'forge'}`)),
          cap.description ? h('div', { class: 'note' }, cap.description) : null,
          h('div', { class: 'row', style: { margin: '8px 0' } }, compiled ? core.seg([['sql', 'As written'], ['compiled', 'What runs']], 'sql', (v) => shown.replaceChildren(v === 'sql' ? codeBlock(sql) : h('pre', { class: 'code src' }, compiled))) : null,
            h('span', { class: 'grow' }), editable ? h('button', { class: 'btn sm', onclick: edit }, icon('edit'), 'Edit') : null, h('button', { class: 'btn sm ghost', onclick: () => core.copy(sql) }, icon('copy'))),
          shown);
      }).catch((e) => box.replaceChildren(h('div', { class: 'small' }, e.message)));
      return [h('div', {}, back), box];
    }
    async function wholeApp() {
      try {
        const out = await api(`${base}/source`);
        const file = `${String(out.name || appId).toLowerCase().replace(/[^a-z0-9]+/g, '-').replace(/^-|-$/g, '') || appId}.sql`;
        const dl = () => { const url = URL.createObjectURL(new Blob([out.sql], { type: 'application/sql' })); const a = h('a', { href: url, download: file }); document.body.append(a); a.click(); a.remove(); setTimeout(() => URL.revokeObjectURL(url), 1000); };
        core.drawer({ title: `${out.name} as SQL`, sub: `${num(out.sql.split('\n').length)} lines: every table, screen, rule and function, and the shared pieces it uses`, wide: true,
          body: h('div', { class: 'ac', style: { gap: '10px' } }, h('div', { class: 'row' }, h('button', { class: 'btn sm', onclick: dl }, icon('download'), file), h('button', { class: 'btn sm ghost', onclick: () => core.copy(out.sql) }, icon('copy'), 'Copy')), codeBlock(out.sql)) });
      } catch (e) { toast(e.message, 'bad'); }
    }

    function dataTab(st) {
      if (!st.tables.length) return [h('div', { class: 'note' }, 'No tables yet.')];
      const rows = h('div');
      const pickT = W.pickTable && st.tables.find((t) => t.name === W.pickTable);
      if (pickT) {
        rows.append(h('div', { class: 'small muted' }, h('span', { class: 'spin' }), ' Reading'));
        post(`${base}/sql${atV()}`, { sql: `SELECT * FROM ${pickT.name} ORDER BY created DESC LIMIT 50` }).then((r) => {
          const list = r.rows || [];
          const cols = [...new Set(list.flatMap((x) => Object.keys(x)))].filter((k) => k !== 'meaning').slice(0, 8);
          rows.replaceChildren(list.length ? h('div', { class: 'rows' }, core.table(cols.map((k) => ({ key: k, label: k, render: (x) => { const v = x[k]; return v == null ? '–' : typeof v === 'object' ? JSON.stringify(v) : String(v).slice(0, 80); } })), { rows: list, compact: true })) : h('div', { class: 'note' }, 'No rows yet.'));
        }).catch((e) => rows.replaceChildren(h('div', { class: 'small' }, e.message)));
      }
      return [
        h('div', { class: 'note' }, 'What people keep in the app. Pick a list to see its newest entries; nothing here is lost when the app changes or goes back a version.'),
        h('div', { class: 'list' }, st.tables.map((t) => h('div', { class: `li${W.pickTable === t.name ? ' on' : ''}`, onclick: () => { W.pickTable = W.pickTable === t.name ? null : t.name; paintInspector(); } },
          icon('table'), h('span', { class: 'grow' }, h('div', { class: 'nm' }, t.name), h('div', { class: 'small muted', style: { overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap' } }, (t.columns || []).map((c) => c.name).join(', '))),
          h('span', { class: 'small muted' }, `${num(t.rows)} row${t.rows === 1 ? '' : 's'}`),
          h('button', { class: 'btn sm ghost', type: 'button', title: `Download ${t.name} as a CSV file (a spreadsheet opens it)`, onclick: (e) => { e.stopPropagation(); exportData(t.name); } }, 'CSV')))),
        rows,
      ];
    }

    function versionsTab(st) {
      const head = Number(st.app.head);
      const builtBy = new Map((st.requests || []).filter((r) => r.status === 'done' && Number(r.comp)).map((r) => [Number(r.comp), r]));
      return [
        h('div', { class: 'note' }, 'Each version is a row of sa_compositions naming the pieces it is made of. Going back moves the app to that row; the data stays.'),
        h('div', { class: 'list' }, (st.versions || []).map((c) => h('div', { class: `ver${Number(c.n) === head ? ' on' : ''}` },
          h('div', { class: 'n' }, `v${c.n}`),
          h('div', {}, h('div', {}, c.summary || '(no summary)'), c.request ? h('div', { class: 'q' }, `“${c.request}”`) : null,
            h('div', { class: 'm' }, h('span', {}, core.ago(new Date(c.created).getTime())), h('span', {}, `${(c.caps || []).length} pieces`),
              builtBy.get(Number(c.n)) ? h('button', { class: 'linkbtn', onclick: () => showTimeline(appId, builtBy.get(Number(c.n)).id, `How version ${c.n} was built`) }, 'Timeline') : null,
              Number(c.n) === head ? h('span', { class: 'tag live' }, 'live') : h('button', { class: 'btn sm', onclick: async () => { try { await post(`${base}/restore`, { n: c.n }); toast(`Back to version ${c.n}`, 'ok'); } catch (e) { toast(e.message, 'bad'); } refresh(); } }, 'Go back to this')))))),
      ];
    }

    function designTab(st) {
      const d = st.design;
      if (!d) {
        if (!W.askedDesigns) { W.askedDesigns = true; api(`${base}/designs`).then(() => refresh()).catch(() => {}); }
        return [h('div', { class: 'note' }, h('span', { class: 'spin' }), ' Designing three looks for this app…')];
      }
      const again = h('button', { class: 'btn sm', onclick: async () => { again.disabled = true; again.textContent = 'Designing…'; try { await post(`${base}/designs/new`); await refresh(); } catch (e) { toast(e.message, 'bad'); again.disabled = false; } } }, 'New alternatives');
      return [
        h('div', { class: 'note' }, 'Three designs for this app. Pick one: every screen changes at once, with no rebuild. A design is a few tokens: its look, colours, type, corners, spacing and navigation.'),
        ...d.designs.map((x, i) => h('button', { class: `design${i === d.chosen ? ' on' : ''}`, type: 'button', onclick: async () => {
          d.chosen = i; applyDesign(); paintInspector();
          try { await post(`${base}/designs`, { chosen: i }); } catch (e) { toast(e.message, 'bad'); }
        } }, h('span', { class: 'ac-sw', style: { background: x.light?.accent } }), h('span', { class: 'ac-sw', style: { background: x.dark?.bg } }),
          h('span', { class: 'grow' }, h('b', {}, x.name), x.about ? h('div', { class: 'small muted' }, x.about) : null, h('div', { class: 'small muted' }, [x.look, x.titles && `${x.titles} titles`, x.corners && `${x.corners} corners`, x.nav].filter(Boolean).join(' · '))),
          i === d.chosen ? h('span', { class: 'tag live' }, 'in use') : null)),
        h('div', { class: 'row' }, again, h('span', { class: 'small muted' }, 'keeps the one in use')),
      ];
    }

    // What the app may reach beyond its own tables (engine/reach.mjs): each
    // off until switched on here, and what the app's code uses of it.
    function reachTab(st) {
      const r = st.reach;
      if (!r) return [h('div', { class: 'note' }, 'Nothing to show yet.')];
      const set = async (patch, said) => {
        try { await post(`${base}/reach`, patch); toast(said, 'ok'); } catch (e) { toast(e.message, 'bad'); }
        refresh();
      };
      const rows = Object.entries(r.abilities || {}).map(([k, a]) => {
        const used = !!r.uses?.[k];
        const on = !!r.on?.[k];
        return h('div', { class: `reach${used && !on ? ' wants' : ''}` },
          core.toggle(on, (v) => set({ [k]: v }, `${a.title} ${v ? 'switched on' : 'switched off'} for this app`)),
          h('span', { class: 'grow' }, h('b', {}, a.title), h('span', { class: 'what' }, a.what)),
          h('span', { class: `tag${used ? (on ? ' live' : ' try') : ''}` }, used ? (on ? 'in use' : 'used, off') : 'not used'));
      });
      const t = r.today || {};
      const perDay = h('select', { class: 'input', style: { width: 'auto' }, onchange: (e) => set({ perDay: Number(e.target.value) }, `At most ${e.target.value} questions to the model a day`) },
        [...new Set([20, 50, 200, 1000, r.on?.perDay].filter(Boolean))].sort((a, b) => a - b).map((n) => h('option', { value: n, selected: n === r.on?.perDay }, `${n} a day`)));
      // Hosts, secrets and schedules, from an engine that has them (each
      // part is left out when the engine does not say).
      const hosts = [...(r.on?.hosts || []).map((x) => ({ ...x, allowed: true })), ...Object.entries(r.asked || {}).filter(([host]) => !(r.on?.hosts || []).some((x) => x.host === host)).map(([host, a]) => ({ host, send: !!a.send, asked: a }))];
      const hostRows = hosts.map((x) => h('div', { class: `reach${x.allowed ? '' : ' wants'}` },
        h('span', { class: 'grow' }, h('b', { class: 'mono', style: x.send ? { color: 'var(--crit-ink)' } : null }, x.host), h('span', { class: 'what' }, x.allowed ? (x.send ? 'reads from it and sends to it' : 'reads from it') : `asked ${x.asked.count || 1} time${x.asked.count === 1 ? '' : 's'}, refused`)),
        x.allowed ? h('button', { class: 'btn sm ghost', onclick: () => set({ forget: x.host }, `${x.host} is no longer allowed`) }, 'Remove')
          : h('button', { class: 'btn sm', onclick: () => set({ allow: { host: x.host, send: x.send } }, `${x.host} allowed`) }, 'Allow')));
      const secretRows = (r.secrets || []).map((x) => h('div', { class: 'reach' }, icon('key'), h('span', { class: 'grow' }, h('b', { class: 'mono' }, x.name), h('span', { class: 'what' }, [(x.hosts || []).join(', '), x.used ? `used ${core.ago(new Date(x.used).getTime())}` : 'not used yet'].filter(Boolean).join(' · ')))));
      const scheduleRows = (r.schedules || []).map((x) => h('div', { class: 'reach' }, icon('clock'), h('span', { class: 'grow' }, h('b', {}, x.title || x.slug),
        h('span', { class: 'what' }, [x.when, x.paused ? 'paused' : x.next ? `next ${new Date(x.next).toLocaleString()}` : null, x.last ? `last ${core.ago(new Date(x.last).getTime())}${x.status ? ` (${x.status})` : ''}` : 'not run yet'].filter(Boolean).join(' · ')),
        x.error ? h('span', { class: 'what', style: { color: 'var(--crit-ink)' } }, x.error) : null),
        h('button', { class: 'btn sm', onclick: async () => { try { await post(`${base}/schedules/${x.slug}/run`); toast(`${x.title || x.slug} ran`, 'ok'); } catch (e) { toast(e.message, 'bad'); } refresh(); } }, 'Run now'),
        h('button', { class: 'btn sm ghost', onclick: async () => { try { await post(`${base}/schedules/${x.slug}/${x.paused ? 'resume' : 'pause'}`); } catch (e) { toast(e.message, 'bad'); } refresh(); } }, x.paused ? 'Resume' : 'Pause')));
      return [
        h('div', { class: 'note' }, 'An app reads and writes its own tables only. Anything beyond them is switched on here, per app, and the app’s code says which it uses.'),
        h('div', { class: 'list' }, rows),
        hostRows.length ? [h('div', { class: 'lbl' }, 'Hosts'), h('div', { class: 'list' }, hostRows)] : null,
        scheduleRows.length ? [h('div', { class: 'lbl' }, 'Schedules'), h('div', { class: 'list' }, scheduleRows)] : null,
        secretRows.length ? [h('div', { class: 'lbl' }, 'Secrets'), h('div', { class: 'note' }, 'A secret is pasted once, in the studio, and never shown again.'), h('div', { class: 'list' }, secretRows)] : null,
        h('div', { class: 'row' }, h('span', { class: 'small dim grow' }, 'Questions to the model'), perDay),
        h('div', { class: 'note' }, `Today: ${t.asks || 0} question${t.asks === 1 ? '' : 's'} to the model${t.cached ? ` and ${t.cached} answered from kept answers` : ''}, $${(t.usd || 0).toFixed(3)}; ${t.fetches || 0} page${t.fetches === 1 ? '' : 's'} fetched.`),
      ];
    }

    // Open to the public (engine/publish.mjs): the switch, its addresses,
    // and hosts of the owner's own.
    function publishTab(st) {
      const p = st.publish;
      if (!p) return [h('div', { class: 'note' }, 'This engine cannot publish apps yet: it is older than the app creator.')];
      const set = async (patch, said) => {
        try { await post(`${base}/publish`, patch); if (said) toast(said, 'ok'); } catch (e) { toast(e.message, 'bad'); }
        refresh();
      };
      const onIp = kit.publicUrl ? `${kit.publicUrl}/a/${appId}` : null;
      const links = [...p.urls, ...(onIp ? [onIp] : [])];
      const suggested = String(st.app.name || '').toLowerCase().normalize('NFKD').replace(/[\u0300-\u036f]/g, '').replace(/['’]/g, '').replace(/[^a-z0-9]+/g, '-').replace(/^-+|-+$/g, '').slice(0, 40) || 'my-app';
      const name = h('input', { class: 'input mono', value: p.name || '', placeholder: suggested, 'aria-label': 'The app’s name in its address', style: { flex: '1 1 120px', minWidth: '0' } });
      const host = h('input', { class: 'input mono', placeholder: 'books.example.org', 'aria-label': 'A host of your own', style: { flex: '1 1 160px', minWidth: '0' } });
      const linkRow = (u) => h('div', { class: 'reach' }, icon('ext'), h('a', { class: 'grow mono', href: u, target: '_blank', rel: 'noopener', style: { wordBreak: 'break-all' } }, u.replace(/\/$/, '')),
        h('button', { class: 'btn sm ghost', onclick: () => core.copy(u) }, icon('copy'), 'Copy'));
      // Who can see this? Four choices, from the safest; picked, seen as a
      // stranger would see it, then made so.
      const now = p.on ? (p.access || 'look') : 'off';
      const pick = W.pubPick && W.pubPick.app === appId ? W.pubPick.v : now;
      const CHOICES = [
        ['off', 'Only me', 'Only you and other admins of this cluster can open it, from here or the studio.'],
        ['look', 'Visitors can look', 'Anyone with the address sees what its screens show; nothing they do changes its data.'],
        ['add', 'Customers can look and add', 'Anyone can also add entries and upload files (a booking, an order, a sign-up), and change or delete only what they added themselves, never anyone else’s.'],
        ['change', 'Anyone can do anything', 'Anyone with the address can edit and delete every entry, other people’s included, as you can.'],
      ];
      const radios = CHOICES.map(([v, label, what]) => {
        const id = `ac-pub-${appId}-${v}`;
        const input = h('input', { type: 'radio', name: `ac-pub-${appId}`, id, value: v, checked: pick === v, onchange: () => { W.pubPick = { app: appId, v }; paintInspector(); } });
        return h('label', { class: `pubchoice${pick === v ? ' on' : ''}`, for: id }, input, h('span', { class: 'grow' }, h('b', {}, label, v === now ? h('span', { class: 'small muted' }, ' · now') : null), h('span', { class: 'what' }, what)));
      });
      const contact = (st.tables || []).flatMap((t) => (t.columns || []).filter((c) => CONTACT.test(c.name)).map((c) => ({ table: t.name, column: c.name, rows: t.rows || 0 })));
      const apply = async () => {
        if (pick === 'change') {
          const ok = await core.confirm({ title: 'Let anyone edit and delete?', ok: 'Let anyone do anything', danger: true, typed: p.name || appId,
            body: 'Anyone with the address could edit or delete every entry, other people’s included. For an app strangers use, “Customers can look and add” is usually enough.' });
          if (!ok) return;
        }
        W.pubPick = null;
        await set(pick === 'off' ? { on: false } : { on: true, access: pick }, pick === 'off' ? 'Only you and other admins can open it now' : `Published: ${CHOICES.find((c) => c[0] === pick)[1].toLowerCase()}`);
      };
      const asStranger = () => openStudio(`a/${appId}?stranger=${pick === 'off' ? 'look' : pick}`);
      return [
        h('div', { class: 'lbl' }, 'Who can see this?'),
        h('div', { class: 'pubchoices', role: 'radiogroup', 'aria-label': 'Who can see this app' }, radios),
        contact.length && pick !== 'off' ? h('div', { class: 'pubwarn' }, icon('warn'), ' ', h('b', {}, 'Visible to everyone: '),
          `${contact.map((c) => `${c.table}.${c.column}${c.rows ? ` (${num(c.rows)} entr${c.rows === 1 ? 'y' : 'ies'})` : ''}`).join(', ')} look${contact.length === 1 ? 's' : ''} like people’s emails, phone numbers or addresses. Anyone with the address could read them wherever a screen shows them. To keep them private, ask for sign-in, and for each person to see only their own entries.`) : null,
        h('div', { class: 'row nw', style: { gap: '8px', flexWrap: 'wrap' } },
          h('button', { class: 'btn sm', type: 'button', title: 'Open the app as someone with no account would, with this choice', onclick: asStranger }, icon('eye'), 'See it as a stranger'),
          pick !== now ? h('button', { class: `btn sm ${pick === 'change' ? 'danger' : 'primary'}`, type: 'button', onclick: apply }, pick === 'off' ? 'Make it private' : now === 'off' ? 'Publish' : 'Save') : null,
          pick !== now ? h('button', { class: 'btn sm ghost', type: 'button', onclick: () => { W.pubPick = null; paintInspector(); } }, 'Cancel') : null),
        p.on && links.length ? [h('div', { class: 'lbl' }, 'Its addresses'), h('div', { class: 'list' }, links.map(linkRow))] : null,
        h('div', { class: 'note' }, 'Whatever you choose, what visitors write is checked (only the choices its forms offer, nothing over 10 KB), and they never see how it is made, its versions or its people’s emails, nor reach other apps or this studio.'),
        p.domain ? [
          h('div', { class: 'lbl' }, 'Name'),
          h('div', { class: 'row nw' }, name, h('span', { class: 'small muted mono' }, `.${p.domain}`), h('button', { class: 'btn sm', onclick: () => set({ name: name.value.trim() }, 'Name saved') }, 'Save')),
          h('div', { class: 'lbl' }, 'Hosts of your own'),
          p.hosts.length ? h('div', { class: 'list' }, p.hosts.map((x) => h('div', { class: 'reach' }, h('b', { class: 'grow mono' }, x), h('button', { class: 'btn sm ghost', onclick: () => set({ hosts: p.hosts.filter((y) => y !== x) }, `${x} removed`) }, 'Remove')))) : null,
          h('div', { class: 'row nw' }, host, h('button', { class: 'btn sm', onclick: () => host.value.trim() && set({ hosts: [...p.hosts, host.value.trim()] }, `${host.value.trim()} added`) }, 'Add')),
          h('div', { class: 'note' }, `Point a CNAME record for your host at ${p.name || 'its name'}.${p.domain}; the cluster gets its certificate once the record resolves.`),
        ] : h('div', { class: 'note' }, `This engine has no domain yet, so a published app is reached at ${onIp || '/a/<id> on the engine’s address'}. Give it one (the operator’s spec.apps.domain, with a wildcard DNS record) and each app gets a name of its own, like kids-reading.apps.example.com.`),
        h('div', { class: 'note' }, 'Visitors may also sign up and in, and ask the model or fetch pages if Reach allows it, within its questions a day. Each visitor is held to a rate a minute. Errors they hit are recorded under Usage; they never start a repair.'),
      ];
    }

    // ---- events

    const mine = (d) => d.app === appId;
    function onEvent(type, d) {
      if (!mine(d)) return;
      if (type === 'data') {
        for (const f of W.frames.values()) f.iframe.contentWindow?.postMessage({ __sa: 1, kind: 'change', tables: d.tables }, '*');
        if (W.tab === 'data') paintInspector();
      } else if (type === 'progress') {
        if (W.runningText !== d.text) { W.runningText = d.text; W.runningSource = d.source; W.lines = []; W.lastReplace = false; }
        if (d.replace && W.lastReplace) W.lines[W.lines.length - 1] = d.line; else W.lines.push(d.line);
        W.lastReplace = !!d.replace;
        W.failed = null;
        W.runningId = d.request;
        if (d.steps) W.build = { first: d.first, split: d.split, steps: d.steps, pieces: d.pieces || [], elapsed: d.elapsed || 0, expect: d.expect || {}, at: Date.now() };
        if (W.st) { paintConvo(); status.replaceChildren(h('span', { class: 'spin' }), ' building'); }
      } else if (type === 'preview') {
        W.previews.set(d.slug, { slug: d.slug, title: d.title, order: d.order, screen: d.screen });
        if (!W.previewView || !W.previews.has(W.previewView)) W.previewView = d.slug;
        paintStage();
      } else if (type === 'done' || type === 'failed') {
        W.ended.add(d.request);
        W.previews.clear(); W.previewView = null;
        if (type === 'failed' && !d.candidate) W.failed = { text: W.runningText, error: d.error, said: d.said };
        if (type === 'done') toast(d.candidate ? `Built from use, ready to try (version ${d.n})` : `Version ${d.n} is live${d.summary ? `: ${d.summary}` : ''}`, 'ok', 5000);
        W.lines = []; W.build = null; W.runningText = '';
        refresh();
      } else if (type === 'repair') {
        toast('A piece failed; the engine is repairing it');
      } else if (type === 'drawn') {
        toast(d.high ? `Checked the screens on a phone and a computer: ${d.high} thing${d.high === 1 ? '' : 's'} in the way${d.fixing ? '; fixing' : ''}` : 'Checked the screens on a phone and a computer: nothing in the way');
      } else if (['app', 'suggestions', 'design', 'candidates', 'request'].includes(type)) {
        refresh();
      }
    }
    // Without the engine's events (an older engine), the page polls.
    let poll = null;
    const noEvents = () => { if (!poll) poll = core.every(3000, () => refresh(), false, { always: true }); };

    const onResize = core.debounce(place, 150);
    window.addEventListener('resize', onResize);
    refresh();
    return {
      kind: 'app', onEvent, noEvents,
      stop() {
        W.stopped = true;
        W.act?.destroy();
        clearInterval(pacer);
        fitter?.disconnect();
        poll?.();
        window.removeEventListener('message', onMessage);
        window.removeEventListener('resize', onResize);
        for (const f of W.frames.values()) f.iframe.remove();
      },
    };
  }

  return () => { gone = true; try { page?.stop?.(); } catch (e) { console.warn(e); } };
}
