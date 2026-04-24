//! End-to-end mock for the tool-call loop.
//!
//! Spins up a local HTTP server that mimics the OpenAI chat-completions API,
//! then runs the compiled `ai-reviewer` binary against it so the full pipeline
//! (diff parse → AST → reviewer → tool-call loop → consensus → report) runs
//! without needing real API keys.
//!
//! Call sequence per model:
//!   1. First POST to /v1/chat/completions → mock returns `finish_reason: "tool_calls"`
//!      asking for `read_file("src/prompt.rs")`
//!   2. Second POST (body contains `"role":"tool"`) → mock returns final JSON review result
//!
//! Usage:  cargo run --bin mock_review_e2e

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use tokio::process::Command;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

// ── Mock HTTP server ──────────────────────────────────────────────────────────

/// First-call response: ask the LLM to call `read_file` on `src/prompt.rs`.
const TOOL_CALL_RESPONSE: &str = r#"{
  "id": "chatcmpl-mock-tool",
  "object": "chat.completion",
  "created": 1700000000,
  "model": "mock-model",
  "choices": [{
    "index": 0,
    "message": {
      "role": "assistant",
      "content": null,
      "refusal": null,
      "tool_calls": [{
        "id": "call_001",
        "type": "function",
        "function": {
          "name": "read_file",
          "arguments": "{\"path\": \"src/prompt.rs\", \"start_line\": 1, \"end_line\": 15}"
        }
      }]
    },
    "logprobs": null,
    "finish_reason": "tool_calls"
  }],
  "usage": { "prompt_tokens": 500, "completion_tokens": 30, "total_tokens": 530 }
}"#;

/// Second-call response: the LLM has seen the tool result and issues the verdict.
const FINAL_REVIEW_RESPONSE: &str = r#"{
  "id": "chatcmpl-mock-final",
  "object": "chat.completion",
  "created": 1700000001,
  "model": "mock-model",
  "choices": [{
    "index": 0,
    "message": {
      "role": "assistant",
      "content": "{\"model_id\":\"mock\",\"verdict\":\"pass\",\"confidence\":0.95,\"findings\":[],\"reasoning\":\"Reviewed diff and additional context via tool call. No security or correctness issues found.\"}"
    },
    "logprobs": null,
    "finish_reason": "stop"
  }],
  "usage": { "prompt_tokens": 700, "completion_tokens": 60, "total_tokens": 760 }
}"#;

async fn serve_one_connection(
    mut stream: tokio::net::TcpStream,
    req_counter: Arc<AtomicU32>,
) {
    // Read the full HTTP request.
    let mut raw = Vec::with_capacity(8192);
    let mut buf = [0u8; 4096];
    let mut content_length: usize = 0;
    let mut header_end = 0usize;

    // Read until we have the full headers + body.
    loop {
        let n = match stream.read(&mut buf).await {
            Ok(0) | Err(_) => return,
            Ok(n) => n,
        };
        raw.extend_from_slice(&buf[..n]);

        // Find end of headers.
        if header_end == 0 && let Some(pos) = find_header_end(&raw) {
            header_end = pos;
            // Parse Content-Length from headers.
            let headers = String::from_utf8_lossy(&raw[..header_end]);
            for line in headers.lines() {
                if line.to_lowercase().starts_with("content-length:") {
                    content_length = line
                        .split(':')
                        .nth(1)
                        .unwrap_or("0")
                        .trim()
                        .parse()
                        .unwrap_or(0);
                    break;
                }
            }
        }

        // Check if we have the full body.
        if header_end > 0 && raw.len() >= header_end + content_length {
            break;
        }
    }

    let body = &raw[header_end..header_end + content_length];
    let body_str = String::from_utf8_lossy(body);

    // Decide which response to send.
    // If the request body already contains a tool-result message (role=tool),
    // this is the second call in the loop; otherwise it is the first.
    let is_tool_followup = body_str.contains("\"role\":\"tool\"")
        || body_str.contains("\"role\": \"tool\"");

    let req_num = req_counter.fetch_add(1, Ordering::SeqCst);
    let (response_body, label) = if is_tool_followup {
        (FINAL_REVIEW_RESPONSE, "FINAL_REVIEW")
    } else {
        (TOOL_CALL_RESPONSE, "TOOL_CALL")
    };

    println!(
        "  [mock-server] req #{req_num} → {label} (body {} bytes, tool_followup={is_tool_followup})",
        body_str.len()
    );

    let http = format!(
        "HTTP/1.1 200 OK\r\n\
         Content-Type: application/json\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\
         \r\n\
         {}",
        response_body.len(),
        response_body
    );
    let _ = stream.write_all(http.as_bytes()).await;
}

fn find_header_end(data: &[u8]) -> Option<usize> {
    data.windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|p| p + 4)
}

async fn run_mock_server(listener: TcpListener, counter: Arc<AtomicU32>) {
    println!("  [mock-server] accept loop started");
    loop {
        match listener.accept().await {
            Ok((stream, addr)) => {
                println!("  [mock-server] connection from {addr}");
                let c = counter.clone();
                tokio::spawn(async move { serve_one_connection(stream, c).await });
            }
            Err(e) => {
                eprintln!("  [mock-server] accept error: {e}");
                break;
            }
        }
    }
}

// ── Test inputs ───────────────────────────────────────────────────────────────

