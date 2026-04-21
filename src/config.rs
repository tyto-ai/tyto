use anyhow::{Context, Result};
use figment::{
    providers::{Env, Format, Toml},
    Figment,
};
use serde::Deserialize;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Deserialize, Default, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum StorageMode {
    /// Stored in platform data dir, keyed by project path. Zero config required.
    #[default]
    Managed,
    /// Stored at `local_path` (relative to project root if not absolute).
    Local,
    /// libsql remote backend. `remote_mode = direct` (default) or `replica`.
    Remote,
    /// Subsystem entirely disabled. No DB opened. No tools available.
    Disabled,
}

#[derive(Debug, Clone, Deserialize, Default, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum RemoteMode {
    #[default]
    Direct,
    Replica,
}

#[derive(Clone, Deserialize, Default)]
pub struct StorageConfig {
    #[serde(default)]
    pub mode: StorageMode,
    /// Override the managed-mode base directory.
    pub managed_path: Option<PathBuf>,
    /// Path for local mode (relative to project root if not absolute).
    pub local_path: Option<PathBuf>,
    /// Only relevant when mode = remote. Defaults to direct.
    #[serde(default)]
    pub remote_mode: RemoteMode,
    pub remote_url: Option<String>,
    pub remote_auth_token: Option<String>,
}

impl std::fmt::Debug for StorageConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StorageConfig")
            .field("mode", &self.mode)
            .field("remote_mode", &self.remote_mode)
            .field("remote_url", &self.remote_url)
            .field("remote_auth_token", &self.remote_auth_token.as_deref().map(|_| "[REDACTED]"))
            .finish()
    }
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct MemoryConfig {
    #[serde(flatten)]
    pub storage: StorageConfig,
}

#[derive(Debug, Clone, Deserialize)]
pub struct IndexConfig {
    #[serde(flatten)]
    pub storage: StorageConfig,
    /// Include git commit history for churn analysis.
    #[serde(default = "default_true")]
    pub git_history: bool,
    /// Additional glob patterns to exclude from indexing (merged with built-in excludes).
    #[serde(default)]
    pub exclude: Vec<String>,
}

fn default_true() -> bool { true }

impl Default for IndexConfig {
    fn default() -> Self {
        Self { storage: StorageConfig::default(), git_history: true, exclude: vec![] }
    }
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct Config {
    /// Project identifier. Affects both memory query scoping and managed path keying.
    pub project_id: Option<String>,
    #[serde(default)]
    pub memory: MemoryConfig,
    #[serde(default)]
    pub index: IndexConfig,
    /// Path the project config (`.tyto.toml`) was loaded from, if any.
    /// Used only for `toml_edit` writes -- not for path derivation.
    #[serde(skip)]
    pub source_path: Option<PathBuf>,
    /// Root directory of the project. All paths are derived from this.
    /// Determined at load time: `.tyto.toml` parent -> nearest `.git/` ancestor -> CWD.
    #[serde(skip)]
    pub project_root: PathBuf,
}

impl Config {
    /// Load config with layered precedence: defaults < file < env vars.
    ///
    /// File resolution: walk up from `start_dir` looking for `.tyto.toml`,
    /// then fall back to the global config at `$XDG_CONFIG_HOME/tyto/config.toml`.
    ///
    /// Env var mapping: `TYTO__<SECTION>__<FIELD>` overrides `section.field`.
    /// Double underscore separates nesting levels; single underscore is part of the name.
    ///   TYTO__MEMORY__MODE              -> memory.mode        (managed|local|remote|disabled)
    ///   TYTO__MEMORY__REMOTE_MODE       -> memory.remote_mode (direct|replica)
    ///   TYTO__MEMORY__REMOTE_URL        -> memory.remote_url
    ///   TYTO__MEMORY__REMOTE_AUTH_TOKEN -> memory.remote_auth_token
    ///   TYTO__PROJECT_ID                -> project_id
    pub fn load(start_dir: &Path) -> Result<Self> {
        let project_config = find_project_config(start_dir);
        let global_config = global_config_path().filter(|p| p.exists());

        // Layer: global < project < env vars.
        // Both files are merged so a global Turso backend can be set once and
        // individual projects only need to override project_id.
        let mut fig = Figment::new();
        if let Some(ref path) = global_config {
            fig = fig.merge(Toml::file(path));
        }
        if let Some(ref path) = project_config {
            fig = fig.merge(Toml::file(path));
        }
        // Double underscore is the figment-idiomatic level separator.
        // TYTO__MEMORY__REMOTE_AUTH_TOKEN -> memory.remote_auth_token
        // TYTO__PROJECT_ID               -> project_id
        fig = fig.merge(Env::prefixed("TYTO__").split("__"));

        let mut cfg: Config = fig.extract().context("Failed to load configuration")?;
        cfg.source_path = project_config;
        cfg.project_root = find_project_root(start_dir, cfg.source_path.as_deref());
        Ok(cfg)
    }

