//! Machine envelopes around a prompt: what a harness wraps a person's words
//! in before Claude Code sees them as the prompt (`<system-reminder>`,
//! `<wake>`, `<coordinator-relay>`, `<session-context>`, slash-command
//! markup, and the same tags HTML-escaped when one envelope quotes
//! another).
//!
//! Stored as they came, they were recalled into later prompts as if they
//! were the conversation, and a sentence of a reminder ("never ...") was
//! picked as a rule. [`clean`] keeps the words inside the envelopes that a
//! person or agent wrote and drops the ones that are only machinery;
//! [`is_envelope`] tells a record stored before that apart, so recall and
//! the briefing pass it over.

/// Tags dropped with everything inside them: notices for the model, not
/// the conversation.
const DROP: [&str; 9] = [
    "system-reminder",
    "system-note",
    "session-context",
    "local-command-caveat",
    "local-command-stdout",
    "local-command-stderr",
    "command-message",
    "user-prompt-submit-hook",
    "task-notification",
];

/// Tags whose markup goes and whose text stays: a message inside a wake, a
/// quoted message inside a relay, a pasted block, a slash command's name.
const UNWRAP: [&str; 13] = [
    "wake",
    "project",
    "thread",
    "message",
    "coordinator-relay",
    "relay",
    "cited",
    "note",
    "pasted_content",
    "command-name",
    "command-args",
    "bash-input",
    "bash-stdout",
];

/// Tags that mark text as an envelope: every dropped one, and the outer
/// unwrapped ones. The other unwrapped names (`message`, `note`, ...) are
/// words a person may write in angle brackets, so they are unwrapped only
/// inside text one of these marks.
fn is_marker(name: &str) -> bool {
    DROP.contains(&name) || name == "wake" || name == "coordinator-relay"
}

/// A tag at the start of `s`: `<name ...>`, `</name>`, or the same with
/// `&lt;` and `&gt;`. Returns (name, closing, self-closing, length).
fn tag_at(s: &str) -> Option<(&str, bool, bool, usize)> {
    let (open, rest) = if let Some(r) = s.strip_prefix('<') {
        (1, r)
    } else if let Some(r) = s.strip_prefix("&lt;") {
        (4, r)
    } else {
        return None;
    };
    let (closing, rest) = match rest.strip_prefix('/') {
        Some(r) => (true, r),
        None => (false, rest),
    };
    let n = rest.find(|c: char| !(c.is_ascii_alphanumeric() || c == '-' || c == '_')).unwrap_or(rest.len());
    let name = &rest[..n];
    if name.is_empty() || !(DROP.contains(&name) || UNWRAP.contains(&name)) {
        return None;
    }
    let after = &rest[n..];
    // The name must end the tag's name: `<notes>` is not `<note>`.
    if !(after.starts_with(['>', ' ', '\n', '\t', '/']) || after.starts_with("&gt;")) {
        return None;
    }
    let end = [after.find('>').map(|i| (i, 1)), after.find("&gt;").map(|i| (i, 4))].into_iter().flatten().min_by_key(|x| x.0)?;
    let self_closing = after[..end.0].ends_with('/');
    Some((name, closing, self_closing, open + usize::from(closing) + n + end.0 + end.1))
}

/// The end of the first closing tag of `name` at or after `from`.
fn close_end(text: &str, from: usize, name: &str) -> Option<usize> {
    let mut i = from;
    loop {
        let off = [format!("</{name}"), format!("&lt;/{name}")].iter().filter_map(|c| text[i..].find(c.as_str())).min()?;
        let at = i + off;
        match tag_at(&text[at..]) {
            Some((n, true, _, len)) if n == name => return Some(at + len),
            _ => i = at + 1,
        }
    }
}

/// Whether `text` holds a machine envelope (a record stored before
/// [`clean`] existed, or text that is nothing else).
pub fn is_envelope(text: &str) -> bool {
    if !text.contains('<') && !text.contains("&lt;") {
        return false;
    }
    let mut i = 0;
    while let Some(off) = text[i..].find(['<', '&']) {
        let at = i + off;
        if let Some((name, closing, _, _)) = tag_at(&text[at..]) {
            if !closing && is_marker(name) {
                return true;
            }
        }
        i = at + 1;
    }
    false
}

/// Whether a stored record is a harness envelope that recall passes over:
/// a `prompt` stored before [`clean`] existed. Only prompts are wrapped by
/// a harness; code, tool output and notes that mention one of its tags
/// (a hook that writes `<system-reminder>`) are the work itself, stay
/// searchable, and are rendered through [`defuse`] (CTX-7).
pub fn is_envelope_record(r: &crate::record::Record) -> bool {
    r.kind == crate::record::Kind::Prompt && is_envelope(&r.text)
}

