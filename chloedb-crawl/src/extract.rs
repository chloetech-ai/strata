//! A page's fields, from its HTML: what every page has (title, text,
//! headings, language, links, dates), what a site marks up for search
//! engines (JSON-LD, Open Graph, microdata), key-value tables (a product's
//! specifications), fields learned from the site's templates, and the
//! CSS selectors a crawl is given.

use std::collections::HashSet;
use std::sync::LazyLock;

use scraper::{ElementRef, Html, Node, Selector};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use url::Url;

use crate::urls;

/// A field from a CSS selector: the first element's text (or `attr`), or
/// every element's with `multi`. Read from an object or from its text,
/// `name=selector`, `name=selector@attr`, or `name[]=selector` for every
/// match ([`Rule::parse`]).
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(try_from = "RuleIn")]
pub struct Rule {
    pub name: String,
    pub selector: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attr: Option<String>,
    #[serde(default)]
    pub multi: bool,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum RuleIn {
    Text(String),
    Full {
        name: String,
        selector: String,
        #[serde(default)]
        attr: Option<String>,
        #[serde(default)]
        multi: bool,
    },
}

impl TryFrom<RuleIn> for Rule {
    type Error = String;
    fn try_from(r: RuleIn) -> Result<Rule, String> {
        match r {
            RuleIn::Text(t) => Rule::parse(&t),
            RuleIn::Full { name, selector, attr, multi } => Ok(Rule { name, selector, attr, multi }),
        }
    }
}

impl Rule {
    /// `name=selector`, `name=selector@attr`, or `name[]=selector`.
    pub fn parse(r: &str) -> Result<Rule, String> {
        let (name, sel) = r.split_once('=').ok_or_else(|| format!("rule {r:?}: name=selector"))?;
        let (name, multi) = match name.trim().strip_suffix("[]") {
            Some(n) => (n, true),
            None => (name, false),
        };
        let (sel, attr) = match sel.rsplit_once('@') {
            Some((s, a)) if !a.is_empty() && !a.contains(' ') && !a.contains(']') => (s, Some(a.trim().to_string())),
            _ => (sel, None),
        };
        Ok(Rule { name: name.trim().into(), selector: sel.trim().into(), attr, multi })
    }
}

/// A field learned from the site's pages: the one element of a tag and
/// classes (`p.price_color`) that most pages have once, with text that
/// changes from page to page ([`learn_template`]).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TemplateField {
    pub name: String,
    pub selector: String,
    /// Of the sample pages, the share that had it.
    pub share: f64,
}

/// What [`extract`] reads besides every page's own fields.
#[derive(Default)]
pub struct Options<'a> {
    pub rules: &'a [Rule],
    pub template: &'a [TemplateField],
    /// Report the page's class signatures, for [`learn_template`].
    pub learn: bool,
    /// The most characters of body text kept.
    pub max_text: usize,
}

/// A page's fields.
#[derive(Clone, Debug, Default, Serialize)]
pub struct Page {
    pub title: Option<String>,
    pub description: Option<String>,
    pub lang: Option<String>,
    pub canonical: Option<String>,
    pub headings: Vec<String>,
    pub body: String,
    pub words: usize,
    pub published: Option<String>,
    pub modified: Option<String>,
    pub author: Option<String>,
    pub image: Option<String>,
    pub site_name: Option<String>,
    pub kind: Option<String>,
    pub keywords: Vec<String>,
    /// The links to follow (normalised, once each, `nofollow` left out).
    #[serde(skip)]
    pub links: Vec<Url>,
    /// Of `links`, those to the next page of a list (`rel="next"`, or
    /// named "next"): a list's pages are one step from its first, not one
    /// more each, so `max_depth` does not cut a long list short.
    #[serde(skip)]
    pub next: Vec<Url>,
    /// Links on the page, followed or not.
    pub links_out: usize,
    pub noindex: bool,
    pub nofollow: bool,
    /// The marked-up and learned fields, by where they came from: `ld`
    /// (JSON-LD), `og` (Open Graph), `md` (microdata), `specs` (key-value
    /// tables), `page` (learned from the site's templates).
    pub structured: Map<String, Value>,
    /// The crawl's own selectors' fields.
    pub rules: Map<String, Value>,
    /// With `learn`: each class signature on the page, its text and how
    /// many times it is there.
    #[serde(skip)]
    pub signatures: Vec<(String, String, u32)>,
    #[serde(skip)]
    pub content_hash: u64,
}

