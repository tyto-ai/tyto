use anyhow::Result;
use chrono::Utc;
use std::env;
use std::io::{IsTerminal, Read};

use crate::{config::Config, db::Db, migrations, project_id, retrieve};

const INSTRUCTIONS: &str = "[memso] Store every decision, discovery, gotcha, failure, and unexpected outcome. \
Err on the side of storing - use importance (0.0-1.0) to signal value, not omission. \
Failures and unexpected outcomes: type='gotcha', importance >= 0.8. \
After understanding a subsystem through exploration, store a how-it-works memory before moving on. \
Use topic_key to upsert existing memories. \
Before starting work on a topic involving unknowns, call search_memory with a targeted query - \
the automatic context above uses the raw prompt text and may miss relevant memories. \
capture_note(summary) is a staging log reviewed at next session start - use it to record reasoning behind changes. \
store_memory is durable and immediately searchable - use it for decisions, discoveries, and gotchas \
that need to be findable now or later in this session. They are not interchangeable.\n\
[memso tools] store_memory(content,type,title,[topic_key,importance,tags,facts,source]) | \
search_memory(query,[limit]) | get_memory(id) | get_memories(ids) | \
list_memories([type,tags,limit]) | capture_note(summary,[context]) | delete_memory(id)\n";

const SESSION_INSTRUCTIONS: &str = "[memso] Session start: call get_memories on the IDs in Memory Context below before proceeding. \
When storing memories from Pending Review, set source='reviewed'. \
Use capture_note(summary) to stage tentative observations for later review.\n";

pub async fn run(
    inject_type: &str,
    project_override: Option<String>,
    query_override: Option<String>,
    limit: usize,
    budget: usize,
) -> Result<()> {
    // Stop inject needs no DB - read stop_hook_active from stdin then emit instructions.
    if inject_type == "stop" {
        return run_stop(budget);
    }

    let cwd = env::current_dir()?;
    let config = Config::load(&cwd)?;

    let db = Db::open(&config).await?;
    let conn = db.conn;
    migrations::run(&conn).await?;

    let pid = project_override
        .unwrap_or_else(|| project_id::resolve(&cwd, config.memory.project_id.as_deref()));

    match inject_type {
        "session" | "compact" => run_session(&conn, &pid, budget).await,
        _ => {
            let query = resolve_prompt_query(query_override);
            run_prompt(&conn, &query, &pid, limit, budget).await
        }
    }
}

// Uses BM25-only search (no ONNX model) to stay within the 500ms hook timeout.
// Prompt injection is best-effort context; keyword relevance is sufficient here.
// Full hybrid search is reserved for session-start where latency tolerance is higher.
async fn run_prompt(
    conn: &libsql::Connection,
    query: &str,
    project_id: &str,
    limit: usize,
    budget: usize,
) -> Result<()> {
    let mut output = INSTRUCTIONS.to_string();
    if !query.is_empty() {
        let results = retrieve::search_bm25(conn, query, project_id, limit).await?;
        if !results.is_empty() {
            output.push_str(&format_compact(&results));
        }
    }
    print_within_budget(&output, budget);
    Ok(())
}

// Fires on every Claude response completion. Outputs instructions only - no DB query.
// Guards against infinite loops: if stop_hook_active is true, a Stop hook already
// ran this turn (Claude responded to the hook output), so we skip to avoid compounding.
fn run_stop(budget: usize) -> Result<()> {
    if is_stop_hook_active() {
        return Ok(());
    }
    print_within_budget(INSTRUCTIONS, budget);
    Ok(())
}

fn is_stop_hook_active() -> bool {
    if std::io::stdin().is_terminal() {
        return false;
    }
    let mut buf = String::new();
    if std::io::stdin().read_to_string(&mut buf).is_err() {
        return false;
    }
    serde_json::from_str::<serde_json::Value>(&buf)
        .ok()
        .and_then(|v| v.get("stop_hook_active").and_then(|b| b.as_bool()))
        .unwrap_or(false)
}

