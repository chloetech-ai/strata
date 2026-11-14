//! A question's memory context by search alone: no model call between the
//! question and the answer. The work is done when the memory is written
//! (notes, the user's rules, the messages themselves); at question time a
//! few searches run side by side on one snapshot and their results are packed
//! into a budget of text the asking model reads:
//!
//! * the rules that bear on the question, first, so an answer follows them;
//! * notes in their current versions, each with the note it replaced, dated
//!   and ordered by the messages they came from;
//! * the part of the assistant's reply closest to the question, for the few
//!   exchanges whose replies a search ranks first: what was recommended or
//!   decided is often in the reply and in no note;
//! * excerpts of the messages, for wording and figures a note left out.
//!
//! A question about order or a summary also gets every live note: its
//! answer is spread over exchanges a search for its words misses.
//!
//! `memory_ask` answers with it. On BEAM 100K (400 questions, the same
//! reader and judge for all three) it scores 62.9 with about 21K characters
//! and no model call, against 55.0 for the query engine's picks and 63.4 for
//! an observational memory that hands the reader its whole 153K-character
//! log; over two runs of what each writes, 61.6 against 62.1
//! (`results/search/beam_100k`, `chloectx-bench search`).

use std::collections::{HashMap, HashSet};
use std::time::Instant;

use crate::recall::{self, RankParams};
use crate::record::{Kind, Record};
use crate::store::{Filter, Store};
use crate::Result;

/// How much of each kind of record a context holds.
#[derive(Clone, Debug)]
pub struct ContextParams {
    /// The context, in characters (the replies come on top).
    pub budget: usize,
    /// Searched for, of each kind (the packing keeps what fits).
    pub notes: usize,
    pub excerpts: usize,
    /// Characters of a message shown in an excerpt.
    pub excerpt_chars: usize,
    /// Rules shown at most, how close to the question one must be, and the
    /// share of the budget they may take (the closest first).
    pub rules: usize,
    pub rule_min_cos: f64,
    pub rule_share: f64,
    /// The shares of what the rules leave that notes, then excerpts, take
    /// first; the rest is filled from both in rank order.
    pub shares: [f64; 2],
    /// For a question about order or a summary (see [`broad`]), the budget
    /// is scaled by this.
    pub broad_scale: f64,
    /// The first `replies` exchanges a search of the assistant's answers
    /// finds, each with the start of the user's message (at most `reply_user_chars`) and the part of the
    /// assistant's reply closest to the question (at most `reply_chars`).
    pub replies: usize,
    pub reply_chars: usize,
    pub reply_user_chars: usize,
    /// Replies for a broad question, and the characters of each: more and
    /// shorter, since its answer is spread over many exchanges.
    pub broad_replies: usize,
    pub broad_reply_chars: usize,
    /// For a broad question, every live note rather than the ones a search
    /// finds, oldest first, at most this many characters (the newest that
    /// fit; on top of the budget). A summary needs what the search for its
    /// words misses (0: none).
    pub broad_timeline: usize,
}

impl Default for ContextParams {
    fn default() -> Self {
        ContextParams {
            budget: 12_000,
            notes: 30,
            excerpts: 12,
            excerpt_chars: 400,
            rules: 4,
            rule_min_cos: 0.3,
            rule_share: 0.2,
            shares: [0.4, 0.25],
            broad_scale: 1.5,
            replies: 3,
            reply_chars: 1000,
            reply_user_chars: 200,
            broad_replies: 8,
            broad_reply_chars: 500,
            broad_timeline: 50_000,
        }
    }
}

