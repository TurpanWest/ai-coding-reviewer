/// Generic LLM reviewer backed by any Rig `CompletionModel`.
///
/// A single `LlmReviewer<M>` replaces the old `MinimaxReviewer` and
/// `DeepSeekReviewer` hand-rolled HTTP clients.  It works with any provider
/// that Rig supports (OpenAI, DeepSeek, Anthropic, Gemini, …).
use async_trait::async_trait;
use rig::completion::{Chat, CompletionModel};
use rig::message::Message;
use tracing::{debug, warn};

use crate::ast::FileAstContext;
use crate::models::{ReviewError, ReviewFocus, ReviewResult};
use crate::prompt::{build_correction_prompt, build_system_prompt, build_user_prompt};

// ── Public struct ─────────────────────────────────────────────────────────────

pub struct LlmReviewer<M: CompletionModel> {
    model: M,
    label: String,
    max_retries: u32,
    focus: ReviewFocus,
}

impl<M: CompletionModel + Clone> LlmReviewer<M> {
    pub fn new(model: M, label: impl Into<String>, max_retries: u32, focus: ReviewFocus) -> Self {
        Self { model, label: label.into(), max_retries, focus }
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
        let system_prompt = build_system_prompt(policy_text, self.focus);
        let user_prompt = build_user_prompt(contexts);

        // Build a lightweight agent with the system prompt as its preamble.
        // Agent<M> implements the `Chat` trait, which gives us multi-turn
        // conversation history for the self-correction retry loop.
        let agent = rig::agent::AgentBuilder::new(self.model.clone())
            .preamble(&system_prompt)
            .build();

        // history accumulates the full prior conversation for correction rounds.
        // On attempt 0: history is empty, current_prompt is the initial user prompt.
        // On attempt N: history has [user, assistant, user, assistant, …] pairs,
        //               current_prompt is the latest correction message.
        let mut history: Vec<Message> = vec![];
        let mut current_prompt = user_prompt.clone();
        let mut last_raw = String::new();
        let mut last_error = String::new();

        for attempt in 0..=self.max_retries {
            debug!(attempt, label = %self.label, "Review attempt");

            let raw = agent
                .chat(current_prompt.clone(), history.clone())
                .await
                .map_err(|e| ReviewError::Completion(e.to_string()))?;

            last_raw = raw.clone();
            let cleaned = strip_json_fences(strip_think_block(&raw));

            match serde_json::from_str::<ReviewResult>(cleaned) {
                Ok(mut result) => {
                    result.model_id = self.label.clone();
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

                    // Append the failed exchange so the model has full context.
                    history.push(Message::user(current_prompt.clone()));
                    history.push(Message::assistant(raw.clone()));

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

        Err(ReviewError::MaxRetriesExceeded {
            attempts: self.max_retries + 1,
            parse_error: last_error,
            raw: last_raw,
        })
    }
}

// ── Utility functions ─────────────────────────────────────────────────────────

/// Strip `<think>…</think>` reasoning blocks emitted by some models.
///
/// Handles two cases:
/// 1. Normal: `<think>…</think>\n{json}` — extracts content after closing tag.
/// 2. Truncated: model hit token limit inside the think block, no closing tag,
///    but a JSON object was embedded — scan for the first `{` at the outermost
///    level and return from there.  If nothing is found, return `raw` unchanged.
fn strip_think_block(raw: &str) -> &str {
    // Case 1: well-formed closing tag followed by content
    if let Some(end_pos) = raw.rfind("</think>") {
        let after = &raw[end_pos + 8..]; // len("</think>") == 8
        let trimmed = after.trim();
        if !trimmed.is_empty() {
            return trimmed;
        }
    }

    // Case 2: truncated think block — find the last top-level `{` that starts
    // a JSON object (heuristic: last `{` not preceded by another `{`).
    // This recovers JSON embedded or appended after a think block without a
    // proper closing tag.
    if raw.contains("<think>")
        && let Some(brace_pos) = raw.rfind('{')
    {
        let candidate = raw[brace_pos..].trim();
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
