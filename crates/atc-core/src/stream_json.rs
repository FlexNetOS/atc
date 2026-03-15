//! Parser for Claude's `--output-format stream-json` JSONL output.
//!
//! Used by `atc logs` for rendering and by `atc retry` for reading result events.

use serde::Deserialize;
use std::path::Path;

/// A result event from a Claude stream-json log.
#[derive(Debug, Clone, PartialEq)]
pub struct ResultEvent {
    pub subtype: String,
    pub total_cost_usd: Option<f64>,
    pub num_turns: Option<u32>,
    pub duration_ms: Option<u64>,
}

/// Parsed stream event for rendering.
#[derive(Debug, Clone, PartialEq)]
pub enum StreamEvent {
    /// Assistant text content block.
    AssistantText(String),
    /// Assistant tool_use content block.
    ToolUse { name: String, input: String },
    /// Result summary event.
    Result(ResultEvent),
    /// Events we skip (user, system, tool_result).
    Skip,
}

// --- Internal deserialization types ---

#[derive(Deserialize)]
struct RawEvent {
    #[serde(rename = "type")]
    event_type: String,
    #[serde(default)]
    message: Option<RawMessage>,
    // Result fields (flat at top level)
    #[serde(default)]
    subtype: Option<String>,
    #[serde(default)]
    total_cost_usd: Option<f64>,
    #[serde(default)]
    num_turns: Option<u32>,
    #[serde(default)]
    duration_ms: Option<u64>,
}

#[derive(Deserialize)]
struct RawMessage {
    #[serde(default)]
    content: Vec<RawContentBlock>,
}

#[derive(Deserialize)]
struct RawContentBlock {
    #[serde(rename = "type")]
    block_type: String,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    input: Option<serde_json::Value>,
}

/// Parse a single JSONL line into a `StreamEvent`.
/// Returns `None` for invalid JSON lines (skipped silently).
pub fn parse_stream_event(line: &str) -> Option<StreamEvent> {
    let raw: RawEvent = serde_json::from_str(line).ok()?;

    match raw.event_type.as_str() {
        "assistant" => {
            let message = raw.message?;
            // Return events for each content block.
            // For simplicity, we return the first meaningful block.
            // The caller iterates over blocks via parse_stream_events.
            for block in &message.content {
                match block.block_type.as_str() {
                    "text" => {
                        if let Some(text) = &block.text {
                            return Some(StreamEvent::AssistantText(text.clone()));
                        }
                    }
                    "tool_use" => {
                        let name = block.name.clone().unwrap_or_default();
                        let input = block
                            .input
                            .as_ref()
                            .map(|v| serde_json::to_string(v).unwrap_or_default())
                            .unwrap_or_default();
                        return Some(StreamEvent::ToolUse { name, input });
                    }
                    _ => {}
                }
            }
            // Assistant event with no recognized content blocks
            Some(StreamEvent::Skip)
        }
        "result" => {
            let subtype = raw.subtype.unwrap_or_else(|| "unknown".to_string());
            Some(StreamEvent::Result(ResultEvent {
                subtype,
                total_cost_usd: raw.total_cost_usd,
                num_turns: raw.num_turns,
                duration_ms: raw.duration_ms,
            }))
        }
        _ => Some(StreamEvent::Skip),
    }
}

/// Parse all stream events from a single JSONL line.
/// An assistant message may contain multiple content blocks.
/// Returns an empty vec for invalid JSON or skip events.
pub fn parse_stream_events(line: &str) -> Vec<StreamEvent> {
    let raw: RawEvent = match serde_json::from_str(line) {
        Ok(r) => r,
        Err(_) => return vec![],
    };

    match raw.event_type.as_str() {
        "assistant" => {
            let message = match raw.message {
                Some(m) => m,
                None => return vec![],
            };
            let mut events = Vec::new();
            for block in &message.content {
                match block.block_type.as_str() {
                    "text" => {
                        if let Some(text) = &block.text {
                            events.push(StreamEvent::AssistantText(text.clone()));
                        }
                    }
                    "tool_use" => {
                        let name = block.name.clone().unwrap_or_default();
                        let input = block
                            .input
                            .as_ref()
                            .map(|v| serde_json::to_string(v).unwrap_or_default())
                            .unwrap_or_default();
                        events.push(StreamEvent::ToolUse { name, input });
                    }
                    _ => {}
                }
            }
            events
        }
        "result" => {
            let subtype = raw.subtype.unwrap_or_else(|| "unknown".to_string());
            vec![StreamEvent::Result(ResultEvent {
                subtype,
                total_cost_usd: raw.total_cost_usd,
                num_turns: raw.num_turns,
                duration_ms: raw.duration_ms,
            })]
        }
        _ => vec![],
    }
}

/// Parse a single line looking only for a result event.
/// Returns `None` if the line is not a result event or is invalid JSON.
pub fn parse_result_event(line: &str) -> Option<ResultEvent> {
    match parse_stream_event(line)? {
        StreamEvent::Result(r) => Some(r),
        _ => None,
    }
}

