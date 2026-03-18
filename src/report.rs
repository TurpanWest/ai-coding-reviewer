use chrono::Utc;

use crate::consensus::{gate_failure_reason, CONFIDENCE_THRESHOLD};
use crate::models::{ConsensusResult, Finding, PairResult, ReviewResult, Severity, Verdict};

// ── Public entry point ────────────────────────────────────────────────────────

/// Render a full Markdown cross-comparison report for a 4-model dual-pair review.
pub fn render_report(result: &ConsensusResult) -> String {
    let mut md = String::new();

    md.push_str("# AI Code Review Report\n\n");
    md.push_str(&format!(
        "_Generated: {}_\n\n",
        Utc::now().format("%Y-%m-%d %H:%M:%S UTC")
    ));

    let (badge, color_note) = match result.verdict {
        Verdict::Pass => ("✅ PASS", "Gate passed — all pairs approved with sufficient confidence."),
        Verdict::Fail => ("❌ FAIL", "Gate blocked — see failure analysis below."),
    };
    md.push_str(&format!("## Verdict: {badge}\n\n"));
    md.push_str(&format!("> {color_note}\n\n"));

    if !result.gate_passed {
        md.push_str("### Failure Analysis\n\n");
        md.push_str(&format!("**Reason**: {}\n\n", gate_failure_reason(result)));
    }

    // ── Confidence Summary (4 rows) ───────────────────────────────────────
    md.push_str("## Confidence Summary\n\n");
    md.push_str("| Model | Focus | Verdict | Confidence | Gate |\n");
    md.push_str("|---|---|---|---|---|\n");
    for (label, r, focus) in [
        (&result.pair_style.label_a, &result.pair_style.result_a, "Style"),
        (&result.pair_style.label_b, &result.pair_style.result_b, "Style"),
        (&result.pair_logic.label_a, &result.pair_logic.result_a, "Logic"),
        (&result.pair_logic.label_b, &result.pair_logic.result_b, "Logic"),
    ] {
        md.push_str(&confidence_row(label, r, focus));
    }
    md.push_str(&format!("\n_Threshold: {:.0}%_\n\n", CONFIDENCE_THRESHOLD * 100.0));

    // ── Findings (merged across both pairs) ───────────────────────────────
    md.push_str("## Findings (merged & deduplicated)\n\n");
    if result.all_findings.is_empty() {
        md.push_str("_No findings reported by any model._\n\n");
    } else {
        md.push_str("| # | Severity | Rule | Location | Description |\n");
        md.push_str("|---|---|---|---|---|\n");
        for (i, f) in result.all_findings.iter().enumerate() {
            md.push_str(&finding_row(i + 1, f));
        }
        md.push('\n');

        md.push_str("### Finding Details\n\n");
        for (i, f) in result.all_findings.iter().enumerate() {
            md.push_str(&finding_card(i + 1, f));
        }
    }

    // ── Model Reports (grouped by pair) ───────────────────────────────────
    md.push_str("## Model Reports\n\n");
    md.push_str(&pair_section("Style Review", &result.pair_style));
    md.push_str(&pair_section("Logic Review", &result.pair_logic));

    md
}

/// Compact one-line summary printed to stdout on any run.
pub fn render_summary(result: &ConsensusResult) -> String {
    let sa = &result.pair_style.result_a;
    let sb = &result.pair_style.result_b;
    let la = &result.pair_logic.result_a;
    let lb = &result.pair_logic.result_b;
    let n = result.all_findings.len();

    format!(
        "[ai-reviewer] Verdict: {verdict}  |  \
         {sla}[S]: {sav} ({sac:.0}%)  |  \
         {slb}[S]: {sbv} ({sbc:.0}%)  |  \
         {lla}[L]: {lav} ({lac:.0}%)  |  \
         {llb}[L]: {lbv} ({lbc:.0}%)  |  \
         Findings: {n}",
        verdict = result.verdict,
        sla = result.pair_style.label_a,
        sav = sa.verdict,
        sac = sa.confidence * 100.0,
        slb = result.pair_style.label_b,
        sbv = sb.verdict,
        sbc = sb.confidence * 100.0,
        lla = result.pair_logic.label_a,
        lav = la.verdict,
        lac = la.confidence * 100.0,
        llb = result.pair_logic.label_b,
        lbv = lb.verdict,
        lbc = lb.confidence * 100.0,
        n = n,
    )
}

// ── Rendering helpers ─────────────────────────────────────────────────────────

fn confidence_row(label: &str, r: &ReviewResult, focus: &str) -> String {
    let gate_ok = r.confidence >= CONFIDENCE_THRESHOLD && matches!(r.verdict, Verdict::Pass);
    let gate_sym = if gate_ok { "✅" } else { "❌" };
    format!(
        "| `{}` | {} | {} | {:.1}% | {} |\n",
        label,
        focus,
        r.verdict,
        r.confidence * 100.0,
        gate_sym,
    )
}

fn pair_section(title: &str, pair: &PairResult) -> String {
    let pair_status = if pair.pair_passed { "✅ PASS" } else { "❌ FAIL" };
    let mut s = format!("### Pair: {title} — {pair_status}\n\n");
    s.push_str(&format!("#### {} (`{}`)\n\n", pair.label_a, pair.result_a.model_id));
    s.push_str(&model_body(&pair.result_a));
    s.push_str(&format!("#### {} (`{}`)\n\n", pair.label_b, pair.result_b.model_id));
    s.push_str(&model_body(&pair.result_b));
    s
}

fn model_body(r: &ReviewResult) -> String {
    let mut s = String::new();
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

fn finding_row(n: usize, f: &Finding) -> String {
    let sev = severity_badge(&f.severity);
    let loc = format!("`{}:{}`", f.location.file, f.location.line_start);
    let desc = f.description.replace('|', "\\|");
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

fn severity_badge(s: &Severity) -> &'static str {
    match s {
        Severity::Critical => "🔴 CRITICAL",
        Severity::High => "🟠 HIGH",
        Severity::Medium => "🟡 MEDIUM",
        Severity::Low => "🔵 LOW",
        Severity::Info => "⚪ INFO",
    }
}
