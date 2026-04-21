use anyhow::Result;
use chrono::Utc;
use std::env;
use std::io::{IsTerminal, Read};

use crate::{config::{StorageMode, Config}, db::Db, migrations, project_id, retrieve};

const INSTRUCTIONS: &str = "[tyto] Store every decision, discovery, gotcha, failure, and unexpected outcome. \
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
[tyto tools] store_memories(memories:[{content,type,title,[topic_key,importance,tags,facts,source,pinned]}]) | \
search_memory(query,[limit,detail]) | get_memories(ids) | \
list_memories([type,tags,limit,detail]) | capture_note(summary,[context]) | \
pin_memories(ids,pin) | delete_memories(ids) | \
list_stale_memories() | evict_stale_memories() | session_context()\n";

/// The three observable states of `tyto serve` from inject's perspective.
enum ServeState {
    /// No serve process is running. inject should open the DB directly.
    NotRunning,
    /// serve.lock is held but serve.ready does not yet exist.
    /// The server is still initialising (DB open, migrations, model download).
    /// MCP tools will return a "syncing" error until this transitions to Ready.
    Loading,
    /// serve.ready exists. The DB and embedder are up and all tools work.
    Ready,
}

/// Detect the current serve state via serve.ready (fast) and serve.lock (slow).
/// The OS releases serve.lock automatically on any exit, so there are no stale files.
fn serve_state(config: &Config) -> ServeState {
    if config.serve_ready_path().exists() {
        return ServeState::Ready;
    }
    use fs4::fs_std::FileExt;
    let lock_path = config.serve_lock_path();
    let Ok(file) = std::fs::OpenOptions::new().write(true).create(true).truncate(false).open(&lock_path) else {
        return ServeState::NotRunning;
    };
    // Ok(true)  = we acquired the lock -> nothing is running
    // Ok(false) = another process holds it -> serve is loading
    match file.try_lock_exclusive() {
        Ok(true) => { let _ = file.unlock(); ServeState::NotRunning }
        Ok(false) => ServeState::Loading,
        Err(_) => ServeState::NotRunning,
    }
}

/// Emitted to the agent when serve is still loading (first-run model download).
/// UX requirement: always tell the user what is happening, why, and how long.
const LOADING_MESSAGE: &str =
    "[tyto] The memory server is still starting up.\n\
     On first install, tyto downloads a ~22MB embedding model — this takes up to a minute.\n\
     Memory tools (search_memory, store_memories, etc.) will return a 'loading' message \
     until startup is complete.\n\n\
     Do the following right now, before responding to the user's request:\n\
     1. Inform the user: \"tyto is loading its embedding model (first-run setup, ~1 minute). \
        Memory context is not available yet.\"\n\
     2. Continue with the user's request as normal — do not block on tyto.\n\
     3. Once tyto has finished loading, call the session_context tool to load memory \
        context for this session. If session_context returns a 'loading' message, \
        wait a few seconds and retry.";

fn build_session_instructions(session_path: Option<&std::path::Path>) -> String {
    match session_path {
        None => String::new(),
        Some(path) => format!(
            "[tyto] Session start — BEFORE responding to the user, read this file \
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
            "[tyto] CRITICAL: Memory system unavailable - memories were NOT loaded for this \
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

    // No project_id means no config or config without project_id. Skip all DB work
    // so no DB is opened. On session/compact types, emit a message so
    // the AI can surface the setup instructions to the user.
    if config.project_id.is_none() {
        if inject_type == "session" || inject_type == "compact" {
            let suggested = crate::project_id::infer(&config.project_root);
            println!(
                "[tyto] tyto has loaded, but there is no `.tyto.toml` configuration \
                 file for this project, so memories will not be stored or retrieved this \
                 session.\n\
                 Inform the user of this. Offer to set up tyto by creating a \
                 `.tyto.toml` file. Suggested configuration:\n\n\
                 ```toml\n\
                 project_id = \"{suggested}\"\n\
                 ```\n\
                 Ask the user whether to use this value or a different project_id."
            );
        }
        return Ok(());
    }

    // Check for a crash log written by a previous `tyto serve` session.
    // Output to stdout so it lands in additionalContext before any memory content.
    let crash_log_path = config.db_path()
        .parent()
        .map(|p| p.join("crash.log"))
        .unwrap_or_else(|| std::path::PathBuf::from("crash.log"));
    if let Ok(crash) = std::fs::read_to_string(&crash_log_path) {
        println!(
            "[tyto] WARNING: tyto crashed in a previous session. \
             Inform the user of this before doing anything else - \
             recent memories may not have been saved.\nCrash report: {crash}"
        );
    }

    // If `tyto serve` is running, skip Db::open to avoid racing on the DB file.
    // Not applicable for remote mode: Turso handles concurrent connections safely.
    // We distinguish Ready (tools work) from Loading (tools return "syncing") so we
    // can give the user accurate information about what is happening and what to expect.
    if !matches!(config.memory.storage.mode, StorageMode::Remote) {
        match serve_state(&config) {
            ServeState::Ready => {
                if inject_type == "session" || inject_type == "compact" {
                    println!(
                        "{INSTRUCTIONS}[tyto] MCP server is running — memory context is available via tools. \
                         Use search_memory / list_memories for context retrieval this session."
                    );
                }
                return Ok(());
            }
            ServeState::Loading => {
                // Only session/compact hooks tell the agent about the loading state.
                // Prompt hooks emit nothing — the agent already has instructions from
                // the MCP server and there is no memory context to inject yet.
                // Stop hooks emit nothing — there is nothing to checkpoint during loading.
                if inject_type == "session" || inject_type == "compact" {
                    println!("{LOADING_MESSAGE}");
                }
                return Ok(());
            }
            ServeState::NotRunning => {} // fall through to direct DB access
        }
    }

    let db = Db::open(&config).await?;
    let conn = db.conn;
    migrations::run(&conn).await?;

    let pid = project_override
        .unwrap_or_else(|| project_id::resolve(&config.project_root, config.project_id.as_deref()));

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
            output.push_str(&crate::format::compact(&results, 0, None));
        }
    }
    print_within_budget(&output, budget);
    Ok(())
}

