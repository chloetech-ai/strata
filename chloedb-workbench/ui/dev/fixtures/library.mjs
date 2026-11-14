// Library and the quick search (SPEC.md, "Fourth build", rows 1 and 10)
// for a demo repo "shop": a Rust API (api/), a React web app (web/) and a
// Python recommender (recs/), with four agents at work. The answers have
// the shapes of the real routes: GET /api/search/code, /api/search/index,
// POST /api/search/reindex, GET /api/library, /api/library/:id/pages,
// POST /api/library/crawl, GET /api/search/docs and /api/search/quick.
import { NOW } from "./base.mjs";

export const SHOP = "/home/user/src/shop";
const WT = "/home/user/.local/share/chloedb-workbench/worktrees/shop-7e1d02aa";
const ago = (min) => NOW - min * 60000;
const iso = (min) => new Date(ago(min)).toISOString();

function agent(id, name, o) {
  return {
    id, name, repo: SHOP, cwd: `${WT}/${name}`, branch: `wb/${name}`, parent: "a7c1a000", depth: 1, model: "sonnet",
    running: true, state: "working", state_since: iso(2), created: iso(50), children: [], base: "4be1f07c", ...o,
  };
}
export const shopAgents = [
  agent("a7c1a000", "lead", { parent: null, depth: 0, task: "Discount codes at checkout, end to end", state: "idle", created: iso(58) }),
  agent("a7c1a001", "checkout", { task: "Apply a discount code in the checkout summary", narration: "Wiring the code field into Summary.tsx." }),
  agent("a7c1a002", "cart", { task: "cart_total takes discounts, rounded per line", narration: "Running cargo test cart_total: 11 of 12 pass." }),
  agent("a7c1a003", "search-ui", { task: "Search box with suggestions as you type", state: "waiting", narration: "Debounce 150 ms or 250 ms?" }),
];
const A = Object.fromEntries(shopAgents.map((a) => [a.name, { id: a.id, name: a.name }]));

// ------------------------------------------------------------------ code

const chunk = (path, start, symbol, kind, lang, text, extra = {}) => {
  const lines = text.replace(/\n$/, "").split("\n");
  return { id: `shop:main:${path}#${start}`, path, start_line: start, end_line: start + lines.length - 1, symbol, kind, lang, text: lines.join("\n"), tree: "main", edited_by: null, overlaid_by: [], ...extra };
};