struct Sels {
    title: Selector,
    meta: Selector,
    canonical: Selector,
    html: Selector,
    a: Selector,
    next: Selector,
    ld: Selector,
    itemprop: Selector,
    tr: Selector,
    cells: Selector,
    dl: Selector,
    headings: Selector,
    main: Selector,
    time: Selector,
    body: Selector,
}

static SELS: LazyLock<Sels> = LazyLock::new(|| {
    let s = |q: &str| Selector::parse(q).expect("a valid selector");
    Sels {
        title: s("title"),
        meta: s("meta"),
        canonical: s("link[rel~=canonical][href]"),
        html: s("html"),
        a: s("a[href], area[href]"),
        next: s("link[rel~=next][href]"),
        ld: s(r#"script[type="application/ld+json"]"#),
        itemprop: s("[itemprop]"),
        tr: s("table tr"),
        cells: s("th, td"),
        dl: s("dl"),
        headings: s("h1, h2, h3"),
        main: s("main, [role=main], article"),
        time: s("time[datetime]"),
        body: s("body"),
    }
});

/// Elements whose text is not the page's: scripts, styles, navigation.
const SKIP: &[&str] = &["script", "style", "noscript", "template", "svg", "nav", "footer", "header", "aside", "form", "iframe", "select", "button", "input", "textarea", "head"];
/// Elements that end a line of text.
const BLOCK: &[&str] = &[
    "p", "div", "li", "ul", "ol", "h1", "h2", "h3", "h4", "h5", "h6", "br", "tr", "table", "section", "article", "main", "blockquote", "pre", "dt", "dd", "dl", "hr", "figcaption", "figure", "td", "th",
];

/// JSON-LD types that describe the site rather than the page.
const SITE_TYPES: &[&str] = &["website", "webpage", "breadcrumblist", "organization", "searchaction", "imageobject", "sitenavigationelement", "listitem", "wpheader", "wpfooter", "wpsidebar", "readaction", "entrypoint", "collectionpage"];

pub fn collapse(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for w in s.split_whitespace() {
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(w);
    }
    out
}

fn cut(mut s: String, n: usize) -> String {
    if s.len() > n {
        let mut i = n;
        while !s.is_char_boundary(i) {
            i -= 1;
        }
        s.truncate(i);
    }
    s
}

fn nonempty(s: String) -> Option<String> {
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

/// The text of an element, with a line break after each block, leaving
/// out the elements of [`SKIP`].
fn text_of(root: ElementRef, max: usize) -> String {
    enum Step<'a> {
        Open(ego_tree::NodeRef<'a, Node>),
        Close,
    }
    let mut out = String::new();
    let mut line = String::new();
    let mut stack = vec![Step::Open(*root)];
    let flush = |line: &mut String, out: &mut String| {
        let l = collapse(line);
        if !l.is_empty() {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(&l);
        }
        line.clear();
    };
    while let Some(step) = stack.pop() {
        if out.len() > max {
            break;
        }
        match step {
            Step::Close => flush(&mut line, &mut out),
            Step::Open(n) => match n.value() {
                Node::Text(t) => {
                    line.push_str(t);
                    line.push(' ');
                }
                Node::Element(e) => {
                    let name = e.name();
                    if SKIP.contains(&name) || e.attr("hidden").is_some() || e.attr("aria-hidden") == Some("true") {
                        continue;
                    }
                    if BLOCK.contains(&name) {
                        stack.push(Step::Close);
                    }
                    let kids: Vec<_> = n.children().collect();
                    for k in kids.into_iter().rev() {
                        stack.push(Step::Open(k));
                    }
                }
                _ => {}
            },
        }
    }
    flush(&mut line, &mut out);
    cut(out, max)
}

/// An element's own text: its text children, not its descendants'.
fn own_text(el: ElementRef) -> String {
    let mut s = String::new();
    for c in el.children() {
        if let Node::Text(t) = c.value() {
            s.push_str(t);
            s.push(' ');
        }
    }
    collapse(&s)
}

/// Whether a link's text names the next page of a list: "Next", "Next
/// page", "Older posts", or an arrow alone.
fn names_next(text: &str) -> bool {
    let t = text.trim();
    if matches!(t, "›" | "»" | "→" | ">" | ">>" | "»»") {
        return true;
    }
    let words: String = t.chars().filter(|c| c.is_alphanumeric() || c.is_whitespace()).collect::<String>().to_lowercase();
    matches!(words.split_whitespace().collect::<Vec<_>>().join(" ").as_str(), "next" | "next page" | "next results" | "older posts" | "older entries")
}

fn all_text(el: ElementRef) -> String {
    collapse(&el.text().collect::<Vec<_>>().join(" "))
}

/// A key for a field: lowercase letters, digits and `_`.
pub fn slug(s: &str) -> String {
    let mut out = String::new();
    let mut under = false;
    for c in s.trim().chars() {
        if c.is_alphanumeric() {
            for l in c.to_lowercase() {
                if l.is_ascii() {
                    out.push(l);
                    under = false;
                }
            }
        } else if !under && !out.is_empty() {
            out.push('_');
            under = true;
        }
    }
    let t = out.trim_end_matches('_');
    cut(t.to_string(), 48)
}

/// The fields of the page at `url` from its HTML.
pub fn extract(html: &str, url: &Url, opts: &Options) -> Page {
    let doc = Html::parse_document(html);
    let s = &*SELS;
    let mut p = Page::default();
    let max_text = if opts.max_text == 0 { 200_000 } else { opts.max_text };

    p.title = doc.select(&s.title).next().map(all_text).and_then(nonempty).map(|t| cut(t, 1000));
    p.lang = doc.select(&s.html).next().and_then(|h| h.value().attr("lang")).map(|l| l.trim().to_ascii_lowercase()).and_then(nonempty);
    p.canonical = doc.select(&s.canonical).next().and_then(|l| l.value().attr("href")).and_then(|h| urls::join(url, h)).map(|u| u.to_string());

    // <meta>: description, robots, Open Graph and the rest.
    let mut og = Map::new();
    for m in doc.select(&s.meta) {
        let v = m.value();
        let key = v.attr("property").or(v.attr("name")).map(|k| k.trim().to_ascii_lowercase());
        let content = v.attr("content").map(collapse).unwrap_or_default();
        let Some(key) = key else { continue };
        if content.is_empty() {
            continue;
        }
        match key.as_str() {
            "description" => p.description = Some(cut(content.clone(), 2000)),
            "keywords" => p.keywords = content.split(',').map(|k| cut(collapse(k), 64)).filter(|k| !k.is_empty()).take(30).collect(),
            "author" => p.author = Some(cut(content.clone(), 200)),
            "robots" => {
                let c = content.to_ascii_lowercase();
                p.noindex |= c.contains("noindex") || c.contains("none");
                p.nofollow |= c.contains("nofollow") || c.contains("none");
            }
            "date" | "dc.date" | "dcterms.date" | "pubdate" | "publish_date" => p.published = p.published.take().or(Some(content.clone())),
            _ => {}
        }
        for prefix in ["og:", "article:", "product:", "book:", "music:", "video:", "profile:"] {
            if let Some(rest) = key.strip_prefix(prefix) {
                let k = if prefix == "og:" { slug(rest) } else { slug(&format!("{}_{}", prefix.trim_end_matches(':'), rest)) };
                if !k.is_empty() && !og.contains_key(&k) {
                    og.insert(k, Value::String(cut(content.clone(), 2000)));
                }
            }
        }
    }
    let ogs = |k: &str| og.get(k).and_then(|v| v.as_str()).map(str::to_string);
    p.description = p.description.take().or_else(|| ogs("description"));
    p.image = ogs("image").and_then(|i| urls::join(url, &i)).map(|u| u.to_string());
    p.site_name = ogs("site_name");
    p.kind = ogs("type");
    p.published = p.published.take().or_else(|| ogs("article_published_time"));
    p.modified = ogs("article_modified_time").or_else(|| ogs("updated_time"));
    p.author = p.author.take().or_else(|| ogs("article_author").filter(|a| !a.starts_with("http")));
    if p.title.is_none() {
        p.title = ogs("title");
    }

    // JSON-LD: the entity the page is about.
    let ld = json_ld(&doc);
    if let Some(Value::Object(e)) = &ld {
        let gets = |k: &str| e.get(k).and_then(scalar_text);
        p.kind = e.get("type").and_then(scalar_text).or(p.kind.take());
        if p.title.is_none() {
            p.title = gets("headline").or_else(|| gets("name"));
        }
        if p.description.is_none() {
            p.description = gets("description").map(|d| cut(d, 2000));
        }
        p.published = p.published.take().or_else(|| gets("datePublished")).or_else(|| gets("dateCreated"));
        p.modified = p.modified.take().or_else(|| gets("dateModified"));
        if p.author.is_none() {
            p.author = e.get("author").and_then(name_of).map(|a| cut(a, 200));
        }
        if p.image.is_none() {
            p.image = e.get("image").and_then(name_of).and_then(|i| urls::join(url, &i)).map(|u| u.to_string());
        }
    }

    // Microdata.
    let md = microdata(&doc, url);
    if p.published.is_none() {
        p.published = md.get("datepublished").and_then(scalar_text);
    }
    if p.published.is_none() {
        p.published = doc.select(&s.time).next().and_then(|t| t.value().attr("datetime")).map(str::to_string);
    }

    // Key-value tables.
    let specs = specs(&doc, s);

    p.headings = doc.select(&s.headings).map(all_text).filter(|t| !t.is_empty()).map(|t| cut(t, 300)).take(40).collect();

    // The page's text: its main part, else its body.
    let main = doc
        .select(&s.main)
        .map(|m| (m, text_of(m, max_text)))
        .max_by_key(|(_, t)| t.len())
        .filter(|(_, t)| t.len() > 200);
    p.body = match main {
        Some((_, t)) => t,
        None => doc.select(&s.body).next().map(|b| text_of(b, max_text)).unwrap_or_default(),
    };
    p.words = p.body.split_whitespace().count();
    p.content_hash = twox_hash::XxHash3_64::oneshot(p.body.as_bytes());

    // Links.
    let mut seen = HashSet::new();
    for a in doc.select(&s.a).chain(doc.select(&s.next)) {
        let Some(href) = a.value().attr("href") else { continue };
        p.links_out += 1;
        let rel = a.value().attr("rel").unwrap_or("").to_ascii_lowercase();
        if rel.split_whitespace().any(|r| r == "nofollow" || r == "ugc" || r == "sponsored") {
            continue;
        }
        if let Some(u) = urls::join(url, href) {
            let next = rel.split_whitespace().any(|r| r == "next") || names_next(&all_text(a)) || a.value().attr("aria-label").is_some_and(names_next);
            if next && !p.next.contains(&u) {
                p.next.push(u.clone());
            }
            if seen.insert(u.as_str().to_string()) {
                p.links.push(u);
            }
        }
    }
    if p.nofollow {
        p.links.clear();
        p.next.clear();
    }

    // The crawl's selectors.
    for r in opts.rules {
        let Ok(sel) = Selector::parse(&r.selector) else { continue };
        let value_of = |el: ElementRef| -> Option<String> {
            let v = match &r.attr {
                Some(a) => {
                    let v = el.value().attr(a)?.trim().to_string();
                    if matches!(a.as_str(), "href" | "src") {
                        urls::join(url, &v).map(|u| u.to_string()).unwrap_or(v)
                    } else {
                        v
                    }
                }
                None => all_text(el),
            };
            nonempty(cut(v, 5000))
        };
        let v = if r.multi {
            let vs: Vec<Value> = doc.select(&sel).filter_map(value_of).take(100).map(Value::String).collect();
            if vs.is_empty() {
                continue;
            }
            Value::Array(vs)
        } else {
            match doc.select(&sel).find_map(value_of) {
                Some(v) => Value::String(v),
                None => continue,
            }
        };
        p.rules.insert(r.name.clone(), v);
    }

    // Fields learned from the site's templates.
    let mut page = Map::new();
    for t in opts.template {
        let Ok(sel) = Selector::parse(&t.selector) else { continue };
        if let Some(v) = doc.select(&sel).map(own_text).find(|t| !t.is_empty()) {
            page.insert(t.name.clone(), Value::String(cut(v, 500)));
        }
    }
    if opts.learn {
        p.signatures = signatures(&doc);
    }

    if let Some(ld) = ld {
        p.structured.insert("ld".into(), ld);
    }
    for (k, v) in [("og", og), ("md", md), ("specs", specs), ("page", page)] {
        if !v.is_empty() {
            p.structured.insert(k.into(), Value::Object(v));
        }
    }
    p
}

/// A value's text when it is a string or a number.
fn scalar_text(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => nonempty(collapse(s)),
        Value::Number(n) => Some(n.to_string()),
        Value::Array(a) => a.first().and_then(scalar_text),
        _ => None,
    }
}

/// A person's or a thing's name: the text, or an object's `name` (or
/// `url`), or the first of a list.
fn name_of(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => nonempty(collapse(s)),
        Value::Object(o) => o.get("name").or(o.get("url")).or(o.get("contentUrl")).and_then(scalar_text),
        Value::Array(a) => a.first().and_then(name_of),
        _ => None,
    }
}