const TEST_DIFF: &str = r#"diff --git a/src/prompt.rs b/src/prompt.rs
index 1234567..abcdefg 100644
--- a/src/prompt.rs
+++ b/src/prompt.rs
@@ -1,3 +1,6 @@
 use crate::ast::{CallEdge, FileAstContext, Symbol, SymbolKind};
 use crate::models::REVIEW_JSON_SCHEMA;
+
+// Added a helper constant for the max prompt size (characters).
+/// Rough cap used to avoid hitting provider context limits.
+pub const MAX_PROMPT_CHARS: usize = 200_000;
"#;

const TEST_POLICY: &str = r#"# Review Policy
- No hardcoded secrets
- Validate all user inputs
- Use safe cryptographic primitives
"#;

// ── Main ──────────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // ── Start mock server ─────────────────────────────────────────────────────
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();
    let mock_url = format!("http://127.0.0.1:{port}/v1");
    println!("Mock OpenAI server listening on {mock_url}");

    let req_counter = Arc::new(AtomicU32::new(0));
    let counter_clone = req_counter.clone();
    tokio::spawn(async move { run_mock_server(listener, counter_clone).await });

    // ── Pre-flight: verify mock server is reachable ───────────────────────────
    tokio::time::sleep(std::time::Duration::from_millis(50)).await; // let server start
    let pre = reqwest::Client::new()
        .post(format!("{mock_url}/chat/completions"))
        .header("Content-Type", "application/json")
        .body(r#"{"model":"test","messages":[]}"#)
        .send()
        .await;
    match pre {
        Ok(r) => println!("Pre-flight OK — mock server responded: HTTP {}", r.status()),
        Err(e) => {
            eprintln!("Pre-flight FAILED — cannot reach mock server: {e}");
            std::process::exit(1);
        }
    }
    println!("Pre-flight request count: {}", req_counter.load(Ordering::SeqCst));

    // ── Write temp files ──────────────────────────────────────────────────────
    let tmp = tempfile_dir();
    let diff_path = tmp.join("test.diff");
    let policy_path = tmp.join("policy.md");
    std::fs::write(&diff_path, TEST_DIFF)?;
    std::fs::write(&policy_path, TEST_POLICY)?;

    // ── Locate the ai-reviewer binary ─────────────────────────────────────────
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let bin_name = if cfg!(windows) { "ai-reviewer.exe" } else { "ai-reviewer" };
    let binary = manifest_dir.join("target/debug").join(bin_name);
    if !binary.exists() {
        anyhow::bail!(
            "Binary not found at {}.\nRun `cargo build` first.",
            binary.display()
        );
    }

    // ── Run ai-reviewer against the mock server ───────────────────────────────
    let source_root = manifest_dir.to_string_lossy().into_owned();
    println!("\nRunning: ai-reviewer --diff <test.diff> --policy <policy.md> --source-root {source_root}");
    println!("Reviewer A & B both → {mock_url} (mock)\n");

    let output = Command::new(&binary)
        .arg("--diff")
        .arg(&diff_path)
        .arg("--policy")
        .arg(&policy_path)
        .arg("--source-root")
        .arg(&source_root)
        // Point both reviewers at the mock server.
        .env("REVIEWER_1_API_KEY", "mock-key-a")
        .env("REVIEWER_2_API_KEY", "mock-key-b")
        .env("REVIEWER_1_BASE_URL", &mock_url)
        .env("REVIEWER_2_BASE_URL", &mock_url)
        // Use our mock model IDs.
        .env("REVIEWER_1_MODEL", "mock-model")
        .env("REVIEWER_2_MODEL", "mock-model")
        // Short timeout; mock responds instantly.
        .env("REVIEWER_TIMEOUT", "15")
        .output()    // async — does not block the Tokio runtime
        .await?;

    // ── Print results ─────────────────────────────────────────────────────────
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let total_requests = req_counter.load(Ordering::SeqCst);

    println!("─── stdout ───────────────────────────────────────────────────────────");
    println!("{stdout}");
    if !stderr.is_empty() {
        println!("─── stderr ───────────────────────────────────────────────────────────");
        println!("{stderr}");
    }
    println!("─── summary ──────────────────────────────────────────────────────────");
    println!("Exit code       : {}", output.status.code().unwrap_or(-1));
    // 1 pre-flight + 8 tool_call + 8 final = 17 expected total.
    println!("Total API calls : {total_requests}  (expected 17: 1 pre-flight + 8 tool_call + 8 final)");

    // ── Assertions ────────────────────────────────────────────────────────────
    let mut ok = true;

    // 1 pre-flight + 8 first-round (tool_call) + 8 second-round (final) = 17.
    if total_requests != 17 {
        eprintln!("✗  Expected exactly 17 API requests (1+8+8), got {total_requests}");
        ok = false;
    } else {
        println!("✓  All 8 models completed tool-call loop ({total_requests} total requests)");
    }

    if stdout.contains("PASS") || stdout.contains("Gate PASSED") {
        println!("✓  Consensus result: PASS (mock models agree, confidence ≥ 0.90)");
    } else {
        eprintln!("✗  Expected PASS in output.\nstdout: {stdout}");
        ok = false;
    }

    if ok {
        println!("\n✓  End-to-end tool-call loop mock test PASSED");
    } else {
        eprintln!("\n✗  End-to-end test FAILED — see above");
        std::process::exit(1);
    }

    Ok(())
}

fn tempfile_dir() -> PathBuf {
    std::env::temp_dir()
}