    /// Resolved DB path for the current memory storage mode.
    ///
    /// - Managed: `{data_dir}/tyto/managed/{encoded_path}/memory.db`
    /// - Local:   `{local_path}` (relative to project root if not absolute)
    /// - Remote/Replica: managed path (or local_path if set)
    /// - Remote/Direct:  managed path (parent used for serve.lock/ready/crash.log)
    pub fn db_path(&self) -> PathBuf {
        let s = &self.memory.storage;
        match s.mode {
            StorageMode::Managed | StorageMode::Disabled => {
                self.managed_base(s).join(encode_project_path(&self.project_root)).join("memory.db")
            }
            StorageMode::Local => self.resolve_local_path(s, ".tyto/memory.db"),
            StorageMode::Remote => match s.remote_mode {
                RemoteMode::Replica => {
                    if s.local_path.is_some() {
                        self.resolve_local_path(s, ".tyto/memory.replica.db")
                    } else {
                        self.managed_base(s)
                            .join(encode_project_path(&self.project_root))
                            .join("memory.replica.db")
                    }
                }
                RemoteMode::Direct => {
                    // No real local DB; parent dir used for serve.lock/ready/crash.log.
                    self.managed_base(s)
                        .join(encode_project_path(&self.project_root))
                        .join("memory.remote.db")
                }
            },
        }
    }

    /// Path to the lock file held exclusively by `tyto serve` for its entire lifetime.
    pub fn serve_lock_path(&self) -> PathBuf {
        self.db_path()
            .parent()
            .map(|p| p.join("serve.lock"))
            .unwrap_or_else(|| PathBuf::from("serve.lock"))
    }

    /// Path to the ready file written by `tyto serve` once the DB and embedder are loaded.
    pub fn serve_ready_path(&self) -> PathBuf {
        self.db_path()
            .parent()
            .map(|p| p.join("serve.ready"))
            .unwrap_or_else(|| PathBuf::from("serve.ready"))
    }

    /// Always returns the effective local DB path regardless of remote mode.
    /// Used by `remote enable` as the source/seed database.
    pub fn local_db_path(&self) -> PathBuf {
        let s = &self.memory.storage;
        match s.mode {
            StorageMode::Local => self.resolve_local_path(s, ".tyto/memory.db"),
            _ => {
                self.managed_base(s)
                    .join(encode_project_path(&self.project_root))
                    .join("memory.db")
            }
        }
    }

    /// Path to the code intelligence index database.
    ///
    /// - Managed: `{data_dir}/tyto/managed/{encoded_path}/index.db`
    /// - Local:   `{local_path}` (relative to project root if not absolute)
    pub fn index_db_path(&self) -> PathBuf {
        let s = &self.index.storage;
        match s.mode {
            StorageMode::Managed | StorageMode::Disabled | StorageMode::Remote => {
                self.managed_base(s).join(encode_project_path(&self.project_root)).join("index.db")
            }
            StorageMode::Local => self.resolve_local_path(s, ".tyto/index.db"),
        }
    }

    fn managed_base(&self, s: &StorageConfig) -> PathBuf {
        s.managed_path.clone().unwrap_or_else(|| {
            dirs::data_dir()
                .unwrap_or_else(|| dirs::home_dir().unwrap_or_default().join(".local").join("share"))
                .join("tyto")
                .join("managed")
        })
    }

