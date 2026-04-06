use anyhow::Result;
use chrono::Utc;
use rmcp::{
    ServerHandler, ServiceExt,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{Implementation, InitializeResult, ServerCapabilities},
    tool, tool_handler, tool_router,
    transport::stdio,
};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_with::{DisplayFromStr, PickFirst, json::JsonString, serde_as};
use std::sync::Arc;
use tokio::sync::Mutex;
use uuid::Uuid;

use crate::{
    config::Config,
    db::Db,
    embed::Embedder,
    migrations,
    project_id,
    retrieve,
    store::{self, WriteLock},
};

#[derive(Clone)]
struct MemsoServer {
    conn: Arc<libsql::Connection>,
    embedder: Arc<Mutex<Embedder>>,
    write_lock: WriteLock,
    session_id: String,
    project_id: String,
    tool_router: ToolRouter<Self>,
}

// --- Tool input schemas ---

// Claude Code's MCP client serializes numeric and array parameters as JSON strings
// regardless of the declared JSON schema type (e.g. sends "5" instead of 5).
// PickFirst<(_, DisplayFromStr)> tries native JSON deserialization first, then falls
// back to parsing from a string, accepting both forms transparently.

#[serde_as]
#[derive(Debug, Deserialize, JsonSchema)]
struct StoreMemoryInput {
    /// Full text of the memory to store.
    content: String,
    /// Memory type: decision | gotcha | problem-solution | how-it-works |
    /// what-changed | trade-off | preference | discovery | workflow | fact
    #[serde(rename = "type")]
    memory_type: String,
    /// Short summary shown in search results (one line).
    title: String,
    /// Stable slug for upsert semantics, e.g. "auth-session-store". Omit to always append.
    #[serde(default)]
    topic_key: Option<String>,
    /// Array of short discrete facts extracted from the content.
    // Claude Code MCP client sends arrays as JSON-encoded strings - see comment above.
    #[serde_as(as = "PickFirst<(_, JsonString)>")]
    #[schemars(with = "Vec<String>")]
    #[serde(default)]
    facts: Vec<String>,
    /// Array of tag strings.
    // Claude Code MCP client sends arrays as JSON-encoded strings - see comment above.
    #[serde_as(as = "PickFirst<(_, JsonString)>")]
    #[schemars(with = "Vec<String>")]
    #[serde(default)]
    tags: Vec<String>,
    /// Importance 0.0-1.0. Use 0.9+ for architecture decisions, 0.7+ for gotchas.
    // Claude Code MCP client sends floats as strings - see comment above.
    #[serde_as(as = "Option<PickFirst<(_, DisplayFromStr)>>")]
    #[schemars(with = "Option<f32>")]
    #[serde(default)]
    importance: Option<f32>,
    /// Memory source: omit for default ('realtime'). Set to 'reviewed' when storing
    /// during session-start review to receive a small retention boost.
    #[serde(default)]
    source: Option<String>,
    /// Project scope. Omit to use the server's configured project_id.
    #[serde(default)]
    project_id: Option<String>,
}

