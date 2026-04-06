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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::ReviewError;
    use crate::prompt::ReviewFocus;

    // ── Helpers ───────────────────────────────────────────────────────────────

    fn pass(confidence: f64) -> ReviewResult {
        ReviewResult {
            model_id: "test".into(),
            verdict: Verdict::Pass,
            confidence,
            findings: vec![],
            reasoning: String::new(),
        }
    }

    fn fail_result(confidence: f64) -> ReviewResult {
        ReviewResult { verdict: Verdict::Fail, ..pass(confidence) }
    }

    fn finding(file: &str, line: u32, rule: &str, sev: Severity) -> Finding {
        Finding {
            severity: sev,
            location: CodeLocation { file: file.into(), line_start: line, line_end: line },
            rule_id: rule.into(),
            description: "desc".into(),
            suggestion: "fix".into(),
        }
    }

    fn pair(ra: Result<ReviewResult, ReviewError>, rb: Result<ReviewResult, ReviewError>) -> PairResult {
        evaluate_pair(ra, rb, "A".into(), "B".into(), ReviewFocus::Security, 0, vec![])
    }

    // ── evaluate_pair ─────────────────────────────────────────────────────────

    #[test]
    fn test_pair_both_pass_high_confidence() {
        assert!(pair(Ok(pass(0.95)), Ok(pass(0.92))).pair_passed);
    }

    #[test]
    fn test_pair_one_verdict_fails() {
        assert!(!pair(Ok(fail_result(0.95)), Ok(pass(0.95))).pair_passed);
    }

    #[test]
    fn test_pair_low_confidence_blocks() {
        assert!(!pair(Ok(pass(0.89)), Ok(pass(0.95))).pair_passed);
    }

    #[test]
    fn test_pair_confidence_exactly_at_threshold_passes() {
        assert!(pair(Ok(pass(CONFIDENCE_THRESHOLD)), Ok(pass(CONFIDENCE_THRESHOLD))).pair_passed);
    }

    #[test]
    fn test_pair_reviewer_error_becomes_fail() {
        let p = pair(Ok(pass(0.95)), Err(ReviewError::Completion("timeout".into())));
        assert!(!p.pair_passed);
        assert_eq!(p.result_b.verdict, Verdict::Fail);
        assert_eq!(p.result_b.confidence, 1.0); // synthetic fail has confidence 1.0
    }

    // ── evaluate (gate) ───────────────────────────────────────────────────────

    #[test]
    fn test_evaluate_all_groups_pass() {
        let pairs = vec![
            pair(Ok(pass(0.95)), Ok(pass(0.95))),
            pair(Ok(pass(0.92)), Ok(pass(0.91))),
        ];
        let r = evaluate(pairs);
        assert!(r.gate_passed);
        assert_eq!(r.verdict, Verdict::Pass);
    }

    #[test]
    fn test_evaluate_one_group_fails_blocks_gate() {
        let pairs = vec![
            pair(Ok(pass(0.95)), Ok(pass(0.95))),
            pair(Ok(fail_result(0.95)), Ok(pass(0.95))),
        ];
        let r = evaluate(pairs);
        assert!(!r.gate_passed);
        assert_eq!(r.verdict, Verdict::Fail);
    }

    // ── merge_and_dedup ───────────────────────────────────────────────────────

    #[test]
    fn test_dedup_same_key_higher_severity_wins() {
        let low  = finding("f.rs", 5, "R1", Severity::Low);
        let high = finding("f.rs", 5, "R1", Severity::High);
        let merged = merge_and_dedup(&[low], &[high]);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].severity, Severity::High);
    }

    #[test]
    fn test_dedup_different_line_both_kept() {
        let a = finding("f.rs", 1, "R1", Severity::Low);
        let b = finding("f.rs", 2, "R1", Severity::Low);
        assert_eq!(merge_and_dedup(&[a], &[b]).len(), 2);
    }

    #[test]
    fn test_dedup_sorted_by_severity_descending() {
        let low  = finding("f.rs", 1, "R1", Severity::Low);
        let crit = finding("f.rs", 2, "R2", Severity::Critical);
        let merged = merge_and_dedup(&[low], &[crit]);
        assert_eq!(merged[0].severity, Severity::Critical);
    }

    // ── gate_failure_reason ───────────────────────────────────────────────────

    #[test]
    fn test_gate_failure_reason_passed_returns_simple_string() {
        let pairs = vec![pair(Ok(pass(0.95)), Ok(pass(0.95)))];
        let r = evaluate(pairs);
        assert_eq!(gate_failure_reason(&r), "Gate passed.");
    }

    #[test]
    fn test_gate_failure_reason_mentions_focus() {
        let pairs = vec![pair(Ok(fail_result(0.95)), Ok(pass(0.95)))];
        let r = evaluate(pairs);
        let reason = gate_failure_reason(&r);
        assert!(reason.to_uppercase().contains("SECURITY"));
    }
}

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
