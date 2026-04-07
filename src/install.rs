use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

const MCP_SERVER_NAME: &str = "memso";

fn hook_cmd(bin: &str, suffix: &str) -> String {
    format!("{bin} {suffix}")
}

pub struct InstallResult {
    pub mcp_added: bool,
    pub session_hook_added: bool,
    pub prompt_hook_added: bool,
    pub capture_hook_added: bool,
    pub stop_hook_added: bool,
    pub compact_hook_added: bool,
    pub settings_path: PathBuf,
    pub binary_path: PathBuf,
}

pub fn run(dry_run: bool) -> Result<InstallResult> {
    let binary_path = std::env::current_exe()
        .context("Could not determine path to memso binary")?;
    let bin = binary_path.to_string_lossy();

    let path = settings_path()?;
    let mut root = read_or_empty(&path)?;

    let mcp_added = ensure_mcp_server(&mut root, &binary_path)?;
    let session_hook_added = ensure_hook(&mut root, "SessionStart", &hook_cmd(&bin, "inject --type session --budget 32000"))?;
    let prompt_hook_added = ensure_hook(&mut root, "UserPromptSubmit", &hook_cmd(&bin, "inject --type prompt --budget 32000"))?;
    let capture_hook_added = ensure_hook(&mut root, "PostToolUse", &hook_cmd(&bin, "capture"))?;
    let stop_hook_added = ensure_hook(&mut root, "Stop", &hook_cmd(&bin, "inject --type stop --budget 32000"))?;
    let compact_hook_added = ensure_hook(&mut root, "PostCompact", &hook_cmd(&bin, "inject --type compact --budget 32000"))?;

    let changed = mcp_added || session_hook_added || prompt_hook_added
        || capture_hook_added || stop_hook_added || compact_hook_added;

    if changed && !dry_run {
        write_settings(&path, &root)?;
    }

    Ok(InstallResult {
        mcp_added,
        session_hook_added,
        prompt_hook_added,
        capture_hook_added,
        stop_hook_added,
        compact_hook_added,
        settings_path: path,
        binary_path,
    })
}

/// Ensure `mcpServers.memso` exists with the correct command and args.
/// Returns true if a change was made.
fn ensure_mcp_server(root: &mut Value, binary_path: &Path) -> Result<bool> {
    let cmd = binary_path.to_string_lossy();
    let obj = root
        .as_object_mut()
        .context("settings.json root is not a JSON object")?;

    let servers = obj.entry("mcpServers").or_insert_with(|| json!({}));
    if !servers.is_object() {
        anyhow::bail!("settings.json 'mcpServers' is not an object");
    }

    if let Some(existing) = servers.get(MCP_SERVER_NAME) {
        let cmd_ok = existing.get("command").and_then(|v| v.as_str()) == Some(cmd.as_ref());
        let args_ok = existing
            .get("args")
            .and_then(|v| v.as_array())
            .map(|a| a == &[json!("serve")])
            .unwrap_or(false);
        if cmd_ok && args_ok {
            return Ok(false);
        }
    }

    servers[MCP_SERVER_NAME] = json!({
        "type": "stdio",
        "command": cmd,
        "args": ["serve"]
    });
    Ok(true)
}

/// Ensure a hook entry with the given command exists under the given event.
/// Returns true if a change was made.
fn ensure_hook(root: &mut Value, event: &str, command: &str) -> Result<bool> {
    let obj = root
        .as_object_mut()
        .context("settings.json root is not a JSON object")?;

    let hooks_map = obj.entry("hooks").or_insert_with(|| json!({}));
    let hooks_obj = hooks_map
        .as_object_mut()
        .context("settings.json 'hooks' is not an object")?;

    let event_list = hooks_obj.entry(event).or_insert_with(|| json!([]));
    let list = event_list
        .as_array_mut()
        .with_context(|| format!("settings.json hooks.{event} is not an array"))?;

    // Check if any existing entry already contains this command
    let already_present = list.iter().any(|entry| {
        // Flat format: {"matcher": "", "hooks": [{"type": "command", "command": "..."}]}
        if let Some(inner) = entry.get("hooks").and_then(|h| h.as_array())
            && inner.iter().any(|h| h.get("command").and_then(|c| c.as_str()) == Some(command))
        {
            return true;
        }
        // Simple format: {"command": "..."}
        entry.get("command").and_then(|c| c.as_str()) == Some(command)
    });

    if already_present {
        return Ok(false);
    }

    list.push(json!({
        "matcher": "",
        "hooks": [
            {
                "type": "command",
                "command": command
            }
        ]
    }));
    Ok(true)
}