/// The JSON-LD entity the page is about: of every object in its scripts
/// (and their `@graph`s), the first that is not about the site itself (a
/// `WebSite`, a `BreadcrumbList`), cleaned ([`clean`]).
fn json_ld(doc: &Html) -> Option<Value> {
    let mut cands: Vec<Value> = Vec::new();
    for sc in doc.select(&SELS.ld) {
        let raw = sc.text().collect::<String>();
        let raw = raw.trim().trim_start_matches("<!--").trim_end_matches("-->").trim();
        let raw = raw.trim_start_matches("//<![CDATA[").trim_end_matches("//]]>").trim();
        let Ok(v) = serde_json::from_str::<Value>(raw) else { continue };
        let mut stack = vec![v];
        while let Some(v) = stack.pop() {
            match v {
                Value::Array(a) => stack.extend(a.into_iter().rev()),
                Value::Object(mut o) => {
                    if let Some(Value::Array(g)) = o.remove("@graph") {
                        stack.extend(g.into_iter().rev());
                    }
                    if o.contains_key("@type") {
                        cands.push(Value::Object(o));
                    }
                }
                _ => {}
            }
            if cands.len() > 50 {
                break;
            }
        }
    }
    let type_of = |v: &Value| v.get("@type").and_then(scalar_text).unwrap_or_default().to_ascii_lowercase();
    let mut best = cands.iter().find(|v| !SITE_TYPES.contains(&type_of(v).as_str())).cloned();
    // A WebPage about something: the thing.
    if best.is_none() {
        best = cands.iter().find_map(|v| v.get("mainEntity").filter(|m| m.is_object()).cloned());
    }
    best.map(|v| clean(v, 0))
}