    fn resolve_local_path(&self, s: &StorageConfig, default: &str) -> PathBuf {
        let p = s.local_path.as_deref().unwrap_or(Path::new(default));
        if p.is_absolute() { p.to_path_buf() } else { self.project_root.join(p) }
    }
}

/// Encode an absolute project path into a flat directory name.
/// Mirrors Claude Code's path-encoding convention: replace `/` with `-`.
/// `/home/user/myproject` -> `-home-user-myproject`
fn encode_project_path(path: &Path) -> String {
    path.to_string_lossy().replace('/', "-")
}

fn find_project_config(start: &Path) -> Option<PathBuf> {
    let mut dir = start.to_path_buf();
    loop {
        let candidate = dir.join(".tyto.toml");
        if candidate.exists() {
            return Some(candidate);
        }
        if !dir.pop() {
            return None;
        }
    }
}

/// Determine the project root directory for anchoring paths.
///
/// Walk-up chain:
/// 1. Parent of the project `.tyto.toml` (if found)
/// 2. Nearest ancestor directory containing `.git/`
/// 3. `start_dir` as final fallback (handles global-config-only and no-git cases)
fn find_project_root(start_dir: &Path, project_config: Option<&Path>) -> PathBuf {
    if let Some(parent) = project_config.and_then(|p| p.parent()) {
        return parent.to_path_buf();
    }
    let mut dir = start_dir.to_path_buf();
    loop {
        if dir.join(".git").exists() {
            return dir;
        }
        if !dir.pop() {
            break;
        }
    }
    start_dir.to_path_buf()
}

fn global_config_path() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("tyto").join("config.toml"))
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn db_path_managed_mode() {
        let cfg = Config {
            project_root: PathBuf::from("/some/project"),
            ..Default::default()
        };
        let path = cfg.db_path();
        assert!(path.ends_with("memory.db"));
        assert!(path.to_string_lossy().contains("tyto"));
        assert!(path.to_string_lossy().contains("-some-project"));
    }

    #[test]
    fn db_path_local_mode() {
        let cfg = Config {
            project_root: PathBuf::from("/some/project"),
            memory: MemoryConfig {
                storage: StorageConfig {
                    mode: StorageMode::Local,
                    ..Default::default()
                },
            },
            ..Default::default()
        };
        assert_eq!(cfg.db_path(), PathBuf::from("/some/project/.tyto/memory.db"));
    }

    #[test]
    fn db_path_local_mode_explicit_path() {
        let cfg = Config {
            project_root: PathBuf::from("/some/project"),
            memory: MemoryConfig {
                storage: StorageConfig {
                    mode: StorageMode::Local,
                    local_path: Some(PathBuf::from("custom/memory.db")),
                    ..Default::default()
                },
            },
            ..Default::default()
        };
        assert_eq!(cfg.db_path(), PathBuf::from("/some/project/custom/memory.db"));
    }

    #[test]
    fn local_db_path_managed_returns_managed_path() {
        let cfg = Config {
            project_root: PathBuf::from("/some/project"),
            ..Default::default()
        };
        let path = cfg.local_db_path();
        assert!(path.ends_with("memory.db"));
        assert!(path.to_string_lossy().contains("tyto"));
    }

    #[test]
    fn find_project_root_uses_project_config_parent() {
        let root = find_project_root(
            Path::new("/some/subdir"),
            Some(Path::new("/some/project/.tyto.toml")),
        );
        assert_eq!(root, PathBuf::from("/some/project"));
    }

    #[test]
    fn find_project_root_falls_back_to_start_dir() {
        let root = find_project_root(Path::new("/tmp/norepo"), None);
        assert_eq!(root, PathBuf::from("/tmp/norepo"));
    }

    #[test]
    fn encode_project_path_replaces_slashes() {
        assert_eq!(encode_project_path(Path::new("/home/user/project")), "-home-user-project");
        assert_eq!(encode_project_path(Path::new("/some/project")), "-some-project");
    }
}
