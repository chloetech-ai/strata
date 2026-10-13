//! Removes common credential formats before anything is stored.

use std::sync::OnceLock;

use regex::Regex;

fn patterns() -> &'static [Regex] {
    static P: OnceLock<Vec<Regex>> = OnceLock::new();
    P.get_or_init(|| {
        [
            r"-----BEGIN [A-Z ]*PRIVATE KEY-----[\s\S]*?-----END [A-Z ]*PRIVATE KEY-----",
            r"\bAKIA[0-9A-Z]{16}\b",
            r"\bgh[pousr]_[A-Za-z0-9]{30,}\b",
            r"\bgithub_pat_[A-Za-z0-9_]{22,}\b",
            r"\bsk-(?:ant-|proj-)?[A-Za-z0-9_\-]{20,}",
            r"\bxox[abprs]-[A-Za-z0-9\-]{10,}",
            r"\bAIza[0-9A-Za-z_\-]{35}\b",
            r"(?i)\bbearer\s+[A-Za-z0-9._~+/=\-]{20,}",
            r#"(?i)\b(api[_-]?key|secret|token|password|passwd|private[_-]?key|access[_-]?key)(\s*[:=]\s*)["']?[^\s"'`,;]{8,}"#,
        ]
        .iter()
        .map(|p| Regex::new(p).unwrap())
        .collect()
    })
}

/// `text` with credentials replaced by `[REDACTED]`.
pub fn redact(text: &str) -> String {
    let mut out = std::borrow::Cow::Borrowed(text);
    for (i, re) in patterns().iter().enumerate() {
        if re.is_match(&out) {
            let rep = if i + 1 == patterns().len() { "$1$2[REDACTED]" } else { "[REDACTED]" };
            out = std::borrow::Cow::Owned(re.replace_all(&out, rep).into_owned());
        }
    }
    out.into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_keys_and_keeps_prose() {
        let t = "export ANTHROPIC_API_KEY=sk-ant-api03-abcdefghijklmnopqrstuvwxyz0123 and token: ghp_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let r = redact(t);
        assert!(!r.contains("abcdefghijklmnop"), "{r}");
        assert!(!r.contains("ghp_aaaa"), "{r}");
        assert_eq!(redact("we use a token bucket for rate limits"), "we use a token bucket for rate limits");
        assert!(redact("password = hunter2hunter2").contains("password = [REDACTED]"));
    }
}
