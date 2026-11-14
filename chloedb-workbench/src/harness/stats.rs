//! Comparing two versions' sessions honestly: a difference is shown only
//! when it is bigger than the noise of the counts, and "no detectable
//! difference" is a result, with the smallest effect the counts could have
//! shown (80% power, two-sided 5%).

use serde_json::{json, Value as Json};

/// z for a two-sided 5% test, plus z for 80% power.
const Z_ALPHA: f64 = 1.96;
const Z_POWER: f64 = 0.84;
/// Fewer sessions than this on either side: too few to compare.
pub const MIN_SESSIONS: usize = 3;

/// Two shares (`k` of `n` sessions each): the difference in points, and
/// whether it is bigger than the noise.
pub fn shares(a_k: usize, a_n: usize, b_k: usize, b_n: usize) -> Json {
    if a_n < MIN_SESSIONS || b_n < MIN_SESSIONS {
        return json!({ "result": "too_few", "a_n": a_n, "b_n": b_n, "min": MIN_SESSIONS });
    }
    let (pa, pb) = (a_k as f64 / a_n as f64, b_k as f64 / b_n as f64);
    let pooled = (a_k + b_k) as f64 / (a_n + b_n) as f64;
    // A pooled share of 0 or 1 has no spread: plan with the widest.
    let p = if pooled <= 0.0 || pooled >= 1.0 { 0.5 } else { pooled };
    let se = (p * (1.0 - p) * (1.0 / a_n as f64 + 1.0 / b_n as f64)).sqrt();
    let diff = pb - pa;
    let detectable = ((Z_ALPHA + Z_POWER) * se * 100.0).round();
    let real = pooled > 0.0 && pooled < 1.0 && (diff / se).abs() > Z_ALPHA;
    json!({
        "result": if !real { "no_detectable_difference" } else if diff > 0.0 { "higher" } else { "lower" },
        "a": (pa * 1000.0).round() / 10.0, "b": (pb * 1000.0).round() / 10.0,
        "diff_points": (diff * 1000.0).round() / 10.0,
        "detectable_points": detectable.min(100.0),
        "a_n": a_n, "b_n": b_n,
    })
}

/// Two rates (`total` events over `n` sessions each), as Poisson counts.
pub fn rates(a_total: usize, a_n: usize, b_total: usize, b_n: usize) -> Json {
    if a_n < MIN_SESSIONS || b_n < MIN_SESSIONS {
        return json!({ "result": "too_few", "a_n": a_n, "b_n": b_n, "min": MIN_SESSIONS });
    }
    let (ra, rb) = (a_total as f64 / a_n as f64, b_total as f64 / b_n as f64);
    // The variance of a mean of Poisson counts is rate / n; a rate of 0
    // plans with one event in the window.
    let va = ra.max(1.0 / a_n as f64) / a_n as f64;
    let vb = rb.max(1.0 / b_n as f64) / b_n as f64;
    let se = (va + vb).sqrt();
    let diff = rb - ra;
    let real = (diff / se).abs() > Z_ALPHA;
    json!({
        "result": if !real { "no_detectable_difference" } else if diff > 0.0 { "higher" } else { "lower" },
        "a": (ra * 100.0).round() / 100.0, "b": (rb * 100.0).round() / 100.0,
        "diff": (diff * 100.0).round() / 100.0,
        "detectable": ((Z_ALPHA + Z_POWER) * se * 100.0).round() / 100.0,
        "a_n": a_n, "b_n": b_n,
    })
}

/// One sentence for a comparison, `what` being "done by checks" or
/// "prompts per session".
pub fn sentence(c: &Json, what: &str, a: &str, b: &str) -> String {
    let n = |k: &str| c[k].as_u64().unwrap_or(0);
    match c["result"].as_str().unwrap_or("") {
        "too_few" => format!("Too few sessions to compare {what}: {} on {a}, {} on {b}; {} each are needed.", n("a_n"), n("b_n"), MIN_SESSIONS),
        "no_detectable_difference" => {
            let size = c["detectable_points"].as_f64().map(|p| format!("{p:.0} points")).or_else(|| c["detectable"].as_f64().map(|d| format!("{d:.2}"))).unwrap_or_default();
            format!("No detectable difference in {what} between {a} and {b}. With {} and {} sessions, only a difference of about {size} or more would show.", n("a_n"), n("b_n"))
        }
        r => {
            let d = c["diff_points"].as_f64().map(|p| format!("{:+.0} points", p)).or_else(|| c["diff"].as_f64().map(|d| format!("{d:+.2}"))).unwrap_or_default();
            format!("{b} is {r} on {what} than {a}: {d}, bigger than the noise of {} and {} sessions.", n("a_n"), n("b_n"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small_samples_show_no_detectable_difference() {
        // 7 of 12 against 9 of 12: not detectable, and the size it could
        // detect is large.
        let c = shares(7, 12, 9, 12);
        assert_eq!(c["result"], "no_detectable_difference", "{c}");
        assert!(c["detectable_points"].as_f64().unwrap() > 40.0, "{c}");
        assert!(sentence(&c, "done by checks", "1.0", "1.1").contains("only a difference of about"));
    }

    #[test]
    fn large_differences_show() {
        let c = shares(10, 100, 60, 100);
        assert_eq!(c["result"], "higher", "{c}");
        let r = rates(300, 30, 30, 30);
        assert_eq!(r["result"], "lower", "{r}");
        assert_eq!(shares(1, 2, 2, 2)["result"], "too_few");
    }
}
