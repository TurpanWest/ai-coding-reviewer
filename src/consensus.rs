use std::collections::{HashMap, HashSet};

use crate::models::{
    CodeLocation, ConsensusResult, Finding, PairResult, ReviewError, ReviewFocus, ReviewResult,
    RiskLevel, Severity, Verdict,
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
/// The voting rule is selected by `risk_level`:
///
/// - `Low`    — *any-pass*: a single PASS verdict is enough; confidence is
///   ignored.  A reviewer error producing a synthetic FAIL is non-blocking
///   when the other side passed.
/// - `Medium` — *both-pass + severity-aware confidence*: both reviewers must
///   vote PASS, and the per-focus confidence threshold is enforced only when
///   verdicts disagree (impossible past the both-pass gate) or a MEDIUM+
///   finding is reported.  This is the historical default.
/// - `High`   — *both-pass + confidence*: both reviewers must vote PASS *and*
///   clear the per-focus confidence threshold unconditionally, even with
///   zero findings.
#[allow(clippy::too_many_arguments)]
pub fn evaluate_pair(
    res_a: Result<ReviewResult, ReviewError>,
    res_b: Result<ReviewResult, ReviewError>,
    label_a: String,
    label_b: String,
    focus: ReviewFocus,
    risk_level: RiskLevel,
    group_index: usize,
    files: Vec<String>,
    allowed_rules: &HashSet<String>,
) -> PairResult {
    let mut result_a = unwrap_or_fail(res_a, &label_a);
    let mut result_b = unwrap_or_fail(res_b, &label_b);

    drop_unknown_rules(&mut result_a, &label_a, focus, allowed_rules);
    drop_unknown_rules(&mut result_b, &label_b, focus, allowed_rules);

    let threshold = confidence_threshold_for(focus);

    let a_pass = matches!(result_a.verdict, Verdict::Pass);
    let b_pass = matches!(result_b.verdict, Verdict::Pass);
    let both_confident = result_a.confidence >= threshold && result_b.confidence >= threshold;

    let merged_findings = merge_and_dedup(&result_a.findings, &result_b.findings);
    let has_blocking_finding = merged_findings.iter().any(|f| is_blocking_severity(&f.severity));

    let pair_passed = match risk_level {
        RiskLevel::Low => a_pass || b_pass,
        RiskLevel::Medium => {
            // Identical to the historical (pre-risk-level) gate logic.
            let both_pass = a_pass && b_pass;
            let confidence_required = !both_pass || has_blocking_finding;
            both_pass && (!confidence_required || both_confident)
        }
        RiskLevel::High => a_pass && b_pass && both_confident,
    };

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
        risk_level,
    }
}

// ── Final consensus ───────────────────────────────────────────────────────────