fn settings_path() -> Result<PathBuf> {
    let home = dirs::home_dir().context("Could not determine home directory")?;
    Ok(home.join(".claude").join("settings.json"))
}

fn read_or_empty(path: &Path) -> Result<Value> {
    if !path.exists() {
        return Ok(json!({}));
    }
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("Failed to read {}", path.display()))?;
    if text.trim().is_empty() {
        return Ok(json!({}));
    }
    serde_json::from_str(&text)
        .with_context(|| format!("Failed to parse JSON in {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ensure_mcp_server_adds_when_absent() {
        let mut root = json!({});
        let bin = Path::new("/usr/local/bin/memso");
        let changed = ensure_mcp_server(&mut root, bin).unwrap();
        assert!(changed);
        assert_eq!(root["mcpServers"]["memso"]["command"], "/usr/local/bin/memso");
        assert_eq!(root["mcpServers"]["memso"]["args"], json!(["serve"]));
    }

    #[test]
    fn ensure_mcp_server_skips_when_correct() {
        let mut root = json!({
            "mcpServers": {"memso": {"type": "stdio", "command": "/usr/local/bin/memso", "args": ["serve"]}}
        });
        let bin = Path::new("/usr/local/bin/memso");
        let changed = ensure_mcp_server(&mut root, bin).unwrap();
        assert!(!changed);
    }

    #[test]
    fn ensure_mcp_server_fixes_wrong_args() {
        let mut root = json!({
            "mcpServers": {"memso": {"type": "stdio", "command": "/usr/local/bin/memso", "args": ["wrong"]}}
        });
        let bin = Path::new("/usr/local/bin/memso");
        let changed = ensure_mcp_server(&mut root, bin).unwrap();
        assert!(changed, "should overwrite when args are wrong");
        assert_eq!(root["mcpServers"]["memso"]["args"], json!(["serve"]));
    }

    #[test]
    fn ensure_hook_adds_when_absent() {
        let mut root = json!({});
        let changed = ensure_hook(&mut root, "SessionStart", "memso inject --type session").unwrap();
        assert!(changed);
        let hooks = root["hooks"]["SessionStart"].as_array().unwrap();
        assert_eq!(hooks.len(), 1);
    }

    #[test]
    fn ensure_hook_skips_when_present() {
        let cmd = "memso inject --type session";
        let mut root = json!({
            "hooks": {"SessionStart": [{"matcher": "", "hooks": [{"type": "command", "command": cmd}]}]}
        });
        let changed = ensure_hook(&mut root, "SessionStart", cmd).unwrap();
        assert!(!changed);
    }

    #[test]
    fn ensure_hook_errors_on_non_object_hooks() {
        let mut root = json!({"hooks": "not-an-object"});
        let result = ensure_hook(&mut root, "SessionStart", "cmd");
        assert!(result.is_err());
    }
}

fn write_settings(path: &Path, value: &Value) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let text = serde_json::to_string_pretty(value)? + "\n";
    // Write to a temp file then rename for atomicity - avoids a corrupt
    // settings.json on crash or disk-full mid-write.
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, &text)
        .with_context(|| format!("Failed to write {}", tmp.display()))?;
    std::fs::rename(&tmp, path)
        .with_context(|| format!("Failed to rename {} to {}", tmp.display(), path.display()))
}
