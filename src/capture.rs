use anyhow::Result;
use chrono::Utc;
use std::io::Read;
use uuid::Uuid;

use crate::{config::Config, db::Db, migrations, project_id};

/// Tools whose outputs are worth capturing for later review.
/// Read-only tools (Read, Glob, Grep) are excluded - they carry no memory signal.
const CAPTURE_TOOLS: &[&str] = &["Write", "Edit", "MultiEdit", "Bash"];

const POST_CAPTURE_INSTRUCTION: &str =
    "[memso] State-changing tool just ran. In this response, call capture_note(summary) to record \
WHY - the capture above records what changed, not the reason. \
capture_note is a staging log reviewed at next session; store_memory is durable and immediately searchable. \
If this change involved a decision, discovery, or gotcha that needs to be findable now or later this session, \
use store_memory (type='decision'/'gotcha'/'how-it-works') instead of or in addition to capture_note. \
Skip only if this was purely mechanical with no reasoning worth preserving.";

pub async fn run(project_override: Option<String>) -> Result<()> {
    let mut buf = String::new();
    std::io::stdin().read_to_string(&mut buf)?;
    let buf = buf.trim();
    if buf.is_empty() {
        return Ok(());
    }

    let data: serde_json::Value = serde_json::from_str(buf)?;

    let tool_name = match data.get("tool_name").and_then(|v| v.as_str()) {
        Some(t) => t,
        None => return Ok(()),
    };

    if !CAPTURE_TOOLS.contains(&tool_name) {
        return Ok(());
    }

    let summary = extract_summary(tool_name, &data);

    // Use cwd from hook payload if available, fall back to process cwd.
    let cwd = data
        .get("cwd")
        .and_then(|v| v.as_str())
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());

    let config = Config::load(&cwd)?;
    let db = Db::open(&config).await?;
    let conn = db.conn;
    migrations::run(&conn).await?;

    let pid = project_override
        .unwrap_or_else(|| project_id::resolve(&cwd, config.memory.project_id.as_deref()));

    let id = Uuid::new_v4().to_string();
    let now = Utc::now().to_rfc3339();

    conn.execute(
        "INSERT INTO raw_captures (id, project_id, captured_at, tool_name, summary, raw_data) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        libsql::params![id, pid, now, tool_name.to_string(), summary, buf.to_string()],
    )
    .await?;

    println!("{POST_CAPTURE_INSTRUCTION}");
    Ok(())
}

/// Extract a one-line hint from a string value: first non-empty, non-whitespace line,
/// truncated to `max_len` chars.
fn first_line(s: &str, max_len: usize) -> &str {
    let line = s.lines().map(|l| l.trim()).find(|l| !l.is_empty()).unwrap_or("");
    // Use char boundary to avoid splitting multi-byte UTF-8 characters.
    line.char_indices()
        .nth(max_len)
        .map(|(i, _)| &line[..i])
        .unwrap_or(line)
}

fn extract_summary(tool_name: &str, data: &serde_json::Value) -> String {
    let input = &data["tool_input"];
    match tool_name {
        "Write" => {
            let path = input.get("file_path").and_then(|v| v.as_str()).unwrap_or("?");
            let hint = input.get("content").and_then(|v| v.as_str())
                .map(|s| first_line(s, 80))
                .filter(|s| !s.is_empty())
                .unwrap_or("");
            if hint.is_empty() { format!("Created {path}") }
            else { format!("Created {path}: {hint}") }
        }
        "Edit" | "MultiEdit" => {
            let path = input.get("file_path").and_then(|v| v.as_str()).unwrap_or("?");
            let hint = input.get("new_string").and_then(|v| v.as_str())
                .map(|s| first_line(s, 80))
                .filter(|s| !s.is_empty())
                .unwrap_or("");
            if hint.is_empty() { format!("Edited {path}") }
            else { format!("Edited {path}: {hint}") }
        }
        "Bash" => {
            let cmd = input.get("command").and_then(|v| v.as_str()).unwrap_or("?");
            let cmd_line = cmd.lines().next().unwrap_or(cmd).trim();
            let cmd_short = if cmd_line.len() > 60 { &cmd_line[..60] } else { cmd_line };
            let response = data.get("tool_response");
            let is_error = response
                .and_then(|r| r.get("is_error"))
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let output = response
                .and_then(|r| r.get("output"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let out_line = output.lines().map(|l| l.trim()).find(|l| !l.is_empty()).unwrap_or("");
            let prefix = if is_error { "[FAILED] " } else { "" };
            if out_line.is_empty() {
                format!("{prefix}Ran: {cmd_short}")
            } else {
                let out_short = if out_line.len() > 50 { &out_line[..50] } else { out_line };
                format!("{prefix}Ran: {cmd_short} -> {out_short}")
            }
        }
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn extract_summary_write_includes_path_and_first_line() {
        let data = json!({
            "tool_input": {"file_path": "src/main.rs", "content": "fn main() {}\nmore stuff"}
        });
        let s = extract_summary("Write", &data);
        assert!(s.contains("src/main.rs"), "should include path: {s}");
        assert!(s.contains("fn main()"), "should include first line: {s}");
    }

    #[test]
    fn extract_summary_bash_success() {
        let data = json!({
            "tool_input": {"command": "cargo build"},
            "tool_response": {"is_error": false, "output": "Finished dev profile"}
        });
        let s = extract_summary("Bash", &data);
        assert!(s.contains("cargo build"), "should include command: {s}");
        assert!(!s.starts_with("[FAILED]"), "should not be marked failed: {s}");
    }

    #[test]
    fn extract_summary_bash_failure_has_prefix() {
        let data = json!({
            "tool_input": {"command": "cargo build"},
            "tool_response": {"is_error": true, "output": "error: could not compile"}
        });
        let s = extract_summary("Bash", &data);
        assert!(s.starts_with("[FAILED]"), "should be marked failed: {s}");
    }

    #[test]
    fn first_line_truncates_ascii() {
        assert_eq!(first_line("hello world", 5), "hello");
    }

    #[test]
    fn first_line_handles_multibyte_utf8() {
        // Each CJK character is 3 bytes; truncating at 2 chars must not split a char
        let s = "AB\u{4e2d}\u{6587}suffix";
        let result = first_line(s, 3); // "AB\u{4e2d}" = 3 chars
        assert_eq!(result, "AB\u{4e2d}");
    }

    #[test]
    fn first_line_skips_blank_lines() {
        let result = first_line("\n\n  \nhello", 80);
        assert_eq!(result, "hello");
    }
}