export const CHUNKS = [
  chunk("api/src/cart/total.rs", 18, "cart_total", "fn", "rust", `/// The cart's total in cents: each line's price times its quantity, less
/// the line's discount rounded half up, then the cart-wide discount.
pub fn cart_total(cart: &Cart, discounts: &[Discount]) -> Cents {
    let mut total: Cents = 0;
    for line in &cart.lines {
        let gross = line.unit_price * line.quantity as i64;
        let off = discounts.iter().map(|d| d.line_off(line)).max().unwrap_or(0);
        total += gross - round_half_up(off);
    }
    let cart_off = discounts.iter().filter(|d| d.scope == Scope::Cart).map(|d| d.amount(total)).sum::<Cents>();
    (total - cart_off).max(0)
}`, { overlaid_by: [A.cart] }),
  chunk("web/src/cart/useCart.ts", 41, "cartTotal", "function", "typescript", `export function cartTotal(lines: CartLine[], code?: DiscountCode): number {
  // Mirrors api/src/cart/total.rs: per-line discounts, then the cart's.
  let total = 0;
  for (const line of lines) {
    const off = code?.scope === "line" ? lineOff(code, line) : 0;
    total += line.unitPrice * line.quantity - Math.round(off);
  }
  if (code?.scope === "cart") total -= cartOff(code, total);
  return Math.max(0, total);
}`, { tree: "a7c1a001", edited_by: A.checkout }),
  chunk("api/src/checkout/session.rs", 88, "create_session", "fn", "rust", `pub async fn create_session(db: &Db, cart_id: CartId, code: Option<&str>) -> Result<Session> {
    let cart = db.cart(cart_id).await?;
    let discounts = match code {
        Some(c) => vec![db.discount_by_code(c).await?.ok_or(Error::UnknownCode)?],
        None => vec![],
    };
    let amount = cart_total(&cart, &discounts);
    let intent = stripe::PaymentIntent::create(&db.stripe, amount, cart.currency, idempotency_key(&cart)).await?;
    Ok(Session { cart_id, amount, intent: intent.id, discounts })
}`),
  chunk("web/src/checkout/Summary.tsx", 12, "Summary", "function", "typescript", `export function Summary({ cart }: { cart: Cart }) {
  const [code, setCode] = useState<DiscountCode | undefined>();
  const total = cartTotal(cart.lines, code);
  return (
    <section className="summary">
      <DiscountField onApply={setCode} />
      <Row label="Subtotal" value={money(cart.subtotal)} />
      {code && <Row label={\`Code \${code.text}\`} value={"−" + money(cart.subtotal - total)} />}
      <Row label="Total" value={money(total)} strong />
    </section>
  );
}`, { tree: "a7c1a001", edited_by: A.checkout }),
  chunk("api/tests/cart_total.rs", 1, null, "head", "rust", `use shop_api::cart::{cart_total, Cart, Discount, Scope};

#[test]
fn a_line_discount_rounds_half_up() {
    let cart = Cart::of(&[(1999, 3)]);
    let ten = Discount::percent(10, Scope::Line);
    assert_eq!(cart_total(&cart, &[ten]), 5397);
}`),
  chunk("api/src/cart/discount.rs", 10, "apply_discount", "fn", "rust", `pub fn apply_discount(cart: &mut Cart, code: &DiscountCode, now: Timestamp) -> Result<(), DiscountError> {
    if code.expires_at.is_some_and(|t| t < now) {
        return Err(DiscountError::Expired);
    }
    if code.min_total.is_some_and(|m| cart_total(cart, &[]) < m) {
        return Err(DiscountError::BelowMinimum);
    }
    cart.discounts.push(code.to_discount());
    Ok(())
}`),
  chunk("docs/pricing.md", 1, "How totals are rounded", "heading", "markdown", `## How totals are rounded

Every amount is integer cents. A line's discount is rounded half up on its
own line, before the cart-wide discount, so the receipt's lines add up to
the total the card is charged (cart_total in api/src/cart/total.rs, and
cartTotal in web/src/cart/useCart.ts, which must agree).`),
  chunk("recs/shop_recs/rank.py", 30, "rerank_for_cart", "def", "python", `def rerank_for_cart(items: list[Item], cart: Cart) -> list[Item]:
    """Puts items that would lift the cart over a discount's minimum first."""
    gap = max(0, cart.next_threshold() - cart_total_cents(cart))
    return sorted(items, key=lambda i: (abs(i.price_cents - gap), -i.score))`),
];

const WORDS = (q) => String(q || "").replace(/([a-z])([A-Z])/g, "$1 $2").toLowerCase().split(/[^a-z0-9]+/).filter((w) => w.length > 1);
function score(c, q) {
  const words = WORDS(q);
  if (!words.length) return 0;
  const hay = (c.path + " " + (c.symbol || "") + " " + c.text).replace(/([a-z])([A-Z])/g, "$1 $2").toLowerCase();
  let s = 0;
  for (const w of words) {
    const n = hay.split(w).length - 1;
    if (n) s += 1.4 + Math.log(1 + n) * 2.1 + ((c.symbol || "").toLowerCase().includes(w) ? 3.2 : 0);
  }
  return Math.round(s * 100) / 100;
}

const LANGS = [
  { lang: "rust", files: 64, chunks: 512 }, { lang: "typescript", files: 81, chunks: 436 }, { lang: "python", files: 23, chunks: 141 },
  { lang: "markdown", files: 14, chunks: 88 }, { lang: "sql", files: 9, chunks: 31 }, { lang: "other", files: 19, chunks: 76 },
];
export const INDEX = {
  state: "ready", files: 210, chunks: 1284, langs: LANGS, indexed_ms: ago(14), took_ms: 820, head: "4be1f07c", progress: null, error: null,
  overlays: [
    { agent: A.checkout, files: ["web/src/checkout/Summary.tsx", "web/src/cart/useCart.ts", "web/src/checkout/DiscountField.tsx"] },
    { agent: A.cart, files: ["api/src/cart/total.rs", "api/src/cart/discount.rs", "api/tests/cart_total.rs"] },
    { agent: A["search-ui"], files: ["web/src/search/SearchBox.tsx"] },
  ],
};

