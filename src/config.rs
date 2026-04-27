use anyhow::{Context, Result, bail};
use figment::{
    Figment,
    providers::{Env, Format, Toml},
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
            .field(
                "remote_auth_token",
                &self.remote_auth_token.as_deref().map(|_| "[REDACTED]"),
            )
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

fn default_true() -> bool {
    true
}

impl Default for IndexConfig {
    fn default() -> Self {
        Self {
            storage: StorageConfig::default(),
            git_history: true,
            exclude: vec![],
        }
    }
}

#[derive(Debug, Clone, Deserialize, Default)]
struct ProjectRootConfig {
    project_root: Option<PathBuf>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct Config {
    /// Project identifier. Affects both memory query scoping and managed path keying.
    pub project_id: Option<String>,
    #[serde(default)]
    pub memory: MemoryConfig,
    #[serde(default)]
    pub index: IndexConfig,
    /// Path the project config (`.coree.toml`) was loaded from, if any.
    /// Used only for `toml_edit` writes -- not for path derivation.
    #[serde(skip)]
    pub source_path: Option<PathBuf>,
    /// Root directory of the project. All paths are derived from this.
    /// Set by `Config::load()`: explicit `COREE__PROJECT_ROOT` env var or `project_root` in config,
    /// otherwise `.coree.toml` parent -> nearest `.git/` ancestor -> CWD.
    #[serde(default)]
    project_root: Option<PathBuf>,
}

impl Config {
    /// Returns the project root. Always `Some` after `Config::load()`.
    pub fn project_root(&self) -> &Path {
        self.project_root
            .as_deref()
            .expect("project_root not initialized; use Config::load()")
    }

    /// Load config with layered precedence: defaults < file < env vars.
    ///
    /// File resolution: walk up from `start_dir` looking for `.coree.toml`,
    /// then fall back to the global config at `$XDG_CONFIG_HOME/coree/config.toml`.
    ///
    /// Env var mapping: `COREE__<SECTION>__<FIELD>` overrides `section.field`.
    /// Double underscore separates nesting levels; single underscore is part of the name.
    ///   COREE__PROJECT_ROOT              -> project_root (overrides config file discovery start dir)
    ///   COREE__MEMORY__MODE              -> memory.mode        (managed|local|remote|disabled)
    ///   COREE__MEMORY__REMOTE_MODE       -> memory.remote_mode (direct|replica)
    ///   COREE__MEMORY__REMOTE_URL        -> memory.remote_url
    ///   COREE__MEMORY__REMOTE_AUTH_TOKEN -> memory.remote_auth_token
    ///   COREE__PROJECT_ID                -> project_id
    pub fn load(start_dir: &Path) -> Result<Self> {
        let global_config = global_config_path().filter(|p| p.exists());

        // First pass: extract project_root from global config + env vars so it can be
        // used as the start directory for .coree.toml discovery.
        let bootstrap_root = configured_project_root({
            let mut fig = Figment::new();
            if let Some(ref path) = global_config {
                fig = fig.merge(Toml::file(path));
            }
            fig.merge(Env::prefixed("COREE__").split("__"))
        })?;
        let effective_start = bootstrap_root.as_deref().unwrap_or(start_dir);
        let project_config = find_project_config(effective_start);

        // Second pass: full config load with the discovered project config file.
        let mut fig = Figment::new();
        if let Some(ref path) = global_config {
            fig = fig.merge(Toml::file(path));
        }
        if let Some(ref path) = project_config {
            fig = fig.merge(Toml::file(path));
        }
        fig = fig.merge(Env::prefixed("COREE__").split("__"));

        let mut cfg: Config = fig.extract().context("Failed to load configuration")?;
        cfg.source_path = project_config;
        if cfg.project_root.is_none() {
            cfg.project_root = Some(find_project_root(
                effective_start,
                cfg.source_path.as_deref(),
            ));
        } else {
            validate_project_root(cfg.project_root.as_deref().unwrap())?;
        }
        Ok(cfg)
    }

    /// Resolved DB path for the current memory storage mode.
    ///
    /// - Managed: `{data_dir}/coree/managed/{encoded_path}/memory.db`
    /// - Local:   `{local_path}` (relative to project root if not absolute)
    /// - Remote/Replica: managed path (or local_path if set)
    /// - Remote/Direct:  managed path (parent used for serve.lock/ready/crash.log)
    pub fn db_path(&self) -> PathBuf {
        let s = &self.memory.storage;
        match s.mode {
            StorageMode::Managed | StorageMode::Disabled => self
                .managed_base(s)
                .join(encode_project_path(self.project_root()))
                .join("memory.db"),
            StorageMode::Local => self.resolve_local_path(s, ".coree/memory.db"),
            StorageMode::Remote => match s.remote_mode {
                RemoteMode::Replica => {
                    if s.local_path.is_some() {
                        self.resolve_local_path(s, ".coree/memory.replica.db")
                    } else {
                        self.managed_base(s)
                            .join(encode_project_path(self.project_root()))
                            .join("memory.replica.db")
                    }
                }
                RemoteMode::Direct => {
                    // No real local DB; parent dir used for serve.lock/ready/crash.log.
                    self.managed_base(s)
                        .join(encode_project_path(self.project_root()))
                        .join("memory.remote.db")
                }
            },
        }
    }

    /// Path to the lock file held exclusively by `coree serve` for its entire lifetime.
    pub fn serve_lock_path(&self) -> PathBuf {
        self.db_path()
            .parent()
            .map(|p| p.join("serve.lock"))
            .unwrap_or_else(|| PathBuf::from("serve.lock"))
    }

    /// Path to the lock file used for file-watcher leader election across processes.
    pub fn index_watcher_lock_path(&self) -> PathBuf {
        self.index_db_path()
            .parent()
            .map(|p| p.join("index.watcher.lock"))
            .unwrap_or_else(|| PathBuf::from("index.watcher.lock"))
    }

    /// Path to the ready file written by `coree serve` once the DB and embedder are loaded.
    pub fn serve_ready_path(&self) -> PathBuf {
        self.db_path()
            .parent()
            .map(|p| p.join("serve.ready"))
            .unwrap_or_else(|| PathBuf::from("serve.ready"))
    }

    /// Unix socket path for the local IPC channel between `coree serve` and `coree request`.
    /// On Windows the socket path is converted to a named pipe name in serve/request code.
    pub fn serve_socket_path(&self) -> PathBuf {
        self.db_path()
            .parent()
            .map(|p| p.join("coree.sock"))
            .unwrap_or_else(|| PathBuf::from("coree.sock"))
    }

    /// Windows named pipe name derived from the socket path (unique per data directory).
    #[cfg(windows)]
    pub fn serve_pipe_name(&self) -> String {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        self.serve_socket_path().hash(&mut h);
        format!(r"\\.\pipe\coree-{:016x}", h.finish())
    }

    /// Always returns the effective local DB path regardless of remote mode.
    /// Used by `remote enable` as the source/seed database.
    pub fn local_db_path(&self) -> PathBuf {
        let s = &self.memory.storage;
        match s.mode {
            StorageMode::Local => self.resolve_local_path(s, ".coree/memory.db"),
            _ => self
                .managed_base(s)
                .join(encode_project_path(self.project_root()))
                .join("memory.db"),
        }
    }

    /// Path to the code intelligence index database.
    ///
    /// - Managed: `{data_dir}/coree/managed/{encoded_path}/index.db`
    /// - Local:   `{local_path}` (relative to project root if not absolute)
    pub fn index_db_path(&self) -> PathBuf {
        let s = &self.index.storage;
        match s.mode {
            StorageMode::Managed | StorageMode::Disabled | StorageMode::Remote => self
                .managed_base(s)
                .join(encode_project_path(self.project_root()))
                .join("index.db"),
            StorageMode::Local => self.resolve_local_path(s, ".coree/index.db"),
        }
    }

    fn managed_base(&self, s: &StorageConfig) -> PathBuf {
        s.managed_path.clone().unwrap_or_else(|| {
            dirs::data_dir()
                .unwrap_or_else(|| {
                    dirs::home_dir()
                        .unwrap_or_default()
                        .join(".local")
                        .join("share")
                })
                .join("coree")
                .join("managed")
        })
    }

    fn resolve_local_path(&self, s: &StorageConfig, default: &str) -> PathBuf {
        let p = s.local_path.as_deref().unwrap_or(Path::new(default));
        if p.is_absolute() {
            p.to_path_buf()
        } else {
            self.project_root().join(p)
        }
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
        let candidate = dir.join(".coree.toml");
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
/// 1. Parent of the project `.coree.toml` (if found)
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
    dirs::config_dir().map(|d| d.join("coree").join("config.toml"))
}

fn configured_project_root(fig: Figment) -> Result<Option<PathBuf>> {
    let Some(path) = fig
        .extract::<ProjectRootConfig>()
        .context("Failed to load project_root configuration")?
        .project_root
    else {
        return Ok(None);
    };
    validate_project_root(&path)?;
    Ok(Some(path))
}

fn validate_project_root(path: &Path) -> Result<()> {
    if !path.is_absolute() {
        bail!("project_root must be an absolute path: {}", path.display());
    }
    if !path.is_dir() {
        bail!(
            "project_root must point to an existing directory: {}",
            path.display()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn db_path_managed_mode() {
        let cfg = Config {
            project_root: Some(PathBuf::from("/some/project")),
            ..Default::default()
        };
        let path = cfg.db_path();
        assert!(path.ends_with("memory.db"));
        assert!(path.to_string_lossy().contains("coree"));
        assert!(path.to_string_lossy().contains("-some-project"));
    }

    #[test]
    fn db_path_local_mode() {
        let cfg = Config {
            project_root: Some(PathBuf::from("/some/project")),
            memory: MemoryConfig {
                storage: StorageConfig {
                    mode: StorageMode::Local,
                    ..Default::default()
                },
            },
            ..Default::default()
        };
        assert_eq!(
            cfg.db_path(),
            PathBuf::from("/some/project/.coree/memory.db")
        );
    }

    #[test]
    fn db_path_local_mode_explicit_path() {
        let cfg = Config {
            project_root: Some(PathBuf::from("/some/project")),
            memory: MemoryConfig {
                storage: StorageConfig {
                    mode: StorageMode::Local,
                    local_path: Some(PathBuf::from("custom/memory.db")),
                    ..Default::default()
                },
            },
            ..Default::default()
        };
        assert_eq!(
            cfg.db_path(),
            PathBuf::from("/some/project/custom/memory.db")
        );
    }

    #[test]
    fn local_db_path_managed_returns_managed_path() {
        let cfg = Config {
            project_root: Some(PathBuf::from("/some/project")),
            ..Default::default()
        };
        let path = cfg.local_db_path();
        assert!(path.ends_with("memory.db"));
        assert!(path.to_string_lossy().contains("coree"));
    }

    #[test]
    fn find_project_root_uses_project_config_parent() {
        let root = find_project_root(
            Path::new("/some/subdir"),
            Some(Path::new("/some/project/.coree.toml")),
        );
        assert_eq!(root, PathBuf::from("/some/project"));
    }

    #[test]
    fn find_project_root_falls_back_to_start_dir() {
        let root = find_project_root(Path::new("/tmp/norepo"), None);
        assert_eq!(root, PathBuf::from("/tmp/norepo"));
    }

    #[test]
    fn project_root_can_be_configured_from_project_file() {
        let temp = tempfile::tempdir().unwrap();
        let actual_root = temp.path().join("actual");
        let configured_root = temp.path().join("configured");
        std::fs::create_dir_all(&actual_root).unwrap();
        std::fs::create_dir_all(&configured_root).unwrap();
        std::fs::write(
            actual_root.join(".coree.toml"),
            format!("project_root = \"{}\"\n", configured_root.display()),
        )
        .unwrap();

        let cfg = Config::load(&actual_root).unwrap();

        assert_eq!(cfg.project_root(), configured_root.as_path());
        assert_eq!(cfg.source_path, Some(actual_root.join(".coree.toml")));
    }

    #[test]
    fn encode_project_path_replaces_slashes() {
        assert_eq!(
            encode_project_path(Path::new("/home/user/project")),
            "-home-user-project"
        );
        assert_eq!(
            encode_project_path(Path::new("/some/project")),
            "-some-project"
        );
    }
}
