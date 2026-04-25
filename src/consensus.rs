use std::collections::{HashMap, HashSet};

use crate::models::{
    CodeLocation, ConsensusResult, Finding, PairResult, ReviewError, ReviewFocus, ReviewResult,
    Severity, Verdict,
};
use crate::policy::filter_findings;

// ── Gate thresholds ───────────────────────────────────────────────────────────

/// Strict threshold for security/correctness — RCE-class issues should be
/// gated by high reviewer confidence.
pub const STRICT_CONFIDENCE_THRESHOLD: f64 = 0.90;

/// Lenient threshold for performance/maintainability — these are largely style
/// judgements; treating them with the same bar as exploitable defects produces
/// noise without protection.
pub const LENIENT_CONFIDENCE_THRESHOLD: f64 = 0.80;

/// Per-focus confidence threshold.
pub fn confidence_threshold_for(focus: ReviewFocus) -> f64 {
    match focus {
        ReviewFocus::Security | ReviewFocus::Correctness => STRICT_CONFIDENCE_THRESHOLD,
        ReviewFocus::Performance | ReviewFocus::Maintainability => LENIENT_CONFIDENCE_THRESHOLD,
    }
}

/// A finding is "blocking" if its severity is MEDIUM or higher.  Below that,
/// findings are informational and shouldn't drag the gate down on their own.
fn is_blocking_severity(s: &Severity) -> bool {
    matches!(s, Severity::Critical | Severity::High | Severity::Medium)
}

// ── Pair-level consensus ──────────────────────────────────────────────────────

