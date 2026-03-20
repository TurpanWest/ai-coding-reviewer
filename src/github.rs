use anyhow::{bail, Context, Result};
use reqwest::Client;
use serde::Serialize;

use crate::models::{ConsensusResult, Finding};

// ── Request structs ────────────────────────────────────────────────────────────

#[derive(Serialize)]
struct ReviewComment {
    path: String,
    line: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    start_line: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    start_side: Option<&'static str>,
    side: &'static str,
    body: String,
}

#[derive(Serialize)]
struct CreateReviewRequest<'a> {
    body: &'a str,
    event: &'static str,
    comments: Vec<ReviewComment>,
}

// ── Public API ─────────────────────────────────────────────────────────────────

pub struct GithubConfig {
    pub pr_url: String,
    pub token: String,
}

pub async fn submit_review(
    config: &GithubConfig,
    consensus: &ConsensusResult,
    summary: &str,
) -> Result<()> {
    let (owner, repo, pr_number) = parse_pr_url(&config.pr_url)?;

    let comments: Vec<ReviewComment> = consensus
        .all_findings
        .iter()
        .map(finding_to_comment)
        .collect();

    let event = if consensus.gate_passed {
        "APPROVE"
    } else {
        "REQUEST_CHANGES"
    };

    let payload = CreateReviewRequest {
        body: summary,
        event,
        comments,
    };

    let url = format!(
        "https://api.github.com/repos/{owner}/{repo}/pulls/{pr_number}/reviews"
    );

    let client = Client::builder()
        .user_agent("ai-reviewer")
        .build()
        .context("Failed to build HTTP client")?;

    let response = client
        .post(&url)
        .header("Authorization", format!("Bearer {}", config.token))
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .json(&payload)
        .send()
        .await
        .context("Failed to send GitHub review request")?;

    response
        .error_for_status()
        .context("GitHub API returned an error")?;

    Ok(())
}

// ── Private helpers ────────────────────────────────────────────────────────────

fn strip_git_prefix(path: &str) -> &str {
    path.strip_prefix("b/")
        .or_else(|| path.strip_prefix("a/"))
        .unwrap_or(path)
}

fn finding_to_comment(f: &Finding) -> ReviewComment {
    let multiline = f.location.line_end > f.location.line_start;
    ReviewComment {
        path: strip_git_prefix(&f.location.file).to_string(),
        line: f.location.line_end,
        start_line: if multiline {
            Some(f.location.line_start)
        } else {
            None
        },
        start_side: if multiline { Some("RIGHT") } else { None },
        side: "RIGHT",
        body: format!(
            "**[{}] {}**\n\n{}\n\n> **Suggestion:** {}",
            f.severity, f.rule_id, f.description, f.suggestion
        ),
    }
}

/// Parse `https://github.com/{owner}/{repo}/pull/{number}` into its parts.
fn parse_pr_url(url: &str) -> Result<(String, String, u64)> {
    // Strip protocol and split on '/'
    let stripped = url
        .strip_prefix("https://github.com/")
        .or_else(|| url.strip_prefix("http://github.com/"))
        .with_context(|| format!("PR URL must start with https://github.com/: {url}"))?;

    let parts: Vec<&str> = stripped.splitn(4, '/').collect();
    if parts.len() < 4 || parts[2] != "pull" {
        bail!("PR URL must be in the form https://github.com/{{owner}}/{{repo}}/pull/{{number}}: {url}");
    }

    let owner = parts[0].to_string();
    let repo = parts[1].to_string();
    let pr_number: u64 = parts[3]
        .parse()
        .with_context(|| format!("PR number '{}' is not a valid integer", parts[3]))?;

    Ok((owner, repo, pr_number))
}

// ── Tests ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_pr_url_valid() {
        let (owner, repo, num) =
            parse_pr_url("https://github.com/acme/my-repo/pull/42").unwrap();
        assert_eq!(owner, "acme");
        assert_eq!(repo, "my-repo");
        assert_eq!(num, 42);
    }

    #[test]
    fn test_parse_pr_url_invalid_missing_pull() {
        assert!(parse_pr_url("https://github.com/acme/my-repo/issues/42").is_err());
    }

    #[test]
    fn test_parse_pr_url_invalid_domain() {
        assert!(parse_pr_url("https://gitlab.com/acme/my-repo/pull/42").is_err());
    }

    #[test]
    fn test_strip_git_prefix() {
        assert_eq!(strip_git_prefix("b/src/main.rs"), "src/main.rs");
        assert_eq!(strip_git_prefix("a/src/main.rs"), "src/main.rs");
        assert_eq!(strip_git_prefix("src/main.rs"), "src/main.rs");
    }

    #[test]
    fn test_finding_to_comment_single_line() {
        use crate::models::{CodeLocation, Severity};
        let f = Finding {
            severity: Severity::High,
            location: CodeLocation {
                file: "b/src/main.rs".to_string(),
                line_start: 10,
                line_end: 10,
            },
            rule_id: "SEC-001".to_string(),
            description: "SQL injection risk".to_string(),
            suggestion: "Use parameterized queries".to_string(),
        };
        let c = finding_to_comment(&f);
        assert_eq!(c.path, "src/main.rs");
        assert_eq!(c.line, 10);
        assert!(c.start_line.is_none());
        assert!(c.start_side.is_none());
        assert_eq!(c.side, "RIGHT");
    }

    #[test]
    fn test_finding_to_comment_multi_line() {
        use crate::models::{CodeLocation, Severity};
        let f = Finding {
            severity: Severity::Critical,
            location: CodeLocation {
                file: "src/lib.rs".to_string(),
                line_start: 5,
                line_end: 15,
            },
            rule_id: "PERF-002".to_string(),
            description: "Nested loop O(n²)".to_string(),
            suggestion: "Use a hash map".to_string(),
        };
        let c = finding_to_comment(&f);
        assert_eq!(c.start_line, Some(5));
        assert_eq!(c.start_side, Some("RIGHT"));
        assert_eq!(c.line, 15);
    }
}
