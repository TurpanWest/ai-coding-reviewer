mod ast;
mod consensus;
mod diff;
mod models;
mod prompt;
mod report;
mod telemetry;
mod tools;

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

#[derive(ValueEnum, Clone, Copy, Debug)]
enum ProviderKind {
    Minimax,
    Deepseek,
    Anthropic,
    Gemini,
    Openai,
}

impl ProviderKind {
    /// Human-readable label used in reports and telemetry.
    fn label(self) -> &'static str {
        match self {
            ProviderKind::Minimax   => "MiniMax",
            ProviderKind::Deepseek  => "DeepSeek",
            ProviderKind::Anthropic => "Anthropic",
            ProviderKind::Gemini    => "Gemini",
            ProviderKind::Openai    => "OpenAI",
        }
    }

    /// Default base URL for providers that take one.  `None` means the provider
    /// has no configurable endpoint (i.e. Gemini, which routes through its SDK).
    fn default_base_url(self) -> Option<&'static str> {
        match self {
            ProviderKind::Minimax   => Some("https://api.minimax.chat/v1"),
            // DeepSeek's API is fully OpenAI-compatible.  Routing through
            // openai::Client (rather than rig's deepseek::Client) preserves the
            // `usage` field — the native DeepSeek provider drops it.
            ProviderKind::Deepseek  => Some("https://api.deepseek.com/v1"),
            ProviderKind::Anthropic => Some("https://api.anthropic.com"),
            ProviderKind::Openai    => None,
            ProviderKind::Gemini    => None,
        }
    }

    fn default_model(self) -> &'static str {
        match self {
            ProviderKind::Minimax   => "MiniMax-M2.7",
            ProviderKind::Deepseek  => "deepseek-chat",
            ProviderKind::Anthropic => "claude-sonnet-4-6",
            ProviderKind::Gemini    => "gemini-3.1-pro-preview",
            ProviderKind::Openai    => "gpt-5.4",
        }
    }
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

    /// Reviewer A API key
    #[arg(long, env = "REVIEWER_1_API_KEY")]
    reviewer_1_api_key: Option<String>,

    /// Reviewer A base URL (OpenAI-compat providers only)
    #[arg(long, env = "REVIEWER_1_BASE_URL")]
    reviewer_1_base_url: Option<String>,

    // ── Reviewer B (other side of every group pair) ────────────────────────────

    /// Reviewer B provider — used as the second model in all four focus groups
    #[arg(long, default_value = "deepseek", env = "REVIEWER_2")]
    reviewer_2: ProviderKind,

    /// Reviewer B model ID
    #[arg(long, env = "REVIEWER_2_MODEL")]
    reviewer_2_model: Option<String>,

    /// Reviewer B API key
    #[arg(long, env = "REVIEWER_2_API_KEY")]
    reviewer_2_api_key: Option<String>,

    /// Reviewer B base URL (OpenAI-compat providers only)
    #[arg(long, env = "REVIEWER_2_BASE_URL")]
    reviewer_2_base_url: Option<String>,

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
        let ctx = ast::extract_context(fd, &cli.source_root, &fd.raw_chunk)?;
        info!(
            file = %ctx.file,
            changed_symbols = ctx.changed_symbols.len(),
            call_edges = ctx.call_edges.len(),
            "AST context extracted"
        );
        ast_contexts.push(ctx);
    }

    // ── Resolve API keys ───────────────────────────────────────────────────
    let key_a = resolve_api_key(cli.reviewer_1_api_key.as_deref(), "REVIEWER_1_API_KEY")?;
    let key_b = resolve_api_key(cli.reviewer_2_api_key.as_deref(), "REVIEWER_2_API_KEY")?;

    let base_url_a = cli.reviewer_1_base_url.clone();
    let base_url_b = cli.reviewer_2_base_url.clone();

    // ── All 4 focus groups always run, each reviewing every changed file ───
    // Each group uses the same A+B reviewer pair but with a distinct focus:
    //   G0 = Security · G1 = Correctness · G2 = Performance · G3 = Maintainability
    // Files are NOT split across groups — every group sees all changed files.
    // This ensures complete coverage even when only one file is modified.
    const FOCUSES: [ReviewFocus; 4] = [
        ReviewFocus::Security,
        ReviewFocus::Correctness,
        ReviewFocus::Performance,
        ReviewFocus::Maintainability,
    ];

    let n_groups = FOCUSES.len();
    let file_groups: Vec<Vec<crate::ast::FileAstContext>> = (0..n_groups)
        .map(|_| ast_contexts.clone())
        .collect();

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
                ReviewerCfg {
                    kind: cli.reviewer_1,
                    api_key: key_a.clone(),
                    base_url: base_url_a.clone(),
                    model_id: cli.reviewer_1_model.clone(),
                    max_retries: cli.max_retries,
                    reviewer_timeout: cli.reviewer_timeout,
                    source_root: cli.source_root.clone(),
                },
                focus,
            )
            .with_context(|| format!("Failed to build reviewer A for group {i}"))?;
            let rb = build_reviewer(
                ReviewerCfg {
                    kind: cli.reviewer_2,
                    api_key: key_b.clone(),
                    base_url: base_url_b.clone(),
                    model_id: cli.reviewer_2_model.clone(),
                    max_retries: cli.max_retries,
                    reviewer_timeout: cli.reviewer_timeout,
                    source_root: cli.source_root.clone(),
                },
                focus,
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
    println!("{}", report::render_summary(&consensus));

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

    Ok(consensus.gate_passed)
}

