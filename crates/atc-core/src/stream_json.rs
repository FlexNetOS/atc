//! JSONL result-event parser for Claude stream-json output.
//!
//! Shared between `retry` (reads last result to classify failure)
//! and `logs` (displays result events).

use serde::Deserialize;
use std::io::BufRead;
use std::path::Path;

/// A parsed result event from Claude's stream-json output.
#[derive(Debug, Clone, Deserialize)]
pub struct ResultEvent {
    pub subtype: String,
    #[serde(default)]
    pub total_cost_usd: Option<f64>,
    #[serde(default)]
    pub num_turns: Option<u32>,
    #[serde(default)]
    pub duration_ms: Option<u64>,
}

/// Intermediate struct for parsing a JSONL line to check if it's a result event.
#[derive(Deserialize)]
struct RawEvent {
    #[serde(rename = "type")]
    event_type: Option<String>,
    subtype: Option<String>,
    total_cost_usd: Option<f64>,
    num_turns: Option<u32>,
    duration_ms: Option<u64>,
}

/// Parse a single JSONL line, returning `Some(ResultEvent)` if it is a result event.
pub fn parse_result_event(line: &str) -> Option<ResultEvent> {
    let raw: RawEvent = serde_json::from_str(line).ok()?;
    if raw.event_type.as_deref() == Some("result") {
        Some(ResultEvent {
            subtype: raw.subtype.unwrap_or_default(),
            total_cost_usd: raw.total_cost_usd,
            num_turns: raw.num_turns,
            duration_ms: raw.duration_ms,
        })
    } else {
        None
    }
}

/// Read the last result event from a JSONL log file.
///
/// Scans the file line by line, keeping the last matching result event.
/// Returns `Ok(None)` if the file doesn't exist, is empty, or contains no result events.
pub fn read_last_result(path: &Path) -> anyhow::Result<Option<ResultEvent>> {
    let file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };

    let reader = std::io::BufReader::new(file);
    let mut last_result: Option<ResultEvent> = None;

    for line in reader.lines() {
        let line = line?;
        if let Some(event) = parse_result_event(&line) {
            last_result = Some(event);
        }
    }

    Ok(last_result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn test_parse_result_event_valid() {
        let line = r#"{"type":"result","subtype":"success","total_cost_usd":1.23,"num_turns":5,"duration_ms":30000}"#;
        let event = parse_result_event(line).unwrap();
        assert_eq!(event.subtype, "success");
        assert_eq!(event.total_cost_usd, Some(1.23));
        assert_eq!(event.num_turns, Some(5));
        assert_eq!(event.duration_ms, Some(30000));
    }

    #[test]
    fn test_parse_result_event_error_subtype() {
        let line = r#"{"type":"result","subtype":"error"}"#;
        let event = parse_result_event(line).unwrap();
        assert_eq!(event.subtype, "error");
        assert_eq!(event.total_cost_usd, None);
    }

    #[test]
    fn test_parse_result_event_not_result() {
        let line = r#"{"type":"assistant","subtype":"text"}"#;
        assert!(parse_result_event(line).is_none());
    }

    #[test]
    fn test_parse_result_event_invalid_json() {
        assert!(parse_result_event("not json").is_none());
        assert!(parse_result_event("").is_none());
    }

    #[test]
    fn test_parse_result_event_missing_type() {
        let line = r#"{"subtype":"success"}"#;
        assert!(parse_result_event(line).is_none());
    }

    #[test]
    fn test_read_last_result_from_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.jsonl");
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(f, r#"{{"type":"assistant","subtype":"text"}}"#).unwrap();
        writeln!(
            f,
            r#"{{"type":"result","subtype":"success","total_cost_usd":0.5}}"#
        )
        .unwrap();
        writeln!(
            f,
            r#"{{"type":"result","subtype":"error","total_cost_usd":1.0}}"#
        )
        .unwrap();

        let event = read_last_result(&path).unwrap().unwrap();
        assert_eq!(event.subtype, "error");
        assert_eq!(event.total_cost_usd, Some(1.0));
    }

    #[test]
    fn test_read_last_result_no_results() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.jsonl");
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(f, r#"{{"type":"assistant","subtype":"text"}}"#).unwrap();

        assert!(read_last_result(&path).unwrap().is_none());
    }

    #[test]
    fn test_read_last_result_missing_file() {
        let result = read_last_result(Path::new("/tmp/nonexistent-atc-test.jsonl")).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn test_read_last_result_empty_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("empty.jsonl");
        std::fs::File::create(&path).unwrap();

        assert!(read_last_result(&path).unwrap().is_none());
    }
}