/// A built context: the text, the records it shows (in the order shown),
/// and how long the searches took.
pub struct Built {
    pub text: String,
    pub records: Vec<Record>,
    pub ms: f64,
    /// Why each of `records` is shown (one each, in the same order).
    pub picks: Vec<Pick>,
    /// Records the searches found that the context leaves out, and why:
    /// `superseded` (a later note replaced it), `retired` (retired by hand,
    /// with nothing in its place), `uncertain`, or `budget` (no room left).
    pub left_out: Vec<(Record, &'static str)>,
}

/// How a record came into a context.
#[derive(Clone, Debug, Default, serde::Serialize)]
pub struct Pick {
    /// `rule`, `note`, `timeline` (every note, for a broad question),
    /// `reply` or `excerpt`.
    pub section: &'static str,
    /// Characters of the context it takes (a reply's two messages share one
    /// block, counted on the first).
    pub chars: usize,
    /// Its BM25 score and cosine similarity to the question, when the
    /// search that found it ranked it by them.
    pub bm25: Option<f64>,
    pub cos: Option<f64>,
    /// The earlier version a note replaced, as the context shows it.
    pub replaced: Option<String>,
}

const NOTE_KINDS: [Kind; 5] = [Kind::Decision, Kind::Preference, Kind::Fact, Kind::Todo, Kind::Note];

/// A record's text on one line, its harness tags defused ([`crate::envelope::defuse`]).
fn one_line(s: &str) -> String {
    crate::envelope::defuse(&s.split_whitespace().collect::<Vec<_>>().join(" ")).into_owned()
}

fn date(ts: i64) -> String {
    recall::fmt_time(ts).chars().take(10).collect()
}

fn note_kind(r: &Record) -> &'static str {
    match r.kind {
        Kind::Todo if r.tag == "done" => "done",
        Kind::Todo => "plan",
        Kind::Preference => "rule",
        k => k.as_str(),
    }
}

/// `text` cut to at most `max` bytes at a line or sentence end.
fn brief_text(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.trim().to_string();
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    let head = &text[..end];
    let cut = head.rfind('\n').filter(|&i| i > max / 2).or_else(|| head.rfind(". ").map(|i| i + 1)).unwrap_or(end);
    format!("{}…", head[..cut].trim_end())
}

/// Whether a question asks about many exchanges at once: the order things
/// came up in, or a summary of a stretch of work.
pub fn broad(question: &str) -> bool {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| regex::Regex::new(r"(?i)\b(in (what|which) order|order (of|in which)|in order how|list (them |these |it )?in order|sequence (of|in which)|chronolog\w*|(the|a) timeline of|summar\w*|recap|overview)\b").unwrap()).is_match(question)
}

/// The records of `kind` of the given exchanges, in the order asked, each
/// exchange's in the order written.
fn of_exchanges(store: &Store, at: &[(String, i64)], kind: Kind) -> Result<Vec<Record>> {
    if at.is_empty() {
        return Ok(Vec::new());
    }
    let sessions: HashSet<&str> = at.iter().map(|(s, _)| s.as_str()).collect();
    let list: Vec<String> = sessions.iter().map(|s| format!("'{}'", s.replace('\'', "''"))).collect();
    let sql = format!("SELECT {} FROM memory WHERE kind = '{}' AND session IN ({}) LIMIT 100000", crate::record::COLUMNS, kind.as_str(), list.join(", "));
    let rows: Vec<Record> = store.query(&sql)?.rows.iter().filter_map(|r| crate::store::row_record(r)).collect();
    let mut by: HashMap<(String, i64), Vec<Record>> = HashMap::new();
    for r in rows {
        by.entry((r.session.clone(), r.turn)).or_default().push(r);
    }
    let mut out = Vec::new();
    for k in at {
        if let Some(v) = by.get_mut(k) {
            v.sort_by_key(|r| r.seq);
            out.append(v);
        }
    }
    Ok(out)
}

/// What each note replaced, as a label: " (changed DATE; was: ...)", the
/// date `said` gives the note.
fn change_labels(store: &Store, notes: &[&Record], said: impl Fn(&Record) -> i64) -> HashMap<String, String> {
    let mut want: Vec<String> = notes.iter().filter(|r| !r.ctx.starts_with("Merged from")).flat_map(|r| r.linked("supersedes").map(String::from)).collect();
    want.sort();
    want.dedup();
    if want.is_empty() {
        return HashMap::new();
    }
    let old: HashMap<String, Record> = store.get(&want).unwrap_or_default().into_iter().map(|r| (r.id.clone(), r)).collect();
    let mut out = HashMap::new();
    for r in notes.iter().filter(|r| !r.ctx.starts_with("Merged from")) {
        if let Some(o) = r.linked("supersedes").filter_map(|id| old.get(id)).max_by_key(|o| o.ts) {
            let was: String = one_line(&o.text);
            let was = if was.chars().count() > 140 { format!("{}…", was.chars().take(140).collect::<String>()) } else { was };
            out.insert(r.id.clone(), format!(" (changed {}; was: {was})", date(said(r))));
        }
    }
    out
}

