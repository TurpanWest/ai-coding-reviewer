/// DeepSeek reviewer — uses the OpenAI-compatible Chat Completions API.
///
/// DeepSeek exposes `POST /v1/chat/completions` with the standard OpenAI
/// message format.  We hit it directly with `reqwest` for full control over
/// the retry loop and message history.
use anyhow::Result;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tracing::{debug, warn};

use crate::ast::FileAstContext;
use crate::models::{ReviewError, ReviewResult};
use crate::prompt::{build_correction_prompt, build_system_prompt, build_user_prompt};

// ── API constants ─────────────────────────────────────────────────────────────

#[allow(dead_code)]
const DEFAULT_BASE_URL: &str = "https://api.deepseek.com/v1";
#[allow(dead_code)]
const DEFAULT_MODEL:    &str = "deepseek-chat";
const MAX_TOKENS:       u32  = 4096;

// ── OpenAI-compat request/response shapes ─────────────────────────────────────

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
    message: OaiMessage,
    finish_reason: Option<String>,
}

#[derive(Deserialize, Debug)]
struct OaiUsage {
    prompt_tokens:     u32,
    completion_tokens: u32,
    #[serde(default)]
    prompt_cache_hit_tokens:  u32,
    #[serde(default)]
    prompt_cache_miss_tokens: u32,
}

// ── Public entry point ────────────────────────────────────────────────────────

pub struct DeepSeekReviewer {
    client:     Client,
    api_key:    String,
    base_url:   String,
    model_id:   String,
    max_retries: u32,
}

impl DeepSeekReviewer {
    #[allow(dead_code)]
    pub fn new(api_key: String, max_retries: u32) -> Self {
        Self::with_config(api_key, DEFAULT_BASE_URL.into(), DEFAULT_MODEL.into(), max_retries)
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

        // Build the initial message list: system + user
        let mut messages: Vec<OaiMessage> = vec![
            OaiMessage {
                role:    "system".into(),
                content: system_prompt,
            },
            OaiMessage {
                role:    "user".into(),
                content: user_prompt.clone(),
            },
        ];

        let mut last_raw   = String::new();
        let mut last_error = String::new();

        for attempt in 1..=self.max_retries + 1 {
            debug!(attempt, model = %self.model_id, "DeepSeek review attempt");

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
                    warn!(attempt, error = %last_error, "DeepSeek response failed JSON parse");

                    if attempt > self.max_retries {
                        break;
                    }

                    // Append the bad assistant turn + correction user turn
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

        // DeepSeek prefix cache: requests with an identical system+user prefix
        // are served from cache automatically.  No special header needed; the
        // cache_control hint improves hit rates on some versions.
        let body = json!({
            "model":      self.model_id,
            "max_tokens": MAX_TOKENS,
            "messages":   messages,
            // Ask for JSON output (supported by deepseek-chat; ignored gracefully
            // by deepseek-reasoner, which we handle via strip_json_fences).
            "response_format": { "type": "json_object" },
        });

        debug!(url = %url, "POST DeepSeek");

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
                cache_hit_tokens  = usage.prompt_cache_hit_tokens,
                cache_miss_tokens = usage.prompt_cache_miss_tokens,
                "DeepSeek token usage"
            );
        }

        // Check finish reason — log a warning on length truncation
        if let Some(choice) = api_resp.choices.first() {
            if choice.finish_reason.as_deref() == Some("length") {
                warn!("DeepSeek response was truncated (finish_reason=length). \
                       Consider increasing MAX_TOKENS.");
            }
            return Ok(choice.message.content.clone());
        }

        Err(ReviewError::EmptyResponse)
    }
}

// ── Utility ───────────────────────────────────────────────────────────────────

fn strip_think_block(raw: &str) -> &str {
    if let Some(end_pos) = raw.rfind("</think>") {
        let after = &raw[end_pos + 8..];
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
