/// Generic LLM reviewer backed by any Rig `CompletionModel`.
///
/// A single `LlmReviewer<M>` replaces the old `MinimaxReviewer` and
/// `DeepSeekReviewer` hand-rolled HTTP clients.  It works with any provider
/// that Rig supports (OpenAI, DeepSeek, Anthropic, Gemini, …).
use std::path::PathBuf;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use rig::completion::{Chat, Completion, CompletionModel};
use rig::tool::Tool;
use rig::completion::message::{
    AssistantContent, Message, ToolResultContent, UserContent,
};
use rig::OneOrMany;
use tokio::time::{sleep, timeout};
use tracing::{debug, info, warn, Instrument};

use crate::ast::FileAstContext;
use crate::models::{ReviewError, ReviewFocus, ReviewResult};
use crate::prompt::{
    build_correction_prompt, build_system_prompt, build_system_prompt_with_tools, build_user_prompt,
};
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
    /// When `Some`, the reviewer registers `read_file` / `find_symbol` tools
    /// and uses `agent.prompt()` so the LLM can fetch additional context.
    source_root: Option<PathBuf>,
}

impl<M: CompletionModel + Clone> LlmReviewer<M> {
    pub fn new(
        model: M,
        label: impl Into<String>,
        max_retries: u32,
        focus: ReviewFocus,
        timeout_secs: u64,
        extra_params: Option<serde_json::Value>,
        source_root: Option<PathBuf>,
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
        let (system_prompt, use_tools) = match &self.source_root {
            Some(_) => (build_system_prompt_with_tools(policy_text, self.focus), true),
            None => (build_system_prompt(policy_text, self.focus), false),
        };
        let user_prompt = build_user_prompt(contexts);

        // Build agent — conditionally register file-reading tools.
        let mut builder = rig::agent::AgentBuilder::new(self.model.clone())
            .preamble(&system_prompt);
        if let Some(ref params) = self.extra_params {
            builder = builder.additional_params(params.clone());
        }
        if use_tools {
            let sr = self.source_root.as_ref().unwrap().clone();
            builder = builder
                .tool(ReadFileTool::new(sr.clone()))
                .tool(FindSymbolTool::new(sr));
        }
        let agent = builder.build();

        // history is kept empty for every call. build_correction_prompt embeds
        // the original user prompt, the bad response, the parse error, and the
        // schema inline, so each attempt is fully self-contained.
        //
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

            // Hard per-call timeout: if the LLM hangs, we don't block the CI
            // worker forever — we convert the timeout into a ReviewError so the
            // consensus engine can handle it as a synthetic FAIL.
            //
            // When tools are registered, use `agent.prompt()` — Rig's Prompt
            // trait handles the full multi-turn tool-call loop internally and
            // returns the LLM's final text response after all tool calls have
            // been executed.  When tools are absent, `agent.chat()` with an
            // empty history is equivalent and avoids the `tool_calls: []`
            // serialisation issue some OpenAI-compat providers reject.
            let call_start = Instant::now();
            let raw = if use_tools {
                // ── Multi-turn tool-call loop ─────────────────────────────────
                // Each LLM HTTP call gets its own hard timeout.  We loop until
                // the model returns a plain text response (no more tool calls).
                const MAX_TOOL_TURNS: usize = 8;
                let sr = self.source_root.as_ref().unwrap().clone();
                let read_tool = ReadFileTool::new(sr.clone());
                let find_tool = FindSymbolTool::new(sr);
                let mut history: Vec<Message> = vec![];
                let mut next_prompt = Message::user(&current_prompt);

                let loop_result: Result<String, ReviewError> = 'turns: {
                    for _turn in 0..MAX_TOOL_TURNS {
                        let builder =
                            match agent.completion(next_prompt.clone(), history.clone()).await {
                                Ok(b) => b,
                                Err(e) => {
                                    break 'turns Err(ReviewError::Completion(e.to_string()))
                                }
                            };
                        let resp = match timeout(
                            Duration::from_secs(self.timeout_secs),
                            builder.send(),
                        )
                        .await
                        {
                            Err(_) => break 'turns Err(ReviewError::Completion(format!(
                                "Timeout after {}s waiting for LLM response",
                                self.timeout_secs
                            ))),
                            Ok(Err(e)) => {
                                break 'turns Err(ReviewError::Completion(e.to_string()))
                            }
                            Ok(Ok(r)) => r,
                        };

                        match resp.choice.first() {
                            AssistantContent::Text(text) => {
                                break 'turns Ok(text.text.clone());
                            }
                            AssistantContent::ToolCall(tc) => {
                                let tc = tc.clone();
                                let args_str = tc.function.arguments.to_string();
                                let output = match tc.function.name.as_str() {
                                    "read_file" => {
                                        match serde_json::from_str::<crate::tools::ReadFileArgs>(
                                            &args_str,
                                        ) {
                                            Ok(a) => read_tool
                                                .call(a)
                                                .await
                                                .unwrap_or_else(|e| format!("Tool error: {e}")),
                                            Err(e) => format!("Args parse error: {e}"),
                                        }
                                    }
                                    "find_symbol" => {
                                        match serde_json::from_str::<
                                            crate::tools::FindSymbolArgs,
                                        >(&args_str)
                                        {
                                            Ok(a) => find_tool
                                                .call(a)
                                                .await
                                                .unwrap_or_else(|e| format!("Tool error: {e}")),
                                            Err(e) => format!("Args parse error: {e}"),
                                        }
                                    }
                                    name => format!("Unknown tool: {name}"),
                                };

                                // Append current prompt + assistant tool-call to history.
                                history.push(next_prompt.clone());
                                history.push(Message::Assistant {
                                    content: OneOrMany::one(AssistantContent::ToolCall(
                                        tc.clone(),
                                    )),
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
                    Err(ReviewError::Completion(
                        "Tool call loop exceeded max turns (8)".into(),
                    ))
                };

                match loop_result {
                    Ok(s) => s,
                    Err(e) => {
                        warn!(attempt, label = %self.label, error = %e, "Tool call loop failed");
                        return Err(e);
                    }
                }
            } else {
                let result = timeout(
                    Duration::from_secs(self.timeout_secs),
                    agent.chat(current_prompt.clone(), vec![]),
                )
                .await;
                match result {
                    Err(_) => {
                        let msg = format!(
                            "Timeout after {}s waiting for LLM response",
                            self.timeout_secs
                        );
                        warn!(attempt, label = %self.label, timeout_secs = self.timeout_secs, "LLM call timed out");
                        return Err(ReviewError::Completion(msg));
                    }
                    Ok(Err(e)) => return Err(ReviewError::Completion(e.to_string())),
                    Ok(Ok(s)) => s,
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
