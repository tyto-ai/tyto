use anyhow::Result;
use chrono::Utc;
use crate::mlog;
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
use turso::Connection;


use crate::{
    remote,
    config::{Config, RemoteMode},
    db::{self, Db},
    embed::Embedder,
    index,
    migrations,
    project_id,
    retrieve,
    store::{self, WriteLock},
};

/// Database connection and embedding model, available once background init completes.
struct DbReady {
    conn: Arc<Connection>,
    embedder: Arc<Mutex<Embedder>>,
    #[allow(dead_code)]
    write_lock: WriteLock,
    #[allow(dead_code)]
    handle: db::AnyDb,
    #[allow(dead_code)]
    temp_dir: Option<tempfile::TempDir>,
}

/// State of the database for two-phase startup.
#[derive(Clone)]
enum DbState {
    /// Background init (replica sync + embedder load) still in progress.
    Syncing,
    /// Ready — tools may proceed.
    Ready(Arc<DbReady>),
    /// Init failed permanently; all tool calls return this error.
    Failed(String),
}

#[derive(Clone)]
struct TytoServer {
    db: tokio::sync::watch::Receiver<DbState>,
    idx: tokio::sync::watch::Receiver<index::IndexState>,
    write_lock: WriteLock,
    session_id: String,
    project_id: String,
    config: Arc<Config>,
    #[allow(dead_code)]
    tool_router: ToolRouter<Self>,
    #[allow(dead_code)]
    prompt_router: PromptRouter<Self>,
}

impl TytoServer {
    /// Returns the ready state or an actionable error string if still syncing or failed.
    /// Call at the top of every tool handler that needs the DB or embedder.
    fn try_ready(&self) -> Result<Arc<DbReady>, String> {
        match &*self.db.borrow() {
            DbState::Syncing => Err(
                "tyto is syncing the memory database locally (initial replication). \
                 Please wait a moment and retry."
                    .to_string(),
            ),
            DbState::Ready(r) => Ok(Arc::clone(r)),
            DbState::Failed(msg) => Err(format!("tyto database initialisation failed: {msg}")),
        }
    }

    fn try_index_ready(&self) -> Result<Arc<index::IndexReady>, String> {
        match &*self.idx.borrow() {
            index::IndexState::Opening => Err(
                "Code index is initializing, please retry in a moment.".to_string()
            ),
            index::IndexState::Ready(r) => Ok(Arc::clone(r)),
            index::IndexState::Disabled => Err(
                "Code indexing is disabled. Set [index] mode = \"managed\" in .tyto.toml to enable.".to_string()
            ),
            index::IndexState::Failed(msg) => Err(format!("Code index failed: {msg}")),
        }
    }
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
    /// Detail level: omit or "compact" for title-only (default); "summary" to include facts and tags.
    #[serde(default)]
    detail: Option<String>,
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
    /// Detail level: omit or "compact" for title-only (default); "summary" to include facts and tags.
    #[serde(default)]
    detail: Option<String>,
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
struct CaptureNoteInput {
    /// Brief observation from exploration or tentative finding. Reviewed at next session start.
    summary: String,
    /// Context label for the note (e.g. "exploration", "read", "grep"). Defaults to "note".
    #[serde(default = "default_capture_context")]
    context: String,
}

fn default_capture_context() -> String { "note".to_string() }

// --- Code intelligence tool input schemas ---

#[serde_as]
#[derive(Debug, Deserialize, JsonSchema)]
struct SearchCodeInput {
    /// Natural language query describing the code you are looking for.
    query: String,
    /// Maximum results to return (default 10).
    #[serde_as(as = "Option<PickFirst<(_, DisplayFromStr)>>")]
    #[schemars(with = "Option<usize>")]
    #[serde(default)]
    limit: Option<usize>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct GetSymbolInput {
    /// Symbol name or qualified path to look up (e.g. "validate_jwt_token" or "Auth::validate").
    name: String,
    /// Optional file path to narrow the search (e.g. "src/auth.rs").
    #[serde(default)]
    file_path: Option<String>,
}

#[serde_as]
#[derive(Debug, Deserialize, JsonSchema)]
struct SearchInput {
    /// Natural language query to search across memories and source code simultaneously.
    query: String,
    /// Maximum results per source to return (default 5).
    #[serde_as(as = "Option<PickFirst<(_, DisplayFromStr)>>")]
    #[schemars(with = "Option<usize>")]
    #[serde(default)]
    limit: Option<usize>,
}

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
impl TytoServer {
    #[prompt(
        name = "remote_enable",
        description = "Enable remote sync by migrating the local tyto database to a remote backend"
    )]
    async fn remote_enable(
        &self,
        Parameters(input): Parameters<MigrateToTursoInput>,
    ) -> Vec<PromptMessage> {
        let cmd = match input.to_turso {
            Some(ref url) => format!("tyto remote enable --url {url}"),
            None => "tyto remote enable".to_string(),
        };
        vec![PromptMessage::new_text(
            PromptMessageRole::User,
            format!(
                "Please run `{cmd}` to enable remote sync for tyto. \
                 You will need --url <url> and --token <token> (get these from the Turso dashboard or `turso db show` / `turso db tokens create`). \
                 Set TYTO__MEMORY__REMOTE_AUTH_TOKEN in your environment and restart Claude Code when done."
            ),
        )]
    }
}

// --- Tool implementations ---

