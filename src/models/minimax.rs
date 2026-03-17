/// MiniMax reviewer — uses the OpenAI-compatible Chat Completions API.
///
/// Despite the original plan for Anthropic-compat, the provided API key works
/// on api.minimax.chat/v1/chat/completions (OpenAI format).
/// Both reviewers now share the same wire format; they remain independent
/// client structs to allow future divergence (streaming, tool use, etc.).
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tracing::{debug, warn};

use crate::ast::FileAstContext;
use crate::models::{ReviewError, ReviewResult};
use crate::prompt::{build_correction_prompt, build_system_prompt, build_user_prompt};

// ── API constants ─────────────────────────────────────────────────────────────

#[allow(dead_code)]
const DEFAULT_BASE_URL: &str = "https://api.minimax.chat/v1";
#[allow(dead_code)]
const DEFAULT_MODEL: &str = "MiniMax-M2.5";
const MAX_TOKENS: u32 = 4096;

// ── OpenAI-compat shapes ──────────────────────────────────────────────────────

#[derive(Serialize, Deserialize, Clone, Debug)]
struct OaiMessage {
    role:    String,
    content: String,
}

#[derive(Deserialize, Debug)]
struct OaiResponse {
    choices: Vec<OaiChoice>,
    #[serde(default)]
    usage: Option<OaiUsage>,
}

#[derive(Deserialize, Debug)]
struct OaiChoice {
    message:       OaiMessage,
    finish_reason: Option<String>,
}

#[derive(Deserialize, Debug)]
struct OaiUsage {
    prompt_tokens:     u32,
    completion_tokens: u32,
}

// ── Public entry point ────────────────────────────────────────────────────────

pub struct MinimaxReviewer {
    client:      Client,
    api_key:     String,
    base_url:    String,
    model_id:    String,
    max_retries: u32,
}

impl MinimaxReviewer {
    #[allow(dead_code)]
    pub fn new(api_key: String, max_retries: u32) -> Self {
        Self::with_config(
            api_key,
            DEFAULT_BASE_URL.into(),
            DEFAULT_MODEL.into(),
            max_retries,
        )
    }

    pub fn with_config(
        api_key:     String,
        base_url:    String,
        model_id:    String,
        max_retries: u32,
    ) -> Self {
        let client = Client::builder()
            .timeout(std::time::Duration::from_secs(180))
            .build()
            .expect("Failed to build reqwest client");
        Self { client, api_key, base_url, model_id, max_retries }
    }

    /// Run the full review loop with self-correction retries.
    pub async fn review(
        &self,
        contexts:    &[FileAstContext],
        policy_text: &str,
    ) -> Result<ReviewResult, ReviewError> {
        let system_prompt = build_system_prompt(policy_text);
        let user_prompt   = build_user_prompt(contexts);

        let mut messages: Vec<OaiMessage> = vec![
            OaiMessage { role: "system".into(), content: system_prompt },
            OaiMessage { role: "user".into(),   content: user_prompt.clone() },
        ];

        let mut last_raw   = String::new();
        let mut last_error = String::new();

        for attempt in 1..=self.max_retries + 1 {
            debug!(attempt, model = %self.model_id, "MiniMax review attempt");

            let raw = self.call_api(&messages).await?;
            last_raw = raw.clone();

            let cleaned = strip_json_fences(strip_think_block(&raw));

            match serde_json::from_str::<ReviewResult>(cleaned) {
                Ok(mut result) => {
                    result.model_id = self.model_id.clone();
                    return Ok(result);
                }
                Err(e) => {
                    last_error = e.to_string();
                    warn!(attempt, error = %last_error, "MiniMax response failed JSON parse");

                    if attempt > self.max_retries {
                        break;
                    }

                    messages.push(OaiMessage {
                        role:    "assistant".into(),
                        content: raw.clone(),
                    });
                    let correction = build_correction_prompt(
                        &user_prompt,
                        &raw,
                        &last_error,
                        attempt,
                        self.max_retries + 1,
                    );
                    messages.push(OaiMessage {
                        role:    "user".into(),
                        content: correction,
                    });
                }
            }
        }

        Err(ReviewError::MaxRetriesExceeded {
            attempts:    self.max_retries + 1,
            parse_error: last_error,
            raw:         last_raw,
        })
    }

    // ── Internal HTTP call ────────────────────────────────────────────────────

    async fn call_api(&self, messages: &[OaiMessage]) -> Result<String, ReviewError> {
        let url = format!(
            "{}/chat/completions",
            self.base_url.trim_end_matches('/')
        );

        let body = json!({
            "model":      self.model_id,
            "max_tokens": MAX_TOKENS,
            "messages":   messages,
        });

        debug!(url = %url, "POST MiniMax");

        let resp = self
            .client
            .post(&url)
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await?;

        let status = resp.status();
        if !status.is_success() {
            let body_text = resp.text().await.unwrap_or_default();
            return Err(ReviewError::Api {
                status: status.as_u16(),
                body:   body_text,
            });
        }

        let api_resp: OaiResponse = resp.json().await?;

        if let Some(usage) = &api_resp.usage {
            debug!(
                prompt_tokens     = usage.prompt_tokens,
                completion_tokens = usage.completion_tokens,
                "MiniMax token usage"
            );
        }

        if let Some(choice) = api_resp.choices.first() {
            if choice.finish_reason.as_deref() == Some("length") {
                warn!("MiniMax response truncated (finish_reason=length). Consider increasing MAX_TOKENS.");
            }
            return Ok(choice.message.content.clone());
        }

        Err(ReviewError::EmptyResponse)
    }
}

// ── Utility ───────────────────────────────────────────────────────────────────

/// MiniMax-M2.5 (and similar reasoning models) wrap their chain-of-thought in
/// `<think>...</think>` and then emit the final answer after the closing tag.
/// Strip the thinking block so only the JSON answer remains.
fn strip_think_block(raw: &str) -> &str {
    if let Some(end_pos) = raw.rfind("</think>") {
        let after = &raw[end_pos + 8..]; // len("</think>") == 8
        let trimmed = after.trim();
        if !trimmed.is_empty() {
            return trimmed;
        }
    }
    raw
}

fn strip_json_fences(raw: &str) -> &str {
    let trimmed = raw.trim();
    if let Some(inner) = trimmed
        .strip_prefix("```json")
        .or_else(|| trimmed.strip_prefix("```"))
    {
        if let Some(end) = inner.rfind("```") {
            return inner[..end].trim();
        }
    }
    trimmed
}
