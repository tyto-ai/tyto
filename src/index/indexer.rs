use anyhow::Result;
use chrono::Utc;
use libsql::params;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::Mutex;
use uuid::Uuid;

use crate::embed::{self, Embedder};
use super::git;
use super::parser::{self, Chunk, Lang};

/// Directories and patterns always skipped regardless of .gitignore.
const ALWAYS_EXCLUDE: &[&str] = &[
    ".git", "target", "node_modules", "dist", "build",
    "__pycache__", ".venv", "vendor", ".mypy_cache",
];

pub(crate) fn is_excluded(path: &Path) -> bool {
    for component in path.components() {
        let name = component.as_os_str().to_string_lossy();
        if ALWAYS_EXCLUDE.iter().any(|e| name == *e) {
            return true;
        }
        // Skip generated/lock files
        if name.ends_with(".min.js") || name.ends_with(".min.css")
            || name == "package-lock.json" || name == "yarn.lock"
            || name.ends_with(".lock") || name.ends_with(".sum")
        {
            return true;
        }
    }
    false
}

/// Result of a full index run.
#[derive(Debug, Default)]
pub struct IndexResult {
    pub files_scanned: usize,
    pub files_indexed: usize,
    pub chunks_stored: usize,
}

/// Run a full index of the project root.
/// Uses file content hashes to skip unchanged files.
/// Runs in a Tokio blocking task per file to avoid starving the async runtime.
pub async fn run(
    project_root: PathBuf,
    conn: Arc<libsql::Connection>,
    embedder: Arc<Mutex<Embedder>>,
    git_history: bool,
    extra_excludes: Vec<String>,
) -> Result<IndexResult> {
    let mut result = IndexResult::default();

    // Collect files to process (cheap, synchronous)
    crate::mlog!("index: scanning {}", project_root.display());
    let files: Vec<(PathBuf, Lang)> = {
        let root = project_root.clone();
        tokio::task::spawn_blocking(move || collect_files(&root, &extra_excludes)).await??
    };

    result.files_scanned = files.len();
    crate::mlog!("index: found {} indexable files", files.len());

    // Limit concurrency: parse/embed is CPU+memory intensive
    let cpu_count = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(2);
    let semaphore = Arc::new(tokio::sync::Semaphore::new(cpu_count.max(1)));
    let total = files.len();

    for (i, (file_path, lang)) in files.into_iter().enumerate() {
        let _permit = semaphore.clone().acquire_owned().await?;
        let conn = Arc::clone(&conn);
        let embedder = Arc::clone(&embedder);
        let project_root = project_root.clone();

        match index_file(&project_root, &file_path, &lang, &conn, &embedder, git_history).await {
            Ok(n) if n > 0 => {
                result.files_indexed += 1;
                result.chunks_stored += n;
                let rel = file_path.strip_prefix(&project_root).unwrap_or(&file_path);
                crate::mlog!("index: [{}/{}] {} ({} chunks)", i + 1, total, rel.display(), n);
            }
            Ok(_) => {} // unchanged — no log noise
            Err(e) => {
                let rel = file_path.strip_prefix(&project_root).unwrap_or(&file_path);
                crate::mlog!("index: [{}/{}] skipped {}: {e}", i + 1, total, rel.display());
            }
        }

        // Progress checkpoint every 50 files
        if (i + 1) % 50 == 0 {
            crate::mlog!(
                "index: progress {}/{} files checked, {} indexed so far",
                i + 1, total, result.files_indexed
            );
        }

        tokio::task::yield_now().await;
    }

    Ok(result)
}

/// Collect all indexable files under `root`, respecting .gitignore and built-in excludes.
fn collect_files(root: &Path, extra_excludes: &[String]) -> Result<Vec<(PathBuf, Lang)>> {
    let mut files = Vec::new();

    let walker = ignore::WalkBuilder::new(root)
        .hidden(false) // still respects .gitignore
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .build();

    for entry in walker {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        let path = entry.path();
        if !entry.file_type().is_some_and(|ft| ft.is_file()) {
            continue;
        }
        if is_excluded(path) {
            continue;
        }

        // Check user-configured extra excludes
        if !extra_excludes.is_empty() {
            let rel = path.strip_prefix(root).unwrap_or(path);
            let rel_str = rel.to_string_lossy();
            if extra_excludes.iter().any(|pat| glob_match(pat, &rel_str)) {
                continue;
            }
        }

        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
        if let Some(lang) = Lang::from_extension(ext) {
            files.push((path.to_path_buf(), lang));
        }
    }

    Ok(files)
}

