// The `browser` surface: the agent uses the command center in Chromium, as
// a person would, through Playwright. It reads the page as its
// accessibility tree (what a screen reader gets: roles, names, values), in
// which every element it can act on carries a reference (`e12`), and acts by
// reference: click, type, choose, press a key, scroll, go back. Each action
// answers with the page as it is after it, so a step is one call.
//
// The surface also keeps what a person would not see but the makers want
// to: errors in the console, uncaught exceptions, and API calls that failed,
// counted per step.

import { createRequire } from 'node:module';

const MAX_SNAPSHOT = 16000;

// Playwright is installed with the browser it drives, so it is loaded from
// wherever the machine has it (NODE_PATH included) rather than pinned here.
let pw = null;
export function playwright() {
  if (pw) return pw;
  const req = createRequire(import.meta.url);
  try { pw = req('playwright'); } catch {
    throw new Error('the browser surface needs Playwright and Chromium (npm i -g playwright && npx playwright install chromium)');
  }
  return pw;
}

let shared = null;
/** One Chromium per process; each session gets a context of its own. */
export async function browser() {
  if (!shared) {
    const { chromium } = playwright();
    shared = chromium.launch(process.env.CHLOEDB_SWARM_CHROMIUM ? { executablePath: process.env.CHLOEDB_SWARM_CHROMIUM } : {});
  }
  return shared;
}
export async function closeBrowser() {
  if (shared) { const b = await shared; shared = null; await b.close(); }
}

const NOTE = { note: { type: 'string', description: 'What you are doing now, in a few words, as you would say it aloud.' } };
const tool = (name, description, props = {}, required = []) => ({
  name, description, input_schema: { type: 'object', properties: { ...NOTE, ...props }, required: ['note', ...required] },
});
const ref = { ref: { type: 'string', description: 'The element\'s reference from the page, such as e12.' } };

export const TOOLS = [
  tool('look', 'Read the page as it is now, or one part of it whole: a long page is cut, and scrolling does not change what you read.', { part: { type: 'string', description: 'Optional: the reference of a part to read whole, such as a table, a list or a dialog (e40).' } }),
  tool('click', 'Click an element.', ref, ['ref']),
  tool('type', 'Type into a text field, replacing what is in it.', { ...ref, text: { type: 'string' }, submit: { type: 'boolean', description: 'Press Enter afterwards.' } }, ['ref', 'text']),
  tool('choose', 'Choose an option of a select element.', { ...ref, option: { type: 'string', description: 'The option\'s label or value.' } }, ['ref', 'option']),
  tool('press', 'Press a key or a combination, such as Enter, Escape, Tab or Control+a.', { key: { type: 'string' } }, ['key']),
  tool('scroll', 'Scroll the page.', { direction: { type: 'string', enum: ['up', 'down'] } }, ['direction']),
  tool('back', 'Go back to the previous page.'),
  tool('go', 'Open a page of this site by its path, such as / or /#/tables.', { path: { type: 'string' } }, ['path']),
  tool('wait', 'Wait for the page to finish something, up to 10 seconds.', { seconds: { type: 'number' } }, ['seconds']),
];

/**
 * A failed action in the words a person would use, from Playwright's
 * message and call log: the reviewer needs to tell "a dialog was in front
 * of it" (the person's mistake, or a modal that traps) from a dead control.
 */
