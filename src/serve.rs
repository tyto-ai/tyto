use anyhow::Result;
use chrono::Utc;
use rmcp::{
    RoleServer, ServerHandler, ServiceExt,
    handler::server::{router::{tool::ToolRouter, prompt::PromptRouter}, wrapper::Parameters},
    model::{
        GetPromptRequestParams, GetPromptResult, Implementation, InitializeResult,
        ListPromptsResult, PaginatedRequestParams, PromptMessage, PromptMessageRole,
        ServerCapabilities,
    },
    prompt, prompt_handler, prompt_router, tool, tool_handler, tool_router,
    service::RequestContext,
    transport::stdio,
};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_with::{DisplayFromStr, PickFirst, json::JsonString, serde_as};
use std::sync::Arc;
use tokio::sync::Mutex;
use uuid::Uuid;

use std::path::PathBuf;

use crate::{
    remote,
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
    config: Arc<Config>,
    tool_router: ToolRouter<Self>,
    prompt_router: PromptRouter<Self>,
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
    /// Pin this memory so it is never evicted and always surfaces at session start.
    /// Omit to leave unpinned (default). Use pin_memory to change later.
    #[serde(default)]
    pinned: Option<bool>,
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

#[serde_as]
#[derive(Debug, Deserialize, JsonSchema)]
struct GetMemoriesInput {
    /// IDs of memories to fetch in full.
    // Claude Code MCP client sends arrays as JSON-encoded strings - see comment above.
    #[serde_as(as = "PickFirst<(_, JsonString)>")]
    #[schemars(with = "Vec<String>")]
    ids: Vec<String>,
}

#[serde_as]
#[derive(Debug, Deserialize, JsonSchema)]
struct StoreMemoriesInput {
    /// Array of memories to store. Each follows the same schema as a single store call.
    // Claude Code MCP client sends arrays as JSON-encoded strings - see comment above.
    #[serde_as(as = "PickFirst<(_, JsonString)>")]
    #[schemars(with = "Vec<StoreMemoryInput>")]
    memories: Vec<StoreMemoryInput>,
}

#[serde_as]
#[derive(Debug, Deserialize, JsonSchema)]
struct DeleteMemoriesInput {
    /// IDs of memories to delete.
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

#[serde_as]
#[derive(Debug, Deserialize, JsonSchema)]
struct PinMemoriesInput {
    /// IDs of memories to pin or unpin.
    // Claude Code MCP client sends arrays as JSON-encoded strings - see comment above.
    #[serde_as(as = "PickFirst<(_, JsonString)>")]
    #[schemars(with = "Vec<String>")]
    ids: Vec<String>,
    /// true to pin (exempt from eviction, surfaced at session start); false to unpin.
    // Claude Code MCP client sends booleans as strings - see comment above.
    #[serde_as(as = "PickFirst<(_, DisplayFromStr)>")]
    #[schemars(with = "bool")]
    pin: bool,
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

#[derive(Debug, Deserialize, JsonSchema)]
struct SeedCloudInput {
    /// Overwrite remote database even if it already has data.
    #[serde(default)]
    force: Option<bool>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct MigrateToTursoInput {
    /// Turso database URL (e.g. libsql://mydb-org.turso.io). Leave empty to be prompted interactively.
    #[serde(default)]
    to_turso: Option<String>,
}

// --- Prompt implementations ---

#[prompt_router]
impl MemsoServer {
    #[prompt(
        name = "remote_enable",
        description = "Enable remote sync by migrating the local memso database to a remote backend"
    )]
    async fn remote_enable(
        &self,
        Parameters(input): Parameters<MigrateToTursoInput>,
    ) -> Vec<PromptMessage> {
        let cmd = match input.to_turso {
            Some(ref url) => format!("memso remote enable --url {url}"),
            None => "memso remote enable".to_string(),
        };
        vec![PromptMessage::new_text(
            PromptMessageRole::User,
            format!(
                "Please run `{cmd}` to enable remote sync for memso. \
                 You will need --url <url> and --token <token> (get these from the Turso dashboard or `turso db show` / `turso db tokens create`). \
                 Set REMOTE_AUTH_TOKEN in your environment and restart Claude Code when done."
            ),
        )]
    }
}

// --- Tool implementations ---

#[tool_router]
impl MemsoServer {
    #[tool(description = "Store or upsert one or more memories. Accepts an array - use for batch storage at session-start review or when storing related memories together. Use topic_key for upsert semantics.")]
    async fn store_memories(&self, Parameters(input): Parameters<StoreMemoriesInput>) -> Result<String, String> {
        if input.memories.is_empty() {
            return Ok("No memories provided.".to_string());
        }

        // Generate all embeddings with the lock held for the whole batch.
        let embed_texts: Vec<String> = input.memories.iter()
            .map(|m| format!("{} {}", m.title, m.content))
            .collect();
        let embeddings: Vec<_> = {
            let mut e = self.embedder.lock().await;
            let mut out = Vec::with_capacity(embed_texts.len());
            for text in &embed_texts {
                out.push(e.embed(text).map_err(|e| format!("embed failed: {e}"))?);
            }
            out
        };

        let mut results = Vec::with_capacity(input.memories.len());
        for (memory, embedding) in input.memories.into_iter().zip(embeddings) {
            let project = memory.project_id.unwrap_or_else(|| self.project_id.clone());
            let req = store::StoreRequest {
                content: memory.content,
                memory_type: memory.memory_type,
                title: memory.title,
                tags: memory.tags,
                topic_key: memory.topic_key,
                project_id: project,
                session_id: self.session_id.clone(),
                importance: memory.importance,
                facts: memory.facts,
                source: memory.source,
                pinned: memory.pinned,
            };
            match store::store_memory(&self.conn, embedding, &self.write_lock, req, 30).await {
                Ok(r) => results.push(if r.upserted { format!("Updated {}", r.id) } else { format!("Stored {}", r.id) }),
                Err(e) => results.push(format!("Error: {e}")),
            }
        }
        Ok(results.join("\n"))
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

    #[tool(description = "Fetch the full content of one or more memories by ID in a single call. Use when session-start context lists IDs you need in full.")]
    async fn get_memories(&self, Parameters(input): Parameters<GetMemoriesInput>) -> Result<String, String> {
        if input.ids.is_empty() {
            return Ok("No IDs provided.".to_string());
        }
        let memories = retrieve::get_full_batch(&self.conn, &input.ids)
            .await
            .map_err(|e| format!("get_memories failed: {e}"))?;

        // Index by ID so we can output in the requested order and detect missing ones.
        let by_id: std::collections::HashMap<&str, &retrieve::FullMemory> =
            memories.iter().map(|m| (m.id.as_str(), m)).collect();

        let parts: Vec<String> = input.ids.iter().map(|id| {
            match by_id.get(id.as_str()) {
                Some(m) => format_full_memory(m),
                None => format!("Memory {id} not found"),
            }
        }).collect();

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

    #[tool(description = "Pin or unpin one or more memories. Pinned memories are never evicted and always surface at session start. Use pin=true to pin, pin=false to unpin.")]
    async fn pin_memories(&self, Parameters(input): Parameters<PinMemoriesInput>) -> Result<String, String> {
        if input.ids.is_empty() {
            return Ok("No IDs provided.".to_string());
        }
        let total = input.ids.len();
        let action = if input.pin { "Pinned" } else { "Unpinned" };
        match retrieve::pin_batch(&self.conn, &input.ids, input.pin).await {
            Ok(n) if n as usize == total => Ok(format!("{action} {n} memories")),
            Ok(n) => Ok(format!("{action} {n}/{total} memories ({} not found)", total - n as usize)),
            Err(e) => Err(format!("pin_memories failed: {e}")),
        }
    }

    #[tool(description = "Seed remote database from the local backup. Checks: replica mode is configured, backup file exists. Aborts if remote already has data unless force=true.")]
    async fn remote_sync(&self, Parameters(input): Parameters<SeedCloudInput>) -> Result<String, String> {
        remote::sync(&self.config, input.force.unwrap_or(false))
            .await
            .map_err(|e| format!("remote_sync failed: {e}"))
    }

    #[tool(description = "Delete one or more memories by ID.")]
    async fn delete_memories(&self, Parameters(input): Parameters<DeleteMemoriesInput>) -> Result<String, String> {
        if input.ids.is_empty() {
            return Ok("No IDs provided.".to_string());
        }
        let total = input.ids.len();
        match retrieve::delete_batch(&self.conn, &input.ids).await {
            Ok(n) if n as usize == total => Ok(format!("Deleted {n} memories")),
            Ok(n) => Ok(format!("Deleted {n}/{total} memories ({} not found)", total - n as usize)),
            Err(e) => Err(format!("delete_memories failed: {e}")),
        }
    }
}

#[tool_handler]
#[prompt_handler]
impl ServerHandler for MemsoServer {
    fn get_info(&self) -> InitializeResult {
        InitializeResult::new(ServerCapabilities::builder().enable_tools().enable_prompts().build())
            .with_server_info(Implementation::new("memso", env!("CARGO_PKG_VERSION")))
            .with_instructions(
                "Persistent memory across sessions. \
                 Store every decision, discovery, gotcha, failure, and unexpected outcome - \
                 use importance (0.0-1.0) to signal value, not omission. \
                 Failures and unexpected outcomes: type='gotcha', importance >= 0.8. \
                 After understanding a subsystem through exploration, store a how-it-works memory. \
                 When you find a bug: store it as gotcha before writing the fix. \
                 When you finish understanding a function or module: store how-it-works before moving on. \
                 Store inline as you work - do not defer to end of session. \
                 Use search_memory before significant tasks and get_memories(ids) to fetch full content by ID. \
                 capture_note(summary) = your reasoning before/after a change, reviewed next session. \
                 store_memories = facts you would want to search for today or in a future session. \
                 They are not interchangeable. \
                 Set source='reviewed' when storing memories during session-start review. \
                 Tools: store_memories(memories:[{content,type,title,[topic_key,importance,tags,facts,source,pinned]}]) | \
                 search_memory(query,[limit]) | get_memories(ids) | \
                 list_memories([type,tags,limit]) | capture_note(summary,[context]) | \
                 pin_memories(ids,pin) | delete_memories(ids) | remote_sync()",
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
        config: Arc::new(config),
        tool_router: MemsoServer::tool_router(),
        prompt_router: MemsoServer::prompt_router(),
    };

    let service = server.serve(stdio()).await?;

    // Wait for the MCP client to disconnect OR a shutdown signal (SIGTERM/SIGINT).
    // Awaiting the signal lets tokio flush pending async tasks and libsql write
    // its WAL cleanly, preventing local replica corruption on Claude Code restart.
    tokio::select! {
        result = service.waiting() => { result?; }
        _ = shutdown_signal() => {
            eprintln!("memso: shutting down");
        }
    }
    Ok(())
}

async fn shutdown_signal() {
    use tokio::signal;
    let ctrl_c = async { signal::ctrl_c().await.ok() };
    #[cfg(unix)]
    let terminate = async {
        signal::unix::signal(signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler")
            .recv()
            .await
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        _ = ctrl_c => {}
        _ = terminate => {}
    }
}

fn format_compact(results: &[retrieve::CompactResult]) -> String {
    let mut out = format!("--- Memory Context ({} results) ---\n", results.len());
    for r in results {
        let date = r.created_at.get(..10).unwrap_or(&r.created_at);
        out.push_str(&format!("[{:<18} {:.2}] {}  {}  ~{}c  {}\n", r.memory_type, r.importance, r.id, date, r.content_len, r.title));
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
