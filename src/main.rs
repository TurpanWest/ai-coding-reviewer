mod ast;
mod consensus;
mod diff;
mod github;
mod models;
mod prompt;
mod report;
mod telemetry;

use std::io::Read;
use std::path::PathBuf;
use std::process;

use std::time::Instant;

use anyhow::{Context, Result};
use clap::{Parser as ClapParser, ValueEnum};
use rig::providers::{anthropic, gemini, openai};
use tracing::info;

use crate::models::reviewer::LlmReviewer;
use crate::models::{ReviewFocus, Reviewer};
use crate::telemetry::{record_review, Metrics};

// ── Provider selection ─────────────────────────────────────────────────────────

#[derive(ValueEnum, Clone, Debug)]
enum ProviderKind {
    Minimax,
    Deepseek,
    Anthropic,
    Gemini,
    Openai,
}

// ── CLI definition ─────────────────────────────────────────────────────────────

#[derive(ClapParser, Debug)]
#[command(
    name       = "ai-reviewer",
    version,
    about      = "AI-to-AI code review engine: 2-model × 4-focus quad-group review (Security · Correctness · Performance · Maintainability)",
    long_about = None,
)]
struct Cli {
    /// Path to unified diff file, or "-" to read from stdin
    #[arg(short = 'd', long, value_name = "PATH")]
    diff: String,

    /// Repository root for full-file AST context resolution
    #[arg(short = 's', long, value_name = "PATH", default_value = ".")]
    source_root: PathBuf,

    /// Security/coding policy Markdown file (injected into system prompt)
    #[arg(short = 'p', long, value_name = "PATH")]
    policy: PathBuf,

    /// Confidence gate threshold (0.0–1.0)
    #[arg(short = 't', long, default_value_t = 0.90, value_parser = parse_threshold)]
    threshold: f64,

    /// Output path for the Markdown review report (defaults to <source-root>/review-report.md)
    #[arg(short = 'o', long, value_name = "PATH")]
    output: Option<PathBuf>,

    /// Maximum self-correction retries per model
    #[arg(long, default_value_t = 3)]
    max_retries: u32,

    /// Hard timeout in seconds per individual LLM call (not per retry loop).
    /// If a reviewer hangs longer than this, it is treated as a FAIL.
    #[arg(long, default_value_t = 120, env = "REVIEWER_TIMEOUT")]
    reviewer_timeout: u64,

    /// Maximum number of lines in the diff before refusing to process.
    /// Large diffs silently exceed LLM context windows; fail fast instead.
    #[arg(long, default_value_t = 5000, env = "MAX_DIFF_LINES")]
    max_diff_lines: usize,

    // ── Reviewer A (one side of every group pair) ──────────────────────────────

    /// Reviewer A provider — used as the first model in all four focus groups
    #[arg(long, default_value = "minimax", env = "REVIEWER_1")]
    reviewer_1: ProviderKind,

    /// Reviewer A model ID
    #[arg(long, env = "REVIEWER_1_MODEL")]
    reviewer_1_model: Option<String>,

    /// Reviewer A API key  [fallback: MINIMAX_API_KEY]
    #[arg(long, env = "REVIEWER_1_API_KEY")]
    reviewer_1_api_key: Option<String>,

    /// Reviewer A base URL (OpenAI-compat providers only) [fallback: MINIMAX_BASE_URL]
    #[arg(long, env = "REVIEWER_1_BASE_URL")]
    reviewer_1_base_url: Option<String>,

    // ── Reviewer B (other side of every group pair) ────────────────────────────

    /// Reviewer B provider — used as the second model in all four focus groups
    #[arg(long, default_value = "deepseek", env = "REVIEWER_2")]
    reviewer_2: ProviderKind,

    /// Reviewer B model ID
    #[arg(long, env = "REVIEWER_2_MODEL")]
    reviewer_2_model: Option<String>,

    /// Reviewer B API key  [fallback: DEEPSEEK_API_KEY]
    #[arg(long, env = "REVIEWER_2_API_KEY")]
    reviewer_2_api_key: Option<String>,

    /// Reviewer B base URL (OpenAI-compat providers only) [fallback: DEEPSEEK_BASE_URL]
    #[arg(long, env = "REVIEWER_2_BASE_URL")]
    reviewer_2_base_url: Option<String>,

    /// GitHub PR URL to post review to (e.g. https://github.com/owner/repo/pull/123)
    #[arg(long, value_name = "URL")]
    pr_url: Option<String>,

    /// GitHub token for PR review submission (falls back to GITHUB_TOKEN env var)
    #[arg(long, value_name = "TOKEN", env = "GITHUB_TOKEN")]
    github_token: Option<String>,