/// One exchange: a user's message and the replies after it.
struct Ex {
    ts: i64,
    seq: i64,
    user: Vec<Record>,
    answer: Vec<Record>,
}

/// The exchanges `hits` (records' sessions and turns) came from, in the
/// order of their first hit, and each exchange's messages.
fn ranked_exchanges(store: &Store, hits: &[(String, i64)]) -> Result<(Vec<(String, i64)>, HashMap<(String, i64), Ex>)> {
    // The messages of every session a hit names, walked in order: a user's
    // message starts an exchange, and the replies after it belong to it.
    let mut sessions: Vec<&str> = Vec::new();
    let mut seen_s: HashSet<&str> = HashSet::new();
    for s in hits.iter().map(|(s, _)| s.as_str()) {
        if seen_s.insert(s) {
            sessions.push(s);
        }
    }
    let mut start_of: HashMap<(String, i64), i64> = HashMap::new();
    let mut ex: HashMap<(String, i64), Ex> = HashMap::new();
    if !sessions.is_empty() {
        let list: Vec<String> = sessions.iter().map(|s| format!("'{}'", s.replace('\'', "''"))).collect();
        let sql = format!(
            "SELECT {} FROM memory WHERE kind IN ('{}', '{}') AND session IN ({}) LIMIT 1000000",
            crate::record::COLUMNS,
            Kind::Prompt.as_str(),
            Kind::Answer.as_str(),
            list.join(", ")
        );
        let mut rows: Vec<Record> = store.query(&sql)?.rows.iter().filter_map(|r| crate::store::row_record(r)).collect();
        rows.sort_by(|a, b| (a.session.as_str(), a.seq).cmp(&(b.session.as_str(), b.seq)));
        let mut cur: Option<(String, i64)> = None;
        for r in rows {
            let start = match &cur {
                _ if r.kind == Kind::Prompt => r.turn,
                Some((s, t)) if *s == r.session => *t,
                _ => r.turn,
            };
            cur = Some((r.session.clone(), start));
            start_of.insert((r.session.clone(), r.turn), start);
            let e = ex.entry((r.session.clone(), start)).or_insert(Ex { ts: r.ts, seq: r.seq, user: Vec::new(), answer: Vec::new() });
            if r.kind == Kind::Prompt {
                e.user.push(r);
            } else {
                e.answer.push(r);
            }
        }
    }
    let mut seen: HashSet<(String, i64)> = HashSet::new();
    let ranked: Vec<(String, i64)> = hits
        .iter()
        .map(|(s, t)| (s.clone(), *start_of.get(&(s.clone(), *t)).unwrap_or(t)))
        .filter(|key| ex.contains_key(key) && seen.insert(key.clone()))
        .collect();
    Ok((ranked, ex))
}

