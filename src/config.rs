use anyhow::{Context, Result};
use figment::{
    providers::{Env, Format, Toml},
    Figment,
};
use serde::Deserialize;
use std::env;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Deserialize, Default, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum BackendMode {
    #[default]
    Local,
    Remote,
}

#[derive(Debug, Clone, Deserialize, Default, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum RemoteMode {
    #[default]
    Direct,
    Replica,
}

#[derive(Clone, Deserialize, Default)]
pub struct BackendConfig {
    #[serde(default)]
    pub mode: BackendMode,
    /// Only relevant when mode = remote. Defaults to direct.
    #[serde(default)]
    pub remote_mode: RemoteMode,
    pub remote_url: Option<String>,
    pub auth_token: Option<String>,
}

impl std::fmt::Debug for BackendConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BackendConfig")
            .field("mode", &self.mode)
            .field("remote_mode", &self.remote_mode)
            .field("remote_url", &self.remote_url)
            .field("auth_token", &self.auth_token.as_deref().map(|_| "[REDACTED]"))
            .finish()
    }
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct MemoryConfig {
    pub project_id: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct Config {
    #[serde(default)]
    pub backend: BackendConfig,
    #[serde(default)]
    pub memory: MemoryConfig,
    /// Path this config was loaded from, if any
    #[serde(skip)]
    pub source_path: Option<PathBuf>,
}

impl Config {
    /// Load config with layered precedence: defaults < file < env vars.
    ///
    /// File resolution: walk up from `start_dir` looking for `.memso.toml`,
    /// then fall back to the global config at `$XDG_CONFIG_HOME/memso/config.toml`.
    ///
    /// Env var mapping: `MEMSO_<SECTION>__<FIELD>` overrides `section.field`.
    /// Double underscore separates nesting levels; single underscore is part of the name.
    ///   MEMSO_BACKEND__MODE        -> backend.mode        (local|remote)
    ///   MEMSO_BACKEND__REMOTE_MODE -> backend.remote_mode (direct|replica)
    ///   MEMSO_BACKEND__REMOTE_URL  -> backend.remote_url
    ///   MEMSO_BACKEND__AUTH_TOKEN  -> backend.auth_token
    ///   MEMSO_MEMORY__PROJECT_ID   -> memory.project_id
    pub fn load(start_dir: &Path) -> Result<Self> {
        let config_path = find_project_config(start_dir)
            .or_else(|| global_config_path().filter(|p| p.exists()));

        let mut fig = Figment::new();
        if let Some(ref path) = config_path {
            fig = fig.merge(Toml::file(path));
        }
        // Double underscore is the figment-idiomatic level separator.
        // MEMSO_BACKEND__AUTH_TOKEN -> backend.auth_token
        // MEMSO_MEMORY__PROJECT_ID  -> memory.project_id
        fig = fig.merge(Env::prefixed("MEMSO_").split("__"));

        let mut cfg: Config = fig.extract().context("Failed to load configuration")?;
        cfg.source_path = config_path;
        Ok(cfg)
    }

    /// Resolved DB path for the current backend mode.
    /// - Local mode:   `.memso/memory.db`
    /// - Replica mode: `.memso/memory.replica.db`
    ///
    /// Keeping distinct filenames means switching modes never overwrites or corrupts
    /// the other mode's data, and the local file serves as a natural backup after
    /// `memso remote enable` without any explicit rename step.
    pub fn db_path(&self) -> PathBuf {
        let base = self
            .source_path
            .as_ref()
            .and_then(|p| p.parent())
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
        let filename = match self.backend.mode {
            BackendMode::Local => "memory.db",
            BackendMode::Remote => match self.backend.remote_mode {
                RemoteMode::Replica => "memory.replica.db",
                // Direct mode has no local DB file; this path is used only to derive
                // the .memso/ directory for serve.lock, serve.ready, and crash.log.
                RemoteMode::Direct => "memory.remote.db",
            },
        };
        base.join(".memso").join(filename)
    }

    /// Path to the lock file held exclusively by `memso serve` for its entire lifetime.
    /// The OS releases the lock automatically on any exit (clean, crash, or SIGKILL),
    /// so there are no stale files. `memso inject` uses a non-blocking lock attempt
    /// to detect whether the server is currently running.
    pub fn serve_lock_path(&self) -> PathBuf {
        self.db_path()
            .parent()
            .map(|p| p.join("serve.lock"))
            .unwrap_or_else(|| PathBuf::from("serve.lock"))
    }

    /// Path to the ready file written by `memso serve` once the DB and embedder are loaded.
    /// Absent while syncing; present when tools are available.
    pub fn serve_ready_path(&self) -> PathBuf {
        self.db_path()
            .parent()
            .map(|p| p.join("serve.ready"))
            .unwrap_or_else(|| PathBuf::from("serve.ready"))
    }

    /// Always returns the local-mode DB path (`.memso/memory.db`), regardless of
    /// the current backend mode. Used by `remote enable` and `remote sync` as the
    /// source/seed database - it is the natural backup after switching to replica mode.
    pub fn local_db_path(&self) -> PathBuf {
        let base = self
            .source_path
            .as_ref()
            .and_then(|p| p.parent())
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
        base.join(".memso").join("memory.db")
    }
}

fn find_project_config(start: &Path) -> Option<PathBuf> {
    let mut dir = start.to_path_buf();
    loop {
        let candidate = dir.join(".memso.toml");
        if candidate.exists() {
            return Some(candidate);
        }
        if !dir.pop() {
            return None;
        }
    }
}

fn global_config_path() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("memso").join("config.toml"))
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn db_path_local_mode() {
        let mut cfg = Config::default();
        cfg.source_path = Some(PathBuf::from("/some/project/.memso.toml"));
        assert_eq!(cfg.db_path(), PathBuf::from("/some/project/.memso/memory.db"));
    }

    #[test]
    fn db_path_remote_direct_mode() {
        let mut cfg = Config::default();
        cfg.backend.mode = BackendMode::Remote;
        cfg.backend.remote_mode = RemoteMode::Direct;
        cfg.source_path = Some(PathBuf::from("/some/project/.memso.toml"));
        assert_eq!(cfg.db_path(), PathBuf::from("/some/project/.memso/memory.remote.db"));
    }

    #[test]
    fn db_path_remote_replica_mode() {
        let mut cfg = Config::default();
        cfg.backend.mode = BackendMode::Remote;
        cfg.backend.remote_mode = RemoteMode::Replica;
        cfg.source_path = Some(PathBuf::from("/some/project/.memso.toml"));
        assert_eq!(cfg.db_path(), PathBuf::from("/some/project/.memso/memory.replica.db"));
    }

    #[test]
    fn local_db_path_always_returns_memory_db() {
        let mut cfg = Config::default();
        cfg.backend.mode = BackendMode::Remote;
        cfg.backend.remote_mode = RemoteMode::Replica;
        cfg.source_path = Some(PathBuf::from("/some/project/.memso.toml"));
        assert_eq!(cfg.local_db_path(), PathBuf::from("/some/project/.memso/memory.db"));
    }

}
