use std::path::Path;
use std::process::Command;

#[derive(Debug, Clone)]
pub struct CommitInfo {
    pub sha: String,
    pub message: String,
}

/// Fetch recent significant commits touching a file.
/// Returns up to `limit` commits, filtered for significance.
pub fn file_commits(repo_root: &Path, file_path: &str, limit: usize) -> Vec<CommitInfo> {
    let output = match Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .arg("log")
        .arg(format!("-n{}", limit * 2)) // over-fetch to account for filtering
        .arg("--oneline")
        .arg("--no-merges")
        .arg("--")
        .arg(file_path)
        .output()
    {
        Ok(o) => o,
        Err(_) => return vec![],
    };

    if !output.status.success() {
        return vec![];
    }

    let stdout = match std::str::from_utf8(&output.stdout) {
        Ok(s) => s,
        Err(_) => return vec![],
    };

    stdout
        .lines()
        .filter_map(|line| {
            let (sha, msg) = line.split_once(' ')?;
            if is_significant(msg) {
                Some(CommitInfo {
                    sha: sha.to_string(),
                    message: msg.to_string(),
                })
            } else {
                None
            }
        })
        .take(limit)
        .collect()
}

/// Returns false for noise commits that pollute the history embedding.
fn is_significant(msg: &str) -> bool {
    if msg.len() < 15 {
        return false;
    }
    let lower = msg.to_lowercase();
    let skip_prefixes = ["merge", "revert", "bump", "wip", "fixup!", "squash!", "chore: bump"];
    skip_prefixes.iter().all(|p| !lower.starts_with(p))
}