#[tool_router]
impl TytoServer {
    #[tool(description = "Store or upsert one or more memories. Accepts an array - use for batch storage at session-start review or when storing related memories together. Use topic_key for upsert semantics.")]
    async fn store_memories(&self, Parameters(input): Parameters<StoreMemoriesInput>) -> Result<String, String> {
        let ready = self.try_ready()?;
        if input.memories.is_empty() {
            return Ok("No memories provided.".to_string());
        }

        // Generate all embeddings with the lock held for the whole batch.
        let embed_texts: Vec<String> = input.memories.iter()
            .map(|m| format!("{} {}", m.title, m.content))
            .collect();
        let embeddings: Vec<_> = {
            let mut e = ready.embedder.lock().await;
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
            match store::store_memory(&ready.conn, embedding, &self.write_lock, req, 30).await {
                Ok(r) => results.push(if r.upserted { format!("Updated {}", r.id) } else { format!("Stored {}", r.id) }),
                Err(e) => results.push(format!("Error: {e}")),
            }
        }
        Ok(results.join("\n"))
    }

    #[tool(description = "Search memories using semantic + keyword search. Returns compact summaries with IDs. Pass detail=\"summary\" to also include facts and tags. Use get_memories(ids) to fetch full content.")]
    async fn search_memory(&self, Parameters(input): Parameters<SearchMemoryInput>) -> Result<String, String> {
        let ready = self.try_ready()?;
        let project = input.project_id.unwrap_or_else(|| self.project_id.clone());
        let limit = input.limit.unwrap_or(5);
        let summary = input.detail.as_deref() == Some("summary");

        // Compute embedding with the embedder lock held, then release before DB work.
        let embedding = {
            let mut e = ready.embedder.lock().await;
            e.embed(&input.query).map_err(|e| format!("embed failed: {e}"))?
        };

        retrieve::search(&ready.conn, embedding, &input.query, &project, limit)
            .await
            .map(|results| {
                if results.is_empty() {
                    "No memories found.".to_string()
                } else if summary {
                    crate::format::summary(&results)
                } else {
                    crate::format::compact(&results, 0, None)
                }
            })
            .map_err(|e| format!("search_memory failed: {e}"))
    }

    #[tool(description = "Fetch the full content of one or more memories by ID in a single call. Use when session-start context lists IDs you need in full.")]
    async fn get_memories(&self, Parameters(input): Parameters<GetMemoriesInput>) -> Result<String, String> {
        let ready = self.try_ready()?;
        if input.ids.is_empty() {
            return Ok("No IDs provided.".to_string());
        }
        let memories = retrieve::get_full_batch(&ready.conn, &input.ids, &self.project_id)
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

        let mut out = parts.join("\n---\n");

        // Cross-type similar: per-vector ANN search, deduplicated by ID, best-effort, no model call.
        if !memories.is_empty()
            && let Ok(embeddings) = retrieve::fetch_embeddings(&ready.conn, &input.ids, &self.project_id).await
            && !embeddings.is_empty()
        {
            let exclude: std::collections::HashSet<String> = input.ids.iter().cloned().collect();

            // TUNING: 0.013 derived from empirical score distribution (2026-04-24).
            // Stricter than search — unsolicited results must be genuinely related.
            // Below 0.012 is general project context with no specific connection.
            const MIN_SIMILAR_SCORE: f64 = 0.013;

            let mut mem_seen = std::collections::HashSet::new();
            let mut mem_similar: Vec<retrieve::CompactResult> = Vec::new();
            let mut code_seen = std::collections::HashSet::new();
            let mut code_similar: Vec<index::search::CodeResult> = Vec::new();
            let idx = self.try_index_ready().ok();

            for (mem_id, embedding) in &embeddings {
                let title = by_id.get(mem_id.as_str()).map(|m| m.title.as_str()).unwrap_or("");
                let results = retrieve::search(
                    &ready.conn, embedding.clone(), title, &self.project_id, 8
                ).await.unwrap_or_default();
                for r in results {
                    if r.score >= MIN_SIMILAR_SCORE && !exclude.contains(&r.id) && mem_seen.insert(r.id.clone()) {
                        mem_similar.push(r);
                    }
                }
                if let Some(ref idx) = idx {
                    let code_results = index::search::search_code(
                        &idx.conn, embedding.clone(), title, 5
                    ).await.unwrap_or_default();
                    for r in code_results {
                        if r.rrf_score >= MIN_SIMILAR_SCORE && code_seen.insert(r.id.clone()) {
                            code_similar.push(r);
                        }
                    }
                }
            }

            mem_similar.truncate(5);
            code_similar.truncate(5);

            if !mem_similar.is_empty() || !code_similar.is_empty() {
                out.push_str("\n---\nsimilar:\n");
                for m in &mem_similar {
                    out.push_str(&format!("  [memory] {}  {}  ~{}c\n", m.id, m.title, m.content_len));
                }
                for c in &code_similar {
                    out.push_str(&format!("  [symbol] {}  {}:{}-{}\n",
                        c.qualified_name, c.file_path, c.line_start, c.line_end));
                }
            }
        }

        Ok(out)
    }

    #[tool(description = "List memories with optional filters. Returns compact summaries. Pass detail=\"summary\" to also include facts and tags.")]
    async fn list_memories(&self, Parameters(input): Parameters<ListMemoriesInput>) -> Result<String, String> {
        let ready = self.try_ready()?;
        let project = input.project_id.unwrap_or_else(|| self.project_id.clone());
        let limit = input.limit.unwrap_or(20);
        let summary = input.detail.as_deref() == Some("summary");
        retrieve::list(&ready.conn, &project, input.memory_type.as_deref(), &input.tags, limit, 0.0)
            .await
            .map(|results| {
                if results.is_empty() {
                    "No memories found.".to_string()
                } else if summary {
                    crate::format::summary(&results)
                } else {
                    crate::format::compact(&results, 0, None)
                }
            })
            .map_err(|e| format!("list_memories failed: {e}"))
    }

