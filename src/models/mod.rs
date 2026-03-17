use serde::{Deserialize, Serialize};

pub mod deepseek;
pub mod minimax;

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
    /// e.g. "minimax-text-01" or "deepseek-reasoner"
    pub model_id: String,
    pub verdict: Verdict,
    /// 0.0 – 1.0 — must be >= CONFIDENCE_THRESHOLD to pass the gate
    pub confidence: f64,
    pub findings: Vec<Finding>,
    /// Free-form chain-of-thought reasoning (not gated)
    pub reasoning: String,
}

/// Final output of the consensus engine.
#[derive(Debug, Serialize)]
pub struct ConsensusResult {
    pub verdict: Verdict,
    pub minimax_result: ReviewResult,
    pub deepseek_result: ReviewResult,
    pub merged_findings: Vec<Finding>,
    pub gate_passed: bool,
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
    #[error("HTTP request failed: {0}")]
    Http(#[from] reqwest::Error),

    #[error("API error {status}: {body}")]
    Api { status: u16, body: String },

    #[error("Max retries ({attempts}) exceeded. Last parse error: {parse_error}\nRaw response:\n{raw}")]
    MaxRetriesExceeded {
        attempts: u32,
        parse_error: String,
        raw: String,
    },

    #[error("Response contained no text content")]
    EmptyResponse,
}