export const RECENT = [
  { ts_ms: ago(0.4), path: "api/src/cart/total.rs", tree: A.cart.id, agent: A.cart, chunks: 6, why: "edit" },
  { ts_ms: ago(1.2), path: "web/src/checkout/Summary.tsx", tree: A.checkout.id, agent: A.checkout, chunks: 4, why: "edit" },
  { ts_ms: ago(2.6), path: "web/src/checkout/DiscountField.tsx", tree: A.checkout.id, agent: A.checkout, chunks: 3, why: "edit" },
  { ts_ms: ago(4.1), path: "web/src/search/SearchBox.tsx", tree: A["search-ui"].id, agent: A["search-ui"], chunks: 5, why: "edit" },
  { ts_ms: ago(9.0), path: "api/tests/cart_total.rs", tree: A.cart.id, agent: A.cart, chunks: 9, why: "edit" },
  { ts_ms: ago(14), path: "210 files", tree: "main", agent: null, chunks: 1284, why: "full" },
];

export const SEARCHES = [
  { ts_ms: ago(0.8), agent: A.cart, kind: "code", q: "round_half_up", hits: 4, tokens: 0, cites: ["api/src/money.rs:12-30"] },
  { ts_ms: ago(1.5), agent: A.checkout, kind: "code", q: "cartTotal discount", hits: 8, tokens: 2000, cites: ["web/src/cart/useCart.ts:41-50", "api/src/cart/total.rs:18-29"] },
  { ts_ms: ago(3.2), agent: A.checkout, kind: "docs", q: "PaymentIntent amount update after creation", hits: 5, tokens: 1500, cites: ["https://docs.rs/async-stripe/0.39.1/stripe/struct.PaymentIntent.html"] },
  { ts_ms: ago(6.0), agent: A["search-ui"], kind: "code", q: "useDebounce", hits: 0, tokens: 0, cites: [] },
  { ts_ms: ago(6.4), agent: A["search-ui"], kind: "docs", q: "useDeferredValue vs debounce", hits: 4, tokens: 1200, cites: ["https://react.dev/reference/react/useDeferredValue"] },
  { ts_ms: ago(11), agent: A.cart, kind: "code", q: "Discount scope Cart Line", hits: 7, tokens: 0, cites: ["api/src/cart/discount.rs:1-9"] },
];

function hitsFor(q, agentId, lang) {
  let list = CHUNKS.filter((c) => !agentId ? c.tree === "main" || !CHUNKS.some((x) => x.tree === "main" && x.path === c.path) : c.tree === "main" || c.tree === agentId)
    .map((c) => ({ ...c, score: score(c, q) })).filter((c) => c.score > 0);
  if (!agentId) list = list.map((c) => (c.tree !== "main" ? { ...c, tree: "main", edited_by: null, overlaid_by: [c.edited_by] } : c));
  if (lang) list = list.filter((c) => c.lang === lang);
  return list.sort((a, b) => b.score - a.score);
}

const toks = (s) => Math.ceil(s.length / 3.6);
function packOf(hits, budget) {
  let used = 0;
  const rows = [], parts = [];
  for (const c of hits) {
    const head = `// ${c.path}:${c.start_line}-${c.end_line}${c.symbol ? ` ${c.kind} ${c.symbol}` : ""}`;
    let body = c.text, decision = "take", t = toks(head + body);
    if (used + t > budget) {
      const room = budget - used;
      if (room < 40) { rows.push({ path: c.path, start_line: c.start_line, end_line: c.end_line, symbol: c.symbol, decision: "skip: over budget", tokens: 0 }); continue; }
      body = c.text.slice(0, Math.floor(room * 3.6) - head.length - 4) + " …";
      decision = "excerpt";
      t = toks(head + body);
    }
    used += t;
    rows.push({ path: c.path, start_line: c.start_line, end_line: c.end_line, symbol: c.symbol, decision, tokens: t });
    parts.push(`${head}\n${body}`);
  }
  return { budget, used, text: parts.join("\n\n"), rows };
}

// ------------------------------------------------------------------ docs