    #[tool(description = "Stage a lightweight note for review at next session start. Use during exploration for tentative observations not yet ready for a full memory.")]
    async fn capture_note(&self, Parameters(input): Parameters<CaptureNoteInput>) -> Result<String, String> {
        let ready = self.try_ready()?;
        let id = Uuid::new_v4().to_string();
        let now = Utc::now().to_rfc3339();
        ready.conn
            .execute(
                "INSERT INTO raw_captures (id, project_id, captured_at, tool_name, summary, raw_data) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                (id, self.project_id.clone(), now, input.context, input.summary.clone(), input.summary.clone()),
            )
            .await
            .map(|_| format!("Staged note: {}", input.summary))
            .map_err(|e| format!("capture_note failed: {e}"))
    }

    #[tool(description = "Pin or unpin one or more memories. Pinned memories are never evicted and always surface at session start. Use pin=true to pin, pin=false to unpin.")]
    async fn pin_memories(&self, Parameters(input): Parameters<PinMemoriesInput>) -> Result<String, String> {
        let ready = self.try_ready()?;
        if input.ids.is_empty() {
            return Ok("No IDs provided.".to_string());
        }
        let total = input.ids.len();
        let action = if input.pin { "Pinned" } else { "Unpinned" };
        match retrieve::pin_batch(&ready.conn, &input.ids, &self.project_id, input.pin).await {
            Ok(n) if n as usize == total => Ok(format!("{action} {total} memories")),
            // turso execute() may return an unreliable count on file-based WAL DBs (upstream bug).
            // saturating_sub prevents a display underflow; the actual pin operation is correct.
            Ok(n) => Ok(format!("{action} {n}/{total} memories ({} not found)", total.saturating_sub(n as usize))),
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
        let ready = self.try_ready()?;
        if input.ids.is_empty() {
            return Ok("No IDs provided.".to_string());
        }
        let total = input.ids.len();
        match retrieve::delete_batch(&ready.conn, &input.ids, &self.project_id).await {
            Ok(n) if n as usize == total => Ok(format!("Deleted {total} memories")),
            // turso execute() may return an unreliable count on file-based WAL DBs (upstream bug).
            // saturating_sub prevents a display underflow; the actual delete operation is correct.
            Ok(n) => Ok(format!("Deleted {n}/{total} memories ({} not found)", total.saturating_sub(n as usize))),
            Err(e) => Err(format!("delete_memories failed: {e}")),
        }
    }

    #[tool(description = "List memories eligible for eviction: not pinned, older than 7 days, retention score below threshold. Call before evict_stale_memories to review candidates.")]
    async fn list_stale_memories(&self) -> Result<String, String> {
        let ready = self.try_ready()?;
        match retrieve::list_stale(&ready.conn, &self.project_id).await {
            Ok(stale) if stale.is_empty() => Ok("No stale memories found.".to_string()),
            Ok(stale) => {
                let mut out = format!("{} stale memories eligible for eviction:\n", stale.len());
                for m in &stale {
                    out.push_str(&format!(
                        "  [{:<16}  {:.2}] score={:.3}  days={:.0}  {}\n    id: {}\n",
                        m.memory_type, m.importance, m.score, m.days_since_access, m.title, m.id
                    ));
                }
                Ok(out)
            }
            Err(e) => Err(format!("list_stale_memories failed: {e}")),
        }
    }

    #[tool(description = "Load session memory context: pending review captures and top memories for this project. \
        Call this at session start if tyto was still loading when the session began (embedding model download on first install). \
        Returns a 'loading' message if the database is not yet ready — wait a few seconds and retry. \
        Marks pending captures as presented.")]
    async fn session_context(&self) -> Result<String, String> {
        let ready = self.try_ready()?;
        crate::inject::build_tool_session_content(&ready.conn, &self.project_id)
            .await
            .map_err(|e| format!("session_context failed: {e}"))
    }

    #[tool(description = "Permanently delete all stale memories (not pinned, older than 7 days, retention score below threshold). Call list_stale_memories first to review candidates.")]
    async fn evict_stale_memories(&self) -> Result<String, String> {
        let ready = self.try_ready()?;
        match retrieve::evict_stale(&ready.conn, &self.project_id).await {
            Ok(0) => Ok("No stale memories to evict.".to_string()),
            Ok(n) => Ok(format!("Evicted {n} stale memories.")),
            Err(e) => Err(format!("evict_stale_memories failed: {e}")),
        }
    }

    // --- Code intelligence tools ---

    #[tool(description = "Search source code and git history only, without memory results. Use when specifically looking for code references or callers and memory results would add noise.")]
    async fn search_code(&self, Parameters(input): Parameters<SearchCodeInput>) -> Result<String, String> {
        let db_ready = self.try_ready()?;
        let idx = self.try_index_ready()?;
        let limit = input.limit.unwrap_or(10);

        let embedding = {
            let mut e = db_ready.embedder.lock().await;
            e.embed(&input.query).map_err(|e| format!("embed failed: {e}"))?
        };

        let results = index::search::search_code(&idx.conn, embedding, &input.query, limit)
            .await
            .map_err(|e| format!("search_code failed: {e}"))?;

        if results.is_empty() {
            return Ok("No matching code found.".to_string());
        }

        let n = results.len();
        let mut out = String::new();
        for r in &results {
            out.push_str(&index::search::format_result(r, true));
            out.push_str("---\n");
        }
        out.push_str(&format!(
            "_meta: {{returned: {n}, truncated: {}}}\n",
            n >= limit
        ));
        Ok(out)
    }

    #[tool(description = "Look up a specific symbol by name. Returns signature, doc, git history for that line range, and cross-type similar results. Supply file_path to narrow when the name is ambiguous.")]
    async fn get_symbol(&self, Parameters(input): Parameters<GetSymbolInput>) -> Result<String, String> {
        let idx = self.try_index_ready()?;
        let results = index::search::get_symbol(
            &idx.conn,
            &input.name,
            input.file_path.as_deref(),
        )
        .await
        .map_err(|e| format!("get_symbol failed: {e}"))?;

        if results.is_empty() {
            return Ok(format!("Symbol '{}' not found in the index.", input.name));
        }

        let n = results.len();
        let mut out = String::new();

        for r in &results {
            out.push_str(&index::search::format_result(r, true));

            // Append symbol-level commit history from git log -L (line range tracking)
            if idx.git_history && r.line_start > 0 {
                let commits = tokio::task::spawn_blocking({
                    let root = idx.project_root.clone();
                    let fp = r.file_path.clone();
                    let ls = r.line_start as usize;
                    let le = r.line_end as usize;
                    move || index::git::symbol_commits(&root, &fp, ls, le, 8)
                }).await.unwrap_or_default();

                if !commits.is_empty() {
                    out.push_str(&format!("Recent: {}\n", commits.join("; ")));
                }
            }
            out.push_str("---\n");
        }

        // Cross-type similar results (best-effort: skip if DB not ready or embed fails)
        // TUNING: same MIN_SIMILAR_SCORE as get_memories — unsolicited, must earn place.
        const MIN_SIMILAR_SCORE: f64 = 0.013;
        if let Ok(db_ready) = self.try_ready() {
            let first = &results[0];
            let embed_text = format!(
                "{}: {} {}",
                first.symbol_kind,
                first.qualified_name,
                first.signature.as_deref().unwrap_or("")
            );
            if let Ok(embedding) = {
                let mut e = db_ready.embedder.lock().await;
                e.embed(&embed_text)
            } {
                let mem_similar: Vec<_> = retrieve::search(
                    &db_ready.conn, embedding.clone(), &embed_text, &self.project_id, 5
                ).await.unwrap_or_default()
                    .into_iter()
                    .filter(|r| r.score >= MIN_SIMILAR_SCORE)
                    .collect();

                let exclude: std::collections::HashSet<String> =
                    results.iter().map(|r| r.id.clone()).collect();
                let code_similar: Vec<_> = index::search::search_code(
                    &idx.conn, embedding, &embed_text, 10
                ).await.unwrap_or_default()
                    .into_iter()
                    .filter(|r| !exclude.contains(&r.id) && r.rrf_score >= MIN_SIMILAR_SCORE)
                    .take(5)
                    .collect();

                if !mem_similar.is_empty() || !code_similar.is_empty() {
                    out.push_str("similar:\n");
                    for m in &mem_similar {
                        out.push_str(&format!(
                            "  [memory] {}  {}  ~{}c\n",
                            m.id, m.title, m.content_len
                        ));
                    }
                    for c in &code_similar {
                        out.push_str(&format!(
                            "  [symbol] {}  {}:{}-{}\n",
                            c.qualified_name, c.file_path, c.line_start, c.line_end
                        ));
                    }
                }
            }
        }

        out.push_str(&format!("_meta: {{returned: {n}}}\n"));
        Ok(out)
    }

    #[tool(description = "Search memories, source code, and git history simultaneously. Use this by default. Returns memory results and code results in separate sections.")]
    async fn search(&self, Parameters(input): Parameters<SearchInput>) -> Result<String, String> {
        let t_search = std::time::Instant::now();
        let db_ready = self.try_ready()?;
        let limit = input.limit.unwrap_or(5);

        let embedding = {
            let t_lock = std::time::Instant::now();
            let mut e = db_ready.embedder.lock().await;
            tracing::debug!(elapsed_ms = t_lock.elapsed().as_millis(), "embedder lock wait");
            e.embed(&input.query).map_err(|e| format!("embed failed: {e}"))?
        };

        // Memory search
        let t_mem = std::time::Instant::now();
        let memory_results = retrieve::search(
            &db_ready.conn,
            embedding.clone(),
            &input.query,
            &self.project_id,
            limit,
        )
        .await
        .unwrap_or_default();
        tracing::debug!(elapsed_ms = t_mem.elapsed().as_millis(), results = memory_results.len(), "memory search");

        // Code search (best-effort: don't fail the whole search if index not ready)
        let t_code = std::time::Instant::now();
        let index_state_hint: Option<&str>;
        let code_results = match self.try_index_ready() {
            Ok(idx) => {
                index_state_hint = None;
                let r = index::search::search_code(&idx.conn, embedding, &input.query, limit)
                    .await
                    .unwrap_or_default();
                tracing::debug!(elapsed_ms = t_code.elapsed().as_millis(), results = r.len(), "code search");
                r
            }
            Err(_) => {
                index_state_hint = match &*self.idx.borrow() {
                    index::IndexState::Opening => Some("initializing"),
                    index::IndexState::Failed(_) => Some("failed"),
                    // Disabled is intentional — don't surface it on every search.
                    _ => None,
                };
                vec![]
            }
        };

        // Merge memory and code results into a single list ranked by score.
        // Each item is (score, formatted_string). Code results use a separator
        // line since they span multiple lines (signature, doc, churn, history).
        let mut items: Vec<(f64, String)> = Vec::new();
        for r in &memory_results {
            items.push((r.score, crate::format::compact_single(r)));
        }
        for r in &code_results {
            let mut s = index::search::format_result(r, true);
            s.push_str("---\n");
            items.push((r.rrf_score, s));
        }

        // Sort by score descending, then apply two gates:
        // 1. Top-score gate: if the best result is below 0.020 the whole set is noise
        //    (empirical: irrelevant queries top out at ~0.017; relevant ones at 0.025+).
        //    This avoids returning random cosine hits when nothing is relevant.
        // 2. Floor gate: drop any individual result below 0.010 within an otherwise
        //    relevant result set (removes stragglers that crept past the cosine filter).
        const MIN_TOP_SCORE: f64 = 0.020;
        const MIN_SCORE: f64 = 0.010;
        items.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));