/// The context for `question` from `store`'s records that `filter` lets
/// through.
pub fn build(store: &Store, question: &str, filter: &Filter, p: &ContextParams) -> Result<Built> {
    let t0 = Instant::now();
    let wide;
    let is_broad = broad(question);
    let p = if is_broad {
        wide = ContextParams { budget: (p.budget as f64 * p.broad_scale) as usize, replies: p.broad_replies, reply_chars: p.broad_reply_chars, ..p.clone() };
        &wide
    } else {
        p
    };
    let store = &store.pinned();
    let with = |kinds: &[Kind]| Filter { kinds: kinds.to_vec(), ..filter.clone() };
    // Rules the vector search reads: fewer back means every rule was read.
    let rule_n = (p.rules * 3).max(50);
    // Each search's scores, for the picks' reasons.
    let signals: std::sync::Mutex<HashMap<String, (Option<f64>, Option<f64>)>> = Default::default();
    let one = |q: &str, f: &Filter, n: usize| -> Result<Vec<Record>> {
        let hits = recall::search(store, q, f, &RankParams { n, ..Default::default() })?.0;
        let mut sig = signals.lock().unwrap_or_else(|e| e.into_inner());
        for h in &hits {
            sig.entry(h.record.id.clone()).or_insert((h.bm25, h.cos));
        }
        Ok(hits.into_iter().map(|h| h.record).collect())
    };
    let (notes, messages, answers, rules) = std::thread::scope(|s| {
        let notes = s.spawn(|| one(question, &with(&NOTE_KINDS), p.notes));
        let messages = s.spawn(|| one(question, &with(&[Kind::Prompt, Kind::Answer]), p.excerpts));
        let answers = s.spawn(|| if p.replies > 0 { one(question, &with(&[Kind::Answer]), p.excerpts * 2) } else { Ok(Vec::new()) });
        let rules = s.spawn(|| -> Result<Vec<(Record, f64)>> {
            let qv = store.embedder.embed_query(question)?;
            store.vector_top(&qv, rule_n, &with(&[Kind::Preference]))
        });
        let j = |h: std::thread::ScopedJoinHandle<'_, Result<Vec<Record>>>| h.join().unwrap_or_else(|_| Err(crate::err("search panicked")));
        (
            j(notes),
            j(messages),
            j(answers),
            rules.join().unwrap_or_else(|_| Err(crate::err("search panicked"))),
        )
    });
    let (notes, messages, answers, rules) = (notes?, messages?, answers?, rules?);
    let signals = signals.into_inner().unwrap_or_else(|e| e.into_inner());
    let live = |r: &Record| !crate::record::closed_tag(&r.tag) && r.tag != "uncertain" && r.tag != "cue";
    let mut left_out: Vec<(Record, &'static str)> = Vec::new();
    let mut gone: HashSet<String> = HashSet::new();
    for r in notes.iter().chain(rules.iter().filter(|(_, c)| *c >= p.rule_min_cos).map(|(r, _)| r)) {
        if !live(r) && r.tag != "cue" && gone.insert(r.id.clone()) {
            left_out.push((r.clone(), match r.tag.as_str() {
                "superseded" => "superseded",
                crate::record::RETIRED => "retired",
                _ => "uncertain",
            }));
        }
    }
    let rule_cos: HashMap<String, f64> = rules.iter().map(|(r, c)| (r.id.clone(), *c)).collect();
    // The rules section: the rules close in meaning to the question, then
    // the ones its search for notes found by their words, then, in a space
    // with no more rules than the section holds, every other rule: a rule
    // a context shows is in it, not among the notes after "Rules that
    // apply come first" (APC-24).
    let every_rule = rules.len() < rule_n && rules.iter().filter(|(r, _)| live(r)).count() <= p.rules;
    let mut picked_rules: Vec<Record> = rules.iter().filter(|(r, c)| *c >= p.rule_min_cos && live(r)).map(|(r, _)| r.clone()).collect();
    for r in notes.iter().filter(|r| r.kind == Kind::Preference && live(r)).chain(rules.iter().filter(|_| every_rule).map(|(r, _)| r).filter(|r| live(r))) {
        if !picked_rules.iter().any(|x| x.id == r.id) {
            picked_rules.push(r.clone());
        }
    }
    let rules: Vec<Record> = picked_rules.into_iter().take(p.rules).collect();
    let notes: Vec<Record> = notes.into_iter().filter(|r| live(r) && !rules.iter().any(|x| x.id == r.id)).collect();
    // A broad question's timeline: every live note.
    let all_notes: Vec<Record> = if is_broad && p.broad_timeline > 0 {
        store.latest(&NOTE_KINDS, 1_000_000, filter)?.into_iter().filter(|r| live(r)).collect()
    } else {
        Vec::new()
    };
    let refs: Vec<&Record> = rules.iter().chain(notes.iter()).chain(all_notes.iter()).collect();
    // A note is dated and ordered by the message it was written from: a
    // merged note is written late, but said when its newest source was.
    let at: Vec<(String, i64)> = refs.iter().map(|r| (r.session.clone(), r.turn)).collect();
    let mut source: HashMap<(String, i64), (i64, i64)> = HashMap::new();
    for r in of_exchanges(store, &at, Kind::Prompt)? {
        let e = source.entry((r.session.clone(), r.turn)).or_insert((r.ts, r.seq));
        *e = (e.0.min(r.ts), e.1.min(r.seq));
    }
    let said = |r: &Record| -> (i64, i64) { *source.get(&(r.session.clone(), r.turn)).unwrap_or(&(r.ts, r.seq)) };
    let changes = change_labels(store, &refs, |r| said(r).0);
    let terms: HashSet<String> = chloedb::text::analyze_query(question).into_iter().collect();
    let note_line = |r: &Record| format!("- {} {}: {}{}\n", date(said(r).0), note_kind(r), one_line(&r.text), changes.get(&r.id).map(|s| s.as_str()).unwrap_or(""));
    let excerpt = |r: &Record| {
        let role = if r.kind == Kind::Answer { "assistant" } else { "user" };
        format!("[{} · session {}] {role}: {}\n", recall::fmt_time(r.ts), r.session, one_line(&recall::snippet(&r.text, &terms, p.excerpt_chars)))
    };

    // Rules first, then notes up to their share, then excerpts, then
    // whatever is left in rank order.
    let mut used = 0usize;
    let mut shown: HashSet<String> = HashSet::new();
    let mut rule_lines: Vec<(Record, String)> = Vec::new();
    for r in &rules {
        let line = note_line(r);
        if used + line.len() <= (p.budget as f64 * p.rule_share) as usize {
            used += line.len();
            shown.insert(r.id.clone());
            rule_lines.push((r.clone(), line));
        }
    }
    // The timeline: the newest live notes that fit, shown oldest first in
    // place of the searched ones.
    let mut timeline: Vec<(Record, String)> = Vec::new();
    if !all_notes.is_empty() {
        let mut all: Vec<&Record> = all_notes.iter().filter(|r| !shown.contains(&r.id)).collect();
        all.sort_by_key(|r| said(r));
        let mut size = 0usize;
        for r in all.into_iter().rev() {
            let line = note_line(r);
            if size + line.len() > p.broad_timeline {
                break;
            }
            size += line.len();
            shown.insert(r.id.clone());
            timeline.push((r.clone(), line));
        }
    }
    let lists: [Vec<(Record, String)>; 2] = [notes.iter().map(|r| (r.clone(), note_line(r))).collect(), messages.iter().map(|r| (r.clone(), excerpt(r))).collect()];
    let mut picked: [Vec<(Record, String)>; 2] = Default::default();
    let rest = p.budget.saturating_sub(used);
    let mut next = [0usize; 2];
    let shares = p.shares;
    for k in 0..2 {
        let cap = used + (rest as f64 * shares[k]) as usize;
        while let Some((r, line)) = lists[k].get(next[k]) {
            if used + line.len() > cap.min(p.budget) {
                break;
            }
            next[k] += 1;
            if shown.insert(r.id.clone()) {
                used += line.len();
                picked[k].push((r.clone(), line.clone()));
            }
        }
    }
    let mut progress = true;
    while progress {
        progress = false;
        for k in 0..2 {
            while let Some((r, line)) = lists[k].get(next[k]) {
                next[k] += 1;
                if shown.contains(&r.id) {
                    continue;
                }
                if used + line.len() <= p.budget {
                    used += line.len();
                    shown.insert(r.id.clone());
                    picked[k].push((r.clone(), line.clone()));
                    progress = true;
                }
                break;
            }
        }
    }

    // The replies of the exchanges the searches agree on most.
    let mut replies: Vec<(i64, i64, String, Vec<Record>)> = Vec::new();
    if p.replies > 0 {
        let hits: Vec<(String, i64)> = answers.iter().map(|r| (r.session.clone(), r.turn)).collect();
        let (ranked, ex) = ranked_exchanges(store, &hits)?;
        let joined = |rs: &[Record]| rs.iter().map(|r| r.text.as_str()).collect::<Vec<_>>().join(" ");
        for key in &ranked {
            let e = &ex[key];
            let answer = joined(&e.answer);
            if answer.trim().is_empty() {
                continue;
            }
            let mut block = format!("[{} · session {}]", recall::fmt_time(e.ts), key.0);
            let mut recs = Vec::new();
            if let Some(r) = e.user.first() {
                block += &format!(" user: {}\n", one_line(&brief_text(&joined(&e.user), p.reply_user_chars)));
                recs.push(r.clone());
            } else {
                block.push('\n');
            }
            block += &format!("assistant: {}\n", one_line(&recall::snippet(&answer, &terms, p.reply_chars)));
            recs.push(e.answer[0].clone());
            replies.push((e.ts, e.seq, block, recs));
            if replies.len() == p.replies {
                break;
            }
        }
    }

    let mut text = String::new();
    let mut records = Vec::new();
    let mut picks: Vec<Pick> = Vec::new();
    let in_timeline: HashSet<String> = timeline.iter().map(|(r, _)| r.id.clone()).collect();
    let pick = |r: &Record, section: &'static str, chars: usize| {
        let (bm25, cos) = signals.get(&r.id).copied().unwrap_or((None, rule_cos.get(&r.id).copied()));
        Pick { section, chars, bm25, cos, replaced: changes.get(&r.id).map(|c| c.trim().trim_start_matches('(').trim_end_matches(')').to_string()) }
    };
    // Found, not live, and not shown: no room for it.
    for r in lists[0].iter().skip(next[0]).chain(lists[1].iter().skip(next[1])).map(|(r, _)| r) {
        if !shown.contains(&r.id) && gone.insert(r.id.clone()) {
            left_out.push((r.clone(), "budget"));
        }
    }
    let every = !timeline.is_empty();
    picked[0].append(&mut timeline);
    if !rule_lines.is_empty() || !picked[0].is_empty() {
        // By day, then by where the message each came from stands in the store.
        picked[0].sort_by_key(|(r, _)| {
            let (ts, seq) = said(r);
            (ts.div_euclid(86_400), seq)
        });
        text += if every { "Every note" } else { "Notes" };
        text += " from earlier conversations with the user, in their current versions, oldest first (date, kind, note; what it replaced in brackets). Rules that apply come first; a later note can change what an earlier one says:\n";
        let rule_ids: HashSet<String> = rule_lines.iter().map(|(r, _)| r.id.clone()).collect();
        for (r, l) in rule_lines.into_iter().chain(picked[0].drain(..)) {
            let section = if rule_ids.contains(&r.id) {
                "rule"
            } else if in_timeline.contains(&r.id) {
                "timeline"
            } else {
                "note"
            };
            picks.push(pick(&r, section, l.len()));
            text += &l;
            records.push(r);
        }
        text.push('\n');
    }
    if !replies.is_empty() {
        replies.sort_by_key(|b| (b.0, b.1));
        text += "What the assistant replied in the exchanges closest to the question, oldest first (the start of the user's message, then the part of the reply closest to the question):\n";
        for (_, _, b, recs) in replies {
            for (i, r) in recs.iter().enumerate() {
                picks.push(pick(r, "reply", if i == 0 { b.len() } else { 0 }));
            }
            text += &b;
            records.extend(recs);
        }
        text.push('\n');
    }
    if !picked[1].is_empty() {
        picked[1].sort_by_key(|(r, _)| (r.ts, r.seq));
        text += "Excerpts of the messages:\n";
        for (r, l) in picked[1].drain(..) {
            picks.push(pick(&r, "excerpt", l.len()));
            text += &l;
            records.push(r);
        }
    }
    Ok(Built { text: text.trim_end().to_string(), records, ms: t0.elapsed().as_secs_f64() * 1e3, picks, left_out })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::embed::HashEmbedder;
    use crate::record::Draft;
    use crate::store::StoreOptions;
    use std::sync::Arc;

    const DAY: i64 = 86_400;

    fn store(dir: &std::path::Path) -> Store {
        Store::open(&dir.join("c.strata"), Arc::new(HashEmbedder { dim: 256 }), StoreOptions { sync: false, ..Default::default() }).unwrap()
    }

    fn trek(s: &Store) {
        s.append("s1", 0, DAY, vec![Draft::new(Kind::Prompt, "Which tent should I take to the Andes trek in March?")]).unwrap();
        s.append("s1", 1, DAY + 60, vec![Draft::new(Kind::Answer, "For the Andes trek take the four-season tent; it holds up in snow and wind.")]).unwrap();
        s.append("s2", 0, 2 * DAY, vec![Draft::new(Kind::Prompt, "Give me a pasta recipe with mushrooms.")]).unwrap();
        s.append("s2", 1, 2 * DAY + 60, vec![Draft::new(Kind::Answer, "Fry the mushrooms in butter, then toss them with the pasta.")]).unwrap();
    }

    /// The exchanges the searches agree on show the part of the assistant's
    /// reply about the question.
    #[test]
    fn replies_show_what_the_assistant_said() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(dir.path());
        trek(&s);
        s.flush().unwrap();
        let q = "Which tent did you suggest for the Andes trek?";
        let b = build(&s, q, &Filter::default(), &ContextParams { replies: 1, ..Default::default() }).unwrap();
        let at = b.text.find("What the assistant replied").expect("replies block");
        let block = b.text[at..].split("\n\n").next().unwrap();
        assert!(block.contains("user: Which tent should I take") && block.contains("assistant: For the Andes trek take the four-season tent"), "{}", b.text);
        assert!(!block.contains("pasta"), "{}", b.text);
        assert!(b.records.iter().any(|r| r.kind == Kind::Answer && r.session == "s1"), "{:?}", b.records);
        let none = build(&s, q, &Filter::default(), &ContextParams { replies: 0, ..Default::default() }).unwrap();
        assert!(!none.text.contains("What the assistant replied"), "{}", none.text);
        // A summary question gets its own, shorter replies.
        let wide = build(&s, "Can you summarize what you suggested for the Andes trek?", &Filter::default(), &ContextParams { replies: 0, broad_replies: 2, broad_reply_chars: 40, ..Default::default() }).unwrap();
        let at = wide.text.find("What the assistant replied").expect("replies for a broad question");
        let block = wide.text[at..].split("\n\n").next().unwrap();
        assert!(block.contains("assistant: For the Andes trek") && block.lines().all(|l| l.len() < 200), "{}", wide.text);
    }

    /// Notes come in their current versions with what they replaced, rules
    /// first; a replaced note is not shown on its own.
    #[test]
    fn notes_show_what_they_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(dir.path());
        let old = s.append("s1", 0, DAY, vec![Draft::new(Kind::Fact, "The user's holiday gift budget is 300 dollars.").tag("superseded")]).unwrap();
        s.append("s2", 0, 2 * DAY, vec![Draft::new(Kind::Fact, "The user's holiday gift budget is 450 dollars.").tag("distilled").link("supersedes", old[0].id.clone())]).unwrap();
        s.append("s3", 0, 2 * DAY, vec![Draft::new(Kind::Preference, "Always confirm the budget caps when the user asks about holiday spending.").tag("distilled")]).unwrap();
        s.flush().unwrap();
        // Any rule counts as close to the question (the test embedder's
        // cosines are low).
        let b = build(&s, "What is my holiday gift budget?", &Filter::default(), &ContextParams { rule_min_cos: -1.0, ..Default::default() }).unwrap();
        assert!(b.text.contains("fact: The user's holiday gift budget is 450 dollars. (changed 1970-01-03; was: The user's holiday gift budget is 300 dollars.)"), "{}", b.text);
        assert!(!b.text.contains("\n- 1970-01-02 fact"), "{}", b.text);
        let rule = b.text.find("rule: Always confirm").expect("the rule");
        let fact = b.text.find("fact: The user's holiday gift budget is 450").unwrap();
        assert!(rule < fact, "{}", b.text);
    }

    /// APC-24: a rule a context shows is in the rules section, before the
    /// notes, whether or not it is close in meaning to the question; a
    /// retired one is left out as retired.
    #[test]
    fn a_rule_shown_is_a_rule() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(dir.path());
        s.append("s1", 0, DAY, vec![Draft::new(Kind::Fact, "Refunds above 500 EUR are approved by the finance team.").tag("distilled")]).unwrap();
        s.append("s2", 0, 2 * DAY, vec![Draft::new(Kind::Preference, "Never refund to a different card than the one charged.")]).unwrap();
        s.append("s3", 0, 3 * DAY, vec![Draft::new(Kind::Preference, "Refunds over 100 EUR are always sent by bank transfer.").tag(crate::record::RETIRED)]).unwrap();
        s.flush().unwrap();
        // The test embedder's cosines are low: the live rule is not close in meaning.
        let b = build(&s, "Who approves a refund of 700 EUR?", &Filter::default(), &ContextParams::default()).unwrap();
        let rule = b.records.iter().position(|r| r.text.starts_with("Never refund")).unwrap_or_else(|| panic!("the space's one rule: {}", b.text));
        assert_eq!(b.picks[rule].section, "rule", "{}", b.text);
        assert!(b.text.find("rule: Never refund").unwrap() < b.text.find("fact: Refunds above 500").unwrap(), "{}", b.text);
        assert!(b.text.contains("Rules that apply come first"), "{}", b.text);
        assert!(!b.text.contains("bank transfer"), "{}", b.text);
        let close = build(&s, "Who approves a refund of 700 EUR?", &Filter::default(), &ContextParams { rule_min_cos: -1.0, ..Default::default() }).unwrap();
        assert!(close.left_out.iter().any(|(r, why)| r.text.contains("bank transfer") && *why == "retired"), "{:?}", close.left_out.iter().map(|(r, w)| (&r.text, w)).collect::<Vec<_>>());
    }

    /// A note merged late is dated and placed by the message it came from.
    #[test]
    fn notes_are_ordered_by_their_source() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(dir.path());
        s.append("s1", 0, DAY, vec![Draft::new(Kind::Prompt, "My trek starts on March 3.")]).unwrap();
        s.append("s2", 0, DAY + 60, vec![Draft::new(Kind::Prompt, "I booked the trek flights.")]).unwrap();
        s.append("s2", 0, DAY + 61, vec![Draft::new(Kind::Fact, "The user booked flights for the trek.").tag("distilled")]).unwrap();
        // Merged days later, as consolidation writes it.
        s.append("s1", 0, 5 * DAY, vec![Draft::new(Kind::Fact, "The user's trek starts on March 3.").tag("distilled")]).unwrap();
        s.flush().unwrap();
        let b = build(&s, "When does my trek start and did I book flights?", &Filter::default(), &ContextParams::default()).unwrap();
        let first = b.text.find("- 1970-01-02 fact: The user's trek starts on March 3").expect("first note, dated by its message");
        let second = b.text.find("booked flights").expect("second note");
        assert!(first < second, "{}", b.text);
    }

    /// A summary question gets every live note, oldest first, each once,
    /// with the rules first.
    #[test]
    fn summaries_show_every_note() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(dir.path());
        s.append("s1", 0, DAY, vec![Draft::new(Kind::Preference, "Always answer the user in short bullet points.").tag("distilled")]).unwrap();
        s.append("s1", 1, DAY, vec![Draft::new(Kind::Fact, "The user's trek starts on March 3.").tag("distilled")]).unwrap();
        let old = s.append("s2", 0, 2 * DAY, vec![Draft::new(Kind::Fact, "The user's gear budget is 300 dollars.").tag("superseded")]).unwrap();
        s.append("s3", 0, 3 * DAY, vec![Draft::new(Kind::Fact, "The user's sister moves to Lisbon in May.").tag("distilled")]).unwrap();
        s.append("s4", 0, 4 * DAY, vec![Draft::new(Kind::Fact, "The user's gear budget is 450 dollars.").tag("distilled").link("supersedes", old[0].id.clone())]).unwrap();
        s.flush().unwrap();
        let p = ContextParams { rule_min_cos: -1.0, ..Default::default() };
        let b = build(&s, "Can you summarize how my trek plans developed?", &Filter::default(), &p).unwrap();
        assert!(b.text.starts_with("Every note"), "{}", b.text);
        let at = |t: &str| b.text.find(t).unwrap_or_else(|| panic!("{t}: {}", b.text));
        assert!(at("rule: Always answer") < at("trek starts on March 3") && at("trek starts") < at("sister moves to Lisbon") && at("Lisbon") < at("budget is 450 dollars. (changed"), "{}", b.text);
        assert!(!b.text.contains("- 1970-01-03 fact: The user's gear budget is 300"), "{}", b.text);
        assert_eq!(b.text.matches("Always answer the user").count(), 1, "{}", b.text);
        // Other questions get the notes a search finds.
        let one = build(&s, "When does my trek start?", &Filter::default(), &p).unwrap();
        assert!(one.text.starts_with("Notes from"), "{}", one.text);
    }

    #[test]
    fn broad_questions_ask_for_order_or_a_summary() {
        for q in ["In what order did I bring up the parts of my thesis?", "Can you summarize how my budget changed?", "Can you list in order how I brought up the projects?", "Give me a timeline of the move"] {
            assert!(broad(q), "{q}");
        }
        for q in ["How many cupcakes did I order for the event?", "Have I ever placed an online order for sneakers?", "What approach did you recommend to balance my finances and timeline?", "How do I keep the order service up?"] {
            assert!(!broad(q), "{q}");
        }
    }
}