const lib = (id, name, version, ecosystem, manifest, url, o) => ({ id, name, version, ecosystem, manifest, url, state: "none", pages: 0, error: null, crawled_ms: null, cited: 0, cited_by: [], progress: null, ...o });
export const LIBS = [
  lib("l-async-stripe", "async-stripe", "0.39.1", "crates", "api/Cargo.toml", "https://docs.rs/async-stripe/0.39.1/stripe/", { state: "done", pages: 64, crawled_ms: ago(130), cited: 9, cited_by: [A.checkout, A.cart] }),
  lib("l-react", "react", "19.0.0", "npm", "web/package.json", "https://react.dev/reference/react", { state: "done", pages: 48, crawled_ms: ago(300), cited: 6, cited_by: [A["search-ui"], A.checkout] }),
  lib("l-axum", "axum", "0.7.9", "crates", "api/Cargo.toml", "https://docs.rs/axum/0.7.9/axum/", { state: "done", pages: 112, crawled_ms: ago(60 * 26), cited: 2, cited_by: [A.cart] }),
  lib("l-sqlx", "sqlx", "0.8.2", "crates", "api/Cargo.toml", "https://docs.rs/sqlx/0.8.2/sqlx/", { state: "crawling", pages: 37, progress: { done: 37, total: 140 } }),
  lib("l-zod", "zod", "3.23.8", "npm", "web/package.json", "https://zod.dev/", { state: "done", pages: 19, crawled_ms: ago(60 * 30), cited: 1, cited_by: [A.checkout] }),
  lib("l-tanstack", "@tanstack/react-query", "5.59.0", "npm", "web/package.json", "https://tanstack.com/query/latest/docs/framework/react/overview", { state: "failed", error: "robots.txt disallows /query/latest/docs for every agent" }),
  lib("l-fastapi", "fastapi", "0.115.4", "pypi", "recs/pyproject.toml", "https://fastapi.tiangolo.com/", { state: "done", pages: 88, crawled_ms: ago(60 * 50) }),
  lib("l-serde", "serde", "1.0.214", "crates", "api/Cargo.toml", "https://docs.rs/serde/1.0.214/serde/"),
  lib("l-tokio", "tokio", "1.41.0", "crates", "api/Cargo.toml", "https://docs.rs/tokio/1.41.0/tokio/"),
  lib("l-numpy", "numpy", "2.1.3", "pypi", "recs/pyproject.toml", "https://numpy.org/doc/stable/"),
  lib("l-next", "next", "15.0.3", "npm", "web/package.json", "https://nextjs.org/docs"),
];

