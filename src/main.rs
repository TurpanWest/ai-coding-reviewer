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
use clap::Parser as ClapParser;
use tracing::info;
use tracing_subscriber::EnvFilter;

use crate::models::deepseek::DeepSeekReviewer;
use crate::models::minimax::MinimaxReviewer;

// ── CLI definition ─────────────────────────────────────────────────────────────

#[derive(ClapParser, Debug)]
#[command(
    name        = "ai-reviewer",
    version,
    about       = "AI-to-AI code review engine: MiniMax × DeepSeek cross-validation",
    long_about  = None,
)]
struct Cli {
    /// Path to unified diff file, or "-" to read from stdin
    #[arg(short = 'd', long, value_name = "PATH")]
    diff: String,

    /// Repository root for full-file AST context resolution
    #[arg(short = 's', long, value_name = "PATH", default_value = ".")]
    source_root: PathBuf,

    /// Security/coding policy Markdown file (injected into system prompt & cached)
    #[arg(short = 'p', long, value_name = "PATH")]
    policy: PathBuf,

    /// Confidence gate threshold (0.0–1.0)
    #[arg(short = 't', long, default_value_t = 0.90)]
    threshold: f64,

    /// Output path for the Markdown review report
    #[arg(short = 'o', long, value_name = "PATH", default_value = "review-report.md")]
    output: PathBuf,

    /// Maximum self-correction retries per model
    #[arg(long, default_value_t = 3)]
    max_retries: u32,

    /// MiniMax model ID
    #[arg(long, default_value = "MiniMax-M2.5", env = "MINIMAX_MODEL")]
    model_minimax: String,

    /// MiniMax API base URL (Anthropic-compat endpoint)
    #[arg(long, default_value = "https://api.minimax.chat/v1", env = "MINIMAX_BASE_URL")]
    minimax_base_url: String,

    /// DeepSeek model ID
    #[arg(long, default_value = "deepseek-chat", env = "DEEPSEEK_MODEL")]
    model_deepseek: String,

    /// DeepSeek API base URL (OpenAI-compat endpoint)
    #[arg(long, default_value = "https://api.deepseek.com/v1", env = "DEEPSEEK_BASE_URL")]
    deepseek_base_url: String,

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

    // ── Parse diff ─────────────────────────────────────────────────────────
    let file_diffs = diff::parse_diff(&diff_text)
        .context("Failed to parse unified diff")?;

    if file_diffs.is_empty() {
        eprintln!("[ai-reviewer] No file changes found in diff. Exiting with PASS.");
        return Ok(true);
    }

    let changed_files: Vec<_> = file_diffs.iter().filter(|f| f.has_changes()).collect();
    info!(files = changed_files.len(), "Files with changes");

    // ── Extract AST contexts ───────────────────────────────────────────────
    let mut ast_contexts = Vec::new();
    for fd in &changed_files {
        // Split the raw diff back into per-file chunks for the prompt
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

    // ── API keys ───────────────────────────────────────────────────────────
    let minimax_key = std::env::var("MINIMAX_API_KEY")
        .context("MINIMAX_API_KEY environment variable not set")?;
    let deepseek_key = std::env::var("DEEPSEEK_API_KEY")
        .context("DEEPSEEK_API_KEY environment variable not set")?;

    // ── Build reviewers ────────────────────────────────────────────────────
    let minimax_reviewer = MinimaxReviewer::with_config(
        minimax_key,
        cli.minimax_base_url.clone(),
        cli.model_minimax.clone(),
        cli.max_retries,
    );
    let deepseek_reviewer = DeepSeekReviewer::with_config(
        deepseek_key,
        cli.deepseek_base_url.clone(),
        cli.model_deepseek.clone(),
        cli.max_retries,
    );

    // ── Concurrent dual-model review ───────────────────────────────────────
    info!("Dispatching concurrent review requests to MiniMax and DeepSeek");

    let contexts_mm = ast_contexts.clone();
    let contexts_ds = ast_contexts.clone();
    let policy_mm   = policy_text.clone();
    let policy_ds   = policy_text.clone();

    let (mm_res, ds_res) = tokio::join!(
        async move { minimax_reviewer.review(&contexts_mm, &policy_mm).await },
        async move { deepseek_reviewer.review(&contexts_ds, &policy_ds).await },
    );

    info!(
        minimax_ok  = mm_res.is_ok(),
        deepseek_ok = ds_res.is_ok(),
        "Both reviewers completed"
    );

    // ── Consensus evaluation ───────────────────────────────────────────────
    let consensus = consensus::evaluate(mm_res, ds_res);

    // ── Output ─────────────────────────────────────────────────────────────
    println!("{}", report::render_summary(&consensus));

    // Always write the report — even on PASS, findings (LOW/INFO) are useful.
    let report_md = report::render_report(&consensus);
    std::fs::write(&cli.output, &report_md)
        .with_context(|| format!("Cannot write report to {}", cli.output.display()))?;
    if consensus.gate_passed {
        println!(
            "[ai-reviewer] Gate PASSED — full report: {}",
            cli.output.display()
        );
    } else {
        eprintln!(
            "[ai-reviewer] Gate FAILED — report written to: {}",
            cli.output.display()
        );
    }

    Ok(consensus.gate_passed)
}

// ── Helpers ────────────────────────────────────────────────────────────────────

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

/// Extract the portion of the unified diff that belongs to a specific file,
/// identified by the `+++ b/<path>` header.  Falls back to the full diff
/// text if the file header is not found.
fn extract_file_diff_chunk(full_diff: &str, fd: &diff::FileDiff) -> String {
    let target = match fd.source_path() {
        Some(p) => p.display().to_string(),
        None => return String::new(),
    };

    let mut lines = full_diff.lines().peekable();
    let mut chunk = String::new();
    let mut capturing = false;

    while let Some(line) = lines.next() {
        if line.starts_with("diff --git ") {
            if capturing {
                // Reached next file — stop
                break;
            }
            // Check if this is our target file
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
        // Best-effort: return the full diff
        full_diff.to_owned()
    } else {
        chunk
    }
}