/// Remove all index data for a deleted file.
pub(crate) async fn remove_file(conn: &libsql::Connection, project_root: &Path, file_path: &Path) -> Result<()> {
    let rel_path = file_path.strip_prefix(project_root)
        .unwrap_or(file_path)
        .to_string_lossy()
        .to_string();
    conn.execute("DELETE FROM index_chunks WHERE file_path = ?1", libsql::params![rel_path.clone()]).await?;
    conn.execute("DELETE FROM index_files WHERE path = ?1", libsql::params![rel_path]).await?;
    Ok(())
}

/// Index a single file. Returns number of new/updated chunks stored (0 = unchanged).
pub(crate) async fn index_file(
    project_root: &Path,
    file_path: &Path,
    lang: &Lang,
    conn: &libsql::Connection,
    embedder: &Arc<Mutex<Embedder>>,
    git_history: bool,
) -> Result<usize> {
    let source = tokio::fs::read_to_string(file_path).await?;
    let content_hash = sha256(&source);

    // Relative path for storage (deterministic across machines)
    let rel_path = file_path.strip_prefix(project_root)
        .unwrap_or(file_path)
        .to_string_lossy()
        .to_string();

    // Check if file hash has changed
    let stored_hash: Option<String> = {
        let mut rows = conn.query(
            "SELECT content_hash FROM index_files WHERE path = ?1",
            params![rel_path.clone()],
        ).await?;
        rows.next().await?.and_then(|r| r.get::<String>(0).ok())
    };

    if stored_hash.as_deref() == Some(&content_hash) {
        return Ok(0); // unchanged
    }

    // Delete old chunks for this file (CASCADE removes vectors and FTS entries)
    conn.execute(
        "DELETE FROM index_chunks WHERE file_path = ?1",
        params![rel_path.clone()],
    ).await?;

    // Parse in blocking thread (tree-sitter is synchronous, CPU-bound)
    let source_clone = source.clone();
    let rel_path_clone = rel_path.clone();
    let lang_name = lang.name().to_string();
    let chunks: Vec<Chunk> = {
        let lang_ext = match lang {
            Lang::Rust => "rs",
            Lang::Python => "py",
        };
        tokio::task::spawn_blocking(move || {
            let lang = Lang::from_extension(lang_ext).unwrap();
            parser::parse_file(&source_clone, &rel_path_clone, &lang)
        }).await?
    };

    if chunks.is_empty() {
        // File is indexed but has no extractable symbols (update hash to avoid re-scanning)
        upsert_file_hash(conn, &rel_path, &content_hash).await?;
        return Ok(0);
    }

    // Fetch git commits once and reuse for churn_count, commit storage, and chunk linking.
    let commits = if git_history {
        let root = project_root.to_path_buf();
        let rel = rel_path.clone();
        tokio::task::spawn_blocking(move || git::file_commits(&root, &rel, 10)).await?
    } else {
        vec![]
    };
    let churn_count = commits.len() as i64;

    // Store commit records for history search
    for commit in &commits {
        let _ = conn.execute(
            "INSERT OR IGNORE INTO index_commits (sha, message) VALUES (?1, ?2)",
            params![commit.sha.clone(), commit.message.clone()],
        ).await;
    }

    let now = Utc::now().to_rfc3339();
    let model_id = embed::model_id();
    let mut stored = 0usize;

    for chunk in &chunks {
        let embed_text = parser::build_embed_text(chunk, &rel_path);
        let chunk_hash = sha256(&embed_text);

        // Embed with the shared embedder
        let embedding = {
            let mut e = embedder.lock().await;
            e.embed(&embed_text).map_err(|e| anyhow::anyhow!("embed failed: {e}"))?
        };
        let blob = embed::floats_to_blob(&embedding);

        let chunk_id = Uuid::new_v4().to_string();
        conn.execute(
            "INSERT INTO index_chunks \
             (id, file_path, symbol_name, qualified_name, symbol_kind, signature, \
              doc_comment, body_preview, line_start, line_end, language, \
              churn_count, indexed_at, content_hash) \
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14)",
            params![
                chunk_id.clone(),
                rel_path.clone(),
                chunk.symbol_name.clone(),
                chunk.qualified_name.clone(),
                chunk.symbol_kind.clone(),
                chunk.signature.clone(),
                chunk.doc_comment.clone(),
                chunk.body_preview.clone(),
                chunk.line_start as i64,
                chunk.line_end as i64,
                lang_name.clone(),
                churn_count,
                now.clone(),
                chunk_hash,
            ],
        ).await?;

        conn.execute(
            "INSERT OR REPLACE INTO index_vectors (chunk_id, embed_model, embedding) \
             VALUES (?1, ?2, ?3)",
            params![chunk_id.clone(), model_id.clone(), blob],
        ).await?;

        // Link chunk to the already-fetched commits
        for commit in &commits {
            let _ = conn.execute(
                "INSERT OR IGNORE INTO index_chunk_commits (chunk_id, commit_sha) \
                 VALUES (?1, ?2)",
                params![chunk_id.clone(), commit.sha.clone()],
            ).await;
        }

        stored += 1;
    }

    upsert_file_hash(conn, &rel_path, &content_hash).await?;

    Ok(stored)
}