        if items.is_empty() || items[0].0 < MIN_TOP_SCORE {
            tracing::debug!(elapsed_ms = t_search.elapsed().as_millis(), "search handler (no results above threshold)");
            return Ok("No results found.".to_string());
        }
        items.retain(|(score, _)| *score >= MIN_SCORE);

        let total = items.len();
        let mut out = format!("--- Context ({total} results) ---\n");
        for (_, text) in &items {
            out.push_str(text);
        }
        let all_count = memory_results.len() + code_results.len();
        let truncated = memory_results.len() >= limit || code_results.len() >= limit;
        let index_field = match index_state_hint {
            Some("initializing") => ", code_index: \"initializing — retry for code results\"",
            Some("failed") => ", code_index: \"failed — check tyto-serve.log\"",
            _ => "",
        };
        out.push_str(&format!(
            "_meta: {{returned: {total}, total_before_cutoff: {all_count}, truncated: {truncated}{index_field}}}\n"
        ));
        tracing::debug!(elapsed_ms = t_search.elapsed().as_millis(), total, "search handler total");
        Ok(out)
    }
}

#[tool_handler]
#[prompt_handler]
impl ServerHandler for TytoServer {
    fn get_info(&self) -> InitializeResult {
        InitializeResult::new(ServerCapabilities::builder().enable_tools().enable_prompts().build())
            .with_server_info(Implementation::new("tyto", env!("CARGO_PKG_VERSION")))
            .with_instructions(
                "Persistent memory and code intelligence across sessions. \
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
                 CODE SEARCH: Use search(query) by default to search memories AND code simultaneously. \
                 Use search_code(query) only when you specifically want code/git results without memory noise. \
                 Use get_symbol(name) for exact symbol lookup — returns signature, git line-range history, hotspot score, and cross-type similar results. \
                 hotspot_score reflects recent modification frequency (higher = more volatile, treat with more scrutiny). \
                 Memory tools: store_memories | search_memory | get_memories | list_memories | capture_note | pin_memories | delete_memories | remote_sync | session_context. \
                 Code tools: search(query) | search_code(query) | get_symbol(name,[file_path])",
            )
    }
}

