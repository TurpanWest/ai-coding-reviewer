/// Generic LLM reviewer backed by any Rig `CompletionModel`.
///
/// A single `LlmReviewer<M>` replaces the old `MinimaxReviewer` and
/// `DeepSeekReviewer` hand-rolled HTTP clients.  It works with any provider
/// that Rig supports (OpenAI, DeepSeek, Anthropic, Gemini, …).
use std::path::PathBuf;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use rig::completion::{Completion, CompletionModel};
use rig::tool::Tool;
use rig::completion::message::{
    AssistantContent, Message, ToolResultContent, UserContent,
};
use rig::OneOrMany;
use tokio::time::{sleep, timeout};
use tracing::{debug, info, warn, Instrument};

use crate::ast::FileAstContext;
use crate::models::{ReviewError, ReviewFocus, ReviewResult};
use crate::prompt::{build_correction_prompt, build_system_prompt, build_user_prompt};
use crate::tools::{FindSymbolTool, ReadFileTool};

// ── Public struct ─────────────────────────────────────────────────────────────

pub struct LlmReviewer<M: CompletionModel> {
    model: M,
    label: String,
    max_retries: u32,
    focus: ReviewFocus,
    /// Hard wall-clock timeout per individual LLM call (not per retry loop).
    timeout_secs: u64,
    /// Provider-specific extra parameters merged into every completion request
    /// (e.g. `{"cache_control": {"type": "ephemeral"}}` for Anthropic prefix caching).
    extra_params: Option<serde_json::Value>,
    /// Repository root used by the `read_file` / `find_symbol` tools the
    /// reviewer registers on every call.
    source_root: PathBuf,
}

impl<M: CompletionModel + Clone> LlmReviewer<M> {
    pub fn new(
        model: M,
        label: impl Into<String>,
        max_retries: u32,
        focus: ReviewFocus,
        timeout_secs: u64,
        extra_params: Option<serde_json::Value>,
        source_root: PathBuf,
    ) -> Self {
        Self {
            model,
            label: label.into(),
            max_retries,
            focus,
            timeout_secs,
            extra_params,
            source_root,
        }
    }
}

// ── Private implementation ────────────────────────────────────────────────────

