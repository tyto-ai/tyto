use std::fs;
use std::path::Path;

/// Derive the project ID for the given project root.
///
/// Precedence:
/// 1. `config_value` -- from figment (env var or `.memso.toml`)
/// 2. Git remote URL (origin) -- normalised to a slug; unique across machines
/// 3. Canonical full path of `project_root` -- unique per machine, human-readable
/// 4. `"unknown"` -- final fallback
///
/// Always returns a non-empty string. Logs the resolved value to stderr.
pub fn resolve(project_root: &Path, config_value: Option<&str>) -> String {
    if let Some(v) = config_value.filter(|v| !v.is_empty()) {
        return log_and_return(v.to_string(), ".memso.toml / env var");
    }

    if let Some(slug) = git_remote_slug(project_root) {
        return log_and_return(slug, "git remote URL");
    }

    if let Ok(canonical) = project_root.canonicalize() {
        let path_str = canonical.to_string_lossy().replace('\\', "/");
        return log_and_return(path_str, "canonical path");
    }

    log_and_return("unknown".to_string(), "fallback")
}

/// Read `.git/config` under `project_root` and return a normalised slug for the
/// `origin` remote URL, or `None` if no remote is found.
///
/// Normalisation:
/// - Strip protocol prefix (`git@`, `https://`, `ssh://`, etc.)
/// - Strip trailing `.git`
/// - Replace `/`, `:`, `@` with `-`
fn git_remote_slug(project_root: &Path) -> Option<String> {
    let git_config_path = project_root.join(".git").join("config");
    let content = fs::read_to_string(&git_config_path).ok()?;

    let mut in_origin = false;
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            in_origin = trimmed == r#"[remote "origin"]"#;
            continue;
        }
        if in_origin
            && let Some(rest) = trimmed.strip_prefix("url") {
            let url = rest.trim_start_matches([' ', '=']).trim();
            if !url.is_empty() {
                return Some(normalise_remote_url(url));
            }
        }
    }
    None
}

fn normalise_remote_url(url: &str) -> String {
    // Strip protocol prefix
    let s = if let Some(rest) = url.strip_prefix("git@") {
        rest
    } else if let Some(rest) = url.strip_prefix("https://") {
        rest
    } else if let Some(rest) = url.strip_prefix("http://") {
        rest
    } else if let Some(rest) = url.strip_prefix("ssh://") {
        rest
    } else {
        url
    };
    // Strip trailing .git
    let s = s.strip_suffix(".git").unwrap_or(s);
    // Replace separators with hyphens
    s.chars()
        .map(|c| if matches!(c, '/' | ':' | '@') { '-' } else { c })
        .collect()
}

fn log_and_return(id: String, source: &str) -> String {
    eprintln!("memso: project_id = {id:?} (from {source})");
    id
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalise_ssh_remote() {
        assert_eq!(
            normalise_remote_url("git@github.com:beefsack/memso.git"),
            "github.com-beefsack-memso"
        );
    }

    #[test]
    fn normalise_https_remote() {
        assert_eq!(
            normalise_remote_url("https://github.com/beefsack/memso.git"),
            "github.com-beefsack-memso"
        );
    }

    #[test]
    fn normalise_no_git_suffix() {
        assert_eq!(
            normalise_remote_url("https://github.com/beefsack/memso"),
            "github.com-beefsack-memso"
        );
    }
}