    /// Enable verbose tracing output (or set RUST_LOG=info/debug)
    #[arg(short = 'v', long)]
    verbose: bool,
}

// ── Entry point ───────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() {
    let exit_code = match run().await {
        Ok(passed) => {
            if passed { 0 } else { 1 }
        }
        Err(e) => {
            eprintln!("[ai-reviewer] Fatal error: {e:#}");
            2
        }
    };
    process::exit(exit_code);
}

async fn run() -> Result<bool> {
    let cli = Cli::parse();

    // ── Observability ──────────────────────────────────────────────────────
    // _guard flushes OTel spans on drop (end of this function).
    let _guard = telemetry::init_subscriber(cli.verbose);
    let metrics = Metrics::new().context("Failed to initialise Prometheus metrics")?;

    // ── Read inputs ────────────────────────────────────────────────────────
    let diff_text = read_diff(&cli.diff)?;
    let policy_text = std::fs::read_to_string(&cli.policy)
        .with_context(|| format!("Cannot read policy file: {}", cli.policy.display()))?;

    info!(
        diff_bytes = diff_text.len(),
        policy_bytes = policy_text.len(),
        "Inputs loaded"
    );

    // ── Guard: diff size limit ──────────────────────────────────────────────
    let diff_lines = diff_text.lines().count();
    metrics.diff_lines.set(diff_lines as i64);
    if diff_lines > cli.max_diff_lines {
        anyhow::bail!(
            "Diff has {diff_lines} lines which exceeds --max-diff-lines limit of {}.\n\
             Large diffs exceed LLM context windows and produce unreliable verdicts.\n\
             Split the diff into smaller per-module chunks before reviewing.",
            cli.max_diff_lines
        );
    }
    info!(diff_lines, limit = cli.max_diff_lines, "Diff size check passed");

    // ── Parse diff ─────────────────────────────────────────────────────────
    let file_diffs = diff::parse_diff(&diff_text).context("Failed to parse unified diff")?;

    if file_diffs.is_empty() {
        eprintln!("[ai-reviewer] No file changes found in diff. Exiting with PASS.");
        return Ok(true);
    }

    let changed_files: Vec<_> = file_diffs.iter().filter(|f| f.has_changes()).collect();
    info!(files = changed_files.len(), "Files with changes");

    // ── Extract AST contexts ───────────────────────────────────────────────
    let mut ast_contexts = Vec::new();
    for fd in &changed_files {
        let file_raw_diff = extract_file_diff_chunk(&diff_text, fd);
        let ctx = ast::extract_context(fd, &cli.source_root, &file_raw_diff)?;
        info!(
            file = %ctx.file,
            changed_symbols = ctx.changed_symbols.len(),
            call_edges = ctx.call_edges.len(),
            "AST context extracted"
        );
        ast_contexts.push(ctx);
    }

    // ── Resolve API keys ───────────────────────────────────────────────────
    let key_a = resolve_api_key(
        cli.reviewer_1_api_key.as_deref(),
        "REVIEWER_1_API_KEY",
        "MINIMAX_API_KEY",
    )?;
    let key_b = resolve_api_key(
        cli.reviewer_2_api_key.as_deref(),
        "REVIEWER_2_API_KEY",
        "DEEPSEEK_API_KEY",
    )?;

    let base_url_a = cli.reviewer_1_base_url.clone()
        .or_else(|| std::env::var("MINIMAX_BASE_URL").ok());
    let base_url_b = cli.reviewer_2_base_url.clone()
        .or_else(|| std::env::var("DEEPSEEK_BASE_URL").ok());

    // ── Distribute files round-robin into up to 4 groups ──────────────────
    // Each group is reviewed by the same A+B pair but with a distinct focus:
    //   G1 = Security · G2 = Correctness · G3 = Performance · G4 = Maintainability
    const FOCUSES: [ReviewFocus; 4] = [
        ReviewFocus::Security,
        ReviewFocus::Correctness,
        ReviewFocus::Performance,
        ReviewFocus::Maintainability,
    ];

    let n_groups = FOCUSES.len().min(ast_contexts.len()).max(1);
    let mut file_groups: Vec<Vec<crate::ast::FileAstContext>> = vec![vec![]; n_groups];
    for (i, ctx) in ast_contexts.into_iter().enumerate() {
        file_groups[i % n_groups].push(ctx);
    }

    info!(n_groups, "Dispatching {n_groups}-group 8-LLM concurrent review");

    // ── Build reviewer instances per group and prepare futures ─────────────
    // build_reviewer is cheap (HTTP client + model handle, no network call).
    // Each group gets fresh instances so there is no shared mutable state.
    let group_data: Vec<_> = file_groups
        .into_iter()
        .enumerate()
        .map(|(i, group_ctx)| -> Result<_> {
            let focus = FOCUSES[i];
            let ra = build_reviewer(
                cli.reviewer_1.clone(), key_a.clone(), base_url_a.clone(),
                cli.reviewer_1_model.clone(), cli.max_retries, focus, cli.reviewer_timeout,
            )
            .with_context(|| format!("Failed to build reviewer A for group {i}"))?;
            let rb = build_reviewer(
                cli.reviewer_2.clone(), key_b.clone(), base_url_b.clone(),
                cli.reviewer_2_model.clone(), cli.max_retries, focus, cli.reviewer_timeout,
            )
            .with_context(|| format!("Failed to build reviewer B for group {i}"))?;
            let label_a = ra.label().to_owned();
            let label_b = rb.label().to_owned();
            let file_names: Vec<String> = group_ctx.iter().map(|c| c.file.clone()).collect();
            Ok((i, group_ctx, ra, rb, label_a, label_b, focus, file_names))
        })
        .collect::<Result<_>>()?;

    // ── N-group concurrent review (2 LLMs per group = 8 total calls) ──────
    let group_futures: Vec<_> = group_data
        .into_iter()
        .map(|(i, group_ctx, ra, rb, label_a, label_b, focus, file_names)| {
            let policy = policy_text.clone();
            async move {
                let t = Instant::now();
                let (r_a, r_b) = tokio::join!(
                    ra.review(&group_ctx, &policy),
                    rb.review(&group_ctx, &policy),
                );
                (i, t.elapsed(), r_a, r_b, label_a, label_b, focus, file_names)
            }
        })
        .collect();

    let raw_outputs = futures::future::join_all(group_futures).await;

    info!(groups = raw_outputs.len(), "All groups completed");

    // ── Record metrics and build pair results ──────────────────────────────
    let mut pair_results = Vec::new();
    for (i, dur, r_a, r_b, label_a, label_b, focus, file_names) in raw_outputs {
        record_review(&metrics, &label_a, focus.as_str(), dur, &r_a);
        record_review(&metrics, &label_b, focus.as_str(), dur, &r_b);
        let pair = consensus::evaluate_pair(r_a, r_b, label_a, label_b, focus, i, file_names);
        pair_results.push(pair);
    }

    // ── Consensus evaluation ───────────────────────────────────────────────
    let consensus = consensus::evaluate(pair_results);

    // ── Output ─────────────────────────────────────────────────────────────
    let summary = report::render_summary(&consensus);
    println!("{}", summary);

    let output_path = cli.output.unwrap_or_else(|| cli.source_root.join("review-report.md"));
    let report_md = report::render_report(&consensus);
    std::fs::write(&output_path, &report_md)
        .with_context(|| format!("Cannot write report to {}", output_path.display()))?;

    metrics.gate_passed.set(if consensus.gate_passed { 1 } else { 0 });

    if consensus.gate_passed {
        println!(
            "[ai-reviewer] Gate PASSED — full report: {}",
            output_path.display()
        );
    } else {
        eprintln!(
            "[ai-reviewer] Gate FAILED — report written to: {}",
            output_path.display()
        );
    }

    // ── Export Prometheus metrics ──────────────────────────────────────────
    // Non-fatal: export errors are logged but do not affect the gate verdict.
    metrics.export().await;

    // ── GitHub PR Review (optional, non-fatal) ─────────────────────────────
    if let Some(pr_url) = cli.pr_url {
        match cli.github_token.filter(|t| !t.is_empty()) {
            None => tracing::warn!(
                "--pr-url provided but no GitHub token found; set GITHUB_TOKEN or --github-token"
            ),
            Some(token) => {
                let cfg = github::GithubConfig { pr_url, token };
                match github::submit_review(&cfg, &consensus, &summary).await {
                    Ok(()) => tracing::info!("GitHub PR review submitted successfully"),
                    Err(e) => tracing::warn!("GitHub PR review submission failed (non-fatal): {e:#}"),
                }
            }
        }
    }

    Ok(consensus.gate_passed)
}