const page = (url, title, words, cited_by = [], text = "") => ({ url, title, words, depth: url.endsWith("/stripe/") ? 0 : 1, fetched_ms: ago(130), cited: cited_by.length, cited_by, text });
const cite = (agent, min, q) => ({ agent, ts_ms: ago(min), q });
export const PAGES = {
  "l-async-stripe": [
    page("https://docs.rs/async-stripe/0.39.1/stripe/struct.PaymentIntent.html", "PaymentIntent in stripe", 4210, [cite(A.checkout, 3.2, "PaymentIntent amount update after creation"), cite(A.checkout, 41, "create payment intent idempotency")],
      "A PaymentIntent guides you through the process of collecting a payment from your customer. … pub fn update(client: &Client, id: &PaymentIntentId, params: UpdatePaymentIntent<'_>) -> Response<PaymentIntent> Updates properties on a PaymentIntent object without confirming. Depending on which properties you update, you might need to confirm the PaymentIntent again. For example, updating the amount …"),
    page("https://docs.rs/async-stripe/0.39.1/stripe/struct.CreatePaymentIntent.html", "CreatePaymentIntent in stripe", 2980, [cite(A.cart, 22, "amount currency minor units")],
      "The parameters for PaymentIntent::create. amount: i64 — Amount intended to be collected by this PaymentIntent. A positive integer representing how much to charge in the smallest currency unit (e.g., 100 cents to charge $1.00 or 100 to charge ¥100, a zero-decimal currency) …"),
    page("https://docs.rs/async-stripe/0.39.1/stripe/struct.RequestStrategy.html", "RequestStrategy in stripe", 640, [cite(A.checkout, 39, "idempotency key retry")],
      "pub enum RequestStrategy { Once, Idempotent(String), Retry(u32), ExponentialBackoff(u32) } Idempotent: run the request once with an idempotency key; a retried request with the same key returns the first answer …"),
    page("https://docs.rs/async-stripe/0.39.1/stripe/struct.Coupon.html", "Coupon in stripe", 1810, [cite(A.cart, 47, "percent_off amount_off coupon")], "A coupon contains information about a percent-off or amount-off discount you might want to apply to a customer …"),
    page("https://docs.rs/async-stripe/0.39.1/stripe/struct.PromotionCode.html", "PromotionCode in stripe", 1320, [], "A Promotion Code represents a customer-redeemable code for a coupon …"),
    page("https://docs.rs/async-stripe/0.39.1/stripe/struct.Client.html", "Client in stripe", 980, [], "pub fn new(secret_key: impl Into<String>) -> Self …"),
    page("https://docs.rs/async-stripe/0.39.1/stripe/enum.Currency.html", "Currency in stripe", 2400, [], "Currency is represented as a three-letter ISO currency code, in lowercase …"),
    page("https://docs.rs/async-stripe/0.39.1/stripe/struct.Refund.html", "Refund in stripe", 1450, [], "Refund objects allow you to refund a charge that has previously been created …"),
    page("https://docs.rs/async-stripe/0.39.1/stripe/", "stripe - Rust", 1210, [], "async-stripe: a Rust client for the Stripe API …"),
  ],
  "l-react": [
    page("https://react.dev/reference/react/useDeferredValue", "useDeferredValue – React", 2350, [cite(A["search-ui"], 6.4, "useDeferredValue vs debounce")], "useDeferredValue is a React Hook that lets you defer updating a part of the UI. … Deferring a value is different from debouncing and throttling …"),
    page("https://react.dev/reference/react/useTransition", "useTransition – React", 2900, [cite(A["search-ui"], 12, "keep input responsive while filtering")], "useTransition is a React Hook that lets you render a part of the UI in the background …"),
    page("https://react.dev/reference/react/useState", "useState – React", 3100, [cite(A.checkout, 33, "state from props reset when key changes")], "useState is a React Hook that lets you add a state variable to your component …"),
    page("https://react.dev/reference/react/useMemo", "useMemo – React", 2600, [], "useMemo is a React Hook that lets you cache the result of a calculation between re-renders …"),
  ],
};
export const CITATIONS = [
  { ts_ms: ago(3.2), agent: A.checkout, q: "PaymentIntent amount update after creation", url: PAGES["l-async-stripe"][0].url, title: "PaymentIntent in stripe", library: "async-stripe" },
  { ts_ms: ago(6.4), agent: A["search-ui"], q: "useDeferredValue vs debounce", url: PAGES["l-react"][0].url, title: "useDeferredValue – React", library: "react" },
  { ts_ms: ago(12), agent: A["search-ui"], q: "keep input responsive while filtering", url: PAGES["l-react"][1].url, title: "useTransition – React", library: "react" },
  { ts_ms: ago(22), agent: A.cart, q: "amount currency minor units", url: PAGES["l-async-stripe"][1].url, title: "CreatePaymentIntent in stripe", library: "async-stripe" },
  { ts_ms: ago(33), agent: A.checkout, q: "state from props reset when key changes", url: PAGES["l-react"][2].url, title: "useState – React", library: "react" },
  { ts_ms: ago(39), agent: A.checkout, q: "idempotency key retry", url: PAGES["l-async-stripe"][2].url, title: "RequestStrategy in stripe", library: "async-stripe" },
];

function docHits(q, library) {
  const words = WORDS(q);
  const out = [];
  for (const [lid, pages] of Object.entries(PAGES)) {
    if (library && library !== lid) continue;
    const l = LIBS.find((x) => x.id === lid);
    for (const p of pages) {
      const hay = (p.title + " " + p.text).toLowerCase();
      const s = words.reduce((acc, w) => acc + (hay.includes(w) ? 2 + (p.title.toLowerCase().includes(w) ? 3 : 0) : 0), 0);
      if (s > 0) out.push({ url: p.url, title: p.title, library: { id: lid, name: l.name }, text: p.text.slice(0, 280), score: s });
    }
  }
  return out.sort((a, b) => b.score - a.score);
}

// ------------------------------------------------------------- decisions