/// Read the last result event from a JSONL log file.
/// Scans from end to find the final result event.
pub fn read_last_result(path: &Path) -> anyhow::Result<Option<ResultEvent>> {
    let contents = std::fs::read_to_string(path)?;
    let mut last_result = None;
    for line in contents.lines() {
        if let Some(result) = parse_result_event(line) {
            last_result = Some(result);
        }
    }
    Ok(last_result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_assistant_text() {
        let line = r#"{"type":"assistant","message":{"content":[{"type":"text","text":"Hello world"}]}}"#;
        let event = parse_stream_event(line).unwrap();
        assert_eq!(event, StreamEvent::AssistantText("Hello world".to_string()));
    }

    #[test]
    fn test_parse_assistant_tool_use() {
        let line = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Bash","input":{"command":"ls -la"}}]}}"#;
        let event = parse_stream_event(line).unwrap();
        match event {
            StreamEvent::ToolUse { name, input } => {
                assert_eq!(name, "Bash");
                assert!(input.contains("ls -la"));
            }
            other => panic!("expected ToolUse, got {:?}", other),
        }
    }

    #[test]
    fn test_parse_result_event() {
        let line = r#"{"type":"result","subtype":"success","total_cost_usd":1.23,"num_turns":10,"duration_ms":60000}"#;
        let event = parse_stream_event(line).unwrap();
        match event {
            StreamEvent::Result(r) => {
                assert_eq!(r.subtype, "success");
                assert_eq!(r.total_cost_usd, Some(1.23));
                assert_eq!(r.num_turns, Some(10));
                assert_eq!(r.duration_ms, Some(60000));
            }
            other => panic!("expected Result, got {:?}", other),
        }
    }

    #[test]
    fn test_parse_skip_events() {
        for event_type in &["user", "system", "tool_result"] {
            let line = format!(r#"{{"type":"{}","content":"ignored"}}"#, event_type);
            let event = parse_stream_event(&line).unwrap();
            assert_eq!(event, StreamEvent::Skip);
        }
    }

    #[test]
    fn test_parse_invalid_json_returns_none() {
        assert!(parse_stream_event("not json at all").is_none());
        assert!(parse_stream_event("{broken").is_none());
        assert!(parse_stream_event("").is_none());
    }

    #[test]
    fn test_parse_result_event_helper() {
        let line = r#"{"type":"result","subtype":"error_max_turns","total_cost_usd":5.0,"num_turns":100,"duration_ms":300000}"#;
        let result = parse_result_event(line).unwrap();
        assert_eq!(result.subtype, "error_max_turns");
        assert_eq!(result.total_cost_usd, Some(5.0));
    }

    #[test]
    fn test_parse_result_event_on_non_result_returns_none() {
        let line = r#"{"type":"assistant","message":{"content":[{"type":"text","text":"hi"}]}}"#;
        assert!(parse_result_event(line).is_none());
    }

    #[test]
    fn test_parse_stream_events_multiple_blocks() {
        let line = r#"{"type":"assistant","message":{"content":[{"type":"text","text":"Hello"},{"type":"tool_use","name":"Read","input":{"path":"/tmp"}}]}}"#;
        let events = parse_stream_events(line);
        assert_eq!(events.len(), 2);
        assert_eq!(events[0], StreamEvent::AssistantText("Hello".to_string()));
        match &events[1] {
            StreamEvent::ToolUse { name, .. } => assert_eq!(name, "Read"),
            other => panic!("expected ToolUse, got {:?}", other),
        }
    }

    #[test]
    fn test_read_last_result_from_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.jsonl");
        let content = r#"{"type":"assistant","message":{"content":[{"type":"text","text":"working..."}]}}
{"type":"result","subtype":"success","total_cost_usd":2.50,"num_turns":5,"duration_ms":30000}
"#;
        std::fs::write(&path, content).unwrap();
        let result = read_last_result(&path).unwrap().unwrap();
        assert_eq!(result.subtype, "success");
        assert_eq!(result.total_cost_usd, Some(2.50));
    }

    #[test]
    fn test_read_last_result_no_result_event() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.jsonl");
        let content = r#"{"type":"assistant","message":{"content":[{"type":"text","text":"working..."}]}}
"#;
        std::fs::write(&path, content).unwrap();
        let result = read_last_result(&path).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn test_read_last_result_missing_file() {
        let result = read_last_result(Path::new("/tmp/nonexistent-atc-test-file.jsonl"));
        assert!(result.is_err());
    }

    #[test]
    fn test_result_with_missing_optional_fields() {
        let line = r#"{"type":"result","subtype":"success"}"#;
        let result = parse_result_event(line).unwrap();
        assert_eq!(result.subtype, "success");
        assert_eq!(result.total_cost_usd, None);
        assert_eq!(result.num_turns, None);
        assert_eq!(result.duration_ms, None);
    }
}