/// Re-embed any memories that lack a vector for the current model.
/// Runs as a background task; yields between batches to avoid starving MCP handlers.
async fn reembed_stale(conn: Arc<Connection>, embedder: Arc<Mutex<Embedder>>) {
    const BATCH: i64 = 10;
    let model = crate::embed::model_id();
    let mut total = 0usize;

    loop {
        let batch: Vec<(String, String)> = {
            let mut rows: turso::Rows = match conn
                .query(
                    "SELECT m.id, m.title || ' ' || m.content
                     FROM memories m
                     WHERE m.status = 'active'
                       AND NOT EXISTS (
                         SELECT 1 FROM memory_vectors v
                         WHERE v.memory_id = m.id AND v.embed_model = ?1
                       )
                     LIMIT ?2",
                    (model.clone(), BATCH),
                )
                .await
            {
                Ok(r) => r,
                Err(e) => { eprintln!("tyto: reembed scan failed: {e}"); return; }
            };
            let mut out = Vec::new();
            while let Ok(Some(row)) = rows.next().await {
                if let (Ok(id), Ok(text)) = (row.get::<String>(0), row.get::<String>(1)) {
                    out.push((id, text));
                }
            }
            out
        };

        if batch.is_empty() {
            if total > 0 {
                eprintln!("tyto: re-embedded {total} memories to model {model}");
            }
            return;
        }

        for (id, text) in batch {
            let embedding = {
                let mut e = embedder.lock().await;
                match e.embed(&text) {
                    Ok(v) => v,
                    Err(e) => { eprintln!("tyto: reembed failed for {id}: {e}"); continue; }
                }
            };
            let blob = crate::embed::floats_to_blob(&embedding);
            let _ = conn
                .execute(
                    "DELETE FROM memory_vectors WHERE memory_id = ?1",
                    (id.clone(),),
                )
                .await;
            let _ = conn
                .execute(
                    "INSERT INTO memory_vectors (memory_id, embed_model, embedding) VALUES (?1, ?2, ?3)",
                    (id, model.clone(), blob),
                )
                .await;
            total += 1;
        }

        tokio::task::yield_now().await;
    }
}

pub async fn run(config: Config) -> Result<()> {
    // If no project_id is configured, run in inert mode: MCP server starts so the
    // agent can respond to tool calls, but no DB is opened, no embedder loaded. All
    // tool calls return a helpful "no config" message that the AI can surface to the user.
    if config.project_id.is_none() {
        return serve_no_config(config).await;
    }

    // Init file logger first — all subsequent mlog! calls mirror to this file.
    let log_path = config.db_path()
        .parent()
        .map(|p| p.join("tyto-serve.log"))
        .unwrap_or_else(|| std::path::PathBuf::from("tyto-serve.log"));
    crate::log::init(&log_path);
    crate::log::init_tracing_to_file();

    mlog!("=== tyto serve starting ===");
    mlog!("version: {}", env!("CARGO_PKG_VERSION"));
    mlog!("platform: {}/{}", std::env::consts::OS, std::env::consts::ARCH);
    mlog!("log file: {}", log_path.display());
    if let Ok(cwd) = std::env::current_dir() {
        mlog!("cwd: {}", cwd.display());
    }
    mlog!("project root: {}", config.project_root.display());
    mlog!("memory: {:?}", config.memory.storage);

    let pid = project_id::resolve(&config.project_root, config.project_id.as_deref());

    // Set up crash log and panic hook before any fallible work.
    // crash.log is read by `tyto inject` on next session start and surfaced to the AI.
    let crash_log = config.db_path()
        .parent()
        .map(|p| p.join("crash.log"))
        .unwrap_or_else(|| std::path::PathBuf::from("crash.log"));
    if let Some(parent) = crash_log.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::remove_file(&crash_log); // clear stale crash from previous run
    let crash_log_hook = crash_log.clone();
    std::panic::set_hook(Box::new(move |info| {
        mlog!("PANIC: {info}");
        let msg = format!("[{}] PANIC: {info}\n", chrono::Utc::now().format("%H:%M:%S"));
        let _ = std::fs::write(&crash_log_hook, &msg);
    }));

    let result = serve_inner(config, pid).await;
    if let Err(ref e) = result {
        mlog!("ERROR: {e:#}");
        let msg = format!("[{}] ERROR: {e:#}\n", chrono::Utc::now().format("%H:%M:%S"));
        // Append rather than overwrite: the panic hook may have already written to
        // crash.log and we don't want to erase it.
        use std::io::Write as _;
        let _ = std::fs::OpenOptions::new()
            .create(true).append(true)
            .open(&crash_log)
            .and_then(|mut f| writeln!(f, "{}", msg.trim()));
    }
    mlog!("=== tyto serve exiting ===");
    result
}