impl<M> LlmReviewer<M>
where
    M: CompletionModel + Clone + Send + Sync + 'static,
{
    /// Inner review loop: retries, backoff, JSON correction.
    /// Called from the `Reviewer` trait impl wrapped in a tracing span.
    async fn do_review(
        &self,
        contexts: &[FileAstContext],
        policy_text: &str,
    ) -> Result<ReviewResult, ReviewError> {
        let system_prompt = build_system_prompt(policy_text, self.focus);
        let user_prompt = build_user_prompt(contexts);

        // Build the agent once per review.  Registering the tools on the
        // builder causes their definitions to be sent to the LLM on each
        // completion request; the dispatch loop below executes them locally.
        let read_tool = ReadFileTool::new(self.source_root.clone());
        let find_tool = FindSymbolTool::new(self.source_root.clone());
        let mut builder = rig::agent::AgentBuilder::new(self.model.clone())
            .preamble(&system_prompt)
            .tool(ReadFileTool::new(self.source_root.clone()))
            .tool(FindSymbolTool::new(self.source_root.clone()));
        if let Some(ref params) = self.extra_params {
            builder = builder.additional_params(params.clone());
        }
        let agent = builder.build();

        // History is kept empty across retry attempts.  build_correction_prompt
        // embeds the original user prompt, the bad response, the parse error,
        // and the schema inline, so each attempt is fully self-contained.
        // Passing accumulated history causes rig-core to serialize assistant
        // messages with `"tool_calls": []`, which DeepSeek (and other
        // OpenAI-compat providers) reject with a 400 invalid_request_error.
        let mut current_prompt = user_prompt.clone();
        let mut last_raw = String::new();
        let mut last_error = String::new();

        for attempt in 0..=self.max_retries {
            // Exponential backoff before every retry (not before the first attempt).
            // Delay = 2^(attempt-1) * 1000ms, capped at 16s.
            // This gives: attempt 1 → 1s, 2 → 2s, 3 → 4s, 4 → 8s …
            if attempt > 0 {
                let delay_ms = 1000u64
                    .checked_shl(attempt - 1)
                    .unwrap_or(u64::MAX)
                    .min(16_000);
                debug!(attempt, delay_ms, label = %self.label, "Backoff before retry");
                sleep(Duration::from_millis(delay_ms)).await;
            }

            debug!(attempt, label = %self.label, "Review attempt");

            let prompt_chars = current_prompt.len();
            // Rough token estimate: ~4 chars per token for English/code mixed content.
            let prompt_tokens_est = prompt_chars / 4;
            info!(
                attempt,
                label = %self.label,
                focus = ?self.focus,
                prompt_chars,
                prompt_tokens_est,
                "LLM call start"
            );

            // Run one attempt's multi-turn tool-call loop.  Each individual LLM
            // HTTP call gets its own hard timeout.  Timeouts and transient 5xx
            // errors are retried (up to max_retries); only non-retryable 4xx
            // errors (bad API key, wrong model ID) short-circuit immediately.
            let call_start = Instant::now();
            let raw = match self
                .run_tool_loop(&agent, &read_tool, &find_tool, &current_prompt)
                .await
            {
                Ok(s) => s,
                Err(e) => {
                    warn!(attempt, label = %self.label, error = %e, "Tool call loop failed");
                    if is_retryable_review_error(&e) {
                        last_error = e.to_string();
                        continue;
                    }
                    return Err(e);
                }
            };
            let elapsed_ms = call_start.elapsed().as_millis();

            let response_chars = raw.len();
            let response_tokens_est = response_chars / 4;
            info!(
                attempt,
                label = %self.label,
                focus = ?self.focus,
                elapsed_ms,
                response_chars,
                response_tokens_est,
                "LLM call complete"
            );

            last_raw = raw.clone();
            let cleaned = strip_json_fences(strip_think_block(&raw));

            match serde_json::from_str::<ReviewResult>(cleaned) {
                Ok(mut result) => {
                    result.model_id = self.label.clone();
                    // Record final verdict/confidence on the active OTel span.
                    let span = tracing::Span::current();
                    span.record("verdict", result.verdict.to_string());
                    span.record("confidence", result.confidence);
                    span.record("attempts", attempt + 1);
                    return Ok(result);
                }
                Err(e) => {
                    last_error = e.to_string();
                    warn!(
                        attempt,
                        error = %last_error,
                        label = %self.label,
                        "Response failed JSON parse"
                    );

                    if attempt >= self.max_retries {
                        break;
                    }

                    current_prompt = build_correction_prompt(
                        &user_prompt,
                        &raw,
                        &last_error,
                        attempt + 1,
                        self.max_retries + 1,
                    );
                }
            }
        }

        let total = self.max_retries + 1;
        tracing::Span::current().record("attempts", total);
        Err(ReviewError::MaxRetriesExceeded {
            attempts: total,
            parse_error: last_error,
            raw: last_raw,
        })
    }

    /// Run one attempt's multi-turn tool-call loop: send the user prompt,
    /// execute any tool calls the model issues, feed the results back, and
    /// repeat until the model returns a plain-text response or the turn cap
    /// is hit.  Each individual LLM HTTP call is subject to `self.timeout_secs`.
    async fn run_tool_loop(
        &self,
        agent: &rig::agent::Agent<M>,
        read_tool: &ReadFileTool,
        find_tool: &FindSymbolTool,
        user_prompt: &str,
    ) -> Result<String, ReviewError> {
        const MAX_TOOL_TURNS: usize = 8;

        let mut history: Vec<Message> = vec![];
        let mut next_prompt = Message::user(user_prompt);

        for _turn in 0..MAX_TOOL_TURNS {
            let builder = agent
                .completion(next_prompt.clone(), history.clone())
                .await
                .map_err(|e| ReviewError::Completion(e.to_string()))?;

            let resp = match timeout(Duration::from_secs(self.timeout_secs), builder.send()).await {
                Err(_) => {
                    return Err(ReviewError::Completion(format!(
                        "Timeout after {}s waiting for LLM response",
                        self.timeout_secs
                    )));
                }
                Ok(Err(e)) => return Err(ReviewError::Completion(e.to_string())),
                Ok(Ok(r)) => r,
            };

            match resp.choice.first() {
                AssistantContent::Text(text) => return Ok(text.text.clone()),
                AssistantContent::ToolCall(tc) => {
                    let tc = tc.clone();
                    let output = dispatch_tool(&tc, read_tool, find_tool).await;

                    // Append current prompt + assistant tool-call to history.
                    history.push(next_prompt.clone());
                    history.push(Message::Assistant {
                        content: OneOrMany::one(AssistantContent::ToolCall(tc.clone())),
                    });

                    // The tool result becomes the next user message.
                    next_prompt = Message::User {
                        content: OneOrMany::one(UserContent::tool_result(
                            &tc.id,
                            OneOrMany::one(ToolResultContent::text(output)),
                        )),
                    };
                }
            }
        }
        Err(ReviewError::Completion(format!(
            "Tool call loop exceeded max turns ({MAX_TOOL_TURNS})"
        )))
    }
}