export const DECISIONS = [
  { id: "m5120", text: "Money is integer cents end to end; a line's discount rounds half up on its own line, before the cart's.", ts: iso(36), agent: A.cart, files: ["api/src/cart/total.rs"] },
  { id: "m5133", text: "cartTotal in the web app mirrors cart_total in the API; a test pins them to the same cents on 40 carts.", ts: iso(21), agent: A.checkout, files: ["web/src/cart/useCart.ts"] },
  { id: "m4980", text: "Discount codes are case-insensitive and stored upper-case.", ts: iso(60 * 26), agent: null, files: ["api/src/cart/discount.rs"] },
  { id: "m5141", text: "Search suggestions debounce at 150 ms, as you chose: 250 felt laggy on the demo.", ts: iso(4), agent: A["search-ui"], files: ["web/src/search/SearchBox.tsx"] },
  { id: "m4702", text: "Every Stripe call carries an idempotency key made from the cart id and its version.", ts: iso(60 * 50), agent: null, files: ["api/src/checkout/session.rs"] },
];

// ---------------------------------------------------------------- routes

const crawling = new Map();
export const routes = [
  ["GET", /^\/api\/search\/code$/, (_, url) => {
    const q = url.searchParams.get("q") || "";
    const agentId = url.searchParams.get("agent") || null;
    const tokens = Number(url.searchParams.get("tokens") || 0);
    const limit = Number(url.searchParams.get("limit") || 20);
    const hits = hitsFor(q, agentId, url.searchParams.get("lang")).slice(0, limit);
    return {
      repo: SHOP, q, agent: agentId ? shopAgents.find((a) => a.id === agentId) || null : null, hits,
      pack: tokens ? packOf(hits, tokens) : null, index: INDEX, took_ms: 3 + hits.length % 4, missing: [],
    };
  }],
  ["GET", /^\/api\/search\/index$/, () => ({ repo: SHOP, index: INDEX, recent: RECENT, searches: SEARCHES, agents: shopAgents, missing: [] })],
  ["POST", /^\/api\/search\/reindex$/, () => ({ index: { ...INDEX, indexed_ms: Date.now(), took_ms: 640 }, changed: 3, removed: 0, took_ms: 640 })],
  ["GET", /^\/api\/library$/, () => ({
    repo: SHOP, libraries: LIBS.map((l) => (crawling.has(l.id) ? { ...l, state: "crawling", progress: { done: Math.min(200, Math.round((Date.now() - crawling.get(l.id)) / 400)), total: 200 } } : l)),
    crawler: { kind: "built-in", available: true, via: "curl 8.5.0", note: "same host and folder, depth 1, at most 200 pages, robots.txt respected", chloedb_crawl: false },
    citations: CITATIONS, manifests: ["api/Cargo.toml", "web/package.json", "recs/pyproject.toml"], missing: [],
  })],
  ["GET", /^\/api\/library\/([^/]+)\/pages$/, (m) => {
    const l = LIBS.find((x) => x.id === decodeURIComponent(m[1]));
    if (!l) return [404, { error: "no such library", error_code: "not_found" }];
    return { library: l, pages: (PAGES[l.id] || []).map(({ text, ...p }) => p) };
  }],
  ["POST", /^\/api\/library\/crawl$/, (_, __, body) => {
    const l = LIBS.find((x) => x.id === body?.library) || (body?.url ? lib("l-custom", new URL(body.url).host, null, "site", null, body.url) : null);
    if (!l) return [400, { error: "library or url is required", error_code: "bad_request" }];
    crawling.set(l.id, Date.now());
    return [202, { library: { ...l, state: "queued" }, started: 1 }];
  }],
  ["GET", /^\/api\/search\/docs$/, (_, url) => {
    const hits = docHits(url.searchParams.get("q"), url.searchParams.get("library")).slice(0, 10);
    return { repo: SHOP, q: url.searchParams.get("q"), hits, pack: null, missing: [] };
  }],
  ["GET", /^\/api\/search\/quick$/, (_, url) => {
    const q = url.searchParams.get("q") || "";
    const words = WORDS(q);
    const decisions = DECISIONS.map((d) => ({ ...d, score: words.reduce((s, w) => s + (d.text.replace(/([a-z])([A-Z])/g, "$1 $2").toLowerCase().includes(w) ? 1 : 0), 0) }))
      .filter((d) => d.score > 0).sort((a, b) => b.score - a.score).slice(0, 4);
    return { repo: SHOP, q, code: hitsFor(q).slice(0, 5), docs: docHits(q).slice(0, 4), decisions, missing: [] };
  }],
];