/// Start the MCP server in inert mode when no `.tyto.toml` with a `project_id`
/// is found. The server is fully reachable so the AI can call tools, but every
/// tool call returns a "no config" message with setup instructions.
async fn serve_no_config(config: Config) -> Result<()> {
    let suggested = project_id::infer(&config.project_root);
    let no_config_msg = format!(
        "tyto has loaded, but there is no `.tyto.toml` configuration file for this \
         project, so memories will not be stored or retrieved this session.\n\
         If you would like to enable memories, please ask me to create a `.tyto.toml` \
         file. Suggested configuration based on this project:\n\n\
         ```toml\n\
         project_id = \"{suggested}\"\n\
         ```"
    );
    eprintln!("tyto: running in inert mode (no .tyto.toml with project_id found)");

    // Put the server immediately into a permanent Failed state. Every tool call
    // will return the no-config message via try_ready(). The watch sender is kept
    // alive for the duration so the state never changes.
    let (_db_tx, db_rx) = tokio::sync::watch::channel(DbState::Failed(no_config_msg));
    let (_idx_tx, idx_rx) = tokio::sync::watch::channel(index::IndexState::Disabled);

    let server = TytoServer {
        db: db_rx,
        idx: idx_rx,
        write_lock: store::new_write_lock(),
        session_id: Uuid::new_v4().to_string(),
        project_id: suggested,
        config: Arc::new(config),
        tool_router: TytoServer::tool_router(),
        prompt_router: TytoServer::prompt_router(),
    };

    let service = server.serve(stdio()).await?;

    // Wait for client disconnect or shutdown. Deliberately no wait_db_failed arm:
    // the Failed state here is permanent and intentional, not an error condition.
    tokio::select! {
        result = service.waiting() => result.map(|_| ()).map_err(Into::into),
        _ = shutdown_signal() => Ok(()),
    }
}

