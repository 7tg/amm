//! Token usage and USD cost from Claude Code transcript files (JSONL).
//!
//! Scaffold: the signatures are the contract the dashboard builds against;
//! the bodies are implemented by the "cost" brief.

use serde::Serialize;
use serde_json::Value;
use std::collections::{BTreeMap, HashMap};
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;

/// Token counts summed over the assistant messages of one model.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Usage {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    /// Cache writes with a 5-minute TTL.
    pub cache_write_5m: u64,
    /// Cache writes with a 1-hour TTL.
    pub cache_write_1h: u64,
}

/// Usage of one model inside one transcript.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelUsage {
    pub model: String,
    pub usage: Usage,
    pub cost_usd: f64,
}

/// Reads a transcript and returns usage per model, sorted by model id.
/// A missing file is `Ok(vec![])`; unreadable lines are skipped.
pub fn transcript_usage(path: &Path) -> std::io::Result<Vec<ModelUsage>> {
    let file = match File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    // message.id -> (model, usage); the last occurrence wins.
    let mut by_id: HashMap<String, (String, Usage)> = HashMap::new();
    let mut anonymous: Vec<(String, Usage)> = Vec::new();
    let mut reader = BufReader::new(file);
    let mut buf = Vec::new();
    loop {
        buf.clear();
        if reader.read_until(b'\n', &mut buf)? == 0 {
            break;
        }
        let Ok(v) = serde_json::from_slice::<Value>(&buf) else {
            continue;
        };
        if v.get("type").and_then(Value::as_str) != Some("assistant") {
            continue;
        }
        let Some(msg) = v.get("message") else {
            continue;
        };
        let Some(model) = msg.get("model").and_then(Value::as_str) else {
            continue;
        };
        if model == "<synthetic>" {
            continue;
        }
        let Some(u) = msg.get("usage") else {
            continue;
        };
        let usage = parse_usage(u);
        match msg.get("id").and_then(Value::as_str) {
            Some(id) => {
                by_id.insert(id.to_string(), (model.to_string(), usage));
            }
            None => anonymous.push((model.to_string(), usage)),
        }
    }
    let mut totals: BTreeMap<String, Usage> = BTreeMap::new();
    for (model, u) in by_id.into_values().chain(anonymous) {
        let t = totals.entry(model).or_default();
        t.input += u.input;
        t.output += u.output;
        t.cache_read += u.cache_read;
        t.cache_write_5m += u.cache_write_5m;
        t.cache_write_1h += u.cache_write_1h;
    }
    Ok(totals
        .into_iter()
        .map(|(model, usage)| {
            let cost_usd = cost_usd(&model, &usage);
            ModelUsage {
                model,
                usage,
                cost_usd,
            }
        })
        .collect())
}

fn parse_usage(u: &Value) -> Usage {
    let n = |v: &Value, k: &str| v.get(k).and_then(Value::as_u64).unwrap_or(0);
    let total_write = n(u, "cache_creation_input_tokens");
    let (w5, w1) = match u.get("cache_creation") {
        Some(c) if c.is_object() => (
            n(c, "ephemeral_5m_input_tokens"),
            n(c, "ephemeral_1h_input_tokens"),
        ),
        _ => (total_write, 0),
    };
    Usage {
        input: n(u, "input_tokens"),
        output: n(u, "output_tokens"),
        cache_read: n(u, "cache_read_input_tokens"),
        cache_write_5m: w5,
        cache_write_1h: w1,
    }
}

/// (id, input, output, cache read) in USD per million tokens.
const PRICES: &[(&str, f64, f64, f64)] = &[
    ("claude-fable-5-1", 10.0, 50.0, 0.25),
    ("claude-fable-5", 10.0, 50.0, 0.25),
    ("claude-mythos-5-1", 10.0, 50.0, 0.25),
    ("claude-mythos-5", 10.0, 50.0, 0.25),
    ("claude-opus-5-5", 4.0, 20.0, 0.20),
    ("claude-opus-5", 5.0, 25.0, 0.50),
    ("claude-opus-4-8", 5.0, 25.0, 0.50),
    ("claude-opus-4-7", 5.0, 25.0, 0.50),
    ("claude-opus-4-6", 5.0, 25.0, 0.50),
    ("claude-sonnet-5-5", 2.0, 10.0, 0.20),
    ("claude-sonnet-5", 2.0, 10.0, 0.20),
    ("claude-sonnet-4-6", 3.0, 15.0, 0.30),
    ("claude-haiku-4-5", 1.0, 5.0, 0.10),
];

fn price_of(model: &str) -> Option<(f64, f64, f64)> {
    PRICES
        .iter()
        .filter(|(id, ..)| {
            model.strip_prefix(id).is_some_and(|rest| {
                rest.is_empty()
                    || rest
                        .strip_prefix('-')
                        .is_some_and(|r| r.starts_with(|c: char| c.is_ascii_digit()))
            })
        })
        .max_by_key(|(id, ..)| id.len())
        .map(|&(_, i, o, r)| (i, o, r))
}

