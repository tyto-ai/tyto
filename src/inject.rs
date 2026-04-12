use anyhow::Result;
use chrono::Utc;
use std::env;
use std::io::{IsTerminal, Read};

use crate::{config::{BackendMode, Config}, db::Db, migrations, project_id, retrieve};

const INSTRUCTIONS: &str = "[memso] Store every decision, discovery, gotcha, failure, and unexpected outcome. \
Err on the side of storing - use importance (0.0-1.0) to signal value, not omission. \
Failures and unexpected outcomes: type='gotcha', importance >= 0.8. \
When you find a bug: store it as gotcha before writing the fix. \
When you finish understanding a function or module: store how-it-works before moving on. \
Store inline as you work - do not defer to end of session. \
Use topic_key to upsert existing memories. \
Before starting work, and before exploring any file or module not yet examined this session: \
search memory first — check the compact index for relevant IDs and fetch with get_memories(ids); \
call search_memory for gaps not covered by the index. \
capture_note(summary) = your reasoning before/after a change, reviewed next session. \
store_memory = a fact you would want to search for today or in a future session. \
They are not interchangeable.\n\
[memso tools] store_memories(memories:[{content,type,title,[topic_key,importance,tags,facts,source,pinned]}]) | \
search_memory(query,[limit,detail]) | get_memories(ids) | \
list_memories([type,tags,limit,detail]) | capture_note(summary,[context]) | \
pin_memories(ids,pin) | delete_memories(ids)\n";

/// Returns true if `memso serve` is running (whether still initializing or fully ready).
///
/// Fast path: serve.ready exists -> serve is up, return true immediately.
/// Slow path: try a non-blocking exclusive lock on `serve.lock`. The OS holds
/// that lock for `serve`'s entire lifetime and releases it automatically on any
/// exit — there are no stale files to worry about.
fn is_serve_running(config: &Config) -> bool {
    use fs4::fs_std::FileExt;
    if config.serve_ready_path().exists() {
        return true; // serve is fully up
    }
    let lock_path = config.serve_lock_path();
    let Ok(file) = std::fs::OpenOptions::new().write(true).create(true).truncate(false).open(&lock_path) else {
        return false;
    };
    // try_lock_exclusive returns Ok(true) if we got the lock (no server running),
    // Ok(false) if another process holds it (server is running, still initializing).
    match file.try_lock_exclusive() {
        Ok(true) => { let _ = file.unlock(); false }
        Ok(false) => true,
        Err(_) => false,
    }
}

fn build_session_instructions(session_path: Option<&std::path::Path>) -> String {
    match session_path {
        None => String::new(),
        Some(path) => format!(
            "[memso] Session start — BEFORE responding to the user, read this file \
             completely and execute every instruction defined in it: {}\n",
            path.display()
        ),
    }
}

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

    if let Err(e) = run_inner(inject_type, project_override, query_override, limit, budget).await {
        println!(
            "[memso] CRITICAL: Memory system unavailable - memories were NOT loaded for this \
             session and storing new memories will fail. Inform the user of this immediately \
             before doing anything else.\nCause: {e:#}"
        );
    }
    Ok(())
}

async fn run_inner(
    inject_type: &str,
    project_override: Option<String>,
    query_override: Option<String>,
    limit: usize,
    budget: usize,
) -> Result<()> {
    let cwd = env::current_dir()?;
    let config = Config::load(&cwd)?;

    // Check for a crash log written by a previous `memso serve` session.
    // Output to stdout so it lands in additionalContext before any memory content.
    let crash_log_path = config.db_path()
        .parent()
        .map(|p| p.join("crash.log"))
        .unwrap_or_else(|| std::path::PathBuf::from("crash.log"));
    if let Ok(crash) = std::fs::read_to_string(&crash_log_path) {
        println!(
            "[memso] WARNING: memso crashed in a previous session. \
             Inform the user of this before doing anything else - \
             recent memories may not have been saved.\nCrash report: {crash}"
        );
    }

    // If `memso serve` is running (initializing or ready), skip Db::open to avoid
    // racing on the DB file. Not applicable for remote mode: Turso handles concurrent
    // connections safely and there is no local file to contend on.
    if !matches!(config.backend.mode, BackendMode::Remote) && is_serve_running(&config) {
        if inject_type == "session" || inject_type == "compact" {
            println!(
                "{INSTRUCTIONS}[memso] MCP server is running — memory context is available via tools. \
                 Use search_memory / list_memories for context retrieval this session."
            );
        }
        return Ok(());
    }

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
            output.push_str(&format_compact(&results, 0, None));
        }
    }
    print_within_budget(&output, budget);
    Ok(())
}