async fn serve_inner(config: Config, project_id: String) -> Result<()> {
    // Set up watch channels for memory DB and code index.
    let (db_tx, db_rx) = tokio::sync::watch::channel(DbState::Syncing);
    let (idx_tx, idx_rx) = tokio::sync::watch::channel(index::IndexState::Opening);
    let mut db_rx_monitor = db_rx.clone();

    // Acquire an exclusive lock on serve.lock for this process's entire lifetime.
    // The OS releases it automatically on any exit (clean, crash, or SIGKILL), so
    // inject can use a non-blocking lock attempt to detect whether we are running.
    let lock_file_path = config.serve_lock_path();
    let ready_file = config.serve_ready_path();
    if let Some(parent) = lock_file_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    
    let lock_file = std::fs::OpenOptions::new()
        .write(true).create(true).truncate(false)
        .open(&lock_file_path)
        .map_err(|e| anyhow::anyhow!("Failed to open serve.lock: {e}"))?;

    let session_id = Uuid::new_v4().to_string();
    mlog!("tyto: session {session_id}, project \"{project_id}\"");

    let server = TytoServer {
        db: db_rx,
        idx: idx_rx,
        write_lock: store::new_write_lock(),
        session_id,
        project_id: project_id.clone(),
        config: Arc::new(config.clone()),
        tool_router: TytoServer::tool_router(),
        prompt_router: TytoServer::prompt_router(),
    };

    // Spawn background leader election and initialization task.
    // This task polls the lock every second until it becomes the primary.
    //
    // GOTCHA: Synced replicas do not support multi-process concurrency in this version.
    // We use a continuous polling leader-election strategy to ensure exactly one process 
    // manages the database, while allowing secondary processes to start and proxy tool calls.
    let config_for_bg = config.clone();
    let db_tx_clone = db_tx.clone();
    let idx_tx_clone = idx_tx.clone();
    let server_for_socket = server.clone();
    let ready_file_bg = ready_file.clone();
    let is_primary = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let is_primary_bg = Arc::clone(&is_primary);
    
    tokio::spawn(async move {
        loop {
            if lock_file.try_lock().is_ok() {
                mlog!("tyto: acquired serve.lock (primary)");
                is_primary_bg.store(true, std::sync::atomic::Ordering::SeqCst);
                break;
            }
            // While waiting for the lock, we stay in the initial Syncing/Opening states.
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        }

        // Start local IPC socket so `tyto request` can reach the warm embedder/DB.
        spawn_socket_listener(server_for_socket.clone(), &config_for_bg);

        match init_db_and_embedder(&config_for_bg, Arc::clone(&server_for_socket.write_lock)).await {
            Ok(ready) => {
                let embedder_for_idx = Arc::clone(&ready.embedder);
                let _ = db_tx_clone.send(DbState::Ready(Arc::new(ready)));
                let _ = std::fs::write(&ready_file_bg, "");
                mlog!("tyto: database ready");

                // The primary instance manages the code indexer.
                use crate::config::StorageMode;
                let index_enabled = !matches!(config_for_bg.index.storage.mode, StorageMode::Disabled);
                if index_enabled {
                    let db_path = config_for_bg.index_db_path();
                    let project_root = config_for_bg.project_root.clone();
                    let git_history = config_for_bg.index.git_history;
                    let extra_excludes = config_for_bg.index.exclude.clone();
                    tokio::spawn(async move {
                        mlog!("tyto: opening code index at {}", db_path.display());
                        match index::open(&db_path, project_root.clone(), git_history, Arc::clone(&embedder_for_idx)).await {
                            Ok(idx_ready) => {
                                let idx_ready = Arc::new(idx_ready);
                                let _ = idx_tx_clone.send(index::IndexState::Ready(Arc::clone(&idx_ready)));
                                mlog!("tyto: code index ready, starting background indexing...");
                                // Indexer and watcher get dedicated connections — must NOT share
                                // idx_ready.conn (the search connection) due to turso's per-Connection
                                // ConcurrentGuard which allows only one concurrent operation per instance.
                                let indexer_conn = match idx_ready.new_conn() {
                                    Ok(c) => c,
                                    Err(e) => { mlog!("tyto: failed to create indexer connection: {e:#}"); return; }
                                };
                                let emb = Arc::clone(&embedder_for_idx);
                                match index::indexer::run(project_root.clone(), indexer_conn, emb.clone(), git_history, extra_excludes.clone()).await {
                                    Ok(r) => mlog!(
                                        "tyto: code index complete — {} files, {} chunks",
                                        r.files_indexed, r.chunks_stored
                                    ),
                                    Err(e) => mlog!("tyto: code index run failed: {e:#}"),
                                }
                                let watcher_conn = match idx_ready.new_conn() {
                                    Ok(c) => c,
                                    Err(e) => { mlog!("tyto: failed to create watcher connection: {e:#}"); return; }
                                };
                                let watcher_lock = config_for_bg.index_watcher_lock_path();
                                index::watcher::start(
                                    watcher_lock,
                                    project_root,
                                    watcher_conn,
                                    emb,
                                    git_history,
                                    extra_excludes,
                                );
                            }
                            Err(e) => {
                                mlog!("tyto: code index open failed: {e:#}");
                                let _ = idx_tx_clone.send(index::IndexState::Failed(format!("{e:#}")));
                            }
                        }
                    });
                } else {
                    let _ = idx_tx_clone.send(index::IndexState::Disabled);
                    mlog!("tyto: code indexing disabled in config");
                }
            }
            Err(e) => {
                mlog!("tyto: database init failed: {e:#}");
                let _ = db_tx_clone.send(DbState::Failed(format!("{e:#}")));
                let _ = idx_tx_clone.send(index::IndexState::Disabled);
            }
        }
        // Hand serve.lock ownership to the OS. The fd is closed at true process exit,
        // not during Tokio shutdown. This ensures serve.lock is released only AFTER
        // all other file handles (memory.db, index.db) are also released — eliminating
        // the handover race where a new process acquires serve.lock while the old one
        // still has DB files open during Tokio task cancellation.
        std::mem::forget(lock_file);
    });

    // Start MCP transport immediately — Claude Code sees us as connected right away.
    // Tool calls during the sync window return a "syncing" message instead of blocking.
    let service = server.serve(stdio()).await?;
    mlog!("tyto: ready (waiting for database lock)");

    // Wait for client disconnect, shutdown signal, or a permanent DB init failure.
    // DB init failure is re-raised as an error so run() writes it to crash.log.
    let serve_result: Result<()> = tokio::select! {
        result = service.waiting() => result.map(|_| ()).map_err(Into::into),
        _ = shutdown_signal() => {
            mlog!("tyto: shutting down");
            Ok(())
        }
        _ = wait_db_failed(&mut db_rx_monitor) => {
            let msg = match &*db_rx_monitor.borrow() {
                DbState::Failed(msg) => msg.clone(),
                // Sender dropped without sending Failed — likely a panic in the init task.
                // The panic hook will have written a more detailed message to crash.log.
                _ => "background init task exited unexpectedly (possible panic — check crash.log)".to_string(),
            };
            Err(anyhow::anyhow!("Database init failed: {msg}"))
        }
    };

    // Primary cleans up state files.
    if is_primary.load(std::sync::atomic::Ordering::SeqCst) {
        let _ = std::fs::remove_file(&ready_file);
        #[cfg(unix)]
        let _ = std::fs::remove_file(config.serve_socket_path());
    }
    serve_result
}

/// Bind a local IPC listener and accept MCP connections from `tyto request`.
/// Each accepted connection gets its own cloned TytoServer and a full MCP session.
/// Errors are logged and the listener exits; serve continues unaffected.
fn spawn_socket_listener(server: TytoServer, config: &Config) {
    #[cfg(unix)]
    {
        use tokio::net::UnixListener;
        let socket_path = config.serve_socket_path();
        // Remove stale socket from a previous crash before binding.
        let _ = std::fs::remove_file(&socket_path);
        match UnixListener::bind(&socket_path) {
            Ok(listener) => {
                tokio::spawn(async move {
                    loop {
                        match listener.accept().await {
                            Ok((stream, _)) => {
                                let srv = server.clone();
                                tokio::spawn(async move {
                                    match srv.serve(stream).await {
                                        Ok(service) => { let _ = service.waiting().await; }
                                        Err(e) => mlog!("tyto: socket client error: {e}"),
                                    }
                                });
                            }
                            Err(e) => {
                                mlog!("tyto: socket accept error: {e}");
                                break;
                            }
                        }
                    }
                });
            }
            Err(e) => mlog!("tyto: failed to bind Unix socket: {e}"),
        }
    }

    #[cfg(windows)]
    {
        use tokio::net::windows::named_pipe::ServerOptions;
        let pipe_name = config.serve_pipe_name();
        tokio::spawn(async move {
            loop {
                let pipe = match ServerOptions::new().first_pipe_instance(false).create(&pipe_name) {
                    Ok(p) => p,
                    Err(e) => { mlog!("tyto: named pipe create error: {e}"); break; }
                };
                if pipe.connect().await.is_err() {
                    continue;
                }
                let srv = server.clone();
                tokio::spawn(async move {
                    match srv.serve(pipe).await {
                        Ok(service) => { let _ = service.waiting().await; }
                        Err(e) => mlog!("tyto: pipe client error: {e}"),
                    }
                });
            }
        });
    }

    #[cfg(not(any(unix, windows)))]
    let _ = (server, config);
}