#[serde_as]
#[derive(Debug, Deserialize, JsonSchema)]
struct SearchMemoryInput {
    /// Natural language search query.
    query: String,
    /// Project scope. Omit to use the server's configured project_id.
    #[serde(default)]
    project_id: Option<String>,
    /// Maximum results to return (default 5).
    // Claude Code MCP client sends integers as strings - see comment above.
    #[serde_as(as = "Option<PickFirst<(_, DisplayFromStr)>>")]
    #[schemars(with = "Option<usize>")]
    #[serde(default)]
    limit: Option<usize>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct GetMemoryInput {
    /// ID of the memory to fetch in full.
    id: String,
}

#[serde_as]
#[derive(Debug, Deserialize, JsonSchema)]
struct GetMemoriesInput {
    /// IDs of memories to fetch in full (batch variant of get_memory).
    // Claude Code MCP client sends arrays as JSON-encoded strings - see comment above.
    #[serde_as(as = "PickFirst<(_, JsonString)>")]
    #[schemars(with = "Vec<String>")]
    ids: Vec<String>,
}

#[serde_as]
#[derive(Debug, Deserialize, JsonSchema)]
struct ListMemoriesInput {
    /// Project scope. Omit to use the server's configured project_id.
    #[serde(default)]
    project_id: Option<String>,
    /// Filter by type (optional).
    #[serde(default, rename = "type")]
    memory_type: Option<String>,
    /// Filter by tags (optional). Only memories containing ALL specified tags are returned.
    // Claude Code MCP client sends arrays as JSON-encoded strings - see comment above.
    #[serde_as(as = "PickFirst<(_, JsonString)>")]
    #[schemars(with = "Vec<String>")]
    #[serde(default)]
    tags: Vec<String>,
    /// Maximum results to return (default 20).
    // Claude Code MCP client sends integers as strings - see comment above.
    #[serde_as(as = "Option<PickFirst<(_, DisplayFromStr)>>")]
    #[schemars(with = "Option<usize>")]
    #[serde(default)]
    limit: Option<usize>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct DeleteMemoryInput {
    /// ID of the memory to delete.
    id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct CaptureNoteInput {
    /// Brief observation from exploration or tentative finding. Reviewed at next session start.
    summary: String,
    /// Context label for the note (e.g. "exploration", "read", "grep"). Defaults to "note".
    #[serde(default = "default_capture_context")]
    context: String,
}

fn default_capture_context() -> String { "note".to_string() }

// --- Tool implementations ---

#[tool_router]
impl MemsoServer {
    #[tool(description = "Store or upsert a memory. Use topic_key for upsert semantics.")]
    async fn store_memory(&self, Parameters(input): Parameters<StoreMemoryInput>) -> Result<String, String> {
        let project = input.project_id.unwrap_or_else(|| self.project_id.clone());
        let embed_text = format!("{} {}", input.title, input.content);

        // Compute embedding with the embedder lock held, then release before DB work.
        let embedding = {
            let mut e = self.embedder.lock().await;
            e.embed(&embed_text).map_err(|e| format!("embed failed: {e}"))?
        };

        let req = store::StoreRequest {
            content: input.content,
            memory_type: input.memory_type,
            title: input.title,
            tags: input.tags,
            topic_key: input.topic_key,
            project_id: project,
            session_id: self.session_id.clone(),
            importance: input.importance,
            facts: input.facts,
            source: input.source,
        };
        store::store_memory(&self.conn, embedding, &self.write_lock, req, 30)
            .await
            .map(|r| if r.upserted { format!("Updated memory {}", r.id) } else { format!("Stored memory {}", r.id) })
            .map_err(|e| format!("store_memory failed: {e}"))
    }

    #[tool(description = "Search memories using semantic + keyword search. Returns compact summaries with IDs. Use get_memory or get_memories to fetch full content.")]
    async fn search_memory(&self, Parameters(input): Parameters<SearchMemoryInput>) -> Result<String, String> {
        let project = input.project_id.unwrap_or_else(|| self.project_id.clone());
        let limit = input.limit.unwrap_or(5);

        // Compute embedding with the embedder lock held, then release before DB work.
        let embedding = {
            let mut e = self.embedder.lock().await;
            e.embed(&input.query).map_err(|e| format!("embed failed: {e}"))?
        };

        retrieve::search(&self.conn, embedding, &input.query, &project, limit)
            .await
            .map(|results| if results.is_empty() { "No memories found.".to_string() } else { format_compact(&results) })
            .map_err(|e| format!("search_memory failed: {e}"))
    }

    #[tool(description = "Fetch the full content of a specific memory by ID.")]
    async fn get_memory(&self, Parameters(input): Parameters<GetMemoryInput>) -> Result<String, String> {
        retrieve::get_full(&self.conn, &input.id)
            .await
            .map_err(|e| format!("get_memory failed: {e}"))
            .and_then(|opt| {
                opt.map(|m| format_full_memory(&m))
                    .ok_or_else(|| format!("Memory {} not found", input.id))
            })
    }

    #[tool(description = "Fetch the full content of multiple memories by ID in a single call. Use when session-start context lists several IDs you need in full.")]
    async fn get_memories(&self, Parameters(input): Parameters<GetMemoriesInput>) -> Result<String, String> {
        if input.ids.is_empty() {
            return Ok("No IDs provided.".to_string());
        }
        let mut parts: Vec<String> = Vec::new();
        // Sequential fetches are acceptable here: this is called at most once per session
        // with a small fixed ID set, and libsql connections are not concurrency-safe to
        // share across spawned tasks without Arc cloning + careful lifetime management.
        for id in &input.ids {
            match retrieve::get_full(&self.conn, id).await {
                Ok(Some(m)) => parts.push(format_full_memory(&m)),
                Ok(None) => parts.push(format!("Memory {id} not found")),
                Err(e) => parts.push(format!("Error fetching {id}: {e}")),
            }
        }
        Ok(parts.join("\n---\n"))
    }

    #[tool(description = "List memories with optional filters. Returns compact summaries.")]
    async fn list_memories(&self, Parameters(input): Parameters<ListMemoriesInput>) -> Result<String, String> {
        let project = input.project_id.unwrap_or_else(|| self.project_id.clone());
        let limit = input.limit.unwrap_or(20);
        retrieve::list(&self.conn, &project, input.memory_type.as_deref(), &input.tags, limit, 0.0)
            .await
            .map(|results| if results.is_empty() { "No memories found.".to_string() } else { format_compact(&results) })
            .map_err(|e| format!("list_memories failed: {e}"))
    }

    #[tool(description = "Stage a lightweight note for review at next session start. Use during exploration for tentative observations not yet ready for a full memory.")]
    async fn capture_note(&self, Parameters(input): Parameters<CaptureNoteInput>) -> Result<String, String> {
        let id = Uuid::new_v4().to_string();
        let now = Utc::now().to_rfc3339();
        self.conn
            .execute(
                "INSERT INTO raw_captures (id, project_id, captured_at, tool_name, summary, raw_data) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                libsql::params![id, self.project_id.clone(), now, input.context, input.summary.clone(), input.summary.clone()],
            )
            .await
            .map(|_| format!("Staged note: {}", input.summary))
            .map_err(|e| format!("capture_note failed: {e}"))
    }

    #[tool(description = "Delete a memory by ID.")]
    async fn delete_memory(&self, Parameters(input): Parameters<DeleteMemoryInput>) -> Result<String, String> {
        self.conn
            .execute("UPDATE memories SET status = 'deleted' WHERE id = ?1", libsql::params![input.id.clone()])
            .await
            .map_err(|e| format!("delete_memory failed: {e}"))
            .and_then(|rows| {
                if rows > 0 { Ok(format!("Deleted memory {}", input.id)) }
                else { Err(format!("Memory {} not found", input.id)) }
            })
    }
}

#[tool_handler]
impl ServerHandler for MemsoServer {
    fn get_info(&self) -> InitializeResult {
        InitializeResult::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("memso", env!("CARGO_PKG_VERSION")))
            .with_instructions(
                "Persistent memory across sessions. \
                 Store every decision, discovery, gotcha, failure, and unexpected outcome - \
                 use importance (0.0-1.0) to signal value, not omission. \
                 Failures and unexpected outcomes: type='gotcha', importance >= 0.8. \
                 After understanding a subsystem through exploration, store a how-it-works memory. \
                 Use search_memory before significant tasks and get_memory or get_memories to fetch full content by ID. \
                 Use capture_note(summary) to record your reasoning before or after making changes - \
                 PostToolUse captures describe only what changed, not why. \
                 Set source='reviewed' when storing memories during session-start review. \
                 Tools: store_memory(content,type,title,[topic_key,importance,tags,facts,source]) | \
                 search_memory(query,[limit]) | get_memory(id) | get_memories(ids) | \
                 list_memories([type,tags,limit]) | capture_note(summary,[context]) | delete_memory(id)",
            )
    }
}

pub async fn run(config: Config) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let pid = project_id::resolve(&cwd, config.memory.project_id.as_deref());

