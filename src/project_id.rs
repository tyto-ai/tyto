use std::path::Path;

/// Derive the project ID for the given working directory.
///
/// Precedence:
/// 1. `project_id` in `.memso.toml` (passed in as `config_value`)
/// 2. Basename of the working directory
///
/// Use `MEMSO_MEMORY__PROJECT_ID` env var to override via Figment (takes effect before this).
/// Always returns a non-empty string. Logs the resolved value to stderr.
pub fn resolve(cwd: &Path, config_value: Option<&str>) -> String {
    if let Some(v) = config_value
        && !v.is_empty()
    {
        return log_and_return(v.to_string(), ".memso.toml");
    }

    let basename = cwd
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("unknown")
        .to_string();

    log_and_return(basename, "CWD basename")
}

fn log_and_return(id: String, source: &str) -> String {
    eprintln!("memso: project_id = {id:?} (from {source})");
    id
}
