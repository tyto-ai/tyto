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

/// Returns the SHA and message of the current HEAD commit, or None if not in a git repo.
pub fn head_commit(repo_root: &Path) -> Option<CommitInfo> {
    let output = Command::new("git")
        .arg("-C").arg(repo_root)
        .arg("log").arg("-1").arg("--oneline")
        .output()
        .ok()?;
    if !output.status.success() { return None; }
    let s = std::str::from_utf8(&output.stdout).ok()?.trim();
    let (sha, msg) = s.split_once(' ')?;
    Some(CommitInfo { sha: sha.to_string(), message: msg.to_string() })
}

/// Returns relative paths of files changed in HEAD (works for initial commits too).
pub fn files_in_head_commit(repo_root: &Path) -> Vec<String> {
    let output = match Command::new("git")
        .arg("-C").arg(repo_root)
        .args(["diff-tree", "--no-commit-id", "-r", "--name-only", "HEAD"])
        .output()
    {
        Ok(o) => o,
        Err(_) => return vec![],
    };
    if !output.status.success() { return vec![]; }
    let s = match std::str::from_utf8(&output.stdout) {
        Ok(s) => s,
        Err(_) => return vec![],
    };
    s.lines().map(|l| l.to_string()).filter(|l| !l.is_empty()).collect()
}

/// Returns false for noise commits that pollute the history embedding.
pub(crate) fn is_significant(msg: &str) -> bool {
    if msg.len() < 15 {
        return false;
    }
    let lower = msg.to_lowercase();
    let skip_prefixes = ["merge", "revert", "bump", "wip", "fixup!", "squash!", "chore: bump"];
    skip_prefixes.iter().all(|p| !lower.starts_with(p))
}

#[cfg(test)]
mod tests {
    use super::is_significant;

    #[test]
    fn significant_normal_commit() {
        assert!(is_significant("feat: add user authentication"));
        assert!(is_significant("fix: resolve race condition in indexer"));
        assert!(is_significant("refactor: extract DbReady into separate struct"));
    }

    #[test]
    fn insignificant_too_short() {
        assert!(!is_significant("fix"));
        assert!(!is_significant("wip"));
        assert!(!is_significant("tmp fix"));
        // Exactly 14 chars: below the 15-char threshold
        assert!(!is_significant("short message!"));
    }

    #[test]
    fn insignificant_noise_prefixes() {
        assert!(!is_significant("Merge pull request #42 from foo/bar"));
        assert!(!is_significant("merge branch main into feature/x"));
        assert!(!is_significant("revert \"feat: add something\""));
        assert!(!is_significant("bump version to 1.2.3"));
        assert!(!is_significant("WIP: half-done refactor"));
        assert!(!is_significant("fixup! fix: typo in comment"));
        assert!(!is_significant("squash! feat: add auth"));
        assert!(!is_significant("chore: bump dependencies"));
    }

    #[test]
    fn prefix_match_is_case_insensitive() {
        assert!(!is_significant("MERGE branch main into dev"));
        assert!(!is_significant("Revert previous commit because it broke things"));
        assert!(!is_significant("Bump serde from 1.0.1 to 1.0.2"));
    }

    #[test]
    fn non_noise_prefix_containing_noise_word_is_significant() {
        // "merged" starts with "merge" — must be filtered
        assert!(!is_significant("merged the auth feature into main branch"));
        // "bumping" starts with "bump" — must be filtered
        assert!(!is_significant("bumping all deps to latest versions"));
        // But a commit that merely contains the word mid-sentence is fine
        assert!(is_significant("fix: don't revert index on partial failure"));
    }
}