/// Execute a single tool call issued by the model and return its textual
/// output.  Unknown tool names and argument-parse failures return descriptive
/// strings that are fed back into the conversation so the model can recover.
async fn dispatch_tool(
    tc: &rig::completion::message::ToolCall,
    read_tool: &ReadFileTool,
    find_tool: &FindSymbolTool,
) -> String {
    let args_str = tc.function.arguments.to_string();
    match tc.function.name.as_str() {
        "read_file" => match serde_json::from_str::<crate::tools::ReadFileArgs>(&args_str) {
            Ok(a) => read_tool
                .call(a)
                .await
                .unwrap_or_else(|e| format!("Tool error: {e}")),
            Err(e) => format!("Args parse error: {e}"),
        },
        "find_symbol" => match serde_json::from_str::<crate::tools::FindSymbolArgs>(&args_str) {
            Ok(a) => find_tool
                .call(a)
                .await
                .unwrap_or_else(|e| format!("Tool error: {e}")),
            Err(e) => format!("Args parse error: {e}"),
        },
        name => format!("Unknown tool: {name}"),
    }
}

// ── Reviewer trait impl ───────────────────────────────────────────────────────

#[async_trait]
impl<M> super::Reviewer for LlmReviewer<M>
where
    M: CompletionModel + Clone + Send + Sync + 'static,
{
    fn label(&self) -> &str {
        &self.label
    }

    async fn review(
        &self,
        contexts: &[FileAstContext],
        policy_text: &str,
    ) -> Result<ReviewResult, ReviewError> {
        // Wrap the entire review (including retries) in a tracing span.
        // `tracing-opentelemetry` forwards this span to the OTLP collector when
        // `OTEL_EXPORTER_OTLP_ENDPOINT` is set, enabling distributed tracing
        // of individual reviewer calls within a CI pipeline.
        let span = tracing::info_span!(
            "reviewer.call",
            reviewer   = %self.label,
            focus      = ?self.focus,
            verdict    = tracing::field::Empty,
            confidence = tracing::field::Empty,
            attempts   = tracing::field::Empty,
        );

        self.do_review(contexts, policy_text)
            .instrument(span)
            .await
    }
}

// ── Utility functions ─────────────────────────────────────────────────────────

/// Strip `<think>…</think>` reasoning blocks emitted by some models.
///
/// Handles two cases:
/// 1. Normal: `<think>…</think>\n{json}` — extracts content after closing tag.
/// 2. Truncated: model hit token limit inside the think block, no closing tag,
///    but a JSON object was embedded — scan for the last `{` and return from
///    there.  If nothing is found, return `raw` unchanged.
fn strip_think_block(raw: &str) -> &str {
    // Case 1: well-formed closing tag followed by content
    if let Some(end_pos) = raw.rfind("</think>") {
        let after = &raw[end_pos + 8..]; // len("</think>") == 8
        let trimmed = after.trim();
        if !trimmed.is_empty() {
            return trimmed;
        }
    }

    // Case 2: truncated think block — scan forward from where `<think>` ends
    // looking for a `{` that starts the JSON object.  We use `find` (first
    // occurrence after the opening tag) rather than `rfind` to avoid picking
    // up a stray `{` deep inside the analysis text or code snippets quoted
    // inside the think block.  Best-effort heuristic only.
    if let Some(open_pos) = raw.find("<think>")
        && let Some(brace_offset) = raw[open_pos..].find("\n{")
    {
        let candidate = raw[open_pos + brace_offset + 1..].trim();
        if !candidate.is_empty() {
            return candidate;
        }
    }

    raw
}

/// Strip markdown JSON code fences (```json … ``` or ``` … ```).
fn strip_json_fences(raw: &str) -> &str {
    let trimmed = raw.trim();
    if let Some(inner) = trimmed
        .strip_prefix("```json")
        .or_else(|| trimmed.strip_prefix("```"))
        && let Some(end) = inner.rfind("```")
    {
        return inner[..end].trim();
    }
    trimmed
}

// ── Retry helpers ─────────────────────────────────────────────────────────────

/// Returns `true` when an API error message represents a transient failure that
/// is worth retrying.
///
/// Only 4xx errors that cannot improve on retry are excluded:
/// - 401 / 403: bad API key or missing permission — retrying with the same key
///   will always fail.
/// - 404: wrong model ID or endpoint path — structural misconfiguration.
///
/// Everything else — 5xx server errors, 429 rate limits, network resets, and
/// timeouts — is treated as transient and eligible for the backoff retry loop.
fn is_retryable_api_error(msg: &str) -> bool {
    let lower = msg.to_lowercase();
    let non_retryable = [
        "401",
        "403",
        "404",
        "unauthorized",
        "forbidden",
        "invalid api key",
        "invalid_api_key",
        "permission denied",
        "not found",
    ];
    !non_retryable.iter().any(|s| lower.contains(s))
}