/// A JSON-LD object as the table keeps it: `@type` as `type`, the other
/// `@` keys left out, a list of one object as that object, at most 4
/// levels, 20 items a list and 2,000 characters a string.
fn clean(v: Value, depth: usize) -> Value {
    match v {
        Value::Object(o) => {
            if depth >= 4 {
                return o.get("name").cloned().unwrap_or(Value::Null);
            }
            let mut out = Map::new();
            for (k, v) in o {
                let k = match k.as_str() {
                    "@type" => "type".to_string(),
                    k if k.starts_with('@') => continue,
                    k => k.to_string(),
                };
                let v = clean(v, depth + 1);
                if !v.is_null() {
                    out.insert(k, v);
                }
            }
            if out.is_empty() {
                Value::Null
            } else {
                Value::Object(out)
            }
        }
        Value::Array(a) => {
            let mut a: Vec<Value> = a.into_iter().take(20).map(|x| clean(x, depth + 1)).filter(|x| !x.is_null()).collect();
            match a.len() {
                0 => Value::Null,
                1 if a[0].is_object() => a.pop().expect("one"),
                _ => Value::Array(a),
            }
        }
        Value::String(s) => {
            let s = collapse(&s);
            if s.is_empty() {
                Value::Null
            } else {
                Value::String(cut(s, 2000))
            }
        }
        v => v,
    }
}

