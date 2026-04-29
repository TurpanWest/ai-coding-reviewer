use async_trait::async_trait;
use serde::{Deserialize, Serialize};

pub mod reviewer;

pub use crate::prompt::ReviewFocus;

// ── Risk level (per-change declaration) ──────────────────────────────────────

/// Developer-declared risk level for the current change.  Selects the gate's
/// voting rule; the per-focus confidence thresholds are unchanged.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
#[clap(rename_all = "lowercase")]
pub enum RiskLevel {
    /// Either reviewer voting PASS is enough.  Confidence is not enforced.
    Low,
    /// Default behaviour: both reviewers must vote PASS, and confidence is
    /// enforced only when verdicts disagree or a MEDIUM+ finding is reported.
    Medium,
    /// Both reviewers must vote PASS *and* clear the per-focus confidence
    /// threshold unconditionally — even with zero findings.
    High,
}

impl RiskLevel {
    pub fn as_str(self) -> &'static str {
        match self {
            RiskLevel::Low => "low",
            RiskLevel::Medium => "medium",
            RiskLevel::High => "high",
        }
    }

    /// Short human-readable name of the active voting rule.
    pub fn vote_rule(self) -> &'static str {
        match self {
            RiskLevel::Low => "any-pass",
            RiskLevel::Medium => "both-pass+severity-aware",
            RiskLevel::High => "both-pass+confidence",
        }
    }
}

impl std::fmt::Display for RiskLevel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

// ── Core verdict / severity types ────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Severity {
    Critical,
    High,
    Medium,
    Low,
    Info,
}

impl std::fmt::Display for Severity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            Severity::Critical => "CRITICAL",
            Severity::High => "HIGH",
            Severity::Medium => "MEDIUM",
            Severity::Low => "LOW",
            Severity::Info => "INFO",
        };
        write!(f, "{s}")
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    Pass,
    Fail,
}

impl std::fmt::Display for Verdict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Verdict::Pass => write!(f, "PASS"),
            Verdict::Fail => write!(f, "FAIL"),
        }
    }
}

// ── Structured output types ───────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CodeLocation {
    pub file: String,
    pub line_start: u32,
    pub line_end: u32,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Finding {
    pub severity: Severity,
    pub location: CodeLocation,
    pub rule_id: String,
    pub description: String,
    pub suggestion: String,
}

/// The structured response every reviewer model must produce.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ReviewResult {
    /// Human-readable label, e.g. "MiniMax" or "DeepSeek"
    pub model_id: String,
    pub verdict: Verdict,
    /// 0.0 – 1.0 — must be >= CONFIDENCE_THRESHOLD to pass the gate
    pub confidence: f64,
    pub findings: Vec<Finding>,
    /// Free-form chain-of-thought reasoning (not gated)
    pub reasoning: String,
}

/// Consensus result for one reviewer pair (one of the four focus groups).
#[derive(Debug, Serialize)]
pub struct PairResult {
    /// Human-readable focus name, e.g. "security".
    pub focus: String,
    /// 0-based index (0 = Security, 1 = Correctness, 2 = Performance, 3 = Maintainability).
    pub group_index: usize,
    /// Files assigned to this group.
    pub files: Vec<String>,
    pub label_a: String,
    pub label_b: String,
    pub result_a: ReviewResult,
    pub result_b: ReviewResult,
    pub merged_findings: Vec<Finding>,
    pub pair_passed: bool,
    /// Confidence threshold applied to this pair (focus-dependent).
    pub confidence_threshold: f64,
    /// Risk level under which this pair was evaluated.
    pub risk_level: RiskLevel,
}

/// Final output of the consensus engine combining all four focus groups.
#[derive(Debug, Serialize)]
pub struct ConsensusResult {
    pub verdict: Verdict,
    /// One entry per active review group (up to 4).
    pub groups: Vec<PairResult>,
    /// All findings from every group, merged and deduplicated.
    pub all_findings: Vec<Finding>,
    pub gate_passed: bool,
    /// Risk level declared for this run (drives the voting rule).
    pub risk_level: RiskLevel,
}

// ── Reviewer trait ────────────────────────────────────────────────────────────

#[async_trait]
pub trait Reviewer: Send + Sync {
    fn label(&self) -> &str;
    async fn review(
        &self,
        contexts: &[crate::ast::FileAstContext],
        policy_text: &str,
    ) -> Result<ReviewResult, ReviewError>;
}

// ── JSON Schema embedded in system prompt ─────────────────────────────────────

pub const REVIEW_JSON_SCHEMA: &str = r#"{
  "type": "object",
  "required": ["model_id", "verdict", "confidence", "findings", "reasoning"],
  "properties": {
    "model_id":   { "type": "string" },
    "verdict":    { "type": "string", "enum": ["pass", "fail"] },
    "confidence": { "type": "number", "minimum": 0.0, "maximum": 1.0 },
    "findings": {
      "type": "array",
      "items": {
        "type": "object",
        "required": ["severity", "location", "rule_id", "description", "suggestion"],
        "properties": {
          "severity": { "type": "string", "enum": ["CRITICAL", "HIGH", "MEDIUM", "LOW", "INFO"] },
          "location": {
            "type": "object",
            "required": ["file", "line_start", "line_end"],
            "properties": {
              "file":       { "type": "string" },
              "line_start": { "type": "integer", "minimum": 1 },
              "line_end":   { "type": "integer", "minimum": 1 }
            }
          },
          "rule_id":     { "type": "string" },
          "description": { "type": "string" },
          "suggestion":  { "type": "string" }
        }
      }
    },
    "reasoning": { "type": "string" }
  }
}"#;

// ── Error type for reviewer models ────────────────────────────────────────────

#[derive(Debug, thiserror::Error)]
pub enum ReviewError {
    #[error("Completion error: {0}")]
    Completion(String),

    #[error("Max retries ({attempts}) exceeded. Last parse error: {parse_error}\nRaw response:\n{raw}")]
    MaxRetriesExceeded {
        attempts: u32,
        parse_error: String,
        raw: String,
    },
}
