use std::collections::HashMap;

use crate::models::{CodeLocation, ConsensusResult, Finding, ReviewError, ReviewResult, Severity, Verdict};

// ── Gate threshold ────────────────────────────────────────────────────────────

pub const CONFIDENCE_THRESHOLD: f64 = 0.90;

// ── Main consensus logic ──────────────────────────────────────────────────────

/// Evaluate two model results and produce a final consensus verdict.
///
/// The gate passes **only** when:
/// 1. Both models' confidence >= `CONFIDENCE_THRESHOLD`
/// 2. Both models agree on a `Pass` verdict
///
/// In every other case the gate fails and the merged findings are reported.
pub fn evaluate(
    minimax_res:  Result<ReviewResult, ReviewError>,
    deepseek_res: Result<ReviewResult, ReviewError>,
) -> ConsensusResult {
    let minimax  = unwrap_or_fail(minimax_res,  "minimax");
    let deepseek = unwrap_or_fail(deepseek_res, "deepseek-chat");

    let both_confident = minimax.confidence  >= CONFIDENCE_THRESHOLD
                      && deepseek.confidence >= CONFIDENCE_THRESHOLD;

    let verdicts_agree = matches!(
        (&minimax.verdict, &deepseek.verdict),
        (Verdict::Pass, Verdict::Pass) | (Verdict::Fail, Verdict::Fail)
    );

    let gate_passed = both_confident
        && verdicts_agree
        && matches!(minimax.verdict, Verdict::Pass);

    let verdict = if gate_passed { Verdict::Pass } else { Verdict::Fail };

    let merged_findings = merge_and_dedup(&minimax.findings, &deepseek.findings);

    ConsensusResult {
        verdict,
        minimax_result:  minimax,
        deepseek_result: deepseek,
        merged_findings,
        gate_passed,
    }
}

/// Returns a human-readable explanation of *why* the gate failed.
pub fn gate_failure_reason(result: &ConsensusResult) -> String {
    if result.gate_passed {
        return "Gate passed.".into();
    }

    let m = &result.minimax_result;
    let d = &result.deepseek_result;

    let mut reasons: Vec<String> = Vec::new();

    if m.confidence < CONFIDENCE_THRESHOLD {
        reasons.push(format!(
            "MiniMax confidence too low ({:.2} < {:.2})",
            m.confidence, CONFIDENCE_THRESHOLD
        ));
    }
    if d.confidence < CONFIDENCE_THRESHOLD {
        reasons.push(format!(
            "DeepSeek confidence too low ({:.2} < {:.2})",
            d.confidence, CONFIDENCE_THRESHOLD
        ));
    }
    if m.verdict != d.verdict {
        reasons.push(format!(
            "Verdict conflict: MiniMax={} vs DeepSeek={}",
            m.verdict, d.verdict
        ));
    }
    if matches!(m.verdict, Verdict::Fail) && matches!(d.verdict, Verdict::Fail) {
        reasons.push("Both models confirmed defects".into());
    }

    if reasons.is_empty() {
        "Unknown gate failure".into()
    } else {
        reasons.join("; ")
    }
}

// ── Finding deduplication ─────────────────────────────────────────────────────

/// Merge findings from both models.  Two findings are considered duplicates if
/// they share the same `file` + `line_start` + `rule_id`.  When duplicates
/// exist the one with higher severity is kept; the other's description is
/// appended as context.
fn merge_and_dedup(a: &[Finding], b: &[Finding]) -> Vec<Finding> {
    // Key: (file, line_start, rule_id)
    let mut map: HashMap<(String, u32, String), Finding> = HashMap::new();

    for finding in a.iter().chain(b.iter()) {
        let key = (
            finding.location.file.clone(),
            finding.location.line_start,
            finding.rule_id.clone(),
        );
        map.entry(key)
            .and_modify(|existing| {
                // Keep the higher severity
                if severity_rank(&finding.severity) > severity_rank(&existing.severity) {
                    existing.severity = finding.severity.clone();
                }
                // Merge descriptions if they differ
                if existing.description != finding.description {
                    existing.description = format!(
                        "{} | [alt] {}",
                        existing.description, finding.description
                    );
                }
            })
            .or_insert_with(|| finding.clone());
    }

    // Sort: CRITICAL first, then by file + line
    let mut findings: Vec<Finding> = map.into_values().collect();
    findings.sort_by(|a, b| {
        severity_rank(&b.severity)
            .cmp(&severity_rank(&a.severity))
            .then(a.location.file.cmp(&b.location.file))
            .then(a.location.line_start.cmp(&b.location.line_start))
    });
    findings
}

fn severity_rank(s: &Severity) -> u8 {
    match s {
        Severity::Critical => 5,
        Severity::High     => 4,
        Severity::Medium   => 3,
        Severity::Low      => 2,
        Severity::Info     => 1,
    }
}

// ── Error → synthetic ReviewResult ───────────────────────────────────────────

/// Convert a `ReviewError` into a synthetic `ReviewResult` that represents a
/// hard failure, so that the consensus engine always has two results to work
/// with regardless of network/parse errors.
fn unwrap_or_fail(
    res:      Result<ReviewResult, ReviewError>,
    model_id: &str,
) -> ReviewResult {
    match res {
        Ok(r) => r,
        Err(e) => {
            let description = format!("Reviewer error: {e}");
            ReviewResult {
                model_id:   model_id.to_owned(),
                verdict:    Verdict::Fail,
                confidence: 1.0, // We are 100% confident that an error = block
                findings:   vec![Finding {
                    severity:    Severity::Critical,
                    location:    CodeLocation {
                        file:       "<reviewer-error>".into(),
                        line_start: 0,
                        line_end:   0,
                    },
                    rule_id:     "INTERNAL-001".into(),
                    description,
                    suggestion:  "Check reviewer logs and API key configuration.".into(),
                }],
                reasoning: format!("Reviewer failed to produce a valid result: {e}"),
            }
        }
    }
}
