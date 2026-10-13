//! Long-conversation memory benchmarks (LongMemEval, LoCoMo) as converted by
//! scripts/convert_memory_benchmarks.py: one JSON line per unit, a
//! conversation with the questions asked about it.

use std::path::Path;

use serde::Deserialize;

#[derive(Clone, Debug, Deserialize)]
pub struct Turn {
    pub turn_id: String,
    pub session_id: String,
    pub role: String,
    pub text: String,
    pub timestamp: Option<String>,
    #[serde(default)]
    pub image_caption: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Question {
    pub qid: String,
    pub question: String,
    #[serde(default)]
    pub answer: serde_json::Value,
    pub question_type: String,
    #[serde(default)]
    pub question_date: Option<String>,
    pub evidence_turn_ids: Vec<String>,
    pub evidence_session_ids: Vec<String>,
    pub abstention: bool,
}

impl Question {
    pub fn answer_text(&self) -> String {
        match &self.answer {
            serde_json::Value::String(s) => s.clone(),
            serde_json::Value::Null => String::new(),
            v => v.to_string(),
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
pub struct Unit {
    pub dataset: String,
    pub unit_id: String,
    pub turns: Vec<Turn>,
    pub questions: Vec<Question>,
    /// The turn each turn answers, when it is not simply the previous one
    /// (coding sessions: a tool call or result answers the turn's prompt).
    #[serde(default)]
    pub requests: Vec<Option<usize>>,
}

impl Unit {
    pub fn is_locomo(&self) -> bool {
        self.dataset == "locomo"
    }

    /// A turn as stored: LoCoMo turns carry their speaker, and shared
    /// images their caption.
    pub fn turn_text(&self, i: usize) -> String {
        let t = &self.turns[i];
        let mut text = if self.is_locomo() { format!("{}: {}", t.role, t.text) } else { t.text.clone() };
        if let Some(c) = &t.image_caption {
            text += &format!(" [shares an image: {c}]");
        }
        text
    }

    /// The turn this one answers (an assistant turn's request; in LoCoMo,
    /// the previous turn of the session).
    pub fn request_of(&self, i: usize) -> Option<usize> {
        if !self.requests.is_empty() {
            return self.requests[i];
        }
        if self.turns[i].role == "user" || i == 0 {
            return None;
        }
        (self.turns[i - 1].session_id == self.turns[i].session_id).then_some(i - 1)
    }

    /// The other side of the exchange: the request of a response, or the
    /// response to a request.
    pub fn partner_of(&self, i: usize) -> Option<usize> {
        if !self.requests.is_empty() {
            return self.requests[i];
        }
        if self.turns[i].role == "user" {
            let j = i + 1;
            return (j < self.turns.len() && self.turns[j].session_id == self.turns[i].session_id).then_some(j);
        }
        self.request_of(i)
    }
}

/// Reads every `every`-th unit, at most `limit` of them.
pub fn read_units(path: &Path, every: usize, limit: Option<usize>) -> std::io::Result<Vec<Unit>> {
    let text = std::fs::read_to_string(path)?;
    let mut out = Vec::new();
    for (i, l) in text.lines().filter(|l| !l.trim().is_empty()).enumerate() {
        if i % every.max(1) != 0 {
            continue;
        }
        out.push(serde_json::from_str(l).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?);
        if limit.is_some_and(|n| out.len() >= n) {
            break;
        }
    }
    Ok(out)
}

/// Unix seconds of a LongMemEval ("2023/05/20 (Sat) 02:21") or LoCoMo
/// ("1:56 pm on 8 May, 2023") timestamp; 0 when unparseable.
pub fn parse_time(s: &str) -> i64 {
    let civil = |y: i64, m: i64, d: i64| -> i64 {
        let y2 = if m <= 2 { y - 1 } else { y };
        let era = y2.div_euclid(400);
        let yoe = y2 - era * 400;
        let doy = (153 * ((m + 9) % 12) + 2) / 5 + d - 1;
        era * 146_097 + yoe * 365 + yoe / 4 - yoe / 100 + doy - 719_468
    };
    let digits: Vec<i64> = s.split(|c: char| !c.is_ascii_digit()).filter(|x| !x.is_empty()).filter_map(|x| x.parse().ok()).collect();
    if s.contains('/') && digits.len() >= 5 {
        return civil(digits[0], digits[1], digits[2]) * 86_400 + digits[3] * 3600 + digits[4] * 60;
    }
    const MONTHS: [&str; 12] = ["jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec"];
    let low = s.to_lowercase();
    if let Some(m) = MONTHS.iter().position(|m| low.contains(&format!(" {m}"))) {
        if digits.len() >= 4 {
            let (mut h, mi, d, y) = (digits[0], digits[1], digits[2], digits[3]);
            if low.contains("pm") && h < 12 {
                h += 12;
            }
            if low.contains("am") && h == 12 {
                h = 0;
            }
            return civil(y, m as i64 + 1, d) * 86_400 + h * 3600 + mi * 60;
        }
    }
    0
}
