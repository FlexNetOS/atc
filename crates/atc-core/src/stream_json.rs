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
            // Return the first meaningful content block.
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

/// Artifacts extracted from a stream-json log.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Artifacts {
    /// PR URLs found in tool_use outputs (gh pr create, etc.)
    pub pr_urls: Vec<String>,
    /// Commit SHAs mentioned in output.
    /// TODO(phase-2): populate via commit SHA extraction from assistant text.
    pub commits: Vec<String>,
    /// The last result event, if any
    pub result: Option<ResultEvent>,
    /// Agent summary text (last assistant text block before result)
    pub summary: Option<String>,
}

/// Extract GitHub PR URLs from a text string.
///
/// Handles plain URLs, markdown links like `[text](url)`, and URLs with
/// trailing punctuation. Searches for the URL prefix within each token
/// rather than requiring it at the start, which catches URLs embedded in
/// surrounding syntax.
fn extract_pr_urls(text: &str, urls: &mut Vec<String>) {
    const PREFIX: &str = "https://github.com/";
    const MARKER: &str = "/pull/";

    for token in text.split_whitespace() {
        // Strip quotes (for JSON-encoded tool inputs)
        let token = token.trim_matches('"');

        // Find the URL prefix anywhere in the token (handles markdown links, parens, etc.)
        let Some(start) = token.find(PREFIX) else {
            continue;
        };
        let candidate = &token[start..];
        if !candidate.contains(MARKER) {
            continue;
        }
        // Trim trailing punctuation (but not `/`)
        let url = candidate.trim_end_matches(|c: char| c.is_ascii_punctuation() && c != '/');
        if !urls.iter().any(|u| u == url) {
            urls.push(url.to_string());
        }
    }
}

/// Extract artifacts from parsed stream events.
/// Used by post-completion (Phase 2A) and stale record recovery (Phase 7D).
pub fn extract_artifacts(log_path: &Path) -> Artifacts {
    use std::io::BufRead;
    let mut artifacts = Artifacts::default();

    let file = match std::fs::File::open(log_path) {
        Ok(f) => f,
        Err(_) => return artifacts,
    };
    let reader = std::io::BufReader::new(file);
    let mut last_text: Option<String> = None;

    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => continue,
        };
        for event in parse_stream_events(&line) {
            match &event {
                StreamEvent::AssistantText(text) => {
                    extract_pr_urls(text, &mut artifacts.pr_urls);
                    last_text = Some(text.clone());
                }
                StreamEvent::ToolUse { input, .. } => {
                    extract_pr_urls(input, &mut artifacts.pr_urls);
                }
                StreamEvent::Result(r) => {
                    artifacts.result = Some(r.clone());
                }
                StreamEvent::Skip => {}
            }
        }
    }

    artifacts.summary = last_text;
    artifacts
}

/// Format a single stream event for human-readable display.
/// Used by logs viewer (Phase 1C).
pub fn format_event(event: &StreamEvent) -> Vec<String> {
    match event {
        StreamEvent::AssistantText(text) => text.lines().map(|l| format!(">>> {l}")).collect(),
        StreamEvent::ToolUse { name, input } => {
            let display_input = if input.chars().count() > 120 {
                let truncated: String = input.chars().take(120).collect();
                format!("{truncated}\u{2026}")
            } else {
                input.clone()
            };
            vec![format!("  [tool] {name}: {display_input}")]
        }
        StreamEvent::Result(r) => {
            let cost = r
                .total_cost_usd
                .map(|c| format!("${c}"))
                .unwrap_or_else(|| "-".to_string());
            let turns = r
                .num_turns
                .map(|t| t.to_string())
                .unwrap_or_else(|| "-".to_string());
            let duration = r
                .duration_ms
                .map(|ms| format!("{}s", ms / 1000))
                .unwrap_or_else(|| "-".to_string());
            vec![
                String::new(),
                format!(
                    "=== RESULT: {} | cost={} | turns={} | duration={} ===",
                    r.subtype, cost, turns, duration
                ),
            ]
        }
        StreamEvent::Skip => vec![],
    }
}