/// `text` with the opening `<` of every harness tag escaped (`&lt;`), so
/// that a recalled record that quotes `<system-reminder>` reads as quoted
/// text in the context it is injected into, not as a notice from the
/// harness. Other angle brackets are left as they are.
pub fn defuse(text: &str) -> std::borrow::Cow<'_, str> {
    if !text.contains('<') {
        return std::borrow::Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len() + 16);
    let mut last = 0;
    let mut i = 0;
    while let Some(off) = text[i..].find('<') {
        let at = i + off;
        if tag_at(&text[at..]).is_some_and(|(name, _, _, _)| DROP.contains(&name) || UNWRAP.contains(&name)) {
            out.push_str(&text[last..at]);
            out.push_str("&lt;");
            last = at + 1;
        }
        i = at + 1;
    }
    if last == 0 {
        return std::borrow::Cow::Borrowed(text);
    }
    out.push_str(&text[last..]);
    std::borrow::Cow::Owned(out)
}

/// `text` without its envelopes: the dropped tags go with their content,
/// the unwrapped ones leave their content, and entities that only an
/// escaped envelope put there are decoded. Text with no envelope comes
/// back as it was (the unwrapped names alone do not make one).
pub fn clean(text: &str) -> String {
    if !is_envelope(text) {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    let mut escaped = false;
    let mut changed = false;
    let mut i = 0;
    while i < text.len() {
        let Some(off) = text[i..].find(['<', '&']) else {
            out += &text[i..];
            break;
        };
        out += &text[i..i + off];
        let at = i + off;
        match tag_at(&text[at..]) {
            Some((name, closing, self_closing, len)) => {
                changed = true;
                escaped |= text[at..].starts_with('&');
                i = at + len;
                if !closing && !self_closing && DROP.contains(&name) {
                    // To the matching close (either spelling, attributes
                    // and all), or the end.
                    i = close_end(text, i, name).unwrap_or(text.len());
                }
                // A tag stood for a line break more often than for nothing.
                if !out.ends_with('\n') && !out.is_empty() {
                    out.push('\n');
                }
            }
            None => {
                let c = text[at..].chars().next().unwrap();
                out.push(c);
                i = at + c.len_utf8();
            }
        }
    }
    if !changed {
        return text.to_string();
    }
    if escaped {
        out = out.replace("&quot;", "\"").replace("&#39;", "'").replace("&gt;", ">").replace("&lt;", "<").replace("&amp;", "&");
    }
    // The lines the tags left, without the indentation and blank lines
    // between them.
    out.lines().map(str::trim).filter(|l| !l.is_empty()).collect::<Vec<_>>().join("\n")
}

/// Retires the rules a store picked out of envelopes before [`clean`]
/// existed: a "User preference: ..." cue whose sentence came from the
/// envelope around its turn's prompt (a reminder's "never ..."), not from
/// what the person wrote. Retired as superseded, so the briefing and
/// recall pass them over; the prompts themselves are passed over by
/// [`is_envelope`]. Returns how many were retired.
pub fn retire_envelope_rules(store: &crate::store::Store) -> crate::Result<usize> {
    use chloedb::Value;
    let cues: Vec<crate::record::Record> = store
        .latest(&[crate::record::Kind::Preference], 100_000, &crate::store::Filter::default())?
        .into_iter()
        .filter(|r| r.tag == "cue")
        .collect();
    let mut prompts: std::collections::HashMap<(String, i64), Option<Vec<String>>> = std::collections::HashMap::new();
    let mut retired = 0;
    for c in cues {
        let key = (c.session.clone(), c.turn);
        if !prompts.contains_key(&key) {
            let rs = store.query_with(
                &format!("SELECT text FROM memory WHERE kind = 'prompt' AND session = :s AND turn = {} ORDER BY seq ASC LIMIT 64", c.turn),
                &chloedb::query::Params::new().set("s", Value::Str(c.session.clone())),
            )?;
            let text: String = rs.rows.iter().filter_map(|r| match r.first() {
                Some(Value::Str(t)) => Some(t.as_str()),
                _ => None,
            }).collect::<Vec<_>>().join("");
            // Only a turn whose prompt was an envelope: what the person
            // wrote keeps every rule it states.
            let kept = is_envelope(&text).then(|| crate::capture::preference_sentences(&crate::redact::redact(&clean(&text))));
            prompts.insert(key.clone(), kept);
        }
        let Some(Some(kept)) = prompts.get(&key) else { continue };
        let sentence = c.text.strip_prefix("User preference: ").unwrap_or(&c.text);
        if !kept.iter().any(|k| k == sentence) && store.retag(&c.id, "superseded")? {
            retired += 1;
        }
    }
    Ok(retired)
}

/// [`retire_envelope_rules`] once per store (a file in the store's
/// directory says it ran), for a daemon to call when it starts.
pub fn retire_once(dir: &std::path::Path, store: &crate::store::Store) -> Option<usize> {
    let mark = dir.join("envelopes-retired");
    if mark.exists() {
        return None;
    }
    let n = retire_envelope_rules(store).ok()?;
    let _ = std::fs::write(&mark, format!("{n}\n"));
    Some(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_text_is_left_alone() {
        for t in ["fix the export button", "a < b and c > d", "use <div> and &lt;p&gt; in HTML", "<notes>x</notes>", "the <message> tag of a <note>", ""] {
            assert_eq!(clean(t), t);
            assert!(!is_envelope(t), "{t}");
        }
    }

    #[test]
    fn a_wake_leaves_the_persons_words() {
        let t = r#"<wake reason="mention" current-time="2026-11-13T08:00:00Z">
  <project id="chan_x" type="project">
    <thread ts="cmsg_A">
      <message trigger="true" from="human" trust="principal" author="Dana" id="cmsg_C">can you also add a test?</message>
    </thread>
  </project>
  <system-note>Thread state: the user last wrote 1m ago. Never reply twice.</system-note>
</wake>"#;
        assert!(is_envelope(t));
        assert_eq!(clean(t), "can you also add a test?");
    }

    #[test]
    fn reminders_go_with_their_content() {
        let t = "<system-reminder>\nNever print env var values.\nAlways use PST.\n</system-reminder>\nRename the export button.";
        assert!(is_envelope(t));
        assert_eq!(clean(t), "Rename the export button.");
        // One that is never closed takes the rest with it.
        assert_eq!(clean("Do it.\n<system-reminder>Never ask."), "Do it.");
    }

    #[test]
    fn an_escaped_envelope_inside_a_relay() {
        let t = "<coordinator-relay>&lt;session-context nonce=&quot;n&quot;&gt;\nProject rules: never merge.\n&lt;/session-context nonce=&quot;n&quot;&gt;\n<relay from=\"coordinator\"><cited author=\"user\">I think ChloeCtx should be simpler</cited>\n<note>Repo x. Don&#39;t touch the Mac.</note></relay></coordinator-relay>";
        assert!(is_envelope(t));
        let c = clean(t);
        assert!(!c.contains("never merge"), "{c}");
        assert!(c.contains("I think ChloeCtx should be simpler"), "{c}");
        assert!(c.contains("Don't touch the Mac."), "{c}");
        assert!(!c.contains('<') && !c.contains("&lt;"), "{c}");
    }

    #[test]
    fn slash_commands_keep_their_name_and_arguments() {
        let t = "<command-message>review is running…</command-message>\n<command-name>/review</command-name>\n<command-args>PR 12</command-args>";
        assert_eq!(clean(t), "/review\nPR 12");
        assert_eq!(clean("<local-command-stdout>Total cost: $0.12</local-command-stdout>"), "");
    }

    #[test]
    fn records_that_mention_a_tag_stay_and_are_defused() {
        use crate::record::{Kind, Record};
        let code = "const zorblaxWrap = (s) => `<system-reminder>${s}</system-reminder>`;";
        let rec = |kind: Kind, text: &str| Record {
            id: "s:1".into(), session: "s".into(), kind, tag: "-".into(), turn: 1, seq: 1, ts: 0, importance: 0.5,
            files: String::new(), text: text.into(), ctx: String::new(), links: Vec::new(),
        };
        assert!(!is_envelope_record(&rec(Kind::ToolCall, code)), "an edit that quotes a tag is the work");
        assert!(!is_envelope_record(&rec(Kind::Note, "Hooks wrap notices in <system-reminder> tags.")));
        assert!(is_envelope_record(&rec(Kind::Prompt, "<system-reminder>x</system-reminder>\nRename it.")), "an old prompt's envelope");
        let d = defuse(code);
        assert_eq!(d, "const zorblaxWrap = (s) => `&lt;system-reminder>${s}&lt;/system-reminder>`;");
        assert!(!d.contains("<system-reminder>"), "{d}");
        assert_eq!(defuse("a < b and <div>"), "a < b and <div>");
    }
}