    eprintln!("memso: opening database...");
    let db = Db::open(&config).await?;
    let conn = Arc::new(db.conn);

    eprintln!("memso: running migrations...");
    migrations::run(&conn).await?;

    eprintln!("memso: loading embedding model...");
    let embedder = Arc::new(Mutex::new(Embedder::load()?));

    let session_id = Uuid::new_v4().to_string();
    eprintln!("memso: session {session_id}, project \"{pid}\"");
    eprintln!("memso: ready");

    let server = MemsoServer {
        conn,
        embedder,
        write_lock: store::new_write_lock(),
        session_id,
        project_id: pid,
        tool_router: MemsoServer::tool_router(),
    };

    let service = server.serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}

fn format_compact(results: &[retrieve::CompactResult]) -> String {
    let mut out = format!("--- Memory Context ({} results) ---\n", results.len());
    for r in results {
        let date = r.created_at.get(..10).unwrap_or(&r.created_at);
        out.push_str(&format!("[{:<18}] {}  {}  {}\n", r.memory_type, r.id, date, r.title));
    }
    out.push_str("---\n");
    out
}

fn format_full_memory(m: &retrieve::FullMemory) -> String {
    let facts: Vec<String> = m.facts.as_deref()
        .and_then(|f| serde_json::from_str(f).ok()).unwrap_or_default();
    let tags: Vec<String> = m.tags.as_deref()
        .and_then(|t| serde_json::from_str(t).ok()).unwrap_or_default();
    let facts_str = if facts.is_empty() { "none".to_string() }
        else { format!("- {}", facts.join("\n- ")) };
    format!(
        "[{memory_type}] {title}\nID: {id}\nCreated: {created}\nImportance: {imp:.1}\nTags: {tags}\n\nContent:\n{content}\n\nFacts:\n{facts}",
        memory_type = m.memory_type, title = m.title, id = m.id,
        created = m.created_at, imp = m.importance,
        tags = tags.join(", "), content = m.content, facts = facts_str,
    )
}