/// USD cost of `usage` on `model` (a model id, possibly date-suffixed).
/// Unknown models cost 0.0.
pub fn cost_usd(model: &str, usage: &Usage) -> f64 {
    let Some((input, output, read)) = price_of(model) else {
        return 0.0;
    };
    (usage.input as f64 * input
        + usage.output as f64 * output
        + usage.cache_read as f64 * read
        + usage.cache_write_5m as f64 * input * 1.25
        + usage.cache_write_1h as f64 * input * 2.0)
        / 1_000_000.0
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    static N: AtomicU32 = AtomicU32::new(0);

    fn write_fixture(content: &str) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!(
            "amm-cost-test-{}-{}.jsonl",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        ));
        std::fs::write(&p, content).unwrap();
        p
    }

    fn line(id: &str, model: &str, usage: &str) -> String {
        format!(
            r#"{{"type":"assistant","message":{{"id":"{id}","model":"{model}","usage":{usage}}}}}"#
        )
    }

    #[test]
    fn dedupes_by_id_keeping_last() {
        let c = [
            line("m1", "claude-sonnet-5", r#"{"input_tokens":1,"output_tokens":2}"#),
            line("m1", "claude-sonnet-5", r#"{"input_tokens":10,"output_tokens":20}"#),
        ]
        .join("\n");
        let p = write_fixture(&c);
        let r = transcript_usage(&p).unwrap();
        std::fs::remove_file(&p).unwrap();
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].usage.input, 10);
        assert_eq!(r[0].usage.output, 20);
    }

    #[test]
    fn cache_split_and_fallback() {
        let c = [
            line(
                "a",
                "claude-sonnet-5",
                r#"{"cache_creation_input_tokens":300,"cache_creation":{"ephemeral_5m_input_tokens":100,"ephemeral_1h_input_tokens":200}}"#,
            ),
            line(
                "b",
                "claude-sonnet-5",
                r#"{"cache_creation_input_tokens":50,"cache_read_input_tokens":7}"#,
            ),
        ]
        .join("\n");
        let p = write_fixture(&c);
        let r = transcript_usage(&p).unwrap();
        std::fs::remove_file(&p).unwrap();
        assert_eq!(r[0].usage.cache_write_5m, 150);
        assert_eq!(r[0].usage.cache_write_1h, 200);
        assert_eq!(r[0].usage.cache_read, 7);
    }

    #[test]
    fn multiple_models_sorted_and_costed() {
        let c = [
            line("a", "claude-sonnet-5", r#"{"input_tokens":1000000}"#),
            line("b", "claude-haiku-4-5-20251001", r#"{"input_tokens":1000000}"#),
            line("c", "claude-sonnet-5", r#"{"input_tokens":1000000}"#),
        ]
        .join("\n");
        let p = write_fixture(&c);
        let r = transcript_usage(&p).unwrap();
        std::fs::remove_file(&p).unwrap();
        assert_eq!(r.len(), 2);
        assert_eq!(r[0].model, "claude-haiku-4-5-20251001");
        assert_eq!(r[1].model, "claude-sonnet-5");
        assert_eq!(r[1].usage.input, 2_000_000);
        assert!((r[1].cost_usd - 4.0).abs() < 1e-9);
        assert!((r[0].cost_usd - 1.0).abs() < 1e-9);
    }

    #[test]
    fn skips_synthetic_and_garbage() {
        let c = [
            line("s", "<synthetic>", r#"{"input_tokens":99}"#),
            "not json".to_string(),
            r#"{"type":"user","message":{"id":"u","model":"claude-sonnet-5","usage":{"input_tokens":5}}}"#.to_string(),
            "".to_string(),
            line("ok", "claude-sonnet-5", r#"{"input_tokens":3}"#),
        ]
        .join("\n");
        let p = write_fixture(&c);
        let r = transcript_usage(&p).unwrap();
        std::fs::remove_file(&p).unwrap();
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].usage.input, 3);
    }

    #[test]
    fn missing_file_is_empty() {
        let p = std::env::temp_dir().join("amm-cost-test-does-not-exist.jsonl");
        assert_eq!(transcript_usage(&p).unwrap(), vec![]);
    }

    #[test]
    fn price_math() {
        let u = Usage {
            input: 1_000_000,
            output: 1_000_000,
            cache_read: 1_000_000,
            cache_write_5m: 1_000_000,
            cache_write_1h: 1_000_000,
        };
        // haiku: 1 + 5 + 0.10 + 1.25 + 2
        assert!((cost_usd("claude-haiku-4-5-20251001", &u) - 9.35).abs() < 1e-9);
        // opus 5.5: 4 + 20 + 0.20 + 5 + 8
        assert!((cost_usd("claude-opus-5-5-20260101", &u) - 37.2).abs() < 1e-9);
        // opus 5: 5 + 25 + 0.5 + 6.25 + 10
        assert!((cost_usd("claude-opus-5", &u) - 46.75).abs() < 1e-9);
        assert!((cost_usd("claude-opus-5-20260101", &u) - 46.75).abs() < 1e-9);
    }

    #[test]
    fn unknown_model_is_zero() {
        let u = Usage {
            input: 1_000_000,
            ..Usage::default()
        };
        assert_eq!(cost_usd("gpt-9", &u), 0.0);
        assert_eq!(cost_usd("claude-haiku-4-50", &u), 0.0);
    }
}