// ── Provider builder ───────────────────────────────────────────────────────────

/// Per-reviewer configuration gathered from CLI flags / env vars.
struct ReviewerCfg {
    kind: ProviderKind,
    api_key: String,
    base_url: Option<String>,
    model_id: Option<String>,
    max_retries: u32,
    reviewer_timeout: u64,
    source_root: PathBuf,
}

fn build_reviewer(cfg: ReviewerCfg, focus: ReviewFocus) -> Result<Box<dyn Reviewer>> {
    let label = cfg.kind.label();
    let mid = cfg.model_id.unwrap_or_else(|| cfg.kind.default_model().into());
    let base = cfg
        .base_url
        .or_else(|| cfg.kind.default_base_url().map(str::to_owned));

    // Anthropic benefits from provider-side prefix caching of the stable system
    // prompt (policy + schema).  All OpenAI-compat providers handle caching
    // transparently, so no extra_params is needed there.
    let extra_params = match cfg.kind {
        ProviderKind::Anthropic => {
            Some(serde_json::json!({"cache_control": {"type": "ephemeral"}}))
        }
        _ => None,
    };

    // Each arm builds a different concrete CompletionModel type, so we have to
    // materialise the boxed reviewer inline rather than funnelling through a
    // generic helper.
    let reviewer: Box<dyn Reviewer> = match cfg.kind {
        ProviderKind::Minimax | ProviderKind::Deepseek | ProviderKind::Openai => {
            let client = match base {
                Some(url) => openai::Client::from_url(&cfg.api_key, &url),
                None => openai::Client::new(&cfg.api_key),
            };
            Box::new(LlmReviewer::new(
                client.completion_model(&mid),
                label,
                cfg.max_retries,
                focus,
                cfg.reviewer_timeout,
                extra_params,
                cfg.source_root,
            ))
        }
        ProviderKind::Anthropic => {
            let base = base.unwrap_or_else(|| "https://api.anthropic.com".into());
            let client = anthropic::Client::new(&cfg.api_key, &base, None, "2023-06-01");
            Box::new(LlmReviewer::new(
                client.completion_model(&mid),
                label,
                cfg.max_retries,
                focus,
                cfg.reviewer_timeout,
                extra_params,
                cfg.source_root,
            ))
        }
        ProviderKind::Gemini => {
            let client = gemini::Client::new(&cfg.api_key);
            Box::new(LlmReviewer::new(
                client.completion_model(&mid),
                label,
                cfg.max_retries,
                focus,
                cfg.reviewer_timeout,
                extra_params,
                cfg.source_root,
            ))
        }
    };
    Ok(reviewer)
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

/// Resolve an API key from the CLI arg or its corresponding env var.
///
/// `env_var` is read by clap via `#[arg(env = "...")]` and arrives through
/// `explicit`; it is listed here solely for the error message.
fn resolve_api_key(explicit: Option<&str>, env_var: &str) -> Result<String> {
    if let Some(k) = explicit {
        return Ok(k.to_owned());
    }
    Err(anyhow::anyhow!("API key not set (set {env_var} or pass --reviewer-N-api-key)"))
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