const STOP_INSTRUCTIONS: &str =
    "[tyto] End of turn checkpoint - store anything worth keeping before moving on:\n\
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
            let full_memories = retrieve::get_full_batch(conn, &full_ids, project_id).await?;
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
        let path = std::env::temp_dir().join(format!("tyto-session-{pid}.txt"));
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
        output.push_str(&crate::format::compact(
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
         Your first output line must be exactly: [tyto: init]\n\
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

/// Format session content for the `session_context` MCP tool return value.
/// Unlike `format_session_file` (which is injected at operator/system-prompt level),
/// tool results are processed as external data. Commanding language ("MANDATORY PROCESS",
/// "VIOLATION IF SKIPPED") in tool results reads as prompt injection to model safety
/// training. This function presents the same data in a neutral, informational format.
fn format_tool_session_content(captures: &[PendingCapture], memories_content: &str) -> String {
    let mut out = String::new();

    // Section 1: Raw captures
    out.push_str("=== PENDING CAPTURES FROM LAST SESSION ===\n");
    if captures.is_empty() {
        out.push_str("No raw captures to process from last session.\n");
    } else {
        out.push_str(&format!(
            "{} captures from previous session activity.\n\
             Review and store memories for non-obvious discoveries \
             (source='reviewed'). Routine edits with no finding need no memory.\n\n\
             --- Captures ---\n",
            captures.len()
        ));
        for c in captures {
            let date = c.captured_at.get(..10).unwrap_or(&c.captured_at);
            out.push_str(&format!("[{:<12}] {}  {}\n", c.tool_name, date, c.summary));
        }
        out.push_str("---\n");
    }

    // Section 2: Prior memories
    out.push_str("\n=== PRIOR MEMORIES ===\n");
    if memories_content.is_empty() {
        out.push_str("No prior memories for this project.\n");
    } else {
        out.push_str(memories_content);
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

/// Build session context content for the `session_context` MCP tool.
///
/// Returns the same captures + memories that the SessionStart hook would inject,
/// but as a single String suitable for returning directly from a tool call.
/// Also marks any pending captures as presented.
///
/// Called by `serve::session_context` tool — this is the recovery path when
/// `tyto serve` was still loading at session start.
pub async fn build_tool_session_content(
    conn: &libsql::Connection,
    project_id: &str,
) -> Result<String> {
    let captures = query_pending_captures(conn, project_id).await?;
    let results = retrieve::list(conn, project_id, None, &[], 500, 0.4).await?;

    let mut memories_content = String::new();
    let mut included = 0usize;
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
        included = full_ids.len();
        if !full_ids.is_empty() {
            let full_memories = retrieve::get_full_batch(conn, &full_ids, project_id).await?;
            let full_map: std::collections::HashMap<String, retrieve::FullMemory> =
                full_memories.into_iter().map(|m| (m.id.clone(), m)).collect();
            for compact in results.iter().take(included) {
                if let Some(mem) = full_map.get(&compact.id) {
                    memories_content.push_str(&format_full_memory(mem));
                }
            }
        }
    }

    if !captures.is_empty() {
        mark_captures_presented(conn, project_id).await?;
    }

    let mut out = format_tool_session_content(&captures, &memories_content);
    // Append compact index for memories that didn't fit in the full-content budget.
    if included < results.len() {
        out.push_str(&crate::format::compact(&results[included..], included, None));
    }
    Ok(out)
}

fn print_within_budget(output: &str, budget: usize) {
    if output.len() <= budget {
        print!("{output}");
    } else {
        // Truncate at last newline within budget
        let truncated = &output[..budget];
        if let Some(pos) = truncated.rfind('\n') {
            print!("{}", &truncated[..pos]);
            println!("\n[tyto: output truncated to fit budget]");
        } else {
            print!("{truncated}");
        }
    }
}