const STOP_INSTRUCTIONS: &str =
    "[memso] End of turn checkpoint - store anything worth keeping before moving on:\n\
- Found a bug or unexpected behavior?     -> store_memory type=gotcha importance>=0.8\n\
- Understood how a subsystem works?       -> store_memory type=how-it-works\n\
- Made a design or implementation choice? -> store_memory type=decision\n\
- Changed your approach mid-task?         -> capture_note(why)\n\
Store inline as you work - do not defer to end of session.";

// Fires on every Claude response completion. Outputs a checkpoint prompt - no DB query.
// Guards against infinite loops: if stop_hook_active is true, a Stop hook already
// ran this turn (Claude responded to the hook output), so we skip to avoid compounding.
fn run_stop(budget: usize) -> Result<()> {
    if is_stop_hook_active() {
        return Ok(());
    }
    print_within_budget(STOP_INSTRUCTIONS, budget);
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

const FULL_CONTENT_BUDGET: usize = 30_000;

async fn run_session(
    conn: &libsql::Connection,
    project_id: &str,
    budget: usize,
) -> Result<()> {
    let pid = std::process::id();

    let captures = query_pending_captures(conn, project_id).await?;

    // List all memories above the importance floor sorted by retention score.
    let results = retrieve::list(conn, project_id, None, &[], 500, 0.4).await?;

    // Select IDs whose content fits within the full-content budget using the
    // content_len from the compact query, then fetch only that subset.
    let mut included_in_file = 0usize;
    let mut memories_content = String::new();
    if !results.is_empty() {
        let mut accumulated = 0usize;
        let full_ids: Vec<String> = results
            .iter()
            .take_while(|r| {
                if accumulated >= FULL_CONTENT_BUDGET {
                    return false;
                }
                accumulated += r.content_len;
                true
            })
            .map(|r| r.id.clone())
            .collect();

        if !full_ids.is_empty() {
            let full_memories = retrieve::get_full_batch(conn, &full_ids).await?;
            let full_map: std::collections::HashMap<String, retrieve::FullMemory> =
                full_memories.into_iter().map(|m| (m.id.clone(), m)).collect();

            for (i, compact) in results.iter().enumerate() {
                if let Some(mem) = full_map.get(&compact.id) {
                    memories_content.push_str(&format_full_memory(mem));
                    included_in_file = i + 1;
                } else {
                    break;
                }
            }
        }
    }

    // Write a single session file when either section has content.
    // Write before marking captures presented so a write failure leaves them unpresented.
    let session_path = if !captures.is_empty() || !memories_content.is_empty() {
        let path = std::env::temp_dir().join(format!("memso-session-{pid}.txt"));
        std::fs::write(&path, format_session_file(&captures, &memories_content))?;
        if !captures.is_empty() {
            mark_captures_presented(conn, project_id).await?;
        }
        Some(path)
    } else {
        None
    };

    // Build stdout: single file instruction + compact index.
    // Only memories not already written to the session file appear in the compact index.
    let mut output = INSTRUCTIONS.to_string();
    output.push_str(&build_session_instructions(session_path.as_deref()));
    if !results.is_empty() {
        output.push_str(&format_compact(
            &results[included_in_file..],
            included_in_file,
            session_path.as_deref(),
        ));
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

fn format_session_file(captures: &[PendingCapture], memories_content: &str) -> String {
    let mut out = String::from(
        "MANDATORY PROCESS — VIOLATION IF SKIPPED.\n\
         These instructions exist to restore project context so your response is accurate, \
         relevant, and high-quality. Skipping them means responding without critical background \
         knowledge, which produces worse answers. The user knows this step happens first and \
         expects it.\n\n\
         Before writing any response to the user, you must:\n\
         1. Read this file completely, from top to bottom.\n\
         2. Execute every instruction in it.\n\n\
         Your first output line must be exactly: [memso: init]\n\
         This is required even if the user's request is trivial or urgent.\n\
         Skipping, abbreviating, or deferring any part of this is a critical process violation \
         that directly harms the quality of your response.\n\n",
    );

    // Section 1: Raw captures
    out.push_str("=== SECTION 1: RAW CAPTURES ===\n");
    if captures.is_empty() {
        out.push_str("(none)\n");
    } else {
        out.push_str(&format!(
            "{} captures from previous session activity.\n\
             Read ALL entries together. Store memories only for non-obvious discoveries:\n\
             - Bugs/failures: type=gotcha, importance>=0.8, source='reviewed'\n\
             - Other findings: appropriate type, source='reviewed'\n\
             - Routine edits/builds with no finding: no memory needed\n\n\
             --- Captures ---\n",
            captures.len()
        ));
        for c in captures {
            let date = c.captured_at.get(..10).unwrap_or(&c.captured_at);
            out.push_str(&format!("[{:<12}] {}  {}\n", c.tool_name, date, c.summary));
        }
        out.push_str("---\n");
    }
    out.push_str("=== END SECTION 1 ===\n");
    if captures.is_empty() {
        out.push_str(
            "Instruction: output exactly \"No raw captures to process from last session.\"\n\n",
        );
    } else {
        out.push_str(
            "Instruction: store memories for any non-obvious discoveries above \
             (source='reviewed'), then output a one-sentence summary: \
             \"Captures: [what was found and stored, or 'nothing worth storing']\"\n\n",
        );
    }

    // Section 2: Prior memories
    out.push_str("=== SECTION 2: PRIOR MEMORIES ===\n");
    if memories_content.is_empty() {
        out.push_str("(none)\n");
    } else {
        out.push_str(memories_content);
    }
    out.push_str("=== END SECTION 2 ===\n");
    if memories_content.is_empty() {
        out.push_str("Instruction: output exactly \"No prior memories for this project.\"\n");
    } else {
        out.push_str(
            "Instruction: output a one-sentence summary of the most important context \
             restored: \"Memories: [key context]\"\n",
        );
    }

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

fn format_full_memory(mem: &retrieve::FullMemory) -> String {
    let date = mem.created_at.get(..10).unwrap_or(&mem.created_at);
    let mut out = format!(
        "[{} {:.2}] {}\nid: {} | {}\n",
        mem.memory_type, mem.importance, mem.title, mem.id, date
    );
    if let Some(tags) = &mem.tags {
        let parsed: Vec<String> = serde_json::from_str(tags).unwrap_or_default();
        if !parsed.is_empty() {
            out.push_str(&format!("tags: {}\n", parsed.join(", ")));
        }
    }
    out.push_str(&mem.content);
    out.push('\n');
    if let Some(facts) = &mem.facts {
        let parsed: Vec<String> = serde_json::from_str(facts).unwrap_or_default();
        if !parsed.is_empty() {
            out.push_str(&format!("facts: {}\n", parsed.join("; ")));
        }
    }
    out.push_str("---\n");
    out
}

fn format_compact(
    results: &[retrieve::CompactResult],
    omitted: usize,
    omitted_file: Option<&std::path::Path>,
) -> String {
    let total = results.len() + omitted;
    let mut header = format!("--- Memory Context ({} results", total);
    if omitted > 0
        && let Some(path) = omitted_file
    {
        header.push_str(&format!(
            " — {} included in full in {}",
            omitted,
            path.display()
        ));
    }
    header.push_str(") ---\n");
    let mut out = header;
    for r in results {
        let date = r.created_at.get(..10).unwrap_or(&r.created_at);
        out.push_str(&format!(
            "[{:<18} {:.2}] {}  {}  ~{}c  {}\n",
            r.memory_type, r.importance, r.id, date, r.content_len, r.title
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