/// Convenience wrapper over [`is_retryable_api_error`] for a full `ReviewError`.
fn is_retryable_review_error(e: &ReviewError) -> bool {
    match e {
        ReviewError::Completion(msg) => is_retryable_api_error(msg),
        // MaxRetriesExceeded should never be produced inside the retry loop,
        // but treat it as non-retryable to avoid a double-retry loop.
        ReviewError::MaxRetriesExceeded { .. } => false,
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── strip_think_block ─────────────────────────────────────────────────────

    #[test]
    fn test_strip_think_well_formed() {
        let raw = "<think>reasoning here</think>\n{\"verdict\":\"pass\"}";
        assert_eq!(strip_think_block(raw), "{\"verdict\":\"pass\"}");
    }

    #[test]
    fn test_strip_think_truncated_finds_json() {
        // Model hit token limit inside think block; JSON object follows on new line.
        let raw = "<think>analysis...\n{\"verdict\":\"pass\"}";
        let result = strip_think_block(raw);
        assert!(result.contains("\"verdict\""));
    }

    #[test]
    fn test_strip_think_no_block_returns_input() {
        let raw = "{\"verdict\":\"pass\"}";
        assert_eq!(strip_think_block(raw), raw);
    }

    // ── strip_json_fences ─────────────────────────────────────────────────────

    #[test]
    fn test_strip_fences_json_prefix() {
        let raw = "```json\n{\"verdict\":\"pass\"}\n```";
        assert_eq!(strip_json_fences(raw), "{\"verdict\":\"pass\"}");
    }

    #[test]
    fn test_strip_fences_plain_prefix() {
        let raw = "```\n{\"verdict\":\"pass\"}\n```";
        assert_eq!(strip_json_fences(raw), "{\"verdict\":\"pass\"}");
    }

    #[test]
    fn test_strip_fences_no_fence_returns_trimmed() {
        let raw = "  {\"verdict\":\"pass\"}  ";
        assert_eq!(strip_json_fences(raw), "{\"verdict\":\"pass\"}");
    }

    // ── is_retryable_api_error ────────────────────────────────────────────────

    #[test]
    fn test_non_retryable_401() {
        assert!(!is_retryable_api_error("HTTP 401 Unauthorized"));
        assert!(!is_retryable_api_error("error: unauthorized access"));
    }

    #[test]
    fn test_non_retryable_403() {
        assert!(!is_retryable_api_error("403 Forbidden"));
        assert!(!is_retryable_api_error("permission denied for model"));
    }

    #[test]
    fn test_non_retryable_404() {
        assert!(!is_retryable_api_error("404 model not found"));
        assert!(!is_retryable_api_error("The requested model was not found"));
    }

    #[test]
    fn test_non_retryable_invalid_api_key() {
        assert!(!is_retryable_api_error("Invalid API key provided"));
        assert!(!is_retryable_api_error("invalid_api_key: check your credentials"));
    }

    #[test]
    fn test_retryable_timeout() {
        assert!(is_retryable_api_error("Timeout after 120s waiting for LLM response"));
    }

    #[test]
    fn test_retryable_5xx() {
        assert!(is_retryable_api_error("HTTP 500 Internal Server Error"));
        assert!(is_retryable_api_error("502 Bad Gateway"));
        assert!(is_retryable_api_error("503 Service Unavailable"));
    }

    #[test]
    fn test_retryable_429_rate_limit() {
        assert!(is_retryable_api_error("429 Too Many Requests"));
        assert!(is_retryable_api_error("rate limit exceeded, retry after 5s"));
    }

    #[test]
    fn test_retryable_network_error() {
        assert!(is_retryable_api_error("connection reset by peer"));
        assert!(is_retryable_api_error("error sending request: connection refused"));
    }

    // ── is_retryable_review_error ─────────────────────────────────────────────

    #[test]
    fn test_retryable_review_error_completion_timeout() {
        let e = ReviewError::Completion("Timeout after 120s".into());
        assert!(is_retryable_review_error(&e));
    }

    #[test]
    fn test_non_retryable_review_error_completion_401() {
        let e = ReviewError::Completion("401 Unauthorized".into());
        assert!(!is_retryable_review_error(&e));
    }

    #[test]
    fn test_non_retryable_review_error_max_retries() {
        let e = ReviewError::MaxRetriesExceeded {
            attempts: 4,
            parse_error: "bad json".into(),
            raw: "{}".into(),
        };
        assert!(!is_retryable_review_error(&e));
    }
}
