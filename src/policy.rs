use std::collections::HashSet;

use crate::models::Finding;

/// Rule IDs reserved for the consensus engine itself (not the policy).
/// Findings carrying these IDs are produced by `unwrap_or_fail` when a
/// reviewer call fails, and must survive the post-validation filter.
const INTERNAL_RULE_PREFIX: &str = "INTERNAL-";

/// Extract the canonical set of rule IDs declared in the policy markdown.
///
/// A rule ID is any token wrapped in `**…**` whose body matches
/// `<UPPER>+-<ALNUM_>+` — the convention used by `policy.md` (e.g. `SEC-001`,
/// `LOGIC-002`, `NULL-001`). Other bold markdown is ignored.
pub fn extract_rule_ids(policy_text: &str) -> HashSet<String> {
    let mut ids = HashSet::new();
    let mut rest = policy_text;
    while let Some(start) = rest.find("**") {
        let after_open = &rest[start + 2..];
        match after_open.find("**") {
            Some(end) => {
                let token = &after_open[..end];
                if is_rule_id(token) {
                    ids.insert(token.to_string());
                }
                rest = &after_open[end + 2..];
            }
            None => break,
        }
    }
    ids
}

fn is_rule_id(s: &str) -> bool {
    let (prefix, suffix) = match s.split_once('-') {
        Some(parts) => parts,
        None => return false,
    };
    if prefix.is_empty() || suffix.is_empty() {
        return false;
    }
    if !prefix.chars().all(|c| c.is_ascii_uppercase()) {
        return false;
    }
    suffix
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// Drop findings whose `rule_id` is not in `allowed`.  Returns the dropped
/// findings so the caller can log them (a fabricated rule is a model defect
/// worth surfacing, not silently swallowing).
///
/// Rule IDs prefixed with `INTERNAL-` are always retained — those are
/// produced by the consensus engine itself when a reviewer call fails.
pub fn filter_findings(
    findings: &mut Vec<Finding>,
    allowed: &HashSet<String>,
) -> Vec<Finding> {
    let mut dropped = Vec::new();
    findings.retain(|f| {
        if f.rule_id.starts_with(INTERNAL_RULE_PREFIX) || allowed.contains(&f.rule_id) {
            true
        } else {
            dropped.push(f.clone());
            false
        }
    });
    dropped
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{CodeLocation, Finding, Severity};

    fn finding(rule: &str) -> Finding {
        Finding {
            severity: Severity::Medium,
            location: CodeLocation {
                file: "x.rs".into(),
                line_start: 1,
                line_end: 1,
            },
            rule_id: rule.into(),
            description: "d".into(),
            suggestion: "s".into(),
        }
    }

    // ── extract_rule_ids ──────────────────────────────────────────────────────

    #[test]
    fn extracts_rule_ids_from_policy_format() {
        let policy = "- **SEC-001**: foo\n- **NULL-001**: bar\n- **LOGIC-002**: baz\n";
        let ids = extract_rule_ids(policy);
        assert!(ids.contains("SEC-001"));
        assert!(ids.contains("NULL-001"));
        assert!(ids.contains("LOGIC-002"));
        assert_eq!(ids.len(), 3);
    }

    #[test]
    fn ignores_non_rule_bold_markdown() {
        let policy = "**Section Header**\n- **SEC-001**: real rule\n**lowercase-thing**\n";
        let ids = extract_rule_ids(policy);
        assert_eq!(ids.len(), 1);
        assert!(ids.contains("SEC-001"));
    }

    #[test]
    fn handles_unterminated_bold_marker() {
        // A stray `**` shouldn't panic or run away.
        let ids = extract_rule_ids("**SEC-001**: ok\nstray ** marker without close");
        assert!(ids.contains("SEC-001"));
    }

    #[test]
    fn rejects_token_without_dash() {
        assert!(!is_rule_id("SECURITY"));
    }

    #[test]
    fn rejects_lowercase_prefix() {
        assert!(!is_rule_id("sec-001"));
    }

    // ── filter_findings ───────────────────────────────────────────────────────

    #[test]
    fn drops_unknown_rule_ids() {
        let mut fs = vec![finding("SEC-001"), finding("MADE-UP-999")];
        let allowed: HashSet<String> = ["SEC-001"].into_iter().map(String::from).collect();
        let dropped = filter_findings(&mut fs, &allowed);
        assert_eq!(fs.len(), 1);
        assert_eq!(fs[0].rule_id, "SEC-001");
        assert_eq!(dropped.len(), 1);
        assert_eq!(dropped[0].rule_id, "MADE-UP-999");
    }

    #[test]
    fn always_keeps_internal_rules() {
        let mut fs = vec![finding("INTERNAL-001"), finding("INTERNAL-FOO")];
        let allowed: HashSet<String> = HashSet::new();
        let dropped = filter_findings(&mut fs, &allowed);
        assert_eq!(fs.len(), 2);
        assert!(dropped.is_empty());
    }

    #[test]
    fn empty_allowed_drops_everything_non_internal() {
        let mut fs = vec![finding("SEC-001"), finding("LOGIC-001")];
        let dropped = filter_findings(&mut fs, &HashSet::new());
        assert!(fs.is_empty());
        assert_eq!(dropped.len(), 2);
    }
}