async fn run_session(
    conn: &libsql::Connection,
    project_id: &str,
    budget: usize,
) -> Result<()> {
    let mut output = INSTRUCTIONS.to_string();
    output.push_str(SESSION_INSTRUCTIONS);

    // Surface pending captures for review and mark them as presented.
    let captures = query_pending_captures(conn, project_id).await?;
    if !captures.is_empty() {
        mark_captures_presented(conn, project_id).await?;
        output.push_str(&format_captures(&captures));
    }

    // Surface all memories above the importance floor, sorted by retention score.
    // A generous ceiling (100) ensures nothing important is silently dropped;
    // the budget cap handles output size.
    let results = retrieve::list(conn, project_id, None, &[], 100, 0.4).await?;
    if !results.is_empty() {
        output.push_str(&format_compact(&results));
    }

    print_within_budget(&output, budget);
    Ok(())
}

struct PendingCapture {
    tool_name: String,
    captured_at: String,
    summary: String,
}

async fn query_pending_captures(
    conn: &libsql::Connection,
    project_id: &str,
) -> Result<Vec<PendingCapture>> {
    let mut rows = conn
        .query(
            "SELECT tool_name, captured_at, summary \
             FROM raw_captures \
             WHERE project_id = ?1 AND presented_at IS NULL \
             ORDER BY captured_at ASC",
            libsql::params![project_id.to_string()],
        )
        .await?;

    let mut captures = Vec::new();
    while let Some(row) = rows.next().await? {
        captures.push(PendingCapture {
            tool_name: row.get(0)?,
            captured_at: row.get(1)?,
            summary: row.get(2)?,
        });
    }
    Ok(captures)
}

async fn mark_captures_presented(conn: &libsql::Connection, project_id: &str) -> Result<()> {
    let now = Utc::now().to_rfc3339();
    conn.execute(
        "UPDATE raw_captures SET presented_at = ?1 \
         WHERE project_id = ?2 AND presented_at IS NULL",
        libsql::params![now, project_id.to_string()],
    )
    .await?;
    Ok(())
}

fn format_captures(captures: &[PendingCapture]) -> String {
    let mut out = format!(
        "--- Pending Review ({}) - synthesise related captures into memories with source='reviewed'; failures are gotcha type, importance >= 0.8 ---\n",
        captures.len()
    );
    for c in captures {
        let date = c.captured_at.get(..10).unwrap_or(&c.captured_at);
        out.push_str(&format!("[{:<12}] {}  {}\n", c.tool_name, date, c.summary));
    }
    out.push_str("---\n");
    out
}

/// Resolve the query for prompt injection.
/// Precedence: --query flag > $CLAUDE_USER_PROMPT env > stdin JSON {"prompt":"..."} > stdin raw
fn resolve_prompt_query(query_override: Option<String>) -> String {
    if let Some(q) = query_override {
        return q;
    }

    if let Ok(v) = env::var("CLAUDE_USER_PROMPT")
        && !v.is_empty()
    {
        return v;
    }

    // Try reading from stdin if it's not a tty
    if !std::io::stdin().is_terminal() {
        let mut buf = String::new();
        if std::io::stdin().read_to_string(&mut buf).is_ok() && !buf.trim().is_empty() {
            // Gemini CLI sends {"prompt": "..."}
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&buf)
                && let Some(p) = v.get("prompt").and_then(|p| p.as_str())
            {
                return p.to_string();
            }
            return buf.trim().to_string();
        }
    }

    String::new()
}

fn format_compact(results: &[retrieve::CompactResult]) -> String {
    let mut out = format!("--- Memory Context ({} results) ---\n", results.len());
    for r in results {
        let date = r.created_at.get(..10).unwrap_or(&r.created_at);
        out.push_str(&format!(
            "[{:<18}] {}  {}  {}\n",
            r.memory_type, r.id, date, r.title
        ));
    }
    out.push_str("---\n");
    out
}

fn print_within_budget(output: &str, budget: usize) {
    if output.len() <= budget {
        print!("{output}");
    } else {
        // Truncate at last newline within budget
        let truncated = &output[..budget];
        if let Some(pos) = truncated.rfind('\n') {
            print!("{}", &truncated[..pos]);
            println!("\n[memso: output truncated to fit budget]");
        } else {
            print!("{truncated}");
        }
    }
}