/// Resolves once the DB state transitions to [`DbState::Failed`].
/// Resolves once the DB state transitions to [`DbState::Failed`], or if the
/// background init task exits before reaching [`DbState::Ready`] (panic case).
///
/// Does NOT resolve when init succeeds: after a successful init the sender is
/// dropped with state=Ready, and triggering the failure arm then would be wrong.
async fn wait_db_failed(rx: &mut tokio::sync::watch::Receiver<DbState>) {
    loop {
        match &*rx.borrow() {
            DbState::Failed(_) => return,
            // Init succeeded — park forever so the select never picks this arm.
            DbState::Ready(_) => std::future::pending::<()>().await,
            DbState::Syncing => {}
        }
        if rx.changed().await.is_err() {
            // Sender dropped. If state is Ready, init succeeded — park forever.
            // If state is still Syncing, the task panicked before completing — trigger.
            if matches!(&*rx.borrow(), DbState::Ready(_)) {
                std::future::pending::<()>().await;
            }
            return;
        }
    }
}

async fn init_db_and_embedder(config: &Config, write_lock: WriteLock) -> Result<DbReady> {
    let t_init = std::time::Instant::now();
    mlog!("tyto: opening database...");
    let db = Db::open(config).await?;
    let conn = Arc::new(db.conn);
    let handle = db.handle;
    let temp_dir = db.temp_dir;

    // In replica mode, compact any stale WAL from the previous session before
    // running migrations. A dirty WAL can hide tables that exist in Turso,
    // causing spurious "no such table" errors on reads that immediately follow
    // the sync. Checkpointing merges the WAL into the main db file so the
    // post-sync snapshot is the authoritative starting state.
    //
    // GOTCHA: We use sync_db.checkpoint() instead of 'PRAGMA wal_checkpoint' because 
    // Turso's .execute() fails on pragmas that return rows.
    if let db::AnyDb::Synced(ref sync_db) = handle {
        let t_cp = std::time::Instant::now();
        match sync_db.checkpoint().await {
            Ok(_) => tracing::debug!(elapsed_ms = t_cp.elapsed().as_millis(), "WAL checkpoint"),
            Err(e) => mlog!("tyto: WAL checkpoint failed (non-fatal): {e:#}"),
        }
    }

    mlog!("tyto: running migrations...");
    let t_mig = std::time::Instant::now();
    let mig_result = migrations::run(&conn).await;
    tracing::debug!(elapsed_ms = t_mig.elapsed().as_millis(), "migrations");

    // In replica mode, a stale WAL from a previous session can overlay the main
    // db file after sync, causing "no such table" for tables that exist in Turso.
    // Purge and re-open once to force a clean full re-sync.
    let (conn, handle, temp_dir) = if let Err(ref e) = mig_result {
        let is_replica = matches!(
            config.memory.storage.remote_mode,
            RemoteMode::Replica
        );
        if is_replica {
            mlog!("tyto: CRITICAL: migration failed in replica mode ({e:#}). PURGING local replica to force clean resync...");
            drop(conn);
            db::purge_replica_files(&config.db_path())?;
            let db = Db::open(config).await?;
            let conn = Arc::new(db.conn);
            mlog!("tyto: running migrations (retry)...");
            migrations::run(&conn).await?;
            (conn, db.handle, db.temp_dir)
        } else {
            return Err(mig_result.unwrap_err());
        }
    } else {
        (conn, handle, temp_dir)
    };

    mlog!("tyto: loading embedding model (first run will download ~22MB)...");
    let t_model = std::time::Instant::now();
    let embedder = Arc::new(Mutex::new(Embedder::load()?));
    tracing::debug!(elapsed_ms = t_model.elapsed().as_millis(), "embedder load");
    tracing::debug!(elapsed_ms = t_init.elapsed().as_millis(), "init_db_and_embedder total");

    // Start manual background sync for replicas.
    if let db::AnyDb::Synced(ref sync_db) = handle {
        let sync_db = sync_db.clone();
        let write_lock_clone = Arc::clone(&write_lock);
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(5));
            loop {
                interval.tick().await;
                // Periodic push/pull/checkpoint to stay in sync and keep WAL small.
                //
                // GOTCHA: We must use the write_lock to ensure sync doesn't overlap with local 
                // mutations. Even with a single process, Limbo's sync engine can panic 
                // (e.g., "parent should have a rightmost pointer") if remote frames are applied 
                // while the B-Tree is being modified locally.
                let _guard = write_lock_clone.lock().await;
                if let Err(e) = sync_db.push().await {
                    tracing::error!(error = %e, "replica push failed");
                }
                if let Err(e) = sync_db.pull().await {
                    tracing::error!(error = %e, "replica pull failed");
                }
                if let Err(e) = sync_db.checkpoint().await {
                    tracing::error!(error = %e, "replica checkpoint failed");
                }
            }
        });
    }

    // Re-embed any memories whose vectors were generated by a different model.
    // Runs in background; inject (BM25-only) and search (model-filtered) degrade
    // gracefully while this is in progress.
    tokio::spawn(reembed_stale(Arc::clone(&conn), Arc::clone(&embedder)));

    Ok(DbReady { conn, embedder, write_lock, handle, temp_dir })
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