export function why(message) {
  const m = String(message);
  const first = m.split('\n')[0].replace(/aria-ref=/g, 'ref ');
  if (/intercepts pointer events/.test(m)) {
    const tag = m.match(/<(\w+)([^>]*)>[^\n]*intercepts pointer events/);
    const cls = tag?.[2].match(/class="([^"]*)"/)?.[1];
    return `that element is covered by another one in front of it${tag ? ` (a <${tag[1]}${cls ? ` class="${cls.slice(0, 60)}"` : ''}>, such as a dialog or an overlay)` : ''}, so it could not be clicked`;
  }
  if (/element is not visible/.test(m)) return 'that element is not visible on the page now';
  if (/element is not enabled|disabled/.test(m) && /Timeout/.test(first)) return 'that element is disabled';
  if (/not attached to the DOM|detached/.test(m)) return 'that element is gone: the page changed; look again';
  if (/waiting for locator\('aria-ref=/.test(m) && /Timeout/.test(first)) return 'no element has that reference on the page now; look again';
  if (/Element is not an? (<input>|<select>|input|textarea|select)/i.test(m)) return 'that element does not take typing or a choice';
  return first;
}

/** The lines of an accessibility tree that are the element `ref` and what it holds. */
export function subtree(tree, ref) {
  const r = String(ref || '').replace(/^\[?ref=/, '').replace(/\]$/, '');
  const lines = tree.split('\n');
  const i = lines.findIndex((l) => l.includes(`[ref=${r}]`));
  if (i < 0) return null;
  const depth = lines[i].search(/\S/);
  let j = i + 1;
  while (j < lines.length && (lines[j].trim() === '' || lines[j].search(/\S/) > depth)) j++;
  return lines.slice(i, j).join('\n');
}

/**
 * A page as the agent reads it, at most `max` characters. An open dialog
 * comes first, whole: it is what a person looks at, and it is the last
 * thing in the page's tree, where a cut would drop it.
 */
export function fit(tree, max = MAX_SNAPSHOT) {
  const cut = (t, n) => (t.length > n ? `${t.slice(0, Math.max(0, n))}\n… (the rest is cut: look with a part, the reference of a table, a list or a section, to read it whole)` : t);
  const open = [...tree.matchAll(/^\s*- (?:dialog|alertdialog)\b[^\n]*\[ref=(e\d+)\]/gm)].at(-1);
  if (!open || tree.length <= max) return cut(tree, max);
  const dialog = subtree(tree, open[1]);
  return `A dialog is open:\n${cut(dialog, max)}\n\nThe page behind it:\n${cut(tree, max - dialog.length)}`;
}

export class BrowserSurface {
  /**
   * `url`: the server; `key`: the session's API key (passed to the command
   * center as ?token=, which keeps it for the tab only); `entry`: the hash
   * to open, such as #/tables; `device`: 'desktop' or 'phone'.
   */
  constructor({ url, key, headers = {}, entry = '', device = 'desktop' }) {
    Object.assign(this, { url: url.replace(/\/$/, ''), key, headers, entry, device });
    this.counts = { console: 0, pageErrors: 0, http4xx: 0, http5xx: 0 };
    this.recent = [];
  }

  async open() {
    const b = await browser();
    const { devices } = playwright();
    const opts = this.device === 'phone' ? { ...devices['iPhone 13'] } : { viewport: { width: 1366, height: 850 } };
    delete opts.defaultBrowserType;
    this.ctx = await b.newContext({ ...opts, extraHTTPHeaders: this.headers });
    this.page = await this.ctx.newPage();
    this.page.setDefaultTimeout(8000);
    const note = (kind, text) => { this.recent.push({ kind, text: String(text).slice(0, 300) }); };
    this.page.on('console', (m) => { if (m.type() === 'error') { this.counts.console++; note('console', m.text()); } });
    this.page.on('pageerror', (e) => { this.counts.pageErrors++; note('exception', e.message); });
    this.page.on('response', (r) => {
      const u = r.url();
      if (!u.startsWith(this.url)) return;
      const s = r.status();
      if (s >= 500) { this.counts.http5xx++; note('http', `${s} ${r.request().method()} ${u.slice(this.url.length)}`); } else if (s >= 400) { this.counts.http4xx++; note('http', `${s} ${r.request().method()} ${u.slice(this.url.length)}`); }
    });
    return this;
  }

  toolDefs() { return TOOLS; }

  has(name) { return TOOLS.some((t) => t.name === name); }

  /** The page the agent first sees. */
  async start() {
    await this.page.goto(`${this.url}/?token=${encodeURIComponent(this.key)}${this.entry || ''}`, { waitUntil: 'domcontentloaded' });
    await this.settle();
    return `You opened ChloeDB's command center in a browser.\n\n${await this.snapshot()}`;
  }

  async settle() {
    try { await this.page.waitForLoadState('networkidle', { timeout: 3000 }); } catch { /* a page that polls never idles */ }
    await this.page.waitForTimeout(250);
  }

  async snapshot(part) {
    let tree;
    try { tree = await this.page.ariaSnapshot({ mode: 'ai' }); } catch (e) { tree = `(the page could not be read: ${e.message.split('\n')[0]})`; }
    if (part) {
      const sub = subtree(tree, part);
      tree = sub ? fit(sub) : `(no element has the reference ${part} now; the whole page follows)\n${fit(tree)}`;
    } else tree = fit(tree);
    const u = this.page.url().replace(/([?&])token=[^&#]*/, '$1token=…');
    return `Page: ${await this.page.title().catch(() => '')} (${u})\n${tree}`;
  }

  el(r) { return this.page.locator(`aria-ref=${String(r).replace(/^\[?ref=/, '').replace(/\]$/, '')}`); }

  /** One action; answers like McpSurface.call, with the page after it. */
  async call(name, input = {}) {
    this.recent = [];
    let error = null;
    try {
      switch (name) {
        case 'look': break;
        case 'click': await this.el(input.ref).click(); break;
        case 'type': await this.el(input.ref).fill(String(input.text ?? '')); if (input.submit) await this.el(input.ref).press('Enter'); break;
        case 'choose': await this.el(input.ref).selectOption({ label: input.option }).catch(() => this.el(input.ref).selectOption(input.option)); break;
        case 'press': await this.page.keyboard.press(input.key); break;
        case 'scroll': await this.page.mouse.wheel(0, input.direction === 'up' ? -700 : 700); break;
        case 'back': await this.page.goBack({ waitUntil: 'domcontentloaded' }); break;
        case 'go': {
          const p = String(input.path || '/');
          if (/^[a-z]+:/i.test(p) && !p.startsWith(this.url)) throw new Error('only pages of this site can be opened');
          await this.page.goto(p.startsWith(this.url) ? p : this.url + (p.startsWith('/') ? p : `/${p}`), { waitUntil: 'domcontentloaded' });
          break;
        }
        case 'wait': await this.page.waitForTimeout(Math.min(10, Math.max(0, Number(input.seconds) || 1)) * 1000); break;
        default: throw new Error(`no such action: ${name}`);
      }
    } catch (e) {
      error = why(e.message);
    }
    if (name !== 'wait') await this.settle();
    const page = await this.snapshot(name === 'look' ? input.part : undefined);
    const seen = this.recent.length ? `\n\nThe browser also showed: ${this.recent.map((r) => `${r.kind}: ${r.text}`).join(' | ')}` : '';
    return { text: `${error ? `That did not work: ${error}\n\n` : ''}${page}`, ok: !error, error, url: this.page.url().replace(/([?&])token=[^&#]*/, '$1token=…'), events: this.recent.slice(), note: seen };
  }

  async screenshot(path) {
    try { await this.page.screenshot({ path, fullPage: false }); return path; } catch { return null; }
  }

  async close() { await this.ctx?.close().catch(() => {}); }
}
