use chrono::Utc;

use crate::consensus::{gate_failure_reason, CONFIDENCE_THRESHOLD};
use crate::models::{ConsensusResult, Finding, ReviewResult, Severity, Verdict};

// ── Public entry point ────────────────────────────────────────────────────────

/// Render a full Markdown cross-comparison report.
/// This is written to disk when the gate fails.
pub fn render_report(result: &ConsensusResult) -> String {
    let mut md = String::new();

    // ── Header ────────────────────────────────────────────────────────────────
    md.push_str("# AI Code Review Report\n\n");
    md.push_str(&format!(
        "_Generated: {}_\n\n",
        Utc::now().format("%Y-%m-%d %H:%M:%S UTC")
    ));

    // ── Overall verdict banner ─────────────────────────────────────────────
    let (badge, color_note) = match result.verdict {
        Verdict::Pass => ("✅ PASS", "Gate passed — both models approved with sufficient confidence."),
        Verdict::Fail => ("❌ FAIL", "Gate blocked — see failure analysis below."),
    };
    md.push_str(&format!("## Verdict: {badge}\n\n"));
    md.push_str(&format!("> {color_note}\n\n"));

    if !result.gate_passed {
        md.push_str("### Failure Analysis\n\n");
        md.push_str(&format!(
            "**Reason**: {}\n\n",
            gate_failure_reason(result)
        ));
    }

    // ── Confidence summary table ───────────────────────────────────────────
    md.push_str("## Confidence Summary\n\n");
    md.push_str("| Model | Verdict | Confidence | Gate |\n");
    md.push_str("|---|---|---|---|\n");
    md.push_str(&confidence_row(&result.minimax_result));
    md.push_str(&confidence_row(&result.deepseek_result));
    md.push_str(&format!(
        "\n_Threshold: {:.0}%_\n\n",
        CONFIDENCE_THRESHOLD * 100.0
    ));

    // ── Merged findings ────────────────────────────────────────────────────
    md.push_str("## Findings (merged & deduplicated)\n\n");
    if result.merged_findings.is_empty() {
        md.push_str("_No findings reported by either model._\n\n");
    } else {
        md.push_str("| # | Severity | Rule | Location | Description |\n");
        md.push_str("|---|---|---|---|---|\n");
        for (i, f) in result.merged_findings.iter().enumerate() {
            md.push_str(&finding_row(i + 1, f));
        }
        md.push('\n');

        // Detailed finding cards
        md.push_str("### Finding Details\n\n");
        for (i, f) in result.merged_findings.iter().enumerate() {
            md.push_str(&finding_card(i + 1, f));
        }
    }

    // ── Per-model detail ───────────────────────────────────────────────────
    md.push_str("## Model Reports\n\n");
    md.push_str(&model_section("MiniMax", &result.minimax_result));
    md.push_str(&model_section("DeepSeek", &result.deepseek_result));

    md
}

/// Compact one-line summary printed to stdout on any run.
pub fn render_summary(result: &ConsensusResult) -> String {
    let verdict = &result.verdict;
    let mm = &result.minimax_result;
    let ds = &result.deepseek_result;
    let n  = result.merged_findings.len();

    format!(
        "[ai-reviewer] Verdict: {verdict}  |  \
         MiniMax: {mm_v} ({mm_c:.0}%)  |  \
         DeepSeek: {ds_v} ({ds_c:.0}%)  |  \
         Findings: {n}",
        verdict = verdict,
        mm_v    = mm.verdict,
        mm_c    = mm.confidence * 100.0,
        ds_v    = ds.verdict,
        ds_c    = ds.confidence * 100.0,
        n       = n,
    )
}

// ── Rendering helpers ─────────────────────────────────────────────────────────

fn confidence_row(r: &ReviewResult) -> String {
    let gate_ok = r.confidence >= CONFIDENCE_THRESHOLD
        && matches!(r.verdict, Verdict::Pass);
    let gate_sym = if gate_ok { "✅" } else { "❌" };
    format!(
        "| `{}` | {} | {:.1}% | {} |\n",
        r.model_id,
        r.verdict,
        r.confidence * 100.0,
        gate_sym,
    )
}

fn finding_row(n: usize, f: &Finding) -> String {
    let sev = severity_badge(&f.severity);
    let loc = format!("`{}:{}`", f.location.file, f.location.line_start);
    let desc = f.description.replace('|', "\\|"); // escape table pipes
    format!("| {n} | {sev} | `{}` | {loc} | {desc} |\n", f.rule_id)
}

fn finding_card(n: usize, f: &Finding) -> String {
    let sev = severity_badge(&f.severity);
    format!(
        "#### Finding #{n}: {sev} `{rule}`\n\n\
         - **Location**: `{file}` lines {ls}–{le}\n\
         - **Description**: {desc}\n\
         - **Suggestion**: {sug}\n\n",
        sev  = sev,
        rule = f.rule_id,
        file = f.location.file,
        ls   = f.location.line_start,
        le   = f.location.line_end,
        desc = f.description,
        sug  = f.suggestion,
    )
}

fn model_section(name: &str, r: &ReviewResult) -> String {
    let mut s = format!("### {name} (`{}`)\n\n", r.model_id);
    s.push_str(&format!(
        "- **Verdict**: {}  |  **Confidence**: {:.1}%\n\n",
        r.verdict,
        r.confidence * 100.0
    ));
    s.push_str("**Reasoning**:\n\n");
    s.push_str(&format!("> {}\n\n", r.reasoning.replace('\n', "\n> ")));

    if r.findings.is_empty() {
        s.push_str("_No findings._\n\n");
    } else {
        s.push_str(&format!("**Findings ({}):**\n\n", r.findings.len()));
        for (i, f) in r.findings.iter().enumerate() {
            let sev = severity_badge(&f.severity);
            s.push_str(&format!(
                "{}. {sev} `{}` — {} _({}:{}–{})_\n",
                i + 1,
                f.rule_id,
                f.description,
                f.location.file,
                f.location.line_start,
                f.location.line_end,
            ));
        }
        s.push('\n');
    }
    s
}

fn severity_badge(s: &Severity) -> &'static str {
    match s {
        Severity::Critical => "🔴 CRITICAL",
        Severity::High     => "🟠 HIGH",
        Severity::Medium   => "🟡 MEDIUM",
        Severity::Low      => "🔵 LOW",
        Severity::Info     => "⚪ INFO",
    }
}