/// Microdata: every `itemprop`'s value by its name (lowercase), a list
/// when the page has several.
fn microdata(doc: &Html, url: &Url) -> Map<String, Value> {
    let mut out: Map<String, Value> = Map::new();
    for el in doc.select(&SELS.itemprop).take(500) {
        let v = el.value();
        if v.attr("itemscope").is_some() {
            continue;
        }
        let value = match v.name() {
            "meta" => v.attr("content").map(collapse),
            "a" | "link" | "area" => v.attr("href").and_then(|h| urls::join(url, h)).map(|u| u.to_string()),
            "img" | "audio" | "video" | "source" | "embed" | "iframe" | "track" => v.attr("src").and_then(|h| urls::join(url, h)).map(|u| u.to_string()),
            "time" => v.attr("datetime").map(str::to_string).or_else(|| Some(all_text(el))),
            "data" | "meter" => v.attr("value").map(str::to_string),
            _ => v.attr("content").map(collapse).or_else(|| Some(all_text(el))),
        };
        let Some(value) = value.map(|x| cut(x, 1000)).filter(|x| !x.is_empty()) else { continue };
        for prop in v.attr("itemprop").unwrap_or("").split_whitespace() {
            let name = slug(prop.rsplit('/').next().unwrap_or(prop));
            if name.is_empty() {
                continue;
            }
            match out.get_mut(&name) {
                None => {
                    out.insert(name, Value::String(value.clone()));
                }
                Some(Value::Array(a)) if a.len() < 20 => a.push(Value::String(value.clone())),
                Some(Value::Array(_)) => {}
                Some(old) => {
                    let first = old.take();
                    *old = Value::Array(vec![first, Value::String(value.clone())]);
                }
            }
        }
    }
    out
}

