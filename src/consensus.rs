use std::collections::HashMap;

use crate::models::{
    CodeLocation, ConsensusResult, Finding, PairResult, ReviewError, ReviewFocus, ReviewResult,
    Severity, Verdict,
};

// ── Gate threshold ────────────────────────────────────────────────────────────

pub const CONFIDENCE_THRESHOLD: f64 = 0.90;

// ── Pair-level consensus ──────────────────────────────────────────────────────

/// Evaluate a single reviewer pair and produce a `PairResult`.
///
/// The pair passes only when:
/// 1. Both models' confidence >= `CONFIDENCE_THRESHOLD`
/// 2. Both models agree on a `Pass` verdict
pub fn evaluate_pair(
    res_a: Result<ReviewResult, ReviewError>,
    res_b: Result<ReviewResult, ReviewError>,
    label_a: String,
    label_b: String,
    focus: ReviewFocus,
    group_index: usize,
    files: Vec<String>,
) -> PairResult {
    let result_a = unwrap_or_fail(res_a, &label_a);
    let result_b = unwrap_or_fail(res_b, &label_b);

    let both_confident = result_a.confidence >= CONFIDENCE_THRESHOLD
        && result_b.confidence >= CONFIDENCE_THRESHOLD;

    let both_pass = matches!(result_a.verdict, Verdict::Pass)
        && matches!(result_b.verdict, Verdict::Pass);

    let pair_passed = both_confident && both_pass;

    let merged_findings = merge_and_dedup(&result_a.findings, &result_b.findings);

    PairResult {
        focus: focus.as_str().to_owned(),
        group_index,
        files,
        label_a,
        label_b,
        result_a,
        result_b,
        merged_findings,
        pair_passed,
    }
}

// ── Final consensus ───────────────────────────────────────────────────────────

/// Combine all group pair results into the overall `ConsensusResult`.
/// The gate passes only when **every group** passes.
pub fn evaluate(groups: Vec<PairResult>) -> ConsensusResult {
    let gate_passed = groups.iter().all(|g| g.pair_passed);
    let verdict = if gate_passed { Verdict::Pass } else { Verdict::Fail };

    let flat: Vec<Finding> = groups.iter()
        .flat_map(|g| g.merged_findings.iter().cloned())
        .collect();
    let all_findings = merge_and_dedup(&flat, &[]);

    ConsensusResult { verdict, groups, all_findings, gate_passed }
}

/// Returns a human-readable explanation of *why* the gate failed.
pub fn gate_failure_reason(result: &ConsensusResult) -> String {
    if result.gate_passed {
        return "Gate passed.".into();
    }

    let mut reasons: Vec<String> = Vec::new();

    for group in &result.groups {
        if !group.pair_passed {
            let a = &group.result_a;
            let b = &group.result_b;
            let la = &group.label_a;
            let lb = &group.label_b;
            let focus = group.focus.to_uppercase();
            let g = group.group_index + 1;

            if a.confidence < CONFIDENCE_THRESHOLD {
                reasons.push(format!(
                    "[G{g}/{focus}] {la} confidence too low ({:.2} < {:.2})",
                    a.confidence, CONFIDENCE_THRESHOLD
                ));
            }
            if b.confidence < CONFIDENCE_THRESHOLD {
                reasons.push(format!(
                    "[G{g}/{focus}] {lb} confidence too low ({:.2} < {:.2})",
                    b.confidence, CONFIDENCE_THRESHOLD
                ));
            }
            if a.verdict != b.verdict {
                reasons.push(format!(
                    "[G{g}/{focus}] Verdict conflict: {la}={} vs {lb}={}",
                    a.verdict, b.verdict
                ));
            }
            if matches!(a.verdict, Verdict::Fail) && matches!(b.verdict, Verdict::Fail) {
                reasons.push(format!("[G{g}/{focus}] Both models confirmed defects"));
            }
        }
    }

    if reasons.is_empty() {
        "Unknown gate failure".into()
    } else {
        reasons.join("; ")
    }
}

// ── Finding deduplication ─────────────────────────────────────────────────────

/// Merge findings from two slices.  Two findings are duplicates if they share
/// the same `file` + `line_start` + `rule_id`.  When duplicates exist the one
/// with higher severity is kept; the other's description is appended as context.
fn merge_and_dedup(a: &[Finding], b: &[Finding]) -> Vec<Finding> {
    let mut map: HashMap<(String, u32, String), Finding> = HashMap::new();

    for finding in a.iter().chain(b.iter()) {
        let key = (
            finding.location.file.clone(),
            finding.location.line_start,
            finding.rule_id.clone(),
        );
        map.entry(key)
            .and_modify(|existing| {
                if severity_rank(&finding.severity) > severity_rank(&existing.severity) {
                    existing.severity = finding.severity.clone();
                }
                if existing.description != finding.description {
                    existing.description = format!(
                        "{} | [alt] {}",
                        existing.description, finding.description
                    );
                }
            })
            .or_insert_with(|| finding.clone());
    }

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
        Severity::High => 4,
        Severity::Medium => 3,
        Severity::Low => 2,
        Severity::Info => 1,
    }
}

// ── Error → synthetic ReviewResult ───────────────────────────────────────────

fn unwrap_or_fail(res: Result<ReviewResult, ReviewError>, label: &str) -> ReviewResult {
    match res {
        Ok(r) => r,
        Err(e) => {
            let description = format!("Reviewer error: {e}");
            ReviewResult {
                model_id: label.to_owned(),
                verdict: Verdict::Fail,
                confidence: 1.0,
                findings: vec![Finding {
                    severity: Severity::Critical,
                    location: CodeLocation {
                        file: "<reviewer-error>".into(),
                        line_start: 0,
                        line_end: 0,
                    },
                    rule_id: "INTERNAL-001".into(),
                    description,
                    suggestion: "Check reviewer logs and API key configuration.".into(),
                }],
                reasoning: format!("Reviewer failed to produce a valid result: {e}"),
            }
        }
    }
}