/// Evaluate a single reviewer pair and produce a `PairResult`.
///
/// The pair passes only when both models return `Pass`.  Once that's true,
/// the confidence threshold is applied as a *tiebreaker*: it must be cleared
/// only when the pair is in a contested state — either there's a disagreement
/// (one model voted `Fail`, handled above as auto-fail) or the merged findings
/// include something at MEDIUM severity or higher.  When both models are
/// confident PASS and report nothing meaningful, low confidence on its own
/// will not block the gate — that path is essentially style judgement.
#[allow(clippy::too_many_arguments)]
pub fn evaluate_pair(
    res_a: Result<ReviewResult, ReviewError>,
    res_b: Result<ReviewResult, ReviewError>,
    label_a: String,
    label_b: String,
    focus: ReviewFocus,
    group_index: usize,
    files: Vec<String>,
    allowed_rules: &HashSet<String>,
) -> PairResult {
    let mut result_a = unwrap_or_fail(res_a, &label_a);
    let mut result_b = unwrap_or_fail(res_b, &label_b);

    drop_unknown_rules(&mut result_a, &label_a, focus, allowed_rules);
    drop_unknown_rules(&mut result_b, &label_b, focus, allowed_rules);

    let threshold = confidence_threshold_for(focus);

    let both_pass = matches!(result_a.verdict, Verdict::Pass)
        && matches!(result_b.verdict, Verdict::Pass);

    let merged_findings = merge_and_dedup(&result_a.findings, &result_b.findings);
    let has_blocking_finding = merged_findings.iter().any(|f| is_blocking_severity(&f.severity));

    let confidence_required = !both_pass || has_blocking_finding;
    let both_confident = result_a.confidence >= threshold && result_b.confidence >= threshold;

    let pair_passed = both_pass && (!confidence_required || both_confident);

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
        confidence_threshold: threshold,
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
            let threshold = group.confidence_threshold;

            let both_pass = matches!(a.verdict, Verdict::Pass)
                && matches!(b.verdict, Verdict::Pass);
            let has_blocking_finding = group
                .merged_findings
                .iter()
                .any(|f| is_blocking_severity(&f.severity));
            let confidence_required = !both_pass || has_blocking_finding;

            if confidence_required && a.confidence < threshold {
                reasons.push(format!(
                    "[G{g}/{focus}] {la} confidence too low ({:.2} < {:.2})",
                    a.confidence, threshold
                ));
            }
            if confidence_required && b.confidence < threshold {
                reasons.push(format!(
                    "[G{g}/{focus}] {lb} confidence too low ({:.2} < {:.2})",
                    b.confidence, threshold
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

// ── Rule-ID post-validation ──────────────────────────────────────────────────

/// Drop findings whose `rule_id` is not declared in the policy.  Models
/// occasionally invent rule IDs; we surface those as a warning and discard
/// them rather than letting them reach the report.
fn drop_unknown_rules(
    result: &mut ReviewResult,
    label: &str,
    focus: ReviewFocus,
    allowed: &HashSet<String>,
) {
    let dropped = filter_findings(&mut result.findings, allowed);
    if !dropped.is_empty() {
        let ids: Vec<&str> = dropped.iter().map(|f| f.rule_id.as_str()).collect();
        tracing::warn!(
            reviewer = label,
            focus = focus.as_str(),
            count = dropped.len(),
            unknown_rule_ids = ?ids,
            "Dropped findings with rule_ids not in policy"
        );
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

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

    fn allowed_rules() -> HashSet<String> {
        ["R1", "R2"].into_iter().map(String::from).collect()
    }

    fn pair(ra: Result<ReviewResult, ReviewError>, rb: Result<ReviewResult, ReviewError>) -> PairResult {
        pair_with_focus(ra, rb, ReviewFocus::Security)
    }

    fn pair_with_focus(
        ra: Result<ReviewResult, ReviewError>,
        rb: Result<ReviewResult, ReviewError>,
        focus: ReviewFocus,
    ) -> PairResult {
        evaluate_pair(
            ra,
            rb,
            "A".into(),
            "B".into(),
            focus,
            0,
            vec![],
            &allowed_rules(),
        )
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
    fn test_pair_low_confidence_passes_without_blocking_findings() {
        // No findings of MEDIUM+ severity → confidence threshold doesn't apply.
        assert!(pair(Ok(pass(0.70)), Ok(pass(0.65))).pair_passed);
    }

    #[test]
    fn test_pair_low_confidence_blocks_with_blocking_finding() {
        let mut a = pass(0.85);
        a.findings = vec![finding("f.rs", 1, "R1", Severity::Medium)];
        let p = pair(Ok(a), Ok(pass(0.85)));
        assert!(!p.pair_passed);
    }

    #[test]
    fn test_pair_low_confidence_passes_with_only_low_findings() {
        // LOW / INFO are non-blocking — confidence threshold stays disengaged.
        let mut a = pass(0.70);
        a.findings = vec![finding("f.rs", 1, "R1", Severity::Low)];
        let p = pair(Ok(a), Ok(pass(0.70)));
        assert!(p.pair_passed);
    }

    #[test]
    fn test_pair_confidence_exactly_at_strict_threshold_passes_with_blocking() {
        let mut a = pass(STRICT_CONFIDENCE_THRESHOLD);
        a.findings = vec![finding("f.rs", 1, "R1", Severity::High)];
        let p = pair(Ok(a), Ok(pass(STRICT_CONFIDENCE_THRESHOLD)));
        assert!(p.pair_passed);
    }

    #[test]
    fn test_performance_focus_uses_lenient_threshold() {
        // 0.82 would block under the strict 0.90 threshold, but performance
        // findings only need to clear 0.80 — and only when there's a blocking
        // finding to begin with.
        let mut a = pass(0.82);
        a.findings = vec![finding("f.rs", 1, "R1", Severity::High)];
        let p = pair_with_focus(Ok(a), Ok(pass(0.82)), ReviewFocus::Performance);
        assert!(p.pair_passed);
        assert_eq!(p.confidence_threshold, LENIENT_CONFIDENCE_THRESHOLD);
    }

    #[test]
    fn test_performance_focus_blocks_below_lenient_threshold() {
        let mut a = pass(0.75);
        a.findings = vec![finding("f.rs", 1, "R1", Severity::Medium)];
        let p = pair_with_focus(Ok(a), Ok(pass(0.85)), ReviewFocus::Performance);
        assert!(!p.pair_passed);
    }

    #[test]
    fn test_security_focus_keeps_strict_threshold() {
        let mut a = pass(0.85);
        a.findings = vec![finding("f.rs", 1, "R1", Severity::High)];
        let p = pair_with_focus(Ok(a), Ok(pass(0.95)), ReviewFocus::Security);
        // 0.85 < strict 0.90 with a HIGH finding → blocks.
        assert!(!p.pair_passed);
        assert_eq!(p.confidence_threshold, STRICT_CONFIDENCE_THRESHOLD);
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

    // ── rule_id post-validation ───────────────────────────────────────────────

    #[test]
    fn test_evaluate_pair_drops_fabricated_rule_ids() {
        let mut result = pass(0.95);
        result.findings = vec![
            finding("f.rs", 1, "R1", Severity::Low),
            finding("f.rs", 2, "FAKE-999", Severity::High),
        ];
        let p = pair(Ok(result), Ok(pass(0.95)));
        // The fabricated rule_id is gone from both the per-reviewer findings
        // and the merged pair result.
        let kept_rules: Vec<&str> =
            p.result_a.findings.iter().map(|f| f.rule_id.as_str()).collect();
        assert_eq!(kept_rules, vec!["R1"]);
        let merged_rules: Vec<&str> =
            p.merged_findings.iter().map(|f| f.rule_id.as_str()).collect();
        assert_eq!(merged_rules, vec!["R1"]);
    }

    #[test]
    fn test_evaluate_pair_keeps_internal_rule_id_from_reviewer_error() {
        // unwrap_or_fail synthesises an INTERNAL-001 finding; the filter
        // must let it through even when the policy never declares it.
        let p = pair(Ok(pass(0.95)), Err(ReviewError::Completion("boom".into())));
        let internal: Vec<&str> = p
            .result_b
            .findings
            .iter()
            .map(|f| f.rule_id.as_str())
            .collect();
        assert_eq!(internal, vec!["INTERNAL-001"]);
    }

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