async fn upsert_file_hash(conn: &libsql::Connection, path: &str, hash: &str) -> Result<()> {
    let now = Utc::now().to_rfc3339();
    conn.execute(
        "INSERT OR REPLACE INTO index_files (path, content_hash, indexed_at) \
         VALUES (?1, ?2, ?3)",
        params![path.to_string(), hash.to_string(), now],
    ).await?;
    Ok(())
}

fn sha256(data: &str) -> String {
    let mut h = Sha256::new();
    h.update(data.as_bytes());
    hex::encode(h.finalize())
}

/// Simple glob match for exclude patterns. Handles `**` and `*` wildcards.
pub(crate) fn glob_match(pattern: &str, path: &str) -> bool {
    let pattern = pattern.replace('\\', "/");
    let path = path.replace('\\', "/");
    // "vendor/**" or "vendor/" → anything under that directory
    if let Some(prefix) = pattern.strip_suffix("/**").or_else(|| pattern.strip_suffix('/')) {
        return path.starts_with(&format!("{prefix}/")) || path == prefix;
    }
    // "**/foo" → any path component named foo
    if let Some(suffix) = pattern.strip_prefix("**/") {
        return path == suffix || path.ends_with(&format!("/{suffix}"));
    }
    // Exact match
    path == pattern
}

#[cfg(test)]
mod tests {
    use super::{glob_match, is_excluded};
    use std::path::Path;

    // --- is_excluded ---

    #[test]
    fn excluded_builtin_dirs() {
        assert!(is_excluded(Path::new("target/release/tyto")));
        assert!(is_excluded(Path::new("node_modules/react/index.js")));
        assert!(is_excluded(Path::new(".git/objects/pack/foo")));
        assert!(is_excluded(Path::new("__pycache__/foo.pyc")));
        assert!(is_excluded(Path::new(".venv/lib/site-packages/foo.py")));
        assert!(is_excluded(Path::new("vendor/github.com/foo/bar.go")));
    }

    #[test]
    fn excluded_generated_files() {
        assert!(is_excluded(Path::new("src/bundle.min.js")));
        assert!(is_excluded(Path::new("dist/style.min.css")));
        assert!(is_excluded(Path::new("Cargo.lock")));
        assert!(is_excluded(Path::new("go.sum")));
        assert!(is_excluded(Path::new("package-lock.json")));
        assert!(is_excluded(Path::new("yarn.lock")));
    }

    #[test]
    fn not_excluded_normal_source() {
        assert!(!is_excluded(Path::new("src/main.rs")));
        assert!(!is_excluded(Path::new("src/index/parser.rs")));
        assert!(!is_excluded(Path::new("tests/db.rs")));
        assert!(!is_excluded(Path::new("README.md")));
    }

    // --- glob_match ---

    #[test]
    fn glob_exact_match() {
        assert!(glob_match("src/main.rs", "src/main.rs"));
        assert!(!glob_match("src/main.rs", "src/lib.rs"));
    }

    #[test]
    fn glob_dir_prefix() {
        // "vendor/" or "vendor/**" matches anything inside vendor/
        assert!(glob_match("vendor/**", "vendor/foo/bar.rs"));
        assert!(glob_match("vendor/", "vendor/foo/bar.rs"));
        assert!(glob_match("vendor/**", "vendor"));
        assert!(!glob_match("vendor/**", "src/vendor/foo.rs"));
    }

    #[test]
    fn glob_double_star_prefix() {
        // "**/foo" matches any path ending in /foo or equal to foo
        assert!(glob_match("**/generated", "src/generated"));
        assert!(glob_match("**/generated", "generated"));
        assert!(!glob_match("**/generated", "src/generated/foo.rs"));
    }

    #[test]
    fn glob_windows_backslash_normalised() {
        // Windows paths with backslashes should match forward-slash patterns
        assert!(glob_match("vendor/**", "vendor\\foo\\bar.rs"));
        assert!(glob_match("src/main.rs", "src\\main.rs"));
    }

    #[test]
    fn glob_no_partial_prefix_match() {
        // "vendor/**" must not match a file whose path starts with "vendor" but
        // is a different directory (e.g. "vendor_utils/foo.rs")
        assert!(!glob_match("vendor/**", "vendor_utils/foo.rs"));
    }
}