// ── Provider builder ───────────────────────────────────────────────────────────

fn build_reviewer(
    kind: ProviderKind,
    api_key: String,
    base_url: Option<String>,
    model_id: Option<String>,
    max_retries: u32,
    focus: ReviewFocus,
    reviewer_timeout: u64,
) -> Result<Box<dyn Reviewer>> {
    match kind {
        ProviderKind::Minimax => {
            let url = base_url.unwrap_or_else(|| "https://api.minimax.chat/v1".into());
            let mid = model_id.unwrap_or_else(|| "MiniMax-M2.7".into());
            let client = openai::Client::from_url(&api_key, &url);
            let model = client.completion_model(&mid);
            Ok(Box::new(LlmReviewer::new(model, "MiniMax", max_retries, focus, reviewer_timeout, None)))
        }
        ProviderKind::Deepseek => {
            // DeepSeek's API is fully OpenAI-compatible.  Using openai::Client
            // instead of deepseek::Client routes through rig's OpenAI code path,
            // which correctly extracts and logs token usage from the response.
            // rig's own deepseek provider omits the `usage` field from its
            // CompletionResponse struct, so token counts are silently discarded.
            let url = base_url.unwrap_or_else(|| "https://api.deepseek.com/v1".into());
            let mid = model_id.unwrap_or_else(|| "deepseek-chat".into());
            let client = openai::Client::from_url(&api_key, &url);
            let model = client.completion_model(&mid);
            Ok(Box::new(LlmReviewer::new(model, "DeepSeek", max_retries, focus, reviewer_timeout, None)))
        }
        ProviderKind::Anthropic => {
            let mid = model_id.unwrap_or_else(|| "claude-sonnet-4-6".into());
            let base = base_url.unwrap_or_else(|| "https://api.anthropic.com".into());
            let client = anthropic::Client::new(&api_key, &base, None, "2023-06-01");
            let model = client.completion_model(&mid);
            // Enable automatic prompt caching: the stable system prompt (policy + schema)
            // is cached at the provider side, slashing cost and latency on repeated CI runs.
            let cache = serde_json::json!({"cache_control": {"type": "ephemeral"}});
            Ok(Box::new(LlmReviewer::new(model, "Anthropic", max_retries, focus, reviewer_timeout, Some(cache))))
        }
        ProviderKind::Gemini => {
            let mid = model_id.unwrap_or_else(|| "gemini-3.1-pro-preview".into());
            let client = gemini::Client::new(&api_key);
            let model = client.completion_model(&mid);
            Ok(Box::new(LlmReviewer::new(model, "Gemini", max_retries, focus, reviewer_timeout, None)))
        }
        ProviderKind::Openai => {
            let mid = model_id.unwrap_or_else(|| "gpt-5.4".into());
            let client = if let Some(url) = base_url {
                openai::Client::from_url(&api_key, &url)
            } else {
                openai::Client::new(&api_key)
            };
            let model = client.completion_model(&mid);
            Ok(Box::new(LlmReviewer::new(model, "OpenAI", max_retries, focus, reviewer_timeout, None)))
        }
    }
}