/// Read the last result event from a JSONL log file.
///
/// Scans the file line by line, keeping the last matching result event.
/// Returns `Ok(None)` if the file doesn't exist, is empty, or contains no result events.
pub fn read_last_result(path: &Path) -> anyhow::Result<Option<ResultEvent>> {
    use std::io::BufRead;
    let file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let reader = std::io::BufReader::new(file);
    let mut last_result = None;
    for line in reader.lines() {
        let line = line?;
        if let Some(result) = parse_result_event(&line) {
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
        let line =
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"Hello world"}]}}"#;
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
        let result = read_last_result(Path::new("/tmp/nonexistent-atc-test-file.jsonl")).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn test_read_last_result_empty_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("empty.jsonl");
        std::fs::File::create(&path).unwrap();
        assert!(read_last_result(&path).unwrap().is_none());
    }

    #[test]
    fn test_extract_artifacts_from_log() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.jsonl");
        let content = r#"{"type":"assistant","message":{"content":[{"type":"text","text":"Creating PR at https://github.com/org/repo/pull/42"}]}}
{"type":"assistant","message":{"content":[{"type":"text","text":"Done with the implementation."}]}}
{"type":"result","subtype":"success","total_cost_usd":2.50,"num_turns":5,"duration_ms":30000}
"#;
        std::fs::write(&path, content).unwrap();
        let artifacts = extract_artifacts(&path);
        assert_eq!(
            artifacts.pr_urls,
            vec!["https://github.com/org/repo/pull/42"]
        );
        assert!(artifacts.result.is_some());
        assert_eq!(artifacts.result.unwrap().subtype, "success");
        assert_eq!(
            artifacts.summary.as_deref(),
            Some("Done with the implementation.")
        );
    }

    #[test]
    fn test_extract_artifacts_missing_file() {
        let artifacts = extract_artifacts(Path::new("/tmp/nonexistent-atc-test.jsonl"));
        assert!(artifacts.pr_urls.is_empty());
        assert!(artifacts.result.is_none());
    }

    #[test]
    fn test_format_event_text() {
        let lines = format_event(&StreamEvent::AssistantText("Hello\nWorld".to_string()));
        assert_eq!(lines, vec![">>> Hello", ">>> World"]);
    }

    #[test]
    fn test_format_event_tool_use() {
        let lines = format_event(&StreamEvent::ToolUse {
            name: "Bash".to_string(),
            input: "ls -la".to_string(),
        });
        assert_eq!(lines, vec!["  [tool] Bash: ls -la"]);
    }

    #[test]
    fn test_format_event_result() {
        let lines = format_event(&StreamEvent::Result(ResultEvent {
            subtype: "success".to_string(),
            total_cost_usd: Some(1.23),
            num_turns: Some(10),
            duration_ms: Some(60_000),
        }));
        assert_eq!(lines.len(), 2);
        assert!(lines[1].contains("RESULT: success"));
    }

    #[test]
    fn test_format_event_skip() {
        let lines = format_event(&StreamEvent::Skip);
        assert!(lines.is_empty());
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

    #[test]
    fn test_extract_pr_urls_plain() {
        let mut urls = Vec::new();
        extract_pr_urls("See https://github.com/org/repo/pull/42 for details", &mut urls);
        assert_eq!(urls, vec!["https://github.com/org/repo/pull/42"]);
    }

    #[test]
    fn test_extract_pr_urls_markdown_link() {
        let mut urls = Vec::new();
        extract_pr_urls("[PR](https://github.com/org/repo/pull/42)", &mut urls);
        assert_eq!(urls, vec!["https://github.com/org/repo/pull/42"]);
    }

    #[test]
    fn test_extract_pr_urls_angle_brackets() {
        let mut urls = Vec::new();
        extract_pr_urls("<https://github.com/org/repo/pull/42>", &mut urls);
        assert_eq!(urls, vec!["https://github.com/org/repo/pull/42"]);
    }

    #[test]
    fn test_extract_pr_urls_deduplicates() {
        let mut urls = Vec::new();
        extract_pr_urls(
            "https://github.com/org/repo/pull/42 and https://github.com/org/repo/pull/42",
            &mut urls,
        );
        assert_eq!(urls, vec!["https://github.com/org/repo/pull/42"]);
    }

    #[test]
    fn test_extract_artifacts_strips_trailing_punctuation_from_pr_urls() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.jsonl");
        // PR URL followed by period and comma in prose
        let content = r#"{"type":"assistant","message":{"content":[{"type":"text","text":"Created https://github.com/org/repo/pull/42. Also see https://github.com/org/repo/pull/43,"}]}}
"#;
        std::fs::write(&path, content).unwrap();
        let artifacts = extract_artifacts(&path);
        assert_eq!(
            artifacts.pr_urls,
            vec![
                "https://github.com/org/repo/pull/42",
                "https://github.com/org/repo/pull/43",
            ]
        );
    }
}
