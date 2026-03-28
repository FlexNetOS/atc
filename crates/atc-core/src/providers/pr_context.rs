use async_trait::async_trait;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tracing::{info, warn};

use super::{ContextOutput, ContextProvider, DispatchContext};

/// Timeout for gh subprocess calls (REST API, GraphQL, pr view).
const GH_TIMEOUT: Duration = Duration::from_secs(30);

/// Provider that prefetches PR data and generates triage/summary files.
#[derive(Default)]
pub struct PrContextProvider;

impl PrContextProvider {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl ContextProvider for PrContextProvider {
    fn name(&self) -> &str {
        "pr-context"
    }

    fn declared_template_vars(&self) -> &[&str] {
        &["prefetch"]
    }

    async fn prepare(&self, ctx: &DispatchContext) -> anyhow::Result<ContextOutput> {
        // Determine PR URL: explicit or from params
        let pr_url = ctx
            .pr_url
            .as_deref()
            .or_else(|| ctx.params.get("pr").map(|s| s.as_str()));

        let pr_url = match pr_url {
            Some(url) => url.to_string(),
            None => {
                // No PR URL — no-op
                return Ok(ContextOutput::default());
            }
        };

        info!(pr_url = %pr_url, "pr-context: fetching PR data");

        // Parse owner/repo/number from URL
        let (owner, repo, pr_number) = parse_pr_url(&pr_url)?;

        // Fetch all PR data concurrently
        let (metadata, comments, reviews, threads) = tokio::join!(
            fetch_pr_metadata(&pr_url),
            fetch_review_comments(&owner, &repo, pr_number),
            fetch_reviews(&owner, &repo, pr_number),
            fetch_review_threads(&owner, &repo, pr_number),
        );

        let metadata = metadata.unwrap_or_else(|e| {
            warn!(error = %e, "failed to fetch PR metadata");
            Value::Null
        });
        let comments = comments.unwrap_or_else(|e| {
            warn!(error = %e, "failed to fetch review comments");
            Value::Array(vec![])
        });
        let reviews = reviews.unwrap_or_else(|e| {
            warn!(error = %e, "failed to fetch reviews");
            Value::Array(vec![])
        });
        let threads = threads.unwrap_or_else(|e| {
            warn!(error = %e, "failed to fetch review threads");
            Value::Null
        });

        // Generate triage.md
        let triage = generate_triage(&comments, &threads, &owner, &repo, pr_number);

        // Generate summary.md
        let mut summary = generate_summary(&metadata, &reviews, &comments, &threads);

        // Single-comment mode
        if let Some(comment_url) = ctx.params.get("comment") {
            if let Some(target_section) = fetch_single_comment(comment_url).await {
                summary.push_str("\n\n## TARGET COMMENT\n\n");
                summary.push_str(&target_section);
            }
        }

        // Previous review artifact (run blocking I/O off the async runtime)
        let log_dir = ctx.log_dir.clone();
        let dispatch_id = ctx.dispatch_id.clone();
        let artifact_pr_url = pr_url.clone();
        if let Some(prev_section) = tokio::task::spawn_blocking(move || {
            check_previous_artifact(&log_dir, &dispatch_id, &artifact_pr_url)
        })
        .await
        .ok()
        .flatten()
        {
            summary.push_str("\n\n## Previous Run\n\n");
            summary.push_str(&prev_section);
        }

        let prefetch_dir = PathBuf::from(".dispatch-prefetch");

        let mut output = ContextOutput::default();
        output.files.push((prefetch_dir.join("triage.md"), triage));
        output
            .files
            .push((prefetch_dir.join("summary.md"), summary.clone()));
        output.files.push((
            prefetch_dir.join("pr.json"),
            serde_json::to_string_pretty(&metadata).unwrap_or_default(),
        ));
        output.files.push((
            prefetch_dir.join("comments.json"),
            serde_json::to_string_pretty(&comments).unwrap_or_default(),
        ));
        output.files.push((
            prefetch_dir.join("reviews.json"),
            serde_json::to_string_pretty(&reviews).unwrap_or_default(),
        ));
        output.files.push((
            prefetch_dir.join("threads.json"),
            serde_json::to_string_pretty(&threads).unwrap_or_default(),
        ));
        // Use template_vars only — preamble_sections would duplicate content
        // when a directive template contains {{prefetch}}.
        output.template_vars.insert("prefetch".to_string(), summary);

        Ok(output)
    }
}

/// Parse a GitHub PR URL into (owner, repo, number).
pub fn parse_pr_url(url: &str) -> anyhow::Result<(String, String, u64)> {
    // https://github.com/owner/repo/pull/123
    let stripped = url
        .trim_end_matches('/')
        .strip_prefix("https://github.com/")
        .ok_or_else(|| anyhow::anyhow!("PR URL must start with https://github.com/: {}", url))?;

    let parts: Vec<&str> = stripped.split('/').collect();
    if parts.len() < 4 || parts[0].is_empty() || parts[1].is_empty() || parts[2] != "pull" {
        anyhow::bail!("invalid PR URL format: {}", url);
    }

    // Strip any fragment (e.g., #discussion_r12345)
    let num_str = parts[3].split('#').next().unwrap_or(parts[3]);
    let number: u64 = num_str
        .parse()
        .map_err(|_| anyhow::anyhow!("invalid PR number in URL: {}", url))?;

    Ok((parts[0].to_string(), parts[1].to_string(), number))
}

/// Fetch PR metadata via `gh pr view`.
async fn fetch_pr_metadata(pr_url: &str) -> anyhow::Result<Value> {
    let output = tokio::time::timeout(
        GH_TIMEOUT,
        tokio::process::Command::new("gh")
            .args([
                "pr",
                "view",
                pr_url,
                "--json",
                "title,state,reviewDecision,additions,deletions,commits,headRefName",
            ])
            .kill_on_drop(true)
            .output(),
    )
    .await
    .map_err(|_| anyhow::anyhow!("gh pr view timed out"))??;

    if !output.status.success() {
        anyhow::bail!(
            "gh pr view failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    Ok(serde_json::from_slice(&output.stdout)?)
}

/// Fetch review comments via REST API.
async fn fetch_review_comments(owner: &str, repo: &str, pr_number: u64) -> anyhow::Result<Value> {
    let endpoint = format!(
        "repos/{}/{}/pulls/{}/comments?per_page=100",
        owner, repo, pr_number
    );
    let output = tokio::time::timeout(
        GH_TIMEOUT,
        tokio::process::Command::new("gh")
            .args(["api", &endpoint])
            .kill_on_drop(true)
            .output(),
    )
    .await
    .map_err(|_| anyhow::anyhow!("gh api comments timed out"))??;

    if !output.status.success() {
        anyhow::bail!(
            "gh api comments failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    Ok(serde_json::from_slice(&output.stdout)?)
}

/// Fetch reviews via REST API.
async fn fetch_reviews(owner: &str, repo: &str, pr_number: u64) -> anyhow::Result<Value> {
    let endpoint = format!(
        "repos/{}/{}/pulls/{}/reviews?per_page=100",
        owner, repo, pr_number
    );
    let output = tokio::time::timeout(
        GH_TIMEOUT,
        tokio::process::Command::new("gh")
            .args(["api", &endpoint])
            .kill_on_drop(true)
            .output(),
    )
    .await
    .map_err(|_| anyhow::anyhow!("gh api reviews timed out"))??;

    if !output.status.success() {
        anyhow::bail!(
            "gh api reviews failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    Ok(serde_json::from_slice(&output.stdout)?)
}

/// Fetch review threads via GraphQL with cursor-based pagination.
async fn fetch_review_threads(owner: &str, repo: &str, pr_number: u64) -> anyhow::Result<Value> {
    let query = r#"query($owner: String!, $repo: String!, $pr: Int!, $cursor: String) {
  repository(owner: $owner, name: $repo) {
    pullRequest(number: $pr) {
      reviewThreads(first: 100, after: $cursor) {
        pageInfo {
          hasNextPage
          endCursor
        }
        nodes {
          id
          isResolved
          isOutdated
          comments(first: 100) {
            nodes {
              id
              databaseId
              author { login }
              body
              path
              line
              createdAt
            }
          }
        }
      }
    }
  }
}"#;

    let mut all_nodes: Vec<Value> = Vec::new();
    let mut cursor: Option<String> = None;

    loop {
        let mut args = vec![
            "api".to_string(),
            "graphql".to_string(),
            "-f".to_string(),
            format!("query={}", query),
            "-F".to_string(),
            format!("owner={}", owner),
            "-F".to_string(),
            format!("repo={}", repo),
            "-F".to_string(),
            format!("pr={}", pr_number),
        ];

        if let Some(ref c) = cursor {
            args.push("-f".to_string());
            args.push(format!("cursor={}", c));
        } else {
            // Pass null cursor for first request
            args.push("-F".to_string());
            args.push("cursor=null".to_string());
        }

        let output = tokio::time::timeout(
            GH_TIMEOUT,
            tokio::process::Command::new("gh")
                .args(&args)
                .kill_on_drop(true)
                .output(),
        )
        .await
        .map_err(|_| anyhow::anyhow!("gh api graphql timed out"))??;

        if !output.status.success() {
            anyhow::bail!(
                "gh api graphql failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }

        let page: Value = serde_json::from_slice(&output.stdout)?;

        // Extract nodes from this page
        if let Some(nodes) = page
            .pointer("/data/repository/pullRequest/reviewThreads/nodes")
            .and_then(|v| v.as_array())
        {
            all_nodes.extend(nodes.iter().cloned());
        }

        // Check for next page
        let has_next = page
            .pointer("/data/repository/pullRequest/reviewThreads/pageInfo/hasNextPage")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        if !has_next {
            break;
        }

        cursor = page
            .pointer("/data/repository/pullRequest/reviewThreads/pageInfo/endCursor")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        if cursor.is_none() {
            break;
        }
    }

    // Reconstruct the expected response shape
    Ok(serde_json::json!({
        "data": {
            "repository": {
                "pullRequest": {
                    "reviewThreads": {
                        "nodes": all_nodes
                    }
                }
            }
        }
    }))
}

/// A flattened comment entry for triage generation.
#[derive(Debug, Clone)]
struct TriageEntry {
    path: String,
    line: Option<i64>,
    author: String,
    body: String,
    thread_id: String,
    comment_id: String,
    is_resolved: bool,
    #[allow(dead_code)]
    is_outdated: bool,
}

/// Generate triage.md from comments and threads.
///
/// Produces a self-contained markdown document with full comment text and
/// pre-built `gh api` commands for reply and thread resolution. Agents should
/// need ONLY this file — no JSON parsing required.
pub fn generate_triage(
    comments: &Value,
    threads: &Value,
    owner: &str,
    repo: &str,
    pr_number: u64,
) -> String {
    let mut entries = Vec::new();

    // Parse GraphQL threads
    let thread_nodes = threads
        .pointer("/data/repository/pullRequest/reviewThreads/nodes")
        .and_then(|v| v.as_array());

    if let Some(nodes) = thread_nodes {
        for thread in nodes {
            let thread_id = thread
                .get("id")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown")
                .to_string();
            let is_resolved = thread
                .get("isResolved")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let is_outdated = thread
                .get("isOutdated")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);

            let comment_nodes = thread.pointer("/comments/nodes").and_then(|v| v.as_array());

            if let Some(cnodes) = comment_nodes {
                // Use the first comment as the thread's primary entry
                if let Some(first) = cnodes.first() {
                    let db_id = first.get("databaseId").and_then(|v| v.as_i64());
                    let comment_id = db_id
                        .map(|id| id.to_string())
                        .unwrap_or_else(|| "unknown".to_string());

                    let path = first
                        .get("path")
                        .and_then(|v| v.as_str())
                        .unwrap_or("(no file)")
                        .to_string();
                    let line = first.get("line").and_then(|v| v.as_i64());
                    let author = first
                        .pointer("/author/login")
                        .and_then(|v| v.as_str())
                        .unwrap_or("unknown")
                        .to_string();
                    let body = first
                        .get("body")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();

                    entries.push(TriageEntry {
                        path,
                        line,
                        author,
                        body,
                        thread_id,
                        comment_id,
                        is_resolved,
                        is_outdated,
                    });
                }
            }
        }
    }

    // If no GraphQL data, fall back to REST comments
    if entries.is_empty() {
        if let Value::Array(arr) = comments {
            for c in arr {
                let id = c
                    .get("id")
                    .and_then(|v| v.as_i64())
                    .unwrap_or(0)
                    .to_string();
                let path = c
                    .get("path")
                    .and_then(|v| v.as_str())
                    .unwrap_or("(no file)")
                    .to_string();
                let line = c.get("line").and_then(|v| v.as_i64());
                let author = c
                    .pointer("/user/login")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown")
                    .to_string();
                let body = c
                    .get("body")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();

                entries.push(TriageEntry {
                    path,
                    line,
                    author,
                    body,
                    thread_id: String::new(),
                    comment_id: id,
                    is_resolved: false,
                    is_outdated: false,
                });
            }
        }
    }

    // Sort: unresolved first, then by file path, then by line number
    entries.sort_by(|a, b| {
        a.is_resolved
            .cmp(&b.is_resolved)
            .then_with(|| a.path.cmp(&b.path))
            .then_with(|| a.line.cmp(&b.line))
    });

    // Render self-contained triage document
    let mut md = String::from("# PR Comment Triage\n\n");
    if entries.is_empty() {
        md.push_str("No review comments found.\n");
        return md;
    }

    let unresolved: Vec<&TriageEntry> = entries.iter().filter(|e| !e.is_resolved).collect();
    let resolved: Vec<&TriageEntry> = entries.iter().filter(|e| e.is_resolved).collect();

    // Unresolved section — full detail, agents work through these
    if !unresolved.is_empty() {
        md.push_str(&format!("## Unresolved ({})\n\n", unresolved.len()));
        for (i, entry) in unresolved.iter().enumerate() {
            let location = match entry.line {
                Some(line) => format!("{}:{}", entry.path, line),
                None => entry.path.clone(),
            };

            md.push_str(&format!(
                "### {}. {} — @{}\n\n",
                i + 1,
                location,
                entry.author
            ));

            // Full comment body (trimmed, up to 2000 chars to avoid giant dumps)
            let body = entry.body.trim();
            if body.len() > 2000 {
                let truncated: String = body.chars().take(2000).collect();
                md.push_str(&truncated);
                md.push_str(
                    "\n\n[truncated — see `.dispatch-prefetch/comments.json` for full text]\n\n",
                );
            } else {
                md.push_str(body);
                md.push_str("\n\n");
            }

            // Pre-built commands
            if !entry.comment_id.is_empty() && entry.comment_id != "unknown" {
                md.push_str("```bash\n");
                md.push_str(&format!(
                    "# Reply:\ngh api repos/{}/{}/pulls/{}/comments/{}/replies -f body=\"Fixed in <commit>\"\n",
                    owner, repo, pr_number, entry.comment_id
                ));
                if !entry.thread_id.is_empty() && entry.thread_id != "unknown" {
                    md.push_str(&format!(
                        "# Resolve thread:\ngh api graphql -f query='mutation {{ resolveReviewThread(input: {{threadId: \"{}\"}}) {{ thread {{ isResolved }} }} }}'\n",
                        entry.thread_id
                    ));
                }
                md.push_str("```\n\n");
            }
        }
    }

    // Resolved section — collapsed, for reference only
    if !resolved.is_empty() {
        md.push_str(&format!(
            "<details>\n<summary>Resolved ({}) — skip unless verifying a previous fix</summary>\n\n",
            resolved.len()
        ));
        for entry in &resolved {
            let location = match entry.line {
                Some(line) => format!("{}:{}", entry.path, line),
                None => entry.path.clone(),
            };
            // One-line summary for resolved entries
            let preview: &str = entry.body.lines().next().unwrap_or("");
            let preview = if preview.chars().count() > 120 {
                let t: String = preview.chars().take(117).collect();
                format!("{}...", t)
            } else {
                preview.to_string()
            };
            md.push_str(&format!(
                "- [x] **{}** @{}: {}\n",
                location, entry.author, preview
            ));
        }
        md.push_str("\n</details>\n");
    }

    md
}

/// Generate summary.md from fetched PR data.
pub fn generate_summary(
    metadata: &Value,
    reviews: &Value,
    comments: &Value,
    threads: &Value,
) -> String {
    let mut md = String::from("# PR Summary\n\n");

    // Basic info
    let title = metadata
        .get("title")
        .and_then(|v| v.as_str())
        .unwrap_or("(unknown)");
    let state = metadata
        .get("state")
        .and_then(|v| v.as_str())
        .unwrap_or("UNKNOWN");
    let review_decision = metadata
        .get("reviewDecision")
        .and_then(|v| v.as_str())
        .unwrap_or("NONE");
    let additions = metadata
        .get("additions")
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    let deletions = metadata
        .get("deletions")
        .and_then(|v| v.as_i64())
        .unwrap_or(0);

    md.push_str(&format!("**{}**\n", title));
    md.push_str(&format!(
        "State: {} | Decision: {} | +{}/-{}\n\n",
        state, review_decision, additions, deletions
    ));

    // Review verdicts
    if let Value::Array(arr) = reviews {
        let mut verdicts: Vec<String> = Vec::new();
        for r in arr {
            let reviewer = r
                .pointer("/user/login")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");
            let verdict_state = r.get("state").and_then(|v| v.as_str()).unwrap_or("PENDING");
            verdicts.push(format!("@{}: {}", reviewer, verdict_state));
        }
        if !verdicts.is_empty() {
            md.push_str("## Reviews\n\n");
            for v in &verdicts {
                md.push_str(&format!("- {}\n", v));
            }
            md.push('\n');
        }
    }

    // Comment counts
    let inline_count = if let Value::Array(arr) = comments {
        arr.len()
    } else {
        0
    };

    let (resolved_count, unresolved_count) = count_thread_resolution(threads);

    md.push_str("## Comments\n\n");
    md.push_str(&format!("- Inline comments: {}\n", inline_count));
    md.push_str(&format!("- Resolved threads: {}\n", resolved_count));
    md.push_str(&format!("- Unresolved threads: {}\n", unresolved_count));
    md.push_str("\nSee `.dispatch-prefetch/triage.md` for details.\n");

    md
}

/// Count resolved/unresolved threads from GraphQL data.
fn count_thread_resolution(threads: &Value) -> (usize, usize) {
    let nodes = threads
        .pointer("/data/repository/pullRequest/reviewThreads/nodes")
        .and_then(|v| v.as_array());

    match nodes {
        Some(arr) => {
            let resolved = arr
                .iter()
                .filter(|t| {
                    t.get("isResolved")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false)
                })
                .count();
            (resolved, arr.len() - resolved)
        }
        None => (0, 0),
    }
}

/// Detect comment type from a PR URL fragment and fetch it.
async fn fetch_single_comment(comment_url: &str) -> Option<String> {
    // Auto-detect type from URL fragment
    if let Some(pos) = comment_url.find('#') {
        let fragment = &comment_url[pos + 1..];

        if let Some(id_str) = fragment.strip_prefix("issuecomment-") {
            // Issue comment
            if let Ok((owner, repo, _)) = parse_pr_url(comment_url) {
                let endpoint = format!("repos/{}/{}/issues/comments/{}", owner, repo, id_str);
                return fetch_comment_by_endpoint(&endpoint).await;
            }
        } else if let Some(id_str) = fragment.strip_prefix("discussion_r") {
            // Review comment
            if let Ok((owner, repo, _)) = parse_pr_url(comment_url) {
                let endpoint = format!("repos/{}/{}/pulls/comments/{}", owner, repo, id_str);
                return fetch_comment_by_endpoint(&endpoint).await;
            }
        } else if let Some(id_str) = fragment.strip_prefix("pullrequestreview-") {
            // Review
            if let Ok((owner, repo, pr_number)) = parse_pr_url(comment_url) {
                let endpoint = format!(
                    "repos/{}/{}/pulls/{}/reviews/{}",
                    owner, repo, pr_number, id_str
                );
                return fetch_comment_by_endpoint(&endpoint).await;
            }
        }
    }

    None
}

/// Fetch a single comment from a gh api endpoint and format it.
async fn fetch_comment_by_endpoint(endpoint: &str) -> Option<String> {
    let output = tokio::time::timeout(
        GH_TIMEOUT,
        tokio::process::Command::new("gh")
            .args(["api", endpoint])
            .kill_on_drop(true)
            .output(),
    )
    .await
    .ok()?
    .ok()?;

    if !output.status.success() {
        return None;
    }

    let comment: Value = serde_json::from_slice(&output.stdout).ok()?;
    let author = comment
        .pointer("/user/login")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");
    let body = comment.get("body").and_then(|v| v.as_str()).unwrap_or("");
    let path = comment.get("path").and_then(|v| v.as_str());
    let line = comment.get("line").and_then(|v| v.as_i64());

    let mut section = format!("@{}", author);
    if let Some(p) = path {
        section.push_str(&format!(" on `{}`", p));
        if let Some(l) = line {
            section.push_str(&format!(":{}", l));
        }
    }
    section.push_str(&format!(":\n\n> {}", body.replace('\n', "\n> ")));

    Some(section)
}

/// Check for previous review artifact from a prior dispatch.
/// Filters by PR URL to only return artifacts for the same PR.
/// This performs synchronous I/O and should be called via `spawn_blocking`.
fn check_previous_artifact(log_dir: &Path, dispatch_id: &str, pr_url: &str) -> Option<String> {
    let entries = std::fs::read_dir(log_dir).ok()?;

    let mut artifacts: Vec<(PathBuf, String)> = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.ends_with("-review-artifact.json")
            && name != format!("{}-review-artifact.json", dispatch_id)
        {
            if let Ok(content) = std::fs::read_to_string(entry.path()) {
                // Filter by PR URL — only include artifacts for the same PR.
                // Legacy artifacts (written before pr_url filtering was added) lack
                // the field entirely; treat them as matching to preserve backward
                // compatibility on upgrade.
                if let Ok(artifact) = serde_json::from_str::<Value>(&content) {
                    let artifact_pr = artifact.get("pr_url").and_then(|v| v.as_str());
                    if artifact_pr.is_none() || artifact_pr == Some(pr_url) {
                        artifacts.push((entry.path(), content));
                    }
                }
            }
        }
    }

    // Sort by file modification time (most recent first), falling back to path name
    artifacts.sort_by(|a, b| {
        let time_a = std::fs::metadata(&a.0).and_then(|m| m.modified()).ok();
        let time_b = std::fs::metadata(&b.0).and_then(|m| m.modified()).ok();
        time_b.cmp(&time_a).then_with(|| b.0.cmp(&a.0))
    });
    let (_, content) = artifacts.first()?;

    let artifact: Value = serde_json::from_str(content).ok()?;
    let resolved = artifact.get("resolved_comments").and_then(|v| v.as_array());

    if let Some(resolved_ids) = resolved {
        if !resolved_ids.is_empty() {
            let ids: Vec<String> = resolved_ids
                .iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect();
            return Some(format!(
                "Previous run resolved comments: {}. Focus on remaining.",
                ids.join(", ")
            ));
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_pr_url_valid() {
        let (owner, repo, num) =
            parse_pr_url("https://github.com/anthropics/claude-code/pull/123").unwrap();
        assert_eq!(owner, "anthropics");
        assert_eq!(repo, "claude-code");
        assert_eq!(num, 123);
    }

    #[test]
    fn test_parse_pr_url_with_fragment() {
        let (owner, repo, num) =
            parse_pr_url("https://github.com/anthropics/claude-code/pull/42#discussion_r12345")
                .unwrap();
        assert_eq!(owner, "anthropics");
        assert_eq!(repo, "claude-code");
        assert_eq!(num, 42);
    }

    #[test]
    fn test_parse_pr_url_trailing_slash() {
        let (_, _, num) = parse_pr_url("https://github.com/owner/repo/pull/99/").unwrap();
        assert_eq!(num, 99);
    }

    #[test]
    fn test_parse_pr_url_invalid() {
        assert!(parse_pr_url("https://gitlab.com/foo/bar/pull/1").is_err());
        assert!(parse_pr_url("https://github.com/foo/bar/issues/1").is_err());
        assert!(parse_pr_url("not a url").is_err());
    }

    #[test]
    fn test_generate_triage_with_threads() {
        let comments = Value::Array(vec![]);
        let threads = serde_json::json!({
            "data": {
                "repository": {
                    "pullRequest": {
                        "reviewThreads": {
                            "nodes": [
                                {
                                    "id": "T_001",
                                    "isResolved": false,
                                    "isOutdated": false,
                                    "comments": {
                                        "nodes": [
                                            {
                                                "id": "C_001",
                                                "databaseId": 100,
                                                "author": { "login": "reviewer1" },
                                                "body": "This needs error handling",
                                                "path": "src/auth.rs",
                                                "line": 42,
                                                "createdAt": "2024-01-01T00:00:00Z"
                                            }
                                        ]
                                    }
                                },
                                {
                                    "id": "T_002",
                                    "isResolved": true,
                                    "isOutdated": false,
                                    "comments": {
                                        "nodes": [
                                            {
                                                "id": "C_002",
                                                "databaseId": 200,
                                                "author": { "login": "reviewer2" },
                                                "body": "Typo in variable name",
                                                "path": "src/main.rs",
                                                "line": 10,
                                                "createdAt": "2024-01-01T00:00:00Z"
                                            }
                                        ]
                                    }
                                }
                            ]
                        }
                    }
                }
            }
        });

        let triage = generate_triage(&comments, &threads, "test-owner", "test-repo", 42);

        // Unresolved section with full body
        assert!(
            triage.contains("## Unresolved (1)"),
            "should have unresolved section"
        );
        assert!(
            triage.contains("### 1. src/auth.rs:42 — @reviewer1"),
            "should have entry header"
        );
        assert!(
            triage.contains("This needs error handling"),
            "should have full comment body"
        );
        // Pre-built commands
        assert!(
            triage.contains("gh api repos/test-owner/test-repo/pulls/42/comments/100/replies"),
            "should have reply command"
        );
        assert!(
            triage.contains("resolveReviewThread"),
            "should have resolve command"
        );
        assert!(
            triage.contains("T_001"),
            "should have thread ID in resolve command"
        );
        // Resolved section collapsed
        assert!(
            triage.contains("<details>"),
            "resolved should be in details tag"
        );
        assert!(triage.contains("Resolved (1)"), "should count resolved");
        assert!(
            triage.contains("src/main.rs:10"),
            "resolved entry should be listed"
        );
        // Unresolved before resolved
        let unresolved_pos = triage.find("## Unresolved").unwrap();
        let resolved_pos = triage.find("<details>").unwrap();
        assert!(unresolved_pos < resolved_pos);
    }

    #[test]
    fn test_generate_triage_empty() {
        let comments = Value::Array(vec![]);
        let threads = Value::Null;
        let triage = generate_triage(&comments, &threads, "test-owner", "test-repo", 42);
        assert!(triage.contains("No review comments found."));
    }

    #[test]
    fn test_generate_triage_rest_fallback() {
        let comments = serde_json::json!([
            {
                "id": 500,
                "path": "src/lib.rs",
                "line": 5,
                "user": { "login": "alice" },
                "body": "Consider using a constant here"
            }
        ]);
        let threads = Value::Null;
        let triage = generate_triage(&comments, &threads, "test-owner", "test-repo", 42);
        assert!(triage.contains("src/lib.rs:5"), "should have file:line");
        assert!(triage.contains("@alice"), "should have author");
        assert!(
            triage.contains("Consider using a constant here"),
            "should have full body"
        );
    }

    #[test]
    fn test_generate_summary() {
        let metadata = serde_json::json!({
            "title": "Add auth middleware",
            "state": "OPEN",
            "reviewDecision": "CHANGES_REQUESTED",
            "additions": 150,
            "deletions": 30
        });
        let reviews = serde_json::json!([
            { "user": { "login": "bob" }, "state": "CHANGES_REQUESTED" },
            { "user": { "login": "alice" }, "state": "APPROVED" }
        ]);
        let comments = serde_json::json!([
            { "id": 1, "body": "fix this" },
            { "id": 2, "body": "and this" }
        ]);
        let threads = serde_json::json!({
            "data": {
                "repository": {
                    "pullRequest": {
                        "reviewThreads": {
                            "nodes": [
                                { "id": "t1", "isResolved": true },
                                { "id": "t2", "isResolved": false },
                                { "id": "t3", "isResolved": false }
                            ]
                        }
                    }
                }
            }
        });

        let summary = generate_summary(&metadata, &reviews, &comments, &threads);

        assert!(summary.contains("Add auth middleware"));
        assert!(summary.contains("OPEN"));
        assert!(summary.contains("CHANGES_REQUESTED"));
        assert!(summary.contains("+150/-30"));
        assert!(summary.contains("@bob: CHANGES_REQUESTED"));
        assert!(summary.contains("@alice: APPROVED"));
        assert!(summary.contains("Inline comments: 2"));
        assert!(summary.contains("Resolved threads: 1"));
        assert!(summary.contains("Unresolved threads: 2"));
    }

    #[test]
    fn test_generate_summary_null_metadata() {
        let summary = generate_summary(
            &Value::Null,
            &Value::Array(vec![]),
            &Value::Array(vec![]),
            &Value::Null,
        );
        assert!(summary.contains("# PR Summary"));
        assert!(summary.contains("(unknown)"));
    }

    #[test]
    fn test_check_previous_artifact_filters_by_pr_url() {
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path();

        // Create an artifact for a different PR
        let other_artifact = serde_json::json!({
            "pr_url": "https://github.com/org/repo/pull/99",
            "resolved_comments": ["c1", "c2"]
        });
        std::fs::write(
            log_dir.join("other-dispatch-review-artifact.json"),
            serde_json::to_string(&other_artifact).unwrap(),
        )
        .unwrap();

        // Create an artifact for the target PR
        let target_artifact = serde_json::json!({
            "pr_url": "https://github.com/org/repo/pull/42",
            "resolved_comments": ["c3"]
        });
        std::fs::write(
            log_dir.join("target-dispatch-review-artifact.json"),
            serde_json::to_string(&target_artifact).unwrap(),
        )
        .unwrap();

        // Should find the target PR's artifact
        let result = check_previous_artifact(
            log_dir,
            "current-dispatch",
            "https://github.com/org/repo/pull/42",
        );
        assert!(result.is_some());
        let text = result.unwrap();
        assert!(text.contains("c3"), "expected c3 in result, got: {}", text);

        // Should NOT find an artifact for a non-existent PR
        let result = check_previous_artifact(
            log_dir,
            "current-dispatch",
            "https://github.com/org/repo/pull/999",
        );
        assert!(result.is_none());
    }

    #[test]
    fn test_check_previous_artifact_legacy_missing_pr_url() {
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path();

        // Legacy artifact without pr_url field (written before filtering was added)
        let legacy_artifact = serde_json::json!({
            "resolved_comments": ["c1", "c2"]
        });
        std::fs::write(
            log_dir.join("legacy-dispatch-review-artifact.json"),
            serde_json::to_string(&legacy_artifact).unwrap(),
        )
        .unwrap();

        // Should match any PR URL (backward compatibility)
        let result = check_previous_artifact(
            log_dir,
            "current-dispatch",
            "https://github.com/org/repo/pull/42",
        );
        assert!(result.is_some());
        let text = result.unwrap();
        assert!(
            text.contains("c1") && text.contains("c2"),
            "expected c1, c2 in result, got: {}",
            text
        );
    }
}