/// Key-value tables: rows of a key cell and a value cell, and `<dl>`s.
fn specs(doc: &Html, s: &Sels) -> Map<String, Value> {
    let mut out = Map::new();
    for tr in doc.select(&s.tr) {
        let cells: Vec<ElementRef> = tr.select(&s.cells).collect();
        if cells.len() != 2 || cells[1].value().name() != "td" {
            continue;
        }
        add_spec(&mut out, &all_text(cells[0]), &all_text(cells[1]));
    }
    for dl in doc.select(&s.dl) {
        let mut key: Option<String> = None;
        for c in dl.children().filter_map(ElementRef::wrap) {
            match c.value().name() {
                "dt" => key = Some(all_text(c)),
                "dd" => {
                    if let Some(k) = key.take() {
                        add_spec(&mut out, &k, &all_text(c));
                    }
                }
                _ => {}
            }
        }
    }
    out
}

fn add_spec(out: &mut Map<String, Value>, k: &str, v: &str) {
    if out.len() >= 60 || k.len() > 60 || v.is_empty() || v.len() > 300 {
        return;
    }
    let k = slug(k.trim_end_matches(':'));
    if k.is_empty() || out.contains_key(&k) {
        return;
    }
    out.insert(k, Value::String(v.to_string()));
}

/// Each element with classes and its own short text, as `tag.class.class`
/// (classes sorted), with the text of its first and how many there are.
fn signatures(doc: &Html) -> Vec<(String, String, u32)> {
    let mut found: Vec<(String, String, u32)> = Vec::new();
    let mut index: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    let Some(body) = doc.select(&SELS.body).next() else { return found };
    for el in body.descendants().filter_map(ElementRef::wrap) {
        let v = el.value();
        if SKIP.contains(&v.name()) || el.ancestors().filter_map(ElementRef::wrap).any(|a| SKIP.contains(&a.value().name())) {
            continue;
        }
        let mut classes: Vec<&str> = v.classes().filter(|c| c.chars().all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_') && !c.starts_with(|ch: char| ch.is_ascii_digit())).collect();
        if classes.is_empty() {
            continue;
        }
        classes.sort_unstable();
        let text = own_text(el);
        if text.is_empty() || text.len() > 200 {
            continue;
        }
        let sig = format!("{}.{}", v.name(), classes.join("."));
        match index.get(&sig) {
            Some(&i) => found[i].2 += 1,
            None => {
                if found.len() >= 400 {
                    continue;
                }
                index.insert(sig.clone(), found.len());
                found.push((sig, text, 1));
            }
        }
    }
    found
}

/// Words in a class name that say what a field holds: a signature with
/// one of them names the field.
const MEANING: &[&str] = &[
    "price", "cost", "amount", "title", "name", "author", "byline", "date", "time", "rating", "stars", "score", "brand", "sku", "isbn", "availability", "stock", "category", "tag",
    "genre", "location", "address", "city", "country", "salary", "company", "views", "votes", "likes", "count", "summary", "subtitle",
];

/// The template fields of a site from the class signatures of a sample of
/// its pages: the signatures that at least `min_share` of the pages have
/// at most three times (a list holds one many times; a page holds its own
/// fields once, or a few times with a block of related items), whose text
/// differs from page to page (not a label such as "Add to basket"), and
/// which are not already a page's title. A page's value is the first.
pub fn learn_template(pages: &[(&[(String, String, u32)], Option<&str>)], min_share: f64) -> Vec<TemplateField> {
    use std::collections::HashMap;
    let n = pages.len();
    if n < 3 {
        return Vec::new();
    }
    // signature -> (pages with it once, distinct texts, texts equal to the title)
    let mut stats: HashMap<&str, (usize, HashSet<&str>, usize)> = HashMap::new();
    for (sigs, title) in pages {
        for (sig, text, count) in sigs.iter() {
            if *count > 3 {
                continue;
            }
            let e = stats.entry(sig.as_str()).or_default();
            e.0 += 1;
            e.1.insert(text.as_str());
            if title.is_some_and(|t| t.contains(text.as_str())) {
                e.2 += 1;
            }
        }
    }
    let mut out: Vec<TemplateField> = Vec::new();
    let mut names = HashSet::new();
    let mut cands: Vec<(&str, usize, usize)> = stats.iter().map(|(s, (k, d, t))| (*s, *k, d.len() * 1000 / k.max(&1) + usize::from(*t == 0))).collect();
    cands.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
    for (sig, k, _) in cands {
        let (_, distinct, titles) = &stats[sig];
        let share = k as f64 / n as f64;
        if share < min_share || distinct.len() * 2 < k || *titles * 2 > k {
            continue;
        }
        let mut parts = sig.split('.');
        let tag = parts.next().unwrap_or("");
        let classes: Vec<&str> = parts.collect();
        let named = classes.iter().find(|c| MEANING.iter().any(|m| c.to_ascii_lowercase().contains(m))).or(classes.first()).copied().unwrap_or(tag);
        let mut name = slug(named);
        if name.is_empty() || name.starts_with(|c: char| c.is_ascii_digit()) {
            continue;
        }
        while names.contains(&name) {
            name.push('_');
        }
        names.insert(name.clone());
        out.push(TemplateField { name, selector: sig.to_string(), share });
        if out.len() >= 12 {
            break;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A list's next page, by `rel="next"`, its name or its label, is told
    /// apart from the list's other links (DAP-10).
    #[test]
    fn next_pages_of_a_list() {
        let html = r#"<html><head><link rel="next" href="/l/2"></head><body>
<a href="/item/1">Item one</a> <a href="/l/3">Next &rsaquo;</a> <a href="/l/4" aria-label="Next page">&gt;</a>
<a href="/l/5">»</a> <a href="/l/6">Next steps for your garden</a> <a href="/next-day">delivery</a></body></html>"#;
        let base = Url::parse("https://shop.example/l/1").unwrap();
        let p = extract(html, &base, &Options { rules: &[], template: &[], learn: false, max_text: 10_000 });
        let next: Vec<&str> = p.next.iter().map(|u| u.path()).collect();
        assert_eq!(next, ["/l/3", "/l/4", "/l/5", "/l/2"]);
        assert!(p.links.iter().any(|u| u.path() == "/l/6") && p.links.iter().any(|u| u.path() == "/item/1"));
    }

    const PRODUCT: &str = r#"<!doctype html><html lang="en-GB"><head>
<title>A Light in the Attic | Books</title>
<meta name="description" content="  A book of poems. ">
<meta property="og:type" content="product"><meta property="og:image" content="/img/a.jpg">
<meta property="product:price:amount" content="51.77">
<link rel="canonical" href="https://shop.example/a-light/">
<script type="application/ld+json">{"@context":"https://schema.org","@graph":[
 {"@type":"WebSite","name":"Books"},
 {"@type":"Product","name":"A Light in the Attic","sku":"a897fe39","offers":[{"@type":"Offer","price":"51.77","priceCurrency":"GBP"}],
  "aggregateRating":{"@type":"AggregateRating","ratingValue":3,"reviewCount":0}}]}</script>
</head><body>
<nav><a href="/">Home</a> <a href="/cat/poetry/">Poetry</a></nav>
<main><article><h1>A Light in the Attic</h1>
<p class="price_color">£51.77</p><p class="instock availability">In stock (22 available)</p>
<p>It's hard to imagine a world without A Light in the Attic. This now-classic collection of poetry and drawings
from Shel Silverstein celebrates its 20th anniversary with this special edition. Silverstein's humorous and creative
verse can amuse the dowdiest of readers.</p>
<table><tr><th>UPC</th><td>a897fe39b1053632</td></tr><tr><th>Number of reviews</th><td>0</td></tr></table>
<dl><dt>Pages</dt><dd>176</dd></dl>
<span itemprop="author">Shel Silverstein</span>
<a href="../b/?utm_source=x#top">next</a> <a href="https://ads.example/" rel="sponsored">ad</a>
<script>var x = "not text";</script>
</article></main><footer>Footer text</footer></body></html>"#;

    #[test]
    fn a_product_page() {
        let url = Url::parse("https://shop.example/a-light/index.html").unwrap();
        let rules = vec![Rule { name: "price".into(), selector: ".price_color".into(), attr: None, multi: false }];
        let p = extract(PRODUCT, &url, &Options { rules: &rules, learn: true, ..Default::default() });
        assert_eq!(p.title.as_deref(), Some("A Light in the Attic | Books"));
        assert_eq!(p.description.as_deref(), Some("A book of poems."));
        assert_eq!(p.lang.as_deref(), Some("en-gb"));
        assert_eq!(p.canonical.as_deref(), Some("https://shop.example/a-light/"));
        assert_eq!(p.kind.as_deref(), Some("Product"));
        assert_eq!(p.image.as_deref(), Some("https://shop.example/img/a.jpg"));
        assert_eq!(p.author.as_deref(), None, "microdata's author is in md, not the meta author");
        let ld = &p.structured["ld"];
        assert_eq!(ld["sku"], "a897fe39");
        assert_eq!(ld["offers"]["price"], "51.77", "a list of one offer is the offer");
        assert_eq!(ld["aggregateRating"]["ratingValue"], 3);
        assert_eq!(p.structured["og"]["product_price_amount"], "51.77");
        assert_eq!(p.structured["specs"]["upc"], "a897fe39b1053632");
        assert_eq!(p.structured["specs"]["number_of_reviews"], "0");
        assert_eq!(p.structured["specs"]["pages"], "176");
        assert_eq!(p.structured["md"]["author"], "Shel Silverstein");
        assert_eq!(p.rules["price"], "£51.77");
        assert!(p.body.contains("Silverstein celebrates"));
        assert!(!p.body.contains("not text") && !p.body.contains("Footer"));
        assert!(p.words > 30);
        let links: Vec<&str> = p.links.iter().map(|u| u.as_str()).collect();
        assert!(links.contains(&"https://shop.example/b/"), "{links:?}");
        assert!(!links.iter().any(|l| l.contains("ads.example")));
        assert!(p.signatures.iter().any(|(s, t, n)| s == "p.price_color" && t == "£51.77" && *n == 1));
        assert!(p.signatures.iter().any(|(s, ..)| s == "p.availability.instock"));
    }

    #[test]
    fn learns_template_fields() {
        let sig = |price: &str, title: &str| vec![("p.price_color".to_string(), price.to_string(), 1), ("p.label".to_string(), "Add to basket".to_string(), 1), ("h1.t".to_string(), title.to_string(), 1)];
        let a = sig("£1", "One");
        let b = sig("£2", "Two");
        let c = sig("£3", "Three");
        let pages = vec![(&a[..], Some("One | Shop")), (&b[..], Some("Two | Shop")), (&c[..], Some("Three | Shop"))];
        let t = learn_template(&pages, 0.5);
        assert_eq!(t.len(), 1, "{t:?}");
        assert_eq!(t[0].name, "price_color");
        assert_eq!(t[0].selector, "p.price_color");
        let page = r#"<html><body><p class="price_color">£9.99</p></body></html>"#;
        let p = extract(page, &Url::parse("http://x/").unwrap(), &Options { template: &t, ..Default::default() });
        assert_eq!(p.structured["page"]["price_color"], "£9.99");
    }

    #[test]
    fn robots_meta_and_slugs() {
        let p = extract(r#"<html><head><meta name="robots" content="noindex, nofollow"></head><body><a href="/x">x</a></body></html>"#, &Url::parse("http://x/").unwrap(), &Options::default());
        assert!(p.noindex && p.nofollow && p.links.is_empty());
        assert_eq!(slug(" Price (incl. tax): "), "price_incl_tax");
        assert_eq!(slug("Número de páginas"), "nmero_de_pginas");
    }
}
