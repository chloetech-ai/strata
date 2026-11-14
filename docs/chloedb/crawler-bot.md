# chloedb-crawl, for site owners

You may have found this page through a user agent in your logs:

```text
chloedb-crawl/0.3.3 (+https://chloetech-ai.github.io/chloedb/chloedb/crawler-bot.html)
```

The number after the slash is the crawler's version. Before ChloeDB was renamed, the crawler was called strata-crawl, and its user agent began `strata-crawl/0.1.0`. It is the same crawler, and the robots.txt rules you wrote for `strata-crawl` still apply to it.

This page says what the crawler is, how it behaves on your site, and how to slow it down or keep it out.

## What it is

chloedb-crawl is open-source software (Apache-2.0, part of [ChloeDB](https://github.com/chloetech-ai/chloedb)). It reads a site's pages into a table of a database: their titles, text and the fields their markup carries (JSON-LD, meta tags, tables of specifications). It is not a search engine and not a service of ours. Whoever runs a copy starts its crawls from their own machines, so the requests you see come from them.

A crawl can say whom to reach about it. That goes in the user agent after this page's address:

```text
chloedb-crawl/0.3.3 (+https://chloetech-ai.github.io/chloedb/chloedb/crawler-bot.html; ops@example.com)
```

A crawl that sets a user agent of its own keeps the crawler's name after it (`AcmeBot/2.0 chloedb-crawl/0.3.3 (+...)`), so this page and your robots.txt rules for `chloedb-crawl` still apply.

## How it behaves

Unless whoever runs it changes these:

- **robots.txt.** It reads `/robots.txt` before your site's first page, and again every day while a crawl runs. It follows RFC 9309, using the groups for `chloedb-crawl` and `strata-crawl` (its old name), or else the one for `*`. If robots.txt answers 5xx or 429, or doesn't answer at all, it fetches nothing from your site until it can read it, trying again a minute later (or when your `Retry-After` says, up to 10 minutes). It honours `Crawl-delay`, `nofollow` links and `noindex`/`nofollow` robots meta tags.
- **One request at a time.** It makes at most one request at a time to your site, at least one second apart. The gap grows to your `Crawl-delay` when that is longer, and to twice the time your site takes to answer when it slows down.
- **It backs off.** On a 429, 502, 503 or 504 it waits as long as your `Retry-After` header says, or else 5 seconds, doubled at each refusal in a row up to 5 minutes. It asks for that page again later, at most 4 times. A 403 or a request that times out makes it wait the same way, without asking for that page again. After 10 refusals in a row, or if you ask it to wait more than 10 minutes, it gives up on your site for that crawl.
- **Pages only.** It keeps HTML pages of up to 5 MB, asks for them compressed, skips links to images and files, and fetches each address once per crawl, apart from the retries above. It doesn't run JavaScript, submit forms or log in.

Whoever runs it can switch robots.txt off (`--no-robots`, meant for their own sites) or ask for a faster pace. If that is what you see, the contact in the user agent is the way to reach them, and a 429 or a 403 still makes the crawler wait.

## Keep it out or slow it down

Keep it off the whole site:

```text
User-agent: chloedb-crawl
Disallow: /
```

Keep it out of some paths, and ask for 10 seconds between requests:

```text
User-agent: chloedb-crawl
Crawl-delay: 10
Disallow: /search
Disallow: /cart
```

A change to robots.txt reaches a running crawl within a day. To slow it down sooner, answer `429 Too Many Requests` with a `Retry-After` header. To stop it at once, answer `403`: ten of those in a row and the crawl stops asking your site.

## Reach someone

Write to the contact in the user agent, if the crawl gave one. We wrote the software but don't run other people's crawls, so we can't see or stop them. If the crawler itself misbehaves (it ignores your robots.txt, or doesn't back off as described here), please [open an issue](https://github.com/chloetech-ai/chloedb/issues).
