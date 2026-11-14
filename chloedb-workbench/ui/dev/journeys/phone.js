// Phone and tablet layouts of the journeys mock (JOURNEYS.md, "Phone"). Two
// breakpoints: 1024px (a tablet, or a narrow window: one column of cards,
// the top bar kept) and 640px (a phone: the top bar becomes a back link and
// the page's name, the sections move to a bottom tab bar, every grid is one
// column, nothing scrolls sideways but a box made to). A coarse pointer gets
// 44px targets at any width. The rest of the mock is written desktop first;
// this file only overrides it.
import { useStyle } from "../../levels/common.js";

useStyle("j-phone", `
/* ---- desktop: the phone-only pieces are hidden ---- */
.jt-m,.jt-msearch,.jt-an,.jtab,.jp-mstep,.jp-mbar,.jz-mact,.jz-cn,.ja-mro,.ja-tin{display:none}
.jz-exc>summary{list-style:none;cursor:default;display:flex;align-items:center;gap:6px}
.jz-exc>summary::-webkit-details-marker{display:none}
.jz-exc>summary .ji{display:none}
.jz-xn{font:600 11px var(--sans);color:var(--text-2);background:var(--surface-3);border-radius:99px;padding:0 7px;letter-spacing:0}
.jz-cap{font-size:12.5px;color:var(--text-2);min-height:20px;margin-top:4px}
.jh-more>summary{list-style:none;cursor:pointer;font-size:12.5px;display:inline-block}
.jh-more>summary::-webkit-details-marker{display:none}
.jh-more[open]>summary{margin-bottom:6px}
.ja-termwrap{display:flex;flex-direction:column}

@media (pointer: coarse){
  .btn,.btn.sm{min-height:44px}
  .btn.icon{min-width:44px}
}

/* ---- tablet and narrow windows ---- */
@media (max-width: 1024px){
  .jp{padding:18px 20px 40px}
  .jt-search{min-width:0}
  .jh-objs{grid-template-columns:repeat(2,minmax(0,1fr))}
  .jgrid2,.jh-cols,.jd-grid,.jh-two{grid-template-columns:minmax(0,1fr)}
  .jgrid3,.ju-grid>aside{grid-template-columns:repeat(2,minmax(0,1fr))}
  .jz-bb{grid-template-columns:150px minmax(0,1fr)}
  .jz-exc{grid-column:1/-1;border-left:0;padding-left:0;border-top:1px solid var(--line);padding-top:10px}
  .jd-rank{grid-template-columns:repeat(3,minmax(0,1fr))}
  .jz-trow{grid-template-columns:80px minmax(0,1fr) 160px}
  .jz-trow>.jnote{grid-column:2/-1}
  .jdrawer{width:min(520px,100%)}
}

/* ---- phone ---- */
@media (max-width: 640px){
  :root{--tabh:58px;--topb:calc(52px + env(safe-area-inset-top))}
  body{-webkit-text-size-adjust:100%}
  input,textarea,select{font-size:16px !important}

  /* the top bar: back to the parent, the page's name, agents, search */
  .jt{height:auto;min-height:var(--topb);padding:env(safe-area-inset-top) max(10px,env(safe-area-inset-right)) 0 max(10px,env(safe-area-inset-left));gap:4px}
  .jt>.brand,.jt>.crumbs,.jt>.sep,.jt-search,.jt-needs,.jt-more{display:none}
  .jt-m{display:flex;align-items:center;gap:6px;flex:1;min-width:0;height:52px}
  .jt-m b{font-size:16px;font-weight:650;white-space:nowrap;overflow:hidden;text-overflow:ellipsis}
  .jt-m .brand{display:inline-grid;place-items:center;width:44px;height:44px}.jt-m .mark{width:22px;height:22px}
  .jt-back{display:inline-flex;align-items:center;gap:2px;min-height:44px;padding:0 8px 0 2px;color:var(--accent);text-decoration:none;font-size:15px;flex:none;max-width:40%}
  .jt-back span{white-space:nowrap;overflow:hidden;text-overflow:ellipsis}
  .jt-back .ji{width:20px;height:20px}
  .jt-msearch{display:inline-flex}
  .jt-agents{height:44px;padding:0 6px;border:0;background:transparent;order:2;position:relative}
  .jt-agents .stack,.jt-agents .jt-at,.jt-agents>.ji{display:none}
  .jt-an{display:inline-flex;align-items:center;gap:4px;height:30px;padding:0 9px;border:1px solid var(--line-2);border-radius:99px;font:600 13px var(--sans);color:var(--text-2)}
  .jt-agents .ndot{position:absolute;top:8px;right:6px;margin:0}
  .jt-msearch{order:3}

  /* the bottom tab bar: Start, Needs you, Projects, More */
  .jtab{display:flex;flex:none;height:calc(var(--tabh) + env(safe-area-inset-bottom));padding:0 env(safe-area-inset-right) env(safe-area-inset-bottom) env(safe-area-inset-left);background:var(--surface);border-top:1px solid var(--line);z-index:32;position:relative}
  .jtab-i{flex:1;display:flex;flex-direction:column;align-items:center;justify-content:center;gap:3px;min-height:44px;border:0;background:transparent;color:var(--muted);font:600 11px var(--sans);text-decoration:none;cursor:pointer;padding:0}
  .jtab-i.on{color:var(--accent)}
  .jtab-ic{position:relative;display:inline-flex}.jtab-ic .ji{width:23px;height:23px}
  .jtab-n{position:absolute;top:-5px;left:14px;min-width:18px;height:18px;border-radius:99px;background:var(--warn);color:#241a00;font-size:11px;display:inline-grid;place-items:center;padding:0 5px;box-shadow:0 0 0 2px var(--surface)}

  /* overlays: Needs you fills the screen between the bars; the rest are bottom sheets */
  .jscrim{inset:var(--topb) 0 calc(var(--tabh) + env(safe-area-inset-bottom)) 0}
  .jdrawer{top:var(--topb);bottom:calc(var(--tabh) + env(safe-area-inset-bottom));left:0;right:0;width:auto;border-left:0}
  .jdrawer-h{flex-wrap:wrap;padding:12px 16px}.jdrawer-b{padding:12px 16px}
  .jpop,.jmore,.jsearch{top:auto;left:8px;right:8px;bottom:calc(var(--tabh) + env(safe-area-inset-bottom) + 8px);width:auto;transform:none;max-height:72%;overflow:auto;border-radius:16px}
  .jmore a{min-height:44px;justify-content:center}
  .jpop-r{min-height:44px}
  .jpop-r .line{white-space:normal;grid-column:2/4}
  .jpop-r kbd{display:none}
  .jpop-r .nm{flex-wrap:wrap;gap:2px 8px}
  .jpop-r .term{min-width:44px;min-height:44px}
  .jdrawer-h .muted{display:none}
  .jr-row>.lv-av{align-self:start}
  .ja-dl{align-items:start}
  .jpop-f{flex-wrap:wrap}
  .jpop-f>span:first-child{display:none}

  /* the page: one column, a 16px gutter, nothing wider than the screen */
  .jp,.jp.wide{padding:14px max(16px,env(safe-area-inset-right)) 24px max(16px,env(safe-area-inset-left));max-width:100%}
  .jp-head{flex-direction:column;align-items:stretch;gap:10px;margin:2px 0 14px}
  .jp-head h1,.jh .jp-head h1{font-size:21px}
  .jp-sub{font-size:14px}
  .jgrid2,.jgrid3,.jh-objs,.jh-cols,.jh-two,.jd-grid,.ju-grid>aside,.jx-grid,.jn-who,.jkv,.jd-rank,.jz-trow{grid-template-columns:minmax(0,1fr)}
  .jn-who .jx-k{grid-row:auto;padding:0}
  .jkv dt{margin-top:6px}
  .jrow{flex-wrap:wrap}
  .jp [style*="width"]:not(.jz-churn i):not(.jz-mbar b):not(.jz-wait b){max-width:100%}
  .jp .jrow>b[style*="width"],.jp .jrow>.jx-in{width:auto !important}
  .jx-in{width:100% !important;height:44px}
  .jchip{white-space:normal}
  .lv-code,code{overflow-wrap:anywhere}
  .jc{padding:14px}
  .btn,.btn.sm{min-height:44px;height:auto;padding:6px 14px;white-space:normal;line-height:1.3}
  .btn.icon{min-width:44px;padding:0}
  .jrow>.lv-link,.jc>.lv-link,.jstack>.lv-link,.jh-now>.lv-link{display:inline-flex;align-items:center;min-height:44px}
  .jd-ways{flex-wrap:wrap;margin:-4px 0 14px}
  .jd-ways a{min-height:40px;display:inline-flex;align-items:center}
  .jbig{padding:4px 4px 4px 12px}
  .jbig .btn{height:44px;flex:none}
  .jbig input,.jbig textarea{min-width:0;width:100%}
  .jd-note{display:block}
  .ja-tabs a{padding:9px 10px}
  .jm-dup,.jm-hide{display:none !important}

  /* steps: one line and a bar of segments; New project keeps Back and Next at the bottom */
  .jp-steps:not(.plain){display:none}
  .jp-steps.plain{overflow-x:auto;flex-wrap:nowrap;margin:0 -16px 10px;padding:0 16px;scrollbar-width:none}
  .jp-steps.plain .jp-step{white-space:nowrap;min-height:40px}
  .jp-after{white-space:nowrap}
  .jp-mstep{display:flex;flex-direction:column;gap:6px;font-size:13px;color:var(--text-2);margin-bottom:10px}
  .jp-mstep b{color:var(--accent)}
  .jp-mseg{display:flex;gap:4px}
  .jp-mseg i{flex:1;height:4px;border-radius:99px;background:var(--surface-3)}
  .jp-mseg i.done{background:color-mix(in srgb,var(--accent) 45%,var(--surface-3))}.jp-mseg i.now{background:var(--accent)}
  .jp-mbar{display:grid;grid-template-columns:1fr 2fr;gap:8px;position:sticky;bottom:0;margin:18px -16px -24px;padding:10px 16px;background:color-mix(in srgb,var(--surface) 92%,transparent);backdrop-filter:blur(8px);border-top:1px solid var(--line);z-index:5}
  .jp-mbar .btn{justify-content:center;height:46px;font-size:15px}
  .jn-part{grid-template-columns:26px auto auto minmax(0,1fr) 44px;gap:6px 8px}
  .jn-grip{display:none}
  .jn-pn{grid-row:1/3;align-self:start}
  .jn-pt{grid-column:2/5;grid-row:1}
  .jn-part>.jchip{grid-row:2;justify-self:start}
  .jn-part>.btn{grid-row:1;grid-column:5}

  /* Home */
  .jh-obj{min-height:0;padding:14px;display:grid;grid-template-columns:38px minmax(0,1fr);gap:4px 12px;align-items:start}
  .jh-obj .jh-ic{grid-row:span 2}
  .jh-obj .jh-fact,.jh-obj .jh-go{grid-column:1/-1}
  .jh-obj .jh-go{justify-self:stretch;justify-content:center}
  .jh-obj:not(.primary) .jh-go{display:none}
  .jh-obj:not(.primary) .jh-t{grid-column:2}
  .jh-now{order:-1}
  .jh-proj{grid-template-columns:26px minmax(0,1fr);gap:6px 10px}
  .jh-av{display:none}
  .jh-pl{grid-column:1/-1}
  .jh-proj>.jrow{grid-column:1/-1}
  .jh-proj>.jrow .btn{flex:1;justify-content:center}
  .jh.first{padding-top:20px}
  .jh-obj.big{min-height:0;padding:16px}
  .jh-obj .jbig,.jh-obj .jh-comp{grid-column:1/-1}
  .jh-setup{flex-wrap:wrap}
  .jh-cr{flex-wrap:wrap;min-height:44px}.jh-cr .muted{margin-left:0;width:100%}

  /* Needs you: the whole command, wrapped; Allow and Deny the full width */
  .jq-cmd,.jq-cmd.compact{max-height:none;overflow:visible;white-space:pre-wrap;word-break:normal;overflow-wrap:anywhere;font-size:12.5px}
  .jq-head{flex-wrap:wrap}
  .jq-opts{flex-direction:column}
  .jq-opts .btn{width:100%;height:auto;min-height:48px;padding:10px 12px;justify-content:flex-start;text-align:left;white-space:normal;line-height:1.35}
  .jh-acts,.jk-q>.jrow,.jk-a.needs>.jrow:last-child{display:grid;grid-template-columns:minmax(0,1fr);gap:8px}
  .jh-acts .btn,.jk-q>.jrow .btn,.jk-a.needs>.jrow:last-child .btn{width:100%;justify-content:center;min-height:48px;font-size:15px}
  .jh-more>summary{min-height:44px;display:flex;align-items:center}

  /* Review and ship: the verdict, then Merge or Send back, then the rings beside their legend, then the exceptions folded */
  .jz-verdict{font-size:15px;align-items:flex-start}.jz-verdict .dot{margin-top:5px}
  .jz-mact{display:grid;grid-template-columns:minmax(0,1fr);gap:8px}
  .jz-mact .btn{justify-content:center;min-height:48px;font-size:15px}
  .jz-bb{grid-template-columns:96px minmax(0,1fr);gap:12px}
  .jz-rings{width:96px;height:96px}
  .jz-exc{grid-column:1/-1;border-left:0;padding:10px 0 0;border-top:1px solid var(--line)}
  .jz-exc>summary{cursor:pointer;min-height:44px}
  .jz-exc>summary .ji{display:inline;margin-left:auto}
  .jz-exc[open]>summary .ji{transform:rotate(180deg)}
  .jz-e{align-items:flex-start}.jz-e .dot{margin-top:5px}
  .jr-row{grid-template-columns:24px 30px minmax(0,1fr);gap:8px 10px}
  .jr-row>.jchip,.jr-row>.btn{grid-column:3;justify-self:start}
  .jr-order{flex-wrap:wrap}.jr-order .btn{margin-left:0}
  .jz-piece summary,.jz-fold summary,.jz-sum,.jz-tline{flex-wrap:wrap}
  .jz-cn{display:inline;margin-left:6px;font:600 11px var(--mono);color:var(--text-2)}
  .jz-churn{height:auto}
  .jz-hunk{margin:0 10px 10px}
  .jz-hh{flex-wrap:wrap}
  .ja-dl{grid-template-columns:30px 14px minmax(0,1fr)}
  .ja-dl code{white-space:pre-wrap;overflow-wrap:anywhere}
  .ja-dl .ja-why{grid-column:3;justify-self:start;margin:2px 0 6px;min-height:36px}

  /* Debug: the strip scrolls in its own box, the blast radius is a ranked list */
  .jz-sx{overflow-x:auto;margin:0 -14px;padding:0 14px 6px;-webkit-overflow-scrolling:touch}
  .jz-sxi{min-width:1120px}
  .jz-ticks{height:40px}
  .jz-legend{flex-wrap:wrap;gap:6px 12px}
  .jd-rank li{grid-template-columns:40px minmax(0,1fr);padding:10px 12px}
  .jd-rn{text-align:center}
  .jd-t{grid-template-columns:28px minmax(0,1fr)}
  .jd-t>.btn,.jd-t>.muted{grid-column:2;justify-self:start}
  .jd-f{min-height:44px}

  /* the agent page and its terminal: read only until Type */
  .ja-head{grid-template-columns:40px minmax(0,1fr)}
  .ja-task,.ja-acts{grid-column:1/-1}
  .ja-acts{flex-wrap:wrap}
  .ja-tabs{overflow-x:auto;scrollbar-width:none}
  .ja-tabs a{min-height:44px;white-space:nowrap}
  .ja-tabs a:last-child{margin-left:0}
  .ja-kbar{display:none}
  .ja-mro{display:flex;align-items:center;gap:8px;padding:6px 6px 6px 12px;background:var(--surface-2);font-size:13px;border-bottom:1px solid var(--term-line)}
  .ja-mro>span{flex:1}
  .ja-term{order:2;min-height:320px;font-size:12px;white-space:pre-wrap;overflow-wrap:anywhere;padding:12px}
  .ja-tin{order:3;display:flex;flex-direction:column;gap:8px;padding:8px;background:var(--surface);border-top:1px solid var(--term-line);position:sticky;bottom:0}
  .ja-tin[hidden]{display:none}
  .ja-tkeys{display:flex;gap:6px;overflow-x:auto;scrollbar-width:none}
  .ja-tkeys .btn{flex:none;font-family:var(--mono)}
  .ja-trow{display:flex;gap:8px}
  .ja-trow input{flex:1;min-width:0;height:44px;border:1px solid var(--line-2);border-radius:10px;padding:0 12px;background:var(--surface-2);color:var(--text);font-family:var(--mono)}
  .ja-pvbar{flex-wrap:wrap}
  .ja-shop{padding:16px}

  /* At scale */
  .jz-one{flex-wrap:wrap}.jz-ol{white-space:normal;flex-basis:100%;padding-left:28px}
  .jz-tline{gap:8px 14px}.jz-big{font-size:18px}
  .jz-trow{gap:4px 8px}
}
`);
