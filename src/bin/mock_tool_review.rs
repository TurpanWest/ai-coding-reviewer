//! Mock tool-call review demonstration.
//!
//! Simulates the sequence of tool calls an LLM reviewer would make when it
//! encounters a diff that references symbols not fully shown in the context.
//! Runs entirely locally — no API keys required.
//!
//! Usage:
//!   cargo run --bin mock_tool_review
//!   cargo run --bin mock_tool_review -- --source-root /path/to/repo

// Pull in the project's modules directly (same crate, different binary entry).
#[path = "../tools.rs"]
mod tools;

use std::path::PathBuf;

use rig::tool::Tool;
use tools::{FindSymbolArgs, FindSymbolTool, ReadFileArgs, ReadFileTool};

fn source_root_from_args() -> PathBuf {
    let args: Vec<String> = std::env::args().collect();
    if let Some(pos) = args.iter().position(|a| a == "--source-root")
        && let Some(path) = args.get(pos + 1)
    {
        return PathBuf::from(path);
    }
    // Default: use this project's own source tree as the test subject.
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn separator(label: &str) {
    println!("\n{}", "─".repeat(72));
    println!("  {label}");
    println!("{}", "─".repeat(72));
}

#[tokio::main]
async fn main() {
    let source_root = source_root_from_args();
    println!("Source root: {}", source_root.display());

    let read = ReadFileTool::new(source_root.clone());
    let find = FindSymbolTool::new(source_root.clone());

    // ── Scenario 1: LLM reads a specific file range ───────────────────────────
    separator("Scenario 1 — read_file: src/prompt.rs lines 1-40");
    match read
        .call(ReadFileArgs {
            path: "src/prompt.rs".into(),
            start_line: Some(1),
            end_line: Some(40),
        })
        .await
    {
        Ok(out) => {
            println!("[tool_call] read_file(path=\"src/prompt.rs\", start_line=1, end_line=40)");
            println!("[tool_result]\n{out}");
        }
        Err(e) => println!("[tool_error] {e}"),
    }

    // ── Scenario 2: LLM reads a large file → truncation notice ───────────────
    separator("Scenario 2 — read_file: src/tools.rs (full, expect truncation)");
    match read
        .call(ReadFileArgs {
            path: "src/tools.rs".into(),
            start_line: None,
            end_line: None,
        })
        .await
    {
        Ok(out) => {
            let lines: Vec<&str> = out.lines().collect();
            println!(
                "[tool_call] read_file(path=\"src/tools.rs\")  → {} output lines",
                lines.len()
            );
            // Show first 8 lines + last 3 (the truncation notice).
            for l in lines.iter().take(8) {
                println!("{l}");
            }
            if lines.len() > 11 {
                println!("  ... ({} lines omitted for demo) ...", lines.len() - 11);
                for l in &lines[lines.len() - 3..] {
                    println!("{l}");
                }
            }
        }
        Err(e) => println!("[tool_error] {e}"),
    }

    // ── Scenario 3: LLM searches for a symbol across the codebase ────────────
    separator("Scenario 3 — find_symbol: \"build_system_prompt\" (global search)");
    match find
        .call(FindSymbolArgs {
            name: "build_system_prompt".into(),
            file: None,
        })
        .await
    {
        Ok(out) => {
            println!("[tool_call] find_symbol(name=\"build_system_prompt\")");
            println!("[tool_result]\n{out}");
        }
        Err(e) => println!("[tool_error] {e}"),
    }

    // ── Scenario 4: LLM searches for a symbol in a specific file ─────────────
    separator("Scenario 4 — find_symbol: \"evaluate\" restricted to src/consensus.rs");
    match find
        .call(FindSymbolArgs {
            name: "evaluate".into(),
            file: Some("src/consensus.rs".into()),
        })
        .await
    {
        Ok(out) => {
            println!("[tool_call] find_symbol(name=\"evaluate\", file=\"src/consensus.rs\")");
            println!("[tool_result]\n{out}");
        }
        Err(e) => println!("[tool_error] {e}"),
    }

    // ── Scenario 5: Security — path traversal must be blocked ────────────────
    separator("Scenario 5 — SECURITY: path traversal attempt");
    match read
        .call(ReadFileArgs {
            path: "../../Windows/System32/drivers/etc/hosts".into(),
            start_line: None,
            end_line: None,
        })
        .await
    {
        Ok(_) => eprintln!("[FAIL] Path traversal was NOT blocked — this is a bug!"),
        Err(e) => {
            println!("[tool_call] read_file(\"../../Windows/System32/...\")");
            println!("[BLOCKED] {e}");
            println!("✓  Path traversal correctly rejected");
        }
    }

    // ── Scenario 6: Missing file → clean error, not a panic ──────────────────
    separator("Scenario 6 — read_file: non-existent file");
    match read
        .call(ReadFileArgs {
            path: "src/does_not_exist.rs".into(),
            start_line: None,
            end_line: None,
        })
        .await
    {
        Ok(_) => eprintln!("[FAIL] Should have returned an error"),
        Err(e) => {
            println!("[tool_call] read_file(\"src/does_not_exist.rs\")");
            println!("[tool_error] {e}");
            println!("✓  Missing file returns a clean error (no panic)");
        }
    }

    // ── Scenario 7: Unknown symbol → clean message, not an error ─────────────
    separator("Scenario 7 — find_symbol: symbol that does not exist");
    match find
        .call(FindSymbolArgs {
            name: "totally_nonexistent_symbol_xyz".into(),
            file: None,
        })
        .await
    {
        Ok(out) => {
            println!("[tool_call] find_symbol(\"totally_nonexistent_symbol_xyz\")");
            println!("[tool_result] {out}");
            println!("✓  Missing symbol returns informative message (no error)");
        }
        Err(e) => eprintln!("[FAIL] Unexpected error: {e}"),
    }

    // ── Done ──────────────────────────────────────────────────────────────────
    separator("All scenarios completed");
    println!(
        "\nIn production, these tool calls are issued automatically by the LLM\n\
         via Rig's agent loop — the reviewer requests more code whenever it\n\
         needs to, and the agent executes the tools and feeds results back.\n"
    );
}
