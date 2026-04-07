use anyhow::{Context, Result};
use serde::Deserialize;
use std::env;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum BackendMode {
    #[default]
    Local,
    Replica,
}

#[derive(Clone, Deserialize, Default)]
pub struct BackendConfig {
    #[serde(default)]
    pub mode: BackendMode,
    pub local_path: Option<String>,
    pub remote_url: Option<String>,
    /// Supports "${ENV_VAR}" substitution
    pub auth_token: Option<String>,
}

impl std::fmt::Debug for BackendConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BackendConfig")
            .field("mode", &self.mode)
            .field("local_path", &self.local_path)
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
    /// Load config by walking up from `start_dir`, then global, then defaults.
    pub fn load(start_dir: &Path) -> Result<Self> {
        if let Some(path) = find_project_config(start_dir) {
            let mut cfg = load_file(&path)
                .with_context(|| format!("Failed to read {}", path.display()))?;
            cfg.source_path = Some(path);
            cfg.resolve_env_vars();
            return Ok(cfg);
        }

        if let Some(path) = global_config_path()
            && path.exists()
        {
            let mut cfg = load_file(&path)
                .with_context(|| format!("Failed to read {}", path.display()))?;
            cfg.source_path = Some(path);
            cfg.resolve_env_vars();
            return Ok(cfg);
        }

        Ok(Config::default())
    }

    /// Resolve auth_token: expand "${VAR}" syntax, then fall back to REMOTE_AUTH_TOKEN env var.
    fn resolve_env_vars(&mut self) {
        self.resolve_env_vars_with(|k| env::var(k).ok());
    }

    fn resolve_env_vars_with(&mut self, env: impl Fn(&str) -> Option<String>) {
        if let Some(token) = &self.backend.auth_token
            && let Some(var) = token.strip_prefix("${").and_then(|s| s.strip_suffix('}'))
        {
            self.backend.auth_token = env(var);
        }
        if self.backend.auth_token.is_none() {
            self.backend.auth_token = env("REMOTE_AUTH_TOKEN");
        }
    }

    /// Resolved DB path for the current backend mode.
    /// - Local mode:   `.memso/memory.db`
    /// - Replica mode: `.memso/memory.replica.db`
    ///
    /// Keeping distinct filenames means switching modes never overwrites or corrupts
    /// the other mode's data, and the local file serves as a natural backup after
    /// `memso remote enable` without any explicit rename step.
    pub fn db_path(&self) -> PathBuf {
        if let Some(ref p) = self.backend.local_path {
            return PathBuf::from(p);
        }
        let base = self
            .source_path
            .as_ref()
            .and_then(|p| p.parent())
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
        let filename = match self.backend.mode {
            BackendMode::Local => "memory.db",
            BackendMode::Replica => "memory.replica.db",
        };
        base.join(".memso").join(filename)
    }

    /// Always returns the local-mode DB path (`.memso/memory.db`), regardless of
    /// the current backend mode. Used by `remote enable` and `remote sync` as the
    /// source/seed database - it is the natural backup after switching to replica mode.
    pub fn local_db_path(&self) -> PathBuf {
        if let Some(ref p) = self.backend.local_path {
            return PathBuf::from(p);
        }
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

fn load_file(path: &Path) -> Result<Config> {
    let text = std::fs::read_to_string(path)?;
    let cfg: Config = toml::from_str(&text)?;
    Ok(cfg)
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
    fn db_path_replica_mode() {
        let mut cfg = Config::default();
        cfg.backend.mode = BackendMode::Replica;
        cfg.source_path = Some(PathBuf::from("/some/project/.memso.toml"));
        assert_eq!(cfg.db_path(), PathBuf::from("/some/project/.memso/memory.replica.db"));
    }

    #[test]
    fn db_path_respects_override() {
        let mut cfg = Config::default();
        cfg.backend.local_path = Some("/custom/path.db".to_string());
        assert_eq!(cfg.db_path(), PathBuf::from("/custom/path.db"));
    }

    #[test]
    fn local_db_path_always_returns_memory_db() {
        let mut cfg = Config::default();
        cfg.backend.mode = BackendMode::Replica;
        cfg.source_path = Some(PathBuf::from("/some/project/.memso.toml"));
        assert_eq!(cfg.local_db_path(), PathBuf::from("/some/project/.memso/memory.db"));
    }

    #[test]
    fn resolve_env_vars_substitutes_var() {
        let env = std::collections::HashMap::from([
            ("_MEMSO_TEST_TOKEN", "supersecret"),
        ]);
        let mut cfg = Config::default();
        cfg.backend.auth_token = Some("${_MEMSO_TEST_TOKEN}".to_string());
        cfg.resolve_env_vars_with(|k| env.get(k).map(|s| s.to_string()));
        assert_eq!(cfg.backend.auth_token, Some("supersecret".to_string()));
    }

    #[test]
    fn resolve_env_vars_missing_var_becomes_none() {
        let mut cfg = Config::default();
        cfg.backend.auth_token = Some("${_MEMSO_NONEXISTENT_VAR}".to_string());
        cfg.resolve_env_vars_with(|_| None);
        assert_eq!(cfg.backend.auth_token, None);
    }

    #[test]
    fn resolve_env_vars_falls_back_to_remote_auth_token() {
        let env = std::collections::HashMap::from([
            ("REMOTE_AUTH_TOKEN", "fallback-token"),
        ]);
        let mut cfg = Config::default();
        cfg.resolve_env_vars_with(|k| env.get(k).map(|s| s.to_string()));
        assert_eq!(cfg.backend.auth_token, Some("fallback-token".to_string()));
    }
}