/// Combine all group pair results into the overall `ConsensusResult`.
/// The gate passes only when **every group** passes.
pub fn evaluate(groups: Vec<PairResult>, risk_level: RiskLevel) -> ConsensusResult {
    let gate_passed = groups.iter().all(|g| g.pair_passed);
    let verdict = if gate_passed { Verdict::Pass } else { Verdict::Fail };

    let flat: Vec<Finding> = groups.iter()
        .flat_map(|g| g.merged_findings.iter().cloned())
        .collect();
    let all_findings = merge_and_dedup(&flat, &[]);

    ConsensusResult { verdict, groups, all_findings, gate_passed, risk_level }
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
            let risk = group.risk_level;
            let prefix = format!("[G{g}/{focus} @ risk={}]", risk.as_str());

            let a_pass = matches!(a.verdict, Verdict::Pass);
            let b_pass = matches!(b.verdict, Verdict::Pass);

            // Whether the confidence threshold is the actual gating clause
            // depends on the risk level.
            let confidence_required = match risk {
                RiskLevel::Low => false,
                RiskLevel::Medium => {
                    let both_pass = a_pass && b_pass;
                    let has_blocking_finding = group
                        .merged_findings
                        .iter()
                        .any(|f| is_blocking_severity(&f.severity));
                    !both_pass || has_blocking_finding
                }
                RiskLevel::High => true,
            };

            if confidence_required && a.confidence < threshold {
                reasons.push(format!(
                    "{prefix} {la} confidence too low ({:.2} < {:.2})",
                    a.confidence, threshold
                ));
            }
            if confidence_required && b.confidence < threshold {
                reasons.push(format!(
                    "{prefix} {lb} confidence too low ({:.2} < {:.2})",
                    b.confidence, threshold
                ));
            }

            // Verdict reasons.  Under `low`, a single PASS already passes the
            // pair, so a verdict mismatch is by design and shouldn't appear
            // in the failure reasons.
            match risk {
                RiskLevel::Low => {
                    if !a_pass && !b_pass {
                        reasons.push(format!("{prefix} Both models confirmed defects"));
                    }
                }
                RiskLevel::Medium | RiskLevel::High => {
                    if a.verdict != b.verdict {
                        reasons.push(format!(
                            "{prefix} Verdict conflict: {la}={} vs {lb}={}",
                            a.verdict, b.verdict
                        ));
                    }
                    if !a_pass && !b_pass {
                        reasons.push(format!("{prefix} Both models confirmed defects"));
                    }
                }
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
        pair_with(ra, rb, focus, RiskLevel::Medium)
    }

    fn pair_with_risk(
        ra: Result<ReviewResult, ReviewError>,
        rb: Result<ReviewResult, ReviewError>,
        risk: RiskLevel,
    ) -> PairResult {
        pair_with(ra, rb, ReviewFocus::Security, risk)
    }

    fn pair_with(
        ra: Result<ReviewResult, ReviewError>,
        rb: Result<ReviewResult, ReviewError>,
        focus: ReviewFocus,
        risk: RiskLevel,
    ) -> PairResult {
        evaluate_pair(
            ra,
            rb,
            "A".into(),
            "B".into(),
            focus,
            risk,
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
        let r = evaluate(pairs, RiskLevel::Medium);
        assert!(r.gate_passed);
        assert_eq!(r.verdict, Verdict::Pass);
    }

    #[test]
    fn test_evaluate_one_group_fails_blocks_gate() {
        let pairs = vec![
            pair(Ok(pass(0.95)), Ok(pass(0.95))),
            pair(Ok(fail_result(0.95)), Ok(pass(0.95))),
        ];
        let r = evaluate(pairs, RiskLevel::Medium);
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
        let r = evaluate(pairs, RiskLevel::Medium);
        assert_eq!(gate_failure_reason(&r), "Gate passed.");
    }

    #[test]
    fn test_gate_failure_reason_mentions_focus() {
        let pairs = vec![pair(Ok(fail_result(0.95)), Ok(pass(0.95)))];
        let r = evaluate(pairs, RiskLevel::Medium);
        let reason = gate_failure_reason(&r);
        assert!(reason.to_uppercase().contains("SECURITY"));
    }

    #[test]
    fn test_gate_failure_reason_mentions_risk() {
        let pairs = vec![pair_with_risk(
            Ok(pass(0.85)),
            Ok(pass(0.85)),
            RiskLevel::High,
        )];
        let r = evaluate(pairs, RiskLevel::High);
        let reason = gate_failure_reason(&r);
        assert!(reason.contains("risk=high"), "got: {reason}");
    }

    // ── risk level: low (any-pass) ────────────────────────────────────────────

    #[test]
    fn test_low_risk_single_pass_passes() {
        // a=Pass + b=Fail under low risk → pair passes (single PASS suffices).
        let p = pair_with_risk(Ok(pass(0.95)), Ok(fail_result(0.95)), RiskLevel::Low);
        assert!(p.pair_passed);
    }

    #[test]
    fn test_low_risk_both_fail_blocks() {
        let p = pair_with_risk(Ok(fail_result(0.95)), Ok(fail_result(0.95)), RiskLevel::Low);
        assert!(!p.pair_passed);
    }

    #[test]
    fn test_low_risk_reviewer_error_with_other_pass_passes() {
        // A reviewer error becomes a synthetic Fail; the other side PASS keeps
        // the pair green under low risk.
        let p = pair_with_risk(
            Err(ReviewError::Completion("boom".into())),
            Ok(pass(0.95)),
            RiskLevel::Low,
        );
        assert!(p.pair_passed);
    }

    #[test]
    fn test_low_risk_ignores_low_confidence() {
        // Even with both reviewers PASS at very low confidence and a HIGH
        // finding present, low risk still passes.
        let mut a = pass(0.40);
        a.findings = vec![finding("f.rs", 1, "R1", Severity::High)];
        let p = pair_with_risk(Ok(a), Ok(pass(0.40)), RiskLevel::Low);
        assert!(p.pair_passed);
    }

    // ── risk level: high (always-confidence) ──────────────────────────────────

    #[test]
    fn test_high_risk_blocks_below_strict_even_without_findings() {
        // 0.85 < strict 0.90, no findings.  Under medium this passes (severity
        // exemption); under high it must block — that's the whole point of high.
        let p = pair_with_risk(Ok(pass(0.85)), Ok(pass(0.85)), RiskLevel::High);
        assert!(!p.pair_passed);
    }

    #[test]
    fn test_high_risk_passes_at_or_above_strict() {
        let p = pair_with_risk(Ok(pass(0.92)), Ok(pass(0.91)), RiskLevel::High);
        assert!(p.pair_passed);
    }

    #[test]
    fn test_high_risk_one_fail_blocks() {
        let p = pair_with_risk(Ok(pass(0.95)), Ok(fail_result(0.95)), RiskLevel::High);
        assert!(!p.pair_passed);
    }

    // ── risk level: medium (zero-regression invariant) ────────────────────────

    #[test]
    fn test_medium_risk_preserves_existing_behaviour() {
        // Replays test_pair_low_confidence_passes_without_blocking_findings via
        // the new helper to lock in: medium == today.
        let p = pair_with_risk(Ok(pass(0.70)), Ok(pass(0.65)), RiskLevel::Medium);
        assert!(p.pair_passed);

        // And blocking-finding case still blocks.
        let mut a = pass(0.85);
        a.findings = vec![finding("f.rs", 1, "R1", Severity::Medium)];
        let p = pair_with_risk(Ok(a), Ok(pass(0.85)), RiskLevel::Medium);
        assert!(!p.pair_passed);
    }
}
