mod ast;
mod consensus;
mod diff;
mod models;
mod prompt;
mod report;

use std::io::Read;
use std::path::PathBuf;
use std::process;

use anyhow::{Context, Result};
use clap::{Parser as ClapParser, ValueEnum};
use rig::providers::{anthropic, deepseek, gemini, openai};
use tracing::info;
use tracing_subscriber::EnvFilter;

use crate::models::reviewer::LlmReviewer;
use crate::models::{ReviewFocus, Reviewer};

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
    about      = "AI-to-AI code review engine: 4-model dual-pair cross-validation with multi-provider support",
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

    // ── Style pair: Reviewer 1 ─────────────────────────────────────────────────

    /// Style reviewer A provider
    #[arg(long, default_value = "minimax", env = "REVIEWER_1")]
    reviewer_1: ProviderKind,

    /// Style reviewer A model ID
    #[arg(long, env = "REVIEWER_1_MODEL")]
    reviewer_1_model: Option<String>,

    /// Style reviewer A API key  [fallback: MINIMAX_API_KEY]
    #[arg(long, env = "REVIEWER_1_API_KEY")]
    reviewer_1_api_key: Option<String>,

    /// Style reviewer A base URL (OpenAI-compat providers only) [fallback: MINIMAX_BASE_URL]
    #[arg(long, env = "REVIEWER_1_BASE_URL")]
    reviewer_1_base_url: Option<String>,

    // ── Style pair: Reviewer 2 ─────────────────────────────────────────────────

    /// Style reviewer B provider
    #[arg(long, default_value = "deepseek", env = "REVIEWER_2")]
    reviewer_2: ProviderKind,

    /// Style reviewer B model ID
    #[arg(long, env = "REVIEWER_2_MODEL")]
    reviewer_2_model: Option<String>,

    /// Style reviewer B API key  [fallback: DEEPSEEK_API_KEY]
    #[arg(long, env = "REVIEWER_2_API_KEY")]
    reviewer_2_api_key: Option<String>,

    /// Style reviewer B base URL (OpenAI-compat providers only) [fallback: DEEPSEEK_BASE_URL]
    #[arg(long, env = "REVIEWER_2_BASE_URL")]
    reviewer_2_base_url: Option<String>,

    // ── Logic pair: Reviewer 3 ─────────────────────────────────────────────────

    /// Logic reviewer A provider
    #[arg(long, default_value = "minimax", env = "REVIEWER_3")]
    reviewer_3: ProviderKind,

    /// Logic reviewer A model ID
    #[arg(long, env = "REVIEWER_3_MODEL")]
    reviewer_3_model: Option<String>,

    /// Logic reviewer A API key  [fallback: MINIMAX_API_KEY]
    #[arg(long, env = "REVIEWER_3_API_KEY")]
    reviewer_3_api_key: Option<String>,

    /// Logic reviewer A base URL (OpenAI-compat providers only) [fallback: MINIMAX_BASE_URL]
    #[arg(long, env = "REVIEWER_3_BASE_URL")]
    reviewer_3_base_url: Option<String>,

    // ── Logic pair: Reviewer 4 ─────────────────────────────────────────────────

    /// Logic reviewer B provider
    #[arg(long, default_value = "deepseek", env = "REVIEWER_4")]
    reviewer_4: ProviderKind,

    /// Logic reviewer B model ID
    #[arg(long, env = "REVIEWER_4_MODEL")]
    reviewer_4_model: Option<String>,

    /// Logic reviewer B API key  [fallback: DEEPSEEK_API_KEY]
    #[arg(long, env = "REVIEWER_4_API_KEY")]
    reviewer_4_api_key: Option<String>,

    /// Logic reviewer B base URL (OpenAI-compat providers only) [fallback: DEEPSEEK_BASE_URL]
    #[arg(long, env = "REVIEWER_4_BASE_URL")]
    reviewer_4_base_url: Option<String>,

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

    // ── Logging ────────────────────────────────────────────────────────────
    let filter = if cli.verbose {
        EnvFilter::new("ai_reviewer=debug,info")
    } else {
        EnvFilter::from_default_env()
    };
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .init();

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
    let key_1 = resolve_api_key(
        cli.reviewer_1_api_key.as_deref(),
        "REVIEWER_1_API_KEY",
        "MINIMAX_API_KEY",
    )?;
    let key_2 = resolve_api_key(
        cli.reviewer_2_api_key.as_deref(),
        "REVIEWER_2_API_KEY",
        "DEEPSEEK_API_KEY",
    )?;
    let key_3 = resolve_api_key(
        cli.reviewer_3_api_key.as_deref(),
        "REVIEWER_3_API_KEY",
        "MINIMAX_API_KEY",
    )?;
    let key_4 = resolve_api_key(
        cli.reviewer_4_api_key.as_deref(),
        "REVIEWER_4_API_KEY",
        "DEEPSEEK_API_KEY",
    )?;

    let base_url_1 = cli.reviewer_1_base_url.clone()
        .or_else(|| std::env::var("MINIMAX_BASE_URL").ok());
    let base_url_2 = cli.reviewer_2_base_url.clone()
        .or_else(|| std::env::var("DEEPSEEK_BASE_URL").ok());
    let base_url_3 = cli.reviewer_3_base_url.clone()
        .or_else(|| std::env::var("MINIMAX_BASE_URL").ok());
    let base_url_4 = cli.reviewer_4_base_url.clone()
        .or_else(|| std::env::var("DEEPSEEK_BASE_URL").ok());

    // ── Build reviewers ────────────────────────────────────────────────────
    let reviewer_style_a = build_reviewer(
        cli.reviewer_1.clone(),
        key_1,
        base_url_1,
        cli.reviewer_1_model.clone(),
        cli.max_retries,
        ReviewFocus::Style,
        cli.reviewer_timeout,
    )
    .context("Failed to build style reviewer A")?;

    let reviewer_style_b = build_reviewer(
        cli.reviewer_2.clone(),
        key_2,
        base_url_2,
        cli.reviewer_2_model.clone(),
        cli.max_retries,
        ReviewFocus::Style,
        cli.reviewer_timeout,
    )
    .context("Failed to build style reviewer B")?;

    let reviewer_logic_a = build_reviewer(
        cli.reviewer_3.clone(),
        key_3,
        base_url_3,
        cli.reviewer_3_model.clone(),
        cli.max_retries,
        ReviewFocus::Logic,
        cli.reviewer_timeout,
    )
    .context("Failed to build logic reviewer A")?;

    let reviewer_logic_b = build_reviewer(
        cli.reviewer_4.clone(),
        key_4,
        base_url_4,
        cli.reviewer_4_model.clone(),
        cli.max_retries,
        ReviewFocus::Logic,
        cli.reviewer_timeout,
    )
    .context("Failed to build logic reviewer B")?;

    let label_sa = reviewer_style_a.label().to_owned();
    let label_sb = reviewer_style_b.label().to_owned();
    let label_la = reviewer_logic_a.label().to_owned();
    let label_lb = reviewer_logic_b.label().to_owned();

    info!(
        style_a = %label_sa,
        style_b = %label_sb,
        logic_a = %label_la,
        logic_b = %label_lb,
        "Dispatching 4-way concurrent review"
    );

    // ── 4-way concurrent review ────────────────────────────────────────────
    let ctx_sa = ast_contexts.clone();
    let ctx_sb = ast_contexts.clone();
    let ctx_la = ast_contexts.clone();
    let ctx_lb = ast_contexts.clone();
    let policy_sa = policy_text.clone();
    let policy_sb = policy_text.clone();
    let policy_la = policy_text.clone();
    let policy_lb = policy_text.clone();

    let (r_sa, r_sb, r_la, r_lb) = tokio::join!(
        async move { reviewer_style_a.review(&ctx_sa, &policy_sa).await },
        async move { reviewer_style_b.review(&ctx_sb, &policy_sb).await },
        async move { reviewer_logic_a.review(&ctx_la, &policy_la).await },
        async move { reviewer_logic_b.review(&ctx_lb, &policy_lb).await },
    );

    info!(
        style_a_ok = r_sa.is_ok(),
        style_b_ok = r_sb.is_ok(),
        logic_a_ok = r_la.is_ok(),
        logic_b_ok = r_lb.is_ok(),
        "All 4 reviewers completed"
    );

    // ── Consensus evaluation ───────────────────────────────────────────────
    let style_pair = consensus::evaluate_pair(r_sa, r_sb, label_sa, label_sb, ReviewFocus::Style);
    let logic_pair = consensus::evaluate_pair(r_la, r_lb, label_la, label_lb, ReviewFocus::Logic);
    let consensus = consensus::evaluate(style_pair, logic_pair);

    // ── Output ─────────────────────────────────────────────────────────────
    println!("{}", report::render_summary(&consensus));

    let output_path = cli.output.unwrap_or_else(|| cli.source_root.join("review-report.md"));
    let report_md = report::render_report(&consensus);
    std::fs::write(&output_path, &report_md)
        .with_context(|| format!("Cannot write report to {}", output_path.display()))?;

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
            Ok(Box::new(LlmReviewer::new(model, "MiniMax", max_retries, focus, reviewer_timeout)))
        }
        ProviderKind::Deepseek => {
            let mid = model_id.unwrap_or_else(|| "deepseek-chat".into());
            let client = if let Some(url) = base_url {
                deepseek::Client::from_url(&api_key, &url)
            } else {
                deepseek::Client::new(&api_key)
            };
            let model = client.completion_model(&mid);
            Ok(Box::new(LlmReviewer::new(model, "DeepSeek", max_retries, focus, reviewer_timeout)))
        }
        ProviderKind::Anthropic => {
            let mid = model_id.unwrap_or_else(|| "claude-sonnet-4-6".into());
            let base = base_url.unwrap_or_else(|| "https://api.anthropic.com".into());
            let client = anthropic::Client::new(&api_key, &base, None, "2023-06-01");
            let model = client.completion_model(&mid);
            Ok(Box::new(LlmReviewer::new(model, "Anthropic", max_retries, focus, reviewer_timeout)))
        }
        ProviderKind::Gemini => {
            let mid = model_id.unwrap_or_else(|| "gemini-2.0-flash".into());
            let client = gemini::Client::new(&api_key);
            let model = client.completion_model(&mid);
            Ok(Box::new(LlmReviewer::new(model, "Gemini", max_retries, focus, reviewer_timeout)))
        }
        ProviderKind::Openai => {
            let mid = model_id.unwrap_or_else(|| "gpt-4o".into());
            let client = if let Some(url) = base_url {
                openai::Client::from_url(&api_key, &url)
            } else {
                openai::Client::new(&api_key)
            };
            let model = client.completion_model(&mid);
            Ok(Box::new(LlmReviewer::new(model, "OpenAI", max_retries, focus, reviewer_timeout)))
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