// ── CLI validators ─────────────────────────────────────────────────────────────

fn parse_threshold(s: &str) -> Result<f64, String> {
    let v: f64 = s.parse().map_err(|_| format!("'{s}' is not a valid number"))?;
    if !(0.0..=1.0).contains(&v) {
        return Err(format!(
            "threshold must be between 0.0 and 1.0, got '{s}' ({v})"
        ));
    }
    Ok(v)
}

// ── Helpers ────────────────────────────────────────────────────────────────────

/// Resolve an API key: prefer explicit arg/env, then fall back to legacy env var.
fn resolve_api_key(
    explicit: Option<&str>,
    _primary_env: &str,
    legacy_env: &str,
) -> Result<String> {
    if let Some(k) = explicit {
        return Ok(k.to_owned());
    }
    std::env::var(legacy_env)
        .with_context(|| format!("API key not set (tried {legacy_env})"))
}

fn read_diff(path: &str) -> Result<String> {
    if path == "-" {
        let mut buf = String::new();
        std::io::stdin()
            .read_to_string(&mut buf)
            .context("Failed to read diff from stdin")?;
        Ok(buf)
    } else {
        std::fs::read_to_string(path)
            .with_context(|| format!("Cannot read diff file: {path}"))
    }
}

/// Extract the portion of the unified diff that belongs to a specific file.
fn extract_file_diff_chunk(full_diff: &str, fd: &diff::FileDiff) -> String {
    let target = match fd.source_path() {
        Some(p) => p.display().to_string(),
        None => return String::new(),
    };

    let lines = full_diff.lines();
    let mut chunk = String::new();
    let mut capturing = false;

    for line in lines {
        if line.starts_with("diff --git ") {
            if capturing {
                break;
            }
            if line.contains(&target) {
                capturing = true;
            }
        }
        if capturing {
            chunk.push_str(line);
            chunk.push('\n');
        }
    }

    if chunk.is_empty() {
        full_diff.to_owned()
    } else {
        chunk
    }
}
